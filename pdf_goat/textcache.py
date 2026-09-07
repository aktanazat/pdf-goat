"""Disposable per-page extraction cache for the CLI text verbs."""

from __future__ import annotations

import os
import time
from array import array
from pathlib import Path

_MIB = 1024 * 1024
_HASH_WINDOW = 2 * _MIB
# Budget the SQLite record and key fields so empty-page rows consume the cap.
_ROW_OVERHEAD = 64
_SQL_VARIABLE_LIMIT = 900

_SCHEMA = """
CREATE TABLE IF NOT EXISTS documents (
    id INTEGER PRIMARY KEY,
    size_bytes INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    digest BLOB NOT NULL,
    path TEXT NOT NULL,
    page_count INTEGER NOT NULL,
    row_bytes INTEGER NOT NULL DEFAULT 0,
    last_used INTEGER NOT NULL,
    UNIQUE(size_bytes, mtime_ns, digest)
);
CREATE TABLE IF NOT EXISTS pages (
    document_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    form TEXT NOT NULL,
    page_index INTEGER NOT NULL,
    text_value TEXT,
    char_count INTEGER,
    word_count INTEGER,
    word_text TEXT,
    rects BLOB,
    lines BLOB,
    row_bytes INTEGER NOT NULL,
    PRIMARY KEY(document_id, form, page_index)
);
"""


def document_key(path: Path) -> tuple[int, int, bytes] | None:
    """Return the disposable identity for ``path`` without importing MuPDF."""

    import hashlib

    try:
        stat = path.stat()
        digest = hashlib.blake2b()
        with path.open("rb") as source:
            if stat.st_size <= _HASH_WINDOW:
                digest.update(source.read())
            else:
                digest.update(source.read(_MIB))
                source.seek(-_MIB, os.SEEK_END)
                digest.update(source.read(_MIB))
        return stat.st_size, stat.st_mtime_ns, digest.digest()
    except OSError:
        return None


def _cache_limit() -> int:
    try:
        megabytes = float(os.environ.get("PDF_GOAT_CACHE_MB", "256"))
        return max(0, int(megabytes * _MIB))
    except (TypeError, ValueError, OverflowError):
        return 256 * _MIB


def _now() -> int:
    return time.time_ns()


def _encode(form: str, value: object) -> tuple[object, ...]:
    if form == "text":
        if not isinstance(value, str):
            raise TypeError("text cache values must be strings")
        return (
            value,
            None,
            None,
            None,
            None,
            None,
            len(value.encode("utf-8")) + _ROW_OVERHEAD,
        )
    if form == "count":
        chars, words = value  # type: ignore[misc]
        return (None, int(chars), int(words), None, None, None, 16 + _ROW_OVERHEAD)
    if form == "words":
        word_text, rects, lines = value  # type: ignore[misc]
        rect_blob = array("d", rects).tobytes()
        line_blob = array("i", lines).tobytes()
        row_bytes = (
            len(word_text.encode("utf-8"))
            + len(rect_blob)
            + len(line_blob)
            + _ROW_OVERHEAD
        )
        return (None, None, None, word_text, rect_blob, line_blob, row_bytes)
    raise ValueError(f"unknown cache form: {form}")


def _array_from_blob(typecode: str, blob: bytes | None) -> array:
    values = array(typecode)
    if blob:
        values.frombytes(blob)
    return values


