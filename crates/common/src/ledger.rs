//! The job ledger: `ledger.db` in the state directory, one row per ledgered verb run.

use std::path::Path;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, ErrorCode, Row, params};
use serde_json::{Map, Number, Value};

use crate::error::GoatError;
use crate::paths::mkdir_parents;
use crate::py::{json_compact, repr_bytes, repr_str};

/// The ledger file inside the state directory.
pub const LEDGER_FILE: &str = "ledger.db";

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS jobs(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts TEXT NOT NULL,
            verb TEXT NOT NULL,
            status TEXT NOT NULL,
            inputs TEXT,
            outputs TEXT,
            detail TEXT,
            message TEXT,
            duration_ms INTEGER
        )";

/// One ledger row to write.
#[derive(Clone, Copy, Debug)]
pub struct Job<'a> {
    pub verb: &'a str,
    /// `success` or `error`.
    pub status: &'a str,
    pub inputs: &'a Value,
    pub outputs: &'a Value,
    /// Built by [`ledger_detail`].
    pub detail: &'a Value,
    /// The error text; `None` on success.
    pub message: Option<&'a str>,
    pub duration_ms: i64,
}

/// `_db()`: creates the state directory and the table.
fn open(home: &Path) -> Result<Connection, GoatError> {
    mkdir_parents(home)?;
    let connection = Connection::open(home.join(LEDGER_FILE)).map_err(sql_error)?;
    connection.execute_batch(SCHEMA).map_err(sql_error)?;
    Ok(connection)
}

/// `record_job(...)`: appends one row stamped with the local time; returns its id.
pub fn record_job(home: &Path, job: &Job<'_>) -> Result<i64, GoatError> {
    let connection = open(home)?;
    connection
        .execute(
            "INSERT INTO jobs(ts,verb,status,inputs,outputs,detail,message,duration_ms) \
             VALUES(strftime('%Y-%m-%dT%H:%M:%S','now','localtime'),?,?,?,?,?,?,?)",
            params![
                job.verb,
                job.status,
                json_compact(job.inputs),
                json_compact(job.outputs),
                json_compact(job.detail),
                job.message,
                job.duration_ms
            ],
        )
        .map_err(sql_error)?;
    Ok(connection.last_insert_rowid())
}

/// `cmd_jobs`: the newest `limit` rows, newest first; a negative limit lists every row.
pub fn recent_jobs(home: &Path, limit: i64) -> Result<Vec<Value>, GoatError> {
    let connection = open(home)?;
    let mut statement = connection
        .prepare("SELECT * FROM jobs ORDER BY id DESC LIMIT ?")
        .map_err(sql_error)?;
    let mut rows = statement.query([limit]).map_err(sql_error)?;
    let mut jobs = Vec::new();
    while let Some(row) = rows.next().map_err(sql_error)? {
        let mut job = Map::new();
        for name in ["id", "ts", "verb", "status"] {
            job.insert(name.to_owned(), column(row, name)?);
        }
        for name in ["inputs", "outputs"] {
            job.insert(name.to_owned(), json_column(row, name)?);
        }
        for name in ["duration_ms", "message"] {
            job.insert(name.to_owned(), column(row, name)?);
        }
        jobs.push(Value::Object(job));
    }
    Ok(jobs)
}

/// A column as Python's `sqlite3` returns it, printed through `json.dumps(default=str)`.
fn column(row: &Row<'_>, name: &str) -> Result<Value, GoatError> {
    Ok(match row.get_ref(name).map_err(sql_error)? {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => Value::from(value),
        ValueRef::Real(value) => Number::from_f64(value).map_or(Value::Null, Value::Number),
        ValueRef::Text(bytes) => Value::String(text(bytes, name)?.to_owned()),
        ValueRef::Blob(bytes) => Value::String(repr_bytes(bytes)),
    })
}

