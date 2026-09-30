//! Pinned Model2Vec static embeddings for meaning search.
//!
//! Literal search never touches this crate. Meaning search ranks passages by
//! a static embedding model that lives on disk under `<home>/models`. The
//! model is installed once, by hand, with `pdf-goat setup meaning`; searching
//! never downloads anything. With no model installed a meaning search fails
//! and says how to install it, so an agent can tell "no match" apart from
//! "no model".
//!
//! The model is a Model2Vec static embedding: one vector per tokenizer token,
//! and a sentence embedding is the mean of its token vectors. Every file is
//! pinned by revision, size and SHA-256 and is verified on every load, so a
//! half-written or swapped model is an error rather than silently different
//! results.

mod safetensors;
mod tokenizer;
#[cfg(test)]
mod tokenizer_tests;

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use safetensors::Matrix;
use tokenizer::Tokenizer;

/// minishlab/potion-base-8M, MIT licensed, distilled from BAAI/bge-base-en-v1.5.
pub const MODEL_ID: &str = "minishlab/potion-base-8M";
/// Pinned revision: "main" would let the same command produce different
/// vectors on different days.
pub const MODEL_REVISION: &str = "bf8b056651a2c21b8d2565580b8569da283cab23";
pub const MODEL_LICENSE: &str = "MIT";
pub const MODEL_DIM: usize = 256;

/// One pinned model file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedFile {
    pub name: &'static str,
    /// Lowercase hex SHA-256 of the file.
    pub sha256: &'static str,
    pub size: u64,
}

/// Every file the model needs, in install and report order.
pub const MODEL_FILES: [PinnedFile; 3] = [
    PinnedFile {
        name: "model.safetensors",
        sha256: "f65d0f325faadc1e121c319e2faa41170d3fa07d8c89abd48ca5358d9a223de2",
        size: 30_236_760,
    },
    PinnedFile {
        name: "tokenizer.json",
        sha256: "e67e803f624fb4d67dea1c730d06e1067e1b14d830e2c2202569e3ef0f70bb50",
        size: 683_666,
    },
    PinnedFile {
        name: "config.json",
        sha256: "2a6ac0e9aaa356a68a5688070db78fc3a464fefe85d2f06a1905ce3718687553",
        size: 202,
    },
];
const MODEL_FILE: &PinnedFile = &MODEL_FILES[0];
const TOKENIZER_FILE: &PinnedFile = &MODEL_FILES[1];

const UNKNOWN_TOKEN: &str = "[UNK]";
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const CHUNK: usize = 1 << 20;

/// A meaning search or model setup could not run. The message is written for
/// an operator and is the user-facing error text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeaningError {
    message: String,
}

impl MeaningError {
    pub(crate) fn new(message: String) -> Self {
        Self { message }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for MeaningError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for MeaningError {}

/// The directory holding the installed model: `<home>/models/potion-base-8M`.
pub fn model_dir(home: &Path) -> PathBuf {
    let short = MODEL_ID.rsplit('/').next().unwrap_or(MODEL_ID);
    home.join("models").join(short)
}

fn user_agent() -> String {
    std::env::var("PDF_GOAT_USER_AGENT").unwrap_or_else(|_| "pdf-goat/1.0 (model setup)".into())
}

fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// (sha256 hex, size) of the file at `path`, read in chunks.
fn digest(path: &Path) -> io::Result<(String, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; CHUNK];
    let mut size = 0u64;
    loop {
        let read = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Ok((hex(&hasher.finalize()), size))
}

/// One file's entry in [`Status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileStatus {
    pub name: &'static str,
    pub path: String,
    pub installed: bool,
    /// Bytes on disk, 0 when missing.
    pub size: u64,
    pub verified: bool,
}

/// Which model files are installed and whether they verify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    pub id: &'static str,
    pub revision: &'static str,
    pub license: &'static str,
    pub dim: usize,
    pub directory: String,
    /// True when every file verifies.
    pub installed: bool,
    pub files: Vec<FileStatus>,
}

