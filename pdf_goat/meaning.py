"""Optional meaning-based passage search.

Literal search is the default and never imports this module. Meaning search
ranks passages by a static embedding model that lives on disk under
``$PDF_GOAT_HOME/models``. The model is installed once, by hand, with
``pdf-goat setup meaning``; searching never downloads anything. With no model
installed a meaning search fails and says how to install it, so an agent can
tell "no match" apart from "no model".

The model is a Model2Vec static embedding: one vector per tokenizer token, a
sentence embedding is the mean of its token vectors. There is no neural
network at search time, only a tokenizer and one matrix lookup, which is why
this fits a command-line tool that must stay responsive without a resident
service.

Every file is pinned by revision, size and SHA-256, and is verified on every
load, so a half-written or swapped model is an error rather than silently
different results.
"""

from __future__ import annotations

import hashlib
import json
import os
import shlex
from pathlib import Path

# minishlab/potion-base-8M, MIT licensed, distilled from BAAI/bge-base-en-v1.5.
# 256 dimensions, 29.5 MiB on disk. Revision is pinned: "main" would let the
# same command produce different vectors on different days.
MODEL_ID = "minishlab/potion-base-8M"
MODEL_REVISION = "bf8b056651a2c21b8d2565580b8569da283cab23"
MODEL_LICENSE = "MIT"
MODEL_DIM = 256

# name: (sha256, size in bytes)
MODEL_FILES = {
    "model.safetensors": (
        "f65d0f325faadc1e121c319e2faa41170d3fa07d8c89abd48ca5358d9a223de2",
        30236760,
    ),
    "tokenizer.json": (
        "e67e803f624fb4d67dea1c730d06e1067e1b14d830e2c2202569e3ef0f70bb50",
        683666,
    ),
    "config.json": (
        "2a6ac0e9aaa356a68a5688070db78fc3a464fefe85d2f06a1905ce3718687553",
        202,
    ),
}

_TENSOR_NAME = "embeddings"
_UNKNOWN_TOKEN = "[UNK]"
_DOWNLOAD_TIMEOUT = 120


class MeaningError(Exception):
    """A meaning search could not run. The message is written for an operator."""


def model_dir(home):
    """Return the directory holding the installed model."""
    return Path(home) / "models" / MODEL_ID.split("/")[-1]


def _user_agent():
    return os.environ.get("PDF_GOAT_USER_AGENT", "pdf-goat/1.0 (model setup)")


def _digest(path):
    """Return (sha256, size) for path, reading it in chunks."""
    hasher = hashlib.sha256()
    size = 0
    with open(path, "rb") as handle:
        while True:
            chunk = handle.read(1 << 20)
            if not chunk:
                break
            hasher.update(chunk)
            size += len(chunk)
    return hasher.hexdigest(), size


def status(home):
    """Report which model files are installed and whether they verify."""
    directory = model_dir(home)
    files = []
    for name, (expected_digest, expected_size) in MODEL_FILES.items():
        path = directory / name
        entry = {"name": name, "path": str(path), "installed": path.is_file()}
        if entry["installed"]:
            actual_digest, actual_size = _digest(path)
            entry["size"] = actual_size
            entry["verified"] = (
                actual_digest == expected_digest and actual_size == expected_size
            )
        else:
            entry["size"] = 0
            entry["verified"] = False
        files.append(entry)
    return {
        "id": MODEL_ID,
        "revision": MODEL_REVISION,
        "license": MODEL_LICENSE,
        "dim": MODEL_DIM,
        "directory": str(directory),
        "installed": all(entry["verified"] for entry in files),
        "files": files,
    }


def _read_verified(path, name):
    expected_digest, expected_size = MODEL_FILES[name]
    try:
        data = path.read_bytes()
    except FileNotFoundError:
        raise MeaningError(
            f"the meaning model is not installed ({path} is missing); "
            "run: pdf-goat setup meaning"
        ) from None
    if len(data) != expected_size or hashlib.sha256(data).hexdigest() != expected_digest:
        raise MeaningError(
            f"{path} does not match the pinned {MODEL_ID} revision "
            f"{MODEL_REVISION}; re-run: pdf-goat setup meaning"
        )
    return data


