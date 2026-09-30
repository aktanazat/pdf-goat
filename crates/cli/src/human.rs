//! `render_human(result)`: the text a person reads when stdout is a terminal.

use std::fmt::Write;

use goat_common::GoatError;
use goat_common::output::human_size;
use goat_common::paths::name;
use goat_common::py::{repr_str, rstrip, str_value, truthy};
use serde_json::{Map, Value};

/// The result rendered for a terminal, one `\n` after each line.
pub fn render(result: &Map<String, Value>) -> Result<String, GoatError> {
    let mut out = Out::default();
    let verb = match result.get("verb") {
        Some(Value::String(verb)) => verb.as_str(),
        _ => "",
    };
    match verb {
        "capabilities" => capabilities(result, &mut out)?,
        "inspect" => inspect(result, &mut out)?,
        "preflight" => preflight(result, &mut out)?,
        "info" => info(result, &mut out)?,
        "text" => text(result, &mut out)?,
        "form-list" => form_list(result, &mut out)?,
        "compare-text" => compare_text(result, &mut out)?,
        "jobs" => {
            for job in list(result, "jobs")? {
                let job = object(job)?;
                let outputs = get(job, "outputs")?;
                let outputs = if truthy(outputs) {
                    format!(" -> {} out", length(outputs)?)
                } else {
                    String::new()
                };
                out.line(format_args!(
                    "#{:<4} {}  {:<11} {:<7}{outputs}",
                    field(job, "id")?,
                    field(job, "ts")?,
                    field(job, "verb")?,
                    field(job, "status")?
                ));
            }
        }
        "compress" => {
            out.line(format_args!(
                "{} -> {}  ({}x, saved {})",
                size(result, "original_bytes")?,
                size(result, "compressed_bytes")?,
                field(result, "ratio")?,
                size(result, "saved_bytes")?
            ));
            out.line(format_args!("out  {}", first(result, "outputs")?));
        }
        "convert-from-office" => {
            out.line(format_args!("out  {}", first(result, "outputs")?));
            for warning in list(result, "warnings")? {
                out.line(format_args!("warn {}", str_value(warning)));
            }
            out.line(format_args!(
                "engine={}  pages={}",
                field(result, "engine")?,
                field(result, "pages")?
            ));
        }
        _ => fallback(result, &mut out)?,
    }
    Ok(out.text)
}

fn capabilities(result: &Map<String, Value>, out: &mut Out) -> Result<(), GoatError> {
    out.line(format_args!("{} commands", field(result, "command_count")?));
    out.line(format_args!("families  {}", joined(result, "families")?));
    out.line(format_args!("commands  {}", joined(result, "commands")?));
    let requested = result.get("requested_command").unwrap_or(&Value::Null);
    if truthy(requested) {
        let schema = object(get(
            object(get(result, "schemas")?)?,
            &str_value(requested),
        )?)?;
        let mut commands: Vec<&str> = object(get(schema, "commands")?)?
            .keys()
            .map(String::as_str)
            .collect();
        commands.sort_unstable();
        let listed = if commands.is_empty() {
            "single command".to_owned()
        } else {
            commands.join(", ")
        };
        out.line(format_args!("{}  {listed}", str_value(requested)));
    }
    Ok(())
}

fn inspect(result: &Map<String, Value>, out: &mut Out) -> Result<(), GoatError> {
    let pages = list(result, "pages")?;
    let start = integer(get(result, "start_page")?)?;
    let count = i64::try_from(pages.len()).unwrap_or(i64::MAX);
    out.line(format_args!(
        "{}: pages {start}-{} of {}",
        first(result, "inputs")?,
        start.saturating_add(count).saturating_sub(1),
        field(result, "total_pages")?
    ));
    for page in pages {
        let page = object(page)?;
        out.line(format_args!(
            "  {}: {}x{}pt, {} words, {} images, {} links",
            field(page, "page")?,
            field(page, "width_pt")?,
            field(page, "height_pt")?,
            field(page, "word_count")?,
            field(page, "image_count")?,
            field(page, "link_count")?
        ));
    }
    let next = get(result, "next_page")?;
    if truthy(next) {
        out.line(format_args!("next page  {}", str_value(next)));
    }
    Ok(())
}