/// Report which model files are installed and whether they verify. Never
/// reaches the network.
pub fn status(home: &Path) -> Status {
    let directory = model_dir(home);
    let files: Vec<FileStatus> = MODEL_FILES
        .iter()
        .map(|pinned| {
            let path = directory.join(pinned.name);
            let installed = path.is_file();
            let (size, verified) = match installed.then(|| digest(&path)) {
                Some(Ok((actual_digest, actual_size))) => (
                    actual_size,
                    actual_digest == pinned.sha256 && actual_size == pinned.size,
                ),
                // An unreadable file cannot verify.
                Some(Err(_)) | None => (0, false),
            };
            FileStatus {
                name: pinned.name,
                path: path.display().to_string(),
                installed,
                size,
                verified,
            }
        })
        .collect();
    Status {
        id: MODEL_ID,
        revision: MODEL_REVISION,
        license: MODEL_LICENSE,
        dim: MODEL_DIM,
        directory: directory.display().to_string(),
        installed: files.iter().all(|file| file.verified),
        files,
    }
}

/// The `setup status` verb result in its documented key order.
pub fn setup_status_result(status: &Status) -> Value {
    json!({
        "verb": "setup status",
        "inputs": [],
        "outputs": [],
        "installed": status.installed,
        "model": {
            "id": status.id,
            "revision": status.revision,
            "license": status.license,
            "dim": status.dim,
        },
        "directory": status.directory,
        "files": status.files,
    })
}

fn read_verified(path: &Path, pinned: &PinnedFile) -> Result<Vec<u8>, MeaningError> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(MeaningError::new(format!(
                "the meaning model is not installed ({} is missing); run: pdf-goat setup meaning",
                path.display()
            )));
        }
        Err(error) => {
            return Err(MeaningError::new(format!(
                "could not read {}: {error}",
                path.display()
            )));
        }
    };
    if data.len() as u64 != pinned.size || hex(&Sha256::digest(&data)) != pinned.sha256 {
        return Err(MeaningError::new(format!(
            "{} does not match the pinned {MODEL_ID} revision {MODEL_REVISION}; \
             re-run: pdf-goat setup meaning",
            path.display()
        )));
    }
    Ok(data)
}

/// A loaded static embedding model. Hold one and reuse it: loading reads and
/// verifies 29.5 MiB. Safe to share across threads.
pub struct Model {
    tokenizer: Tokenizer,
    embedding: Matrix,
    unknown: Option<u32>,
}

/// Load the installed model, or explain what is missing. Never reaches the
/// network: an absent model is an error, not a download.
pub fn load(home: &Path) -> Result<Model, MeaningError> {
    let directory = model_dir(home);
    let tokenizer_path = directory.join(TOKENIZER_FILE.name);
    let tokenizer_bytes = read_verified(&tokenizer_path, TOKENIZER_FILE)?;
    let tokenizer_text = std::str::from_utf8(&tokenizer_bytes)
        .map_err(|_| MeaningError::new(format!("{MODEL_ID} tokenizer.json is not UTF-8")))?;
    // The pinned tokenizer declares no truncation or padding and neither is
    // implemented, so every token a passage has reaches its vector.
    let tokenizer = Tokenizer::from_json(tokenizer_text)?;

    let model_path = directory.join(MODEL_FILE.name);
    let embedding = safetensors::load_matrix(&read_verified(&model_path, MODEL_FILE)?)?;
    if embedding.cols != MODEL_DIM {
        return Err(MeaningError::new(format!(
            "{MODEL_ID} has {} dimensions, expected {MODEL_DIM}",
            embedding.cols
        )));
    }
    if tokenizer
        .max_id()
        .is_some_and(|id| id as usize >= embedding.rows)
    {
        return Err(MeaningError::new(format!(
            "{MODEL_ID} tokenizer ids run past the {} embedding rows",
            embedding.rows
        )));
    }
    let unknown = tokenizer.token_to_id(UNKNOWN_TOKEN);
    Ok(Model {
        tokenizer,
        embedding,
        unknown,
    })
}