def _load_matrix(data):
    """Read the single embedding tensor out of a safetensors buffer.

    The file has already been checked against its pinned SHA-256, so this
    parses known bytes: an 8-byte little-endian header length, a JSON header,
    then the tensor data.
    """
    import numpy as np

    header_length = int.from_bytes(data[:8], "little")
    header = json.loads(data[8 : 8 + header_length])
    entry = header.get(_TENSOR_NAME)
    if entry is None:  # pragma: no cover - pinned file always has it
        raise MeaningError(f"{MODEL_ID} has no '{_TENSOR_NAME}' tensor")
    if entry["dtype"] != "F32":  # pragma: no cover - pinned file is float32
        raise MeaningError(f"{MODEL_ID} embeddings are {entry['dtype']}, expected F32")
    start, end = entry["data_offsets"]
    base = 8 + header_length
    matrix = np.frombuffer(data, dtype="<f4", count=(end - start) // 4, offset=base + start)
    return matrix.reshape(entry["shape"])


class Model:
    """A loaded static embedding model.

    Callable from anywhere that has passage text: the command line today, a
    viewer adapter later. Hold one instance and reuse it; loading reads and
    verifies 29.5 MiB.
    """

    def __init__(self, tokenizer, embedding):
        self._tokenizer = tokenizer
        self._embedding = embedding
        self._unknown = tokenizer.token_to_id(_UNKNOWN_TOKEN)

    @property
    def dim(self):
        return int(self._embedding.shape[1])

    def encode(self, texts):
        """Return one unit-length row per text, in the order given.

        No text is truncated: a passage the length of a page contributes every
        one of its tokens to the mean. A text with no known tokens gets a zero
        row, which scores 0 against every query rather than being dropped.
        """
        import numpy as np

        texts = list(texts)
        out = np.zeros((len(texts), self.dim), dtype=np.float32)
        if not texts:
            return out
        for row, encoding in enumerate(
            self._tokenizer.encode_batch(texts, add_special_tokens=False)
        ):
            ids = encoding.ids
            if self._unknown is not None:
                ids = [token for token in ids if token != self._unknown]
            if ids:
                out[row] = self._embedding[ids].mean(axis=0)
        norms = np.linalg.norm(out, axis=1, keepdims=True)
        np.divide(out, np.maximum(norms, 1e-12), out=out)
        return out

    def score(self, query, texts):
        """Return the cosine similarity of query against each text, in order.

        Every text gets a score. Nothing is filtered here: a caller that wants
        the best few sorts and slices, and still knows how many there were.
        """
        rows = self.encode([query, *texts])
        return [float(value) for value in rows[1:] @ rows[0]]


def load(home):
    """Load the installed model, or raise MeaningError explaining what is missing.

    This never reaches the network. If the model is absent the answer is an
    error, not a download.
    """
    try:
        from tokenizers import Tokenizer
    except ImportError as error:
        raise MeaningError(
            "meaning search needs the optional dependencies; "
            "install them with: uv sync --project "
            f"{shlex.quote(str(Path(__file__).resolve().parents[1]))} --extra meaning"
        ) from error
    try:
        import numpy  # noqa: F401
    except ImportError as error:  # pragma: no cover - numpy ships with the deps
        raise MeaningError(
            "meaning search needs numpy; install it with: uv sync --project "
            f"{shlex.quote(str(Path(__file__).resolve().parents[1]))} --extra meaning"
        ) from error

    directory = model_dir(home)
    tokenizer = Tokenizer.from_str(
        _read_verified(directory / "tokenizer.json", "tokenizer.json").decode("utf-8")
    )
    # Upstream Model2Vec inference truncates at 512 tokens by default, which
    # would drop the rest of a dense page without saying so. This pinned
    # tokenizer carries no truncation rule of its own; clearing it keeps that
    # true if a later pinned revision arrives with one. Every token a passage
    # has reaches its vector.
    tokenizer.no_truncation()
    tokenizer.no_padding()
    embedding = _load_matrix(
        _read_verified(directory / "model.safetensors", "model.safetensors")
    )
    if embedding.shape[1] != MODEL_DIM:  # pragma: no cover - pinned file is 256
        raise MeaningError(
            f"{MODEL_ID} has {embedding.shape[1]} dimensions, expected {MODEL_DIM}"
        )
    return Model(tokenizer, embedding)


def install(home, log=None, force=False):
    """Download and verify the pinned model. The only code here that uses the network."""
    import urllib.error
    import urllib.request

    directory = model_dir(home)
    directory.mkdir(parents=True, exist_ok=True)
    written = []
    for name, (expected_digest, expected_size) in MODEL_FILES.items():
        path = directory / name
        if not force and path.is_file():
            actual_digest, actual_size = _digest(path)
            if actual_digest == expected_digest and actual_size == expected_size:
                if log:
                    log(f"{name}: already installed")
                written.append({"name": name, "bytes": actual_size, "downloaded": False})
                continue
        url = f"https://huggingface.co/{MODEL_ID}/resolve/{MODEL_REVISION}/{name}"
        if log:
            log(f"{name}: downloading {expected_size} bytes")
        temporary = path.with_name(name + ".part")
        hasher = hashlib.sha256()
        size = 0
        try:
            request = urllib.request.Request(url, headers={"User-Agent": _user_agent()})
            with urllib.request.urlopen(request, timeout=_DOWNLOAD_TIMEOUT) as response:
                with open(temporary, "wb") as handle:
                    while True:
                        chunk = response.read(1 << 20)
                        if not chunk:
                            break
                        hasher.update(chunk)
                        size += len(chunk)
                        handle.write(chunk)
        except urllib.error.URLError as error:
            temporary.unlink(missing_ok=True)
            raise MeaningError(f"could not download {url}: {error}") from error
        if size != expected_size or hasher.hexdigest() != expected_digest:
            temporary.unlink(missing_ok=True)
            raise MeaningError(
                f"{url} did not match the pinned checksum; nothing was installed"
            )
        temporary.replace(path)
        written.append({"name": name, "bytes": size, "downloaded": True})
    return {"directory": str(directory), "files": written}
