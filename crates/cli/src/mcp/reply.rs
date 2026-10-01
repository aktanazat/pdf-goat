//! Turns a finished `pdf-goat` child into MCP content: its JSON in compact form
//! (summarized, and saved whole, when large), its PNG and JPEG outputs as images, and
//! the tail of its stderr.

use std::io;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;

use crate::child::Finished;
use crate::server::Session;

/// A result whose compact JSON is longer than this is summarized and saved to a file.
const INLINE_BYTES: usize = 24_000;
/// In a summary, a top-level value whose compact JSON is longer than this becomes a marker.
const VALUE_BYTES: usize = 1_000;
/// At most this many images come back inline.
const MAX_IMAGES: usize = 4;
/// An image file larger than this stays on disk only.
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
/// How much of stderr comes back, from its end.
const STDERR_CHARS: usize = 4_000;

/// pdf-goat's JSON result when stdout holds one, and whether the run failed: a
/// nonzero exit or `"ok": false`.
pub fn outcome(finished: &Finished) -> (Option<Value>, bool) {
    let json: Option<Value> = serde_json::from_slice(&finished.stdout).ok();
    let ok = json
        .as_ref()
        .and_then(|value| value.get("ok"))
        .and_then(Value::as_bool);
    let failed = !finished.status.success() || ok == Some(false);
    (json, failed)
}

/// pdf-goat's own words for a failed run: the JSON `error`, else the end of stderr,
/// else the exit status.
pub fn failure_message(finished: &Finished, json: Option<&Value>) -> String {
    match json
        .and_then(|value| value.get("error"))
        .and_then(Value::as_str)
    {
        Some(error) => error.to_owned(),
        None => stderr_tail(&finished.stderr)
            .unwrap_or_else(|| format!("pdf-goat ended with {}", finished.status)),
    }
}

/// The tool result for a finished run; `isError` is set when the run failed.
pub async fn tool_result(finished: &Finished, session: &Session) -> CallToolResult {
    let (json, failed) = outcome(finished);
    let mut content = Vec::new();
    let mut notes = Vec::new();
    match &json {
        Some(value) => {
            let size = compact_len(value);
            if size <= INLINE_BYTES {
                content.push(ContentBlock::text(value.to_string()));
            } else {
                content.push(ContentBlock::text(summary(value).to_string()));
                let path = session.fresh("result", ".json");
                notes.push(match tokio::fs::write(&path, &finished.stdout).await {
                    Ok(()) => format!(
                        "the result is {size} bytes, so values over {VALUE_BYTES} bytes are \
                         shortened above. the full result is kept until the server exits: {}",
                        path.display()
                    ),
                    Err(error) => format!(
                        "the result is {size} bytes, so values over {VALUE_BYTES} bytes are \
                         shortened above; saving the full result to {} failed: {error}",
                        path.display()
                    ),
                });
            }
        }
        None => {
            if !finished.stdout.is_empty() {
                content.push(ContentBlock::text(String::from_utf8_lossy(
                    &finished.stdout,
                )));
            }
            if !finished.status.success() {
                notes.push(format!("pdf-goat ended with {}", finished.status));
            }
        }
    }
    if !failed
        && let Some(outputs) = json
            .as_ref()
            .and_then(|value| value.get("outputs"))
            .and_then(Value::as_array)
    {
        let (images, note) = images(outputs).await;
        content.extend(images);
        notes.extend(note);
    }
    content.extend(notes.into_iter().map(ContentBlock::text));
    content.extend(stderr_tail(&finished.stderr).map(ContentBlock::text));
    if failed {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    }
}

/// The first [`MAX_IMAGES`] PNG or JPEG files among `outputs` of at most
/// [`MAX_IMAGE_BYTES`] each, and a note when any image stays on disk only.
async fn images(outputs: &[Value]) -> (Vec<ContentBlock>, Option<String>) {
    let candidates: Vec<(&str, &str)> = outputs
        .iter()
        .filter_map(Value::as_str)
        .filter_map(|path| image_type(path).map(|mime| (path, mime)))
        .collect();
    let mut shown = Vec::new();
    for &(path, mime) in &candidates {
        if shown.len() == MAX_IMAGES {
            break;
        }
        if let Ok(bytes) = read_image(path).await {
            shown.push(ContentBlock::image(STANDARD.encode(bytes), mime));
        }
    }
    let note = (shown.len() < candidates.len()).then(|| {
        format!(
            "showing {} of {} images (at most {MAX_IMAGES}, each at most 5 MiB); \
             all of them are on disk at the paths in outputs",
            shown.len(),
            candidates.len()
        )
    });
    (shown, note)
}

/// The bytes of an image file no larger than [`MAX_IMAGE_BYTES`].
async fn read_image(path: &str) -> io::Result<Vec<u8>> {
    if tokio::fs::metadata(path).await?.len() > MAX_IMAGE_BYTES {
        return Err(io::Error::from(io::ErrorKind::FileTooLarge));
    }
    tokio::fs::read(path).await
}

fn image_type(path: &str) -> Option<&'static str> {
    let extension = Path::new(path).extension()?;
    if extension.eq_ignore_ascii_case("png") {
        Some("image/png")
    } else if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") {
        Some("image/jpeg")
    } else {
        None
    }
}

/// `value` with every top-level entry over [`VALUE_BYTES`] of compact JSON replaced by
/// a marker that names its shape and size.
fn summary(value: &Value) -> Value {
    match value {
        Value::Object(entries) => entries
            .iter()
            .map(|(key, entry)| (key.clone(), shortened(entry)))
            .collect(),
        other => shortened(other),
    }
}

fn shortened(value: &Value) -> Value {
    let size = compact_len(value);
    let shape = match value {
        _ if size <= VALUE_BYTES => return value.clone(),
        Value::Array(items) => format!("array of {} items", items.len()),
        Value::Object(entries) => format!("object of {} keys", entries.len()),
        Value::String(text) => format!("string of {} characters", text.chars().count()),
        // No number, boolean or null is that long.
        Value::Null | Value::Bool(_) | Value::Number(_) => return value.clone(),
    };
    Value::String(format!("[omitted: {shape}, {size} bytes]"))
}

/// The length of `value`'s compact JSON, counted without building it.
fn compact_len(value: &Value) -> usize {
    struct Count(usize);
    impl io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value).expect("a JSON value serializes into a counter");
    count.0
}

/// The last [`STDERR_CHARS`] characters of stderr, labelled; `None` when it is blank.
fn stderr_tail(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim_end();
    if text.trim_start().is_empty() {
        return None;
    }
    match text.char_indices().rev().nth(STDERR_CHARS - 1) {
        Some((start, _)) if start > 0 => Some(format!(
            "pdf-goat stderr (last {STDERR_CHARS} characters):\n{}",
            &text[start..]
        )),
        _ => Some(format!("pdf-goat stderr:\n{text}")),
    }
}