impl Model {
    /// Embedding width, 256 for the pinned model.
    pub fn dim(&self) -> usize {
        self.embedding.cols
    }

    /// One unit-length row per text, in the order given.
    ///
    /// No text is truncated: a passage the length of a page contributes every
    /// one of its tokens to the mean. Unknown tokens are dropped. A text with
    /// no known tokens gets a zero row, which scores 0 against every query
    /// rather than being dropped.
    pub fn encode<S: AsRef<str>>(&self, texts: &[S]) -> Vec<Vec<f32>> {
        texts
            .iter()
            .map(|text| self.encode_one(text.as_ref()))
            .collect()
    }

    fn encode_one(&self, text: &str) -> Vec<f32> {
        let dim = self.dim();
        let mut row = vec![0f32; dim];
        let mut count = 0u32;
        for id in self.tokenizer.encode(text) {
            if Some(id) == self.unknown {
                continue;
            }
            let start = id as usize * dim;
            // `load` checked every id against the row count.
            let Some(vector) = self.embedding.values.get(start..start + dim) else {
                continue;
            };
            for (sum, value) in row.iter_mut().zip(vector) {
                *sum += value;
            }
            count += 1;
        }
        if count > 0 {
            let count = count as f32;
            for value in &mut row {
                *value /= count;
            }
        }
        let norm = row
            .iter()
            .map(|&value| f64::from(value) * f64::from(value))
            .sum::<f64>()
            .sqrt() as f32;
        let norm = norm.max(1e-12);
        for value in &mut row {
            *value /= norm;
        }
        row
    }

    /// Cosine similarity of `query` against each text, in order. Every text
    /// gets a score; a caller that wants the best few sorts and slices.
    pub fn score<S: AsRef<str>>(&self, query: &str, texts: &[S]) -> Vec<f64> {
        let query = self.encode_one(query);
        texts
            .iter()
            .map(|text| {
                self.encode_one(text.as_ref())
                    .iter()
                    .zip(&query)
                    .map(|(&a, &b)| f64::from(a) * f64::from(b))
                    .sum()
            })
            .collect()
    }
}

/// The `model` object in a `search --meaning` result.
pub fn search_model_json(model: &Model) -> Value {
    json!({
        "id": MODEL_ID,
        "revision": MODEL_REVISION,
        "dim": model.dim(),
    })
}

/// A score rounded to 4 decimals exactly as Python's `round(score, 4)` does
/// (correctly rounded from the binary value, ties to even), which is how a
/// `search --meaning` hit reports its `score`.
pub fn round_score(score: f64) -> f64 {
    format!("{score:.4}").parse().unwrap_or(score)
}

/// One file handled by [`install`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstalledFile {
    pub name: &'static str,
    pub bytes: u64,
    /// False when a verified copy was already in place.
    pub downloaded: bool,
}

/// What [`install`] left on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Installed {
    pub directory: String,
    pub files: Vec<InstalledFile>,
}

/// The `setup meaning` verb result in its documented key order.
pub fn setup_meaning_result(installed: &Installed) -> Value {
    json!({
        "verb": "setup meaning",
        "inputs": [],
        "outputs": [installed.directory],
        "model": {
            "id": MODEL_ID,
            "revision": MODEL_REVISION,
            "license": MODEL_LICENSE,
        },
        "files": installed.files,
    })
}

/// The pinned download URL of one model file.
pub fn file_url(name: &str) -> String {
    format!("https://huggingface.co/{MODEL_ID}/resolve/{MODEL_REVISION}/{name}")
}