fn preflight(result: &Map<String, Value>, out: &mut Out) -> Result<(), GoatError> {
    let or_unknown = |key| {
        result
            .get(key)
            .map_or_else(|| "unknown".to_owned(), str_value)
    };
    out.line(format_args!(
        "risk {}  pages {}  attachments {}",
        field(result, "risk")?,
        or_unknown("pages"),
        or_unknown("attachments")
    ));
    for finding in list(result, "findings")? {
        let finding = object(finding)?;
        let count = finding
            .get("count")
            .map_or_else(String::new, |count| format!(" ({})", str_value(count)));
        out.line(format_args!(
            "  {}: {}{count}",
            field(finding, "severity")?,
            field(finding, "message")?
        ));
    }
    Ok(())
}

fn info(result: &Map<String, Value>, out: &mut Out) -> Result<(), GoatError> {
    out.line(format_args!("file        {}", first(result, "inputs")?));
    out.line(format_args!("pages       {}", field(result, "pages")?));
    out.line(format_args!(
        "size        {}",
        size(result, "file_size_bytes")?
    ));
    out.line(format_args!("encrypted   {}", field(result, "encrypted")?));
    out.line(format_args!(
        "has_forms   {} ({} fields)",
        field(result, "has_forms")?,
        field(result, "form_field_count")?
    ));
    out.line(format_args!("has_text    {}", field(result, "has_text")?));
    let sizes = list(result, "page_sizes_pt")?
        .iter()
        .map(|size| {
            let size = object(size)?;
            Ok(format!(
                "{}x{}pt",
                field(size, "width")?,
                field(size, "height")?
            ))
        })
        .collect::<Result<Vec<_>, GoatError>>()?;
    out.line(format_args!("page_sizes  {}", sizes.join(", ")));
    let metadata = get(result, "metadata")?;
    if truthy(metadata) {
        out.line(format_args!("metadata"));
        for (key, value) in object(metadata)? {
            out.line(format_args!("  {key:<12}{}", str_value(value)));
        }
    }
    Ok(())
}

fn text(result: &Map<String, Value>, out: &mut Out) -> Result<(), GoatError> {
    if result.contains_key("page_count") {
        out.line(format_args!(
            "# {} chars across {} pages -> {}",
            field(result, "char_count")?,
            field(result, "page_count")?,
            first(result, "outputs")?
        ));
        return Ok(());
    }
    let pages = list(result, "pages")?;
    out.line(format_args!(
        "# {} chars across {} pages",
        field(result, "char_count")?,
        pages.len()
    ));
    out.line(format_args!(""));
    for page in pages {
        let page = object(page)?;
        out.line(format_args!("----- page {} -----", field(page, "page")?));
        out.line(format_args!("{}", rstrip(&field(page, "text")?)));
    }
    Ok(())
}

fn form_list(result: &Map<String, Value>, out: &mut Out) -> Result<(), GoatError> {
    out.line(format_args!(
        "{} form widget(s):",
        field(result, "field_count")?
    ));
    for item in list(result, "fields")? {
        let item = object(item)?;
        let state = item.get("checked").map_or_else(String::new, |checked| {
            format!("  checked={}", str_value(checked))
        });
        out.line(format_args!(
            "  p{}  {}  [{}]  = {}{state}",
            field(item, "page")?,
            field(item, "name")?,
            field(item, "type")?,
            field(item, "value")?
        ));
    }
    Ok(())
}