class Cache:
    """Best-effort SQLite cache; failures disable only the cache."""

    def __init__(self, path: Path | str):
        self.path = Path(path)
        self.cap_bytes = _cache_limit()
        self._conn = None
        if self.cap_bytes == 0:
            return
        try:
            import sqlite3

            self.path.parent.mkdir(parents=True, exist_ok=True)
            connection = sqlite3.connect(self.path, timeout=2.0)
            connection.execute("PRAGMA busy_timeout=2000")
            connection.execute("PRAGMA journal_mode=WAL")
            connection.execute("PRAGMA synchronous=OFF")
            connection.execute("PRAGMA foreign_keys=ON")
            connection.executescript(_SCHEMA)
            connection.commit()
            self._conn = connection
        except Exception:  # noqa: BLE001
            self.close()

    @property
    def enabled(self) -> bool:
        return self._conn is not None

    def close(self) -> None:
        connection = self._conn
        self._conn = None
        if connection is not None:
            try:
                connection.close()
            except Exception:  # noqa: BLE001
                return

    def document(self, key: tuple[int, int, bytes]) -> tuple[int, int] | None:
        """Return the row id and page count for ``key``, deleting a damaged row.

        The writer stores a page count read from an open document, so a count this
        module can prove impossible came from a damaged file: one page cannot fit in
        under a byte, and a stored page index at or above the count contradicts it.
        Fewer rows than the count stays legal, because ``--pages`` primes part of a
        document. A rejected row cannot be repaired by a later write, so this deletes
        it and the next run re-primes; a lookup is therefore a read that can write.
        """

        connection = self._conn
        if connection is None:
            return None
        try:
            row = connection.execute(
                "SELECT id, page_count, "
                "(SELECT MAX(page_index) FROM pages WHERE document_id=documents.id) "
                "FROM documents WHERE size_bytes=? AND mtime_ns=? AND digest=?",
                key,
            ).fetchone()
        except Exception:  # noqa: BLE001
            try:
                connection.rollback()
            except Exception:  # noqa: BLE001
                return None
            return None
        if row is None:
            return None
        if (
            not isinstance(row[0], int)
            or row[0] < 1
            or not isinstance(row[1], int)
            or row[1] < 0
            or row[1] > key[0]
            or (isinstance(row[2], int) and row[2] >= row[1])
        ):
            self.discard_document(key)
            return None
        return row[0], row[1]

    def discard_document(self, key: tuple[int, int, bytes]) -> None:
        connection = self._conn
        if connection is None:
            return
        try:
            with connection:
                connection.execute(
                    "DELETE FROM documents "
                    "WHERE size_bytes=? AND mtime_ns=? AND digest=?",
                    key,
                )
        except Exception:  # noqa: BLE001
            try:
                connection.rollback()
            except Exception:  # noqa: BLE001
                return

    def lookup(
        self, key: tuple[int, int, bytes], form: str, indices: list[int]
    ) -> dict[int, object]:
        connection = self._conn
        if connection is None or not indices:
            return {}
        try:
            document = self.document(key)
            if document is None:
                return {}
            document_id = document[0]
            wanted = list(dict.fromkeys(indices))
            wanted_set = set(wanted)
            values: dict[int, object] = {}
            for start in range(0, len(wanted), _SQL_VARIABLE_LIMIT - 2):
                selected = wanted[start : start + _SQL_VARIABLE_LIMIT - 2]
                placeholders = ",".join("?" for _ in selected)
                rows = connection.execute(
                    f"""
                    SELECT page_index, text_value, char_count, word_count,
                           word_text, rects, lines
                    FROM pages
                    WHERE document_id=? AND form=?
                      AND page_index IN ({placeholders})
                    """,
                    (document_id, form, *selected),
                ).fetchall()
                for row in rows:
                    index = row[0]
                    if not isinstance(index, int) or index not in wanted_set:
                        continue
                    try:
                        if form == "text":
                            value = row[1]
                            if not isinstance(value, str):
                                continue
                        elif form == "count":
                            if (
                                not isinstance(row[2], int)
                                or row[2] < 0
                                or not isinstance(row[3], int)
                                or row[3] < 0
                            ):
                                continue
                            value = (row[2], row[3])
                        elif form == "words":
                            if (
                                not isinstance(row[4], str)
                                or not isinstance(row[5], bytes)
                                or not isinstance(row[6], bytes)
                            ):
                                continue
                            rects = _array_from_blob("d", row[5])
                            lines = _array_from_blob("i", row[6])
                            word_count = row[4].count("\n") + 1 if row[4] else 0
                            if (
                                word_count != len(rects) // 4
                                or len(rects) % 4
                                or len(lines) % 2
                                or len(rects) // 4 != len(lines) // 2
                            ):
                                continue
                            value = (row[4], rects, lines)
                        else:
                            continue
                    except (TypeError, ValueError, OverflowError):
                        continue
                    values[index] = value
            if values:
                # Recency is bookkeeping: a failed touch must not cost the caller the
                # rows this call already decoded.
                try:
                    with connection:
                        connection.execute(
                            "UPDATE documents SET last_used=? WHERE id=?",
                            (_now(), document_id),
                        )
                except Exception:  # noqa: BLE001
                    # ``with connection`` already rolled the touch back.
                    return values
            return values
        except Exception:  # noqa: BLE001
            try:
                connection.rollback()
            except Exception:  # noqa: BLE001
                return {}
            return {}

    def write(
        self,
        key: tuple[int, int, bytes],
        path: Path,
        page_count: int,
        form: str,
        entries: dict[int, object],
    ) -> None:
        connection = self._conn
        if connection is None or not entries:
            return
        try:
            encoded = [
                (index, _encode(form, value)) for index, value in entries.items()
            ]
            with connection:
                connection.execute(
                    """
                    INSERT INTO documents(
                        size_bytes, mtime_ns, digest, path, page_count, row_bytes, last_used
                    ) VALUES(?,?,?,?,?,?,?)
                    ON CONFLICT(size_bytes, mtime_ns, digest) DO UPDATE SET
                        path=excluded.path,
                        page_count=excluded.page_count
                    """,
                    (*key, str(path), page_count, 0, _now()),
                )
                document = connection.execute(
                    """
                    SELECT id FROM documents
                    WHERE size_bytes=? AND mtime_ns=? AND digest=?
                    """,
                    key,
                ).fetchone()
                if document is None:
                    return
                document_id = document[0]
                for index, values in encoded:
                    connection.execute(
                        """
                        INSERT INTO pages(
                            document_id, form, page_index, text_value, char_count,
                            word_count, word_text, rects, lines, row_bytes
                        ) VALUES(?,?,?,?,?,?,?,?,?,?)
                        ON CONFLICT(document_id, form, page_index) DO UPDATE SET
                            text_value=excluded.text_value,
                            char_count=excluded.char_count,
                            word_count=excluded.word_count,
                            word_text=excluded.word_text,
                            rects=excluded.rects,
                            lines=excluded.lines,
                            row_bytes=excluded.row_bytes
                        """,
                        (document_id, form, index, *values),
                    )
                row_bytes = connection.execute(
                    "SELECT COALESCE(SUM(row_bytes), 0) FROM pages WHERE document_id=?",
                    (document_id,),
                ).fetchone()[0]
                connection.execute(
                    "UPDATE documents SET row_bytes=?, last_used=? WHERE id=?",
                    (row_bytes, _now(), document_id),
                )
                total = connection.execute(
                    "SELECT COALESCE(SUM(row_bytes), 0) FROM documents"
                ).fetchone()[0]
                while total > self.cap_bytes:
                    oldest = connection.execute(
                        """
                        SELECT id, row_bytes FROM documents
                        ORDER BY last_used, id
                        LIMIT 1
                        """
                    ).fetchone()
                    if oldest is None:
                        break
                    connection.execute("DELETE FROM documents WHERE id=?", (oldest[0],))
                    total -= oldest[1]
        except Exception:  # noqa: BLE001
            try:
                connection.rollback()
            except Exception:  # noqa: BLE001
                return