/// `json.loads(row[name])`.
fn json_column(row: &Row<'_>, name: &str) -> Result<Value, GoatError> {
    let source = match row.get_ref(name).map_err(sql_error)? {
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => text(bytes, name)?,
        other => {
            let kind = match other {
                ValueRef::Null => "NoneType",
                ValueRef::Integer(_) => "int",
                _ => "float",
            };
            return Err(GoatError::exception(
                "TypeError",
                format!("the JSON object must be str, bytes or bytearray, not {kind}"),
            ));
        }
    };
    serde_json::from_str(source)
        .map_err(|error| GoatError::exception("JSONDecodeError", error.to_string()))
}

fn text<'r>(bytes: &'r [u8], name: &str) -> Result<&'r str, GoatError> {
    std::str::from_utf8(bytes).map_err(|_| {
        GoatError::exception(
            "OperationalError",
            format!(
                "Could not decode to UTF-8 column {} with text {}",
                repr_str(name),
                repr_str(&String::from_utf8_lossy(bytes))
            ),
        )
    })
}

/// A SQLite failure as the Python `sqlite3` exception class it raises.
fn sql_error(error: rusqlite::Error) -> GoatError {
    match error {
        rusqlite::Error::SqliteFailure(failure, message) => {
            let kind = match failure.code {
                ErrorCode::ConstraintViolation | ErrorCode::TypeMismatch => "IntegrityError",
                ErrorCode::TooBig => "DataError",
                ErrorCode::ApiMisuse | ErrorCode::ParameterOutOfRange => "InterfaceError",
                ErrorCode::InternalMalfunction | ErrorCode::NotFound => "InternalError",
                ErrorCode::OutOfMemory => "MemoryError",
                ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase => "DatabaseError",
                _ => "OperationalError",
            };
            GoatError::exception(kind, message.unwrap_or_else(|| failure.to_string()))
        }
        other => GoatError::exception("DatabaseError", other.to_string()),
    }
}

/// Keys whose count gets a name of its own instead of `{key}_count`.
const COUNT_NAMES: [(&str, &str); 3] = [
    ("candidates", "candidate_count"),
    ("courses", "course_count"),
    ("terms", "term_count"),
];

/// `_ledger_detail(result)`: the verb's result reduced to what the ledger may keep.
///
/// Scalars stay; text becomes its length in characters, except a `risk` of `low`, `medium`,
/// or `high`; lists and objects become their length. `freshness` keeps its verdict and
/// `parse_quality` its counts and confidence. Inputs, outputs, and the verb are dropped.
pub fn ledger_detail(result: &Map<String, Value>) -> Map<String, Value> {
    let mut detail = Map::new();
    for (key, value) in result {
        if matches!(key.as_str(), "inputs" | "outputs" | "verb") {
            continue;
        }
        match (key.as_str(), value) {
            ("freshness", Value::Object(freshness)) => {
                let verdict = freshness.get("verdict").cloned().unwrap_or(Value::Null);
                detail.insert("freshness_verdict".to_owned(), verdict);
            }
            ("parse_quality", Value::Object(quality)) => {
                for nested in [
                    "confidence",
                    "matched_line_count",
                    "unparsed_line_count",
                    "course_count",
                    "term_count",
                ] {
                    if let Some(item) = quality.get(nested) {
                        detail.insert(nested.to_owned(), item.clone());
                    }
                }
            }
            (_, Value::Null | Value::Bool(_) | Value::Number(_)) => {
                detail.insert(key.clone(), value.clone());
            }
            ("risk", Value::String(risk)) if matches!(risk.as_str(), "low" | "medium" | "high") => {
                detail.insert(key.clone(), value.clone());
            }
            (_, Value::String(text)) => {
                detail.insert(
                    format!("{key}_char_count"),
                    Value::from(text.chars().count()),
                );
            }
            (_, Value::Array(items)) => {
                detail.insert(count_key(key, result), Value::from(items.len()));
            }
            (_, Value::Object(map)) => {
                detail.insert(count_key(key, result), Value::from(map.len()));
            }
        }
    }
    detail
}

fn count_key(key: &str, result: &Map<String, Value>) -> String {
    if key == "pages" && !result.contains_key("total_pages") {
        return "page_count".to_owned();
    }
    COUNT_NAMES
        .iter()
        .find(|(name, _)| *name == key)
        .map_or_else(|| format!("{key}_count"), |(_, count)| (*count).to_owned())
}