fn compare_text(result: &Map<String, Value>, out: &mut Out) -> Result<(), GoatError> {
    out.line(format_args!(
        "identical {}  added {}  removed {}",
        field(result, "identical")?,
        field(result, "added")?,
        field(result, "removed")?
    ));
    for page in list(result, "pages")? {
        let page = object(page)?;
        let found = get(page, "match")?;
        let target = if truthy(found) {
            let found = object(found)?;
            format!(
                "{} p{} ratio {}",
                name(&field(found, "file")?),
                field(found, "page")?,
                field(found, "ratio")?
            )
        } else {
            "no text".to_owned()
        };
        out.line(format_args!(
            "page {} -> {target}  +{} -{}",
            field(page, "page")?,
            field(page, "added")?,
            field(page, "removed")?
        ));
        for line in list(page, "diff")? {
            out.line(format_args!("  {}", str_value(line)));
        }
        if truthy(get(page, "diff_truncated")?) {
            out.line(format_args!("  ..."));
        }
    }
    for page in list(result, "unmatched")? {
        let page = object(page)?;
        out.line(format_args!(
            "unmatched  {} p{}",
            name(&field(page, "file")?),
            field(page, "page")?
        ));
    }
    Ok(())
}

/// Outputs as `out  path` lines, then every other field as `key=value` on one line.
fn fallback(result: &Map<String, Value>, out: &mut Out) -> Result<(), GoatError> {
    let mut line = Vec::new();
    for (key, value) in result {
        match key.as_str() {
            "inputs" | "verb" => {}
            "outputs" => {
                for output in items(value)? {
                    out.line(format_args!("out  {}", str_value(output)));
                }
            }
            _ => line.push(format!("{key}={}", str_value(value))),
        }
    }
    if !line.is_empty() {
        out.line(format_args!("{}", line.join("  ")));
    }
    Ok(())
}

#[derive(Default)]
struct Out {
    text: String,
}

impl Out {
    fn line(&mut self, line: std::fmt::Arguments<'_>) {
        // Writing into a String cannot fail.
        let _ = self.text.write_fmt(line);
        self.text.push('\n');
    }
}

fn get<'r>(map: &'r Map<String, Value>, key: &str) -> Result<&'r Value, GoatError> {
    map.get(key)
        .ok_or_else(|| GoatError::exception("KeyError", repr_str(key)))
}

/// `str(result[key])`.
fn field(map: &Map<String, Value>, key: &str) -> Result<String, GoatError> {
    get(map, key).map(str_value)
}

fn object(value: &Value) -> Result<&Map<String, Value>, GoatError> {
    match value {
        Value::Object(map) => Ok(map),
        other => Err(not_subscriptable(other)),
    }
}

fn items(value: &Value) -> Result<&[Value], GoatError> {
    match value {
        Value::Array(items) => Ok(items),
        other => Err(not_subscriptable(other)),
    }
}

fn list<'r>(map: &'r Map<String, Value>, key: &str) -> Result<&'r [Value], GoatError> {
    items(get(map, key)?)
}

fn first(map: &Map<String, Value>, key: &str) -> Result<String, GoatError> {
    list(map, key)?
        .first()
        .map(str_value)
        .ok_or_else(|| GoatError::exception("IndexError", "list index out of range"))
}

fn joined(map: &Map<String, Value>, key: &str) -> Result<String, GoatError> {
    Ok(list(map, key)?
        .iter()
        .map(str_value)
        .collect::<Vec<_>>()
        .join(", "))
}

fn length(value: &Value) -> Result<usize, GoatError> {
    match value {
        Value::Array(items) => Ok(items.len()),
        Value::Object(map) => Ok(map.len()),
        Value::String(text) => Ok(text.chars().count()),
        other => Err(GoatError::exception(
            "TypeError",
            format!("object of type '{}' has no len()", type_name(other)),
        )),
    }
}

fn integer(value: &Value) -> Result<i64, GoatError> {
    value.as_i64().ok_or_else(|| {
        GoatError::exception(
            "TypeError",
            format!(
                "unsupported operand type(s) for +: '{}' and 'int'",
                type_name(value)
            ),
        )
    })
}

/// `human_size(result[key])`.
fn size(map: &Map<String, Value>, key: &str) -> Result<String, GoatError> {
    integer(get(map, key)?).map(human_size)
}

fn not_subscriptable(value: &Value) -> GoatError {
    GoatError::exception(
        "TypeError",
        format!("'{}' object is not subscriptable", type_name(value)),
    )
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}