/// Download and verify the pinned model; the only network code in the crate.
///
/// `log` receives one progress line per file (`"<name>: already installed"`
/// or `"<name>: downloading <size> bytes"`); the CLI prints each to stderr as
/// `pdf-goat: <line>`. A file already present and verified is kept unless
/// `force`. A download that does not match its pin is removed and nothing is
/// installed in its place.
pub fn install(
    home: &Path,
    force: bool,
    log: &mut dyn FnMut(&str),
) -> Result<Installed, MeaningError> {
    let directory = model_dir(home);
    fs::create_dir_all(&directory).map_err(|error| {
        MeaningError::new(format!("could not create {}: {error}", directory.display()))
    })?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_resolve(Some(DOWNLOAD_TIMEOUT))
        .timeout_connect(Some(DOWNLOAD_TIMEOUT))
        .timeout_send_request(Some(DOWNLOAD_TIMEOUT))
        .timeout_recv_response(Some(DOWNLOAD_TIMEOUT))
        .build()
        .into();
    let agent_name = user_agent();
    let mut written = Vec::with_capacity(MODEL_FILES.len());
    for pinned in &MODEL_FILES {
        let path = directory.join(pinned.name);
        if !force
            && path.is_file()
            && let Ok((actual_digest, actual_size)) = digest(&path)
            && actual_digest == pinned.sha256
            && actual_size == pinned.size
        {
            log(&format!("{}: already installed", pinned.name));
            written.push(InstalledFile {
                name: pinned.name,
                bytes: actual_size,
                downloaded: false,
            });
            continue;
        }
        let url = file_url(pinned.name);
        log(&format!(
            "{}: downloading {} bytes",
            pinned.name, pinned.size
        ));
        let temporary = directory.join(format!("{}.part", pinned.name));
        let result = download(&agent, &agent_name, &url, &temporary, pinned.size);
        let (actual_digest, size) = match result {
            Ok(done) => done,
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(MeaningError::new(format!(
                    "could not download {url}: {error}"
                )));
            }
        };
        if size != pinned.size || actual_digest != pinned.sha256 {
            let _ = fs::remove_file(&temporary);
            return Err(MeaningError::new(format!(
                "{url} did not match the pinned checksum; nothing was installed"
            )));
        }
        fs::rename(&temporary, &path).map_err(|error| {
            MeaningError::new(format!("could not install {}: {error}", path.display()))
        })?;
        written.push(InstalledFile {
            name: pinned.name,
            bytes: size,
            downloaded: true,
        });
    }
    Ok(Installed {
        directory: directory.display().to_string(),
        files: written,
    })
}

/// Stream `url` into `temporary`, returning (sha256 hex, bytes). Stops one
/// byte past `expected_size`, which is already a mismatch.
fn download(
    agent: &ureq::Agent,
    agent_name: &str,
    url: &str,
    temporary: &Path,
    expected_size: u64,
) -> Result<(String, u64), String> {
    let response = agent
        .get(url)
        .header("User-Agent", agent_name)
        .call()
        .map_err(describe_http_error)?;
    let mut reader = response.into_body().into_reader();
    let mut file = File::create(temporary).map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; CHUNK];
    let mut size = 0u64;
    while size <= expected_size {
        let read = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("<urlopen error {error}>")),
        };
        hasher.update(&buffer[..read]);
        size += read as u64;
        file.write_all(&buffer[..read])
            .map_err(|error| error.to_string())?;
    }
    file.flush().map_err(|error| error.to_string())?;
    Ok((hex(&hasher.finalize()), size))
}

/// The same wording Python's urllib errors print: `HTTP Error 404: Not Found`
/// for a status, `<urlopen error ...>` for a transport failure.
fn describe_http_error(error: ureq::Error) -> String {
    match error {
        ureq::Error::StatusCode(code) => {
            let reason = ureq::http::StatusCode::from_u16(code)
                .ok()
                .and_then(|status| status.canonical_reason())
                .unwrap_or("");
            format!("HTTP Error {code}: {reason}")
        }
        other => format!("<urlopen error {other}>"),
    }
}
