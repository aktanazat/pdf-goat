use std::collections::HashSet;
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::verbs::{file, object, path_value};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::{Arg, ArgAction, ArgMatches, Command};
use goat_common::args::{many, optional, required};
use goat_common::paths::{ensure_parent, expanduser, resolve, resolve_lenient};
use goat_common::{Ctx, GoatError, Registry, Verb};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

pub fn register(registry: &mut Registry) {
    registry.family_verb(
        "transcript",
        Verb::new(
            Command::new("read")
                .about("extract transcript identity, terms, courses, and freshness")
                .arg(file())
                .arg(
                    Arg::new("conferred")
                        .long("conferred")
                        .help("conferral date to compare with the printed issue date (YYYY-MM-DD)"),
                )
                .arg(
                    Arg::new("output")
                        .short('o')
                        .long("output")
                        .help("write structured JSON"),
                ),
            read,
        ),
    );
    registry.family_verb(
        "transcript",
        Verb::new(
            Command::new("resolve")
                .about(
                    "rank PDF candidates by printed issue date in the named files or directories",
                )
                .arg(
                    Arg::new("root")
                        .long("root")
                        .action(ArgAction::Append)
                        .required(true)
                        .help("directory or PDF; not recursive"),
                )
                .arg(
                    Arg::new("glob")
                        .long("glob")
                        .action(ArgAction::Append)
                        .help("filename pattern, repeatable"),
                ),
            discover,
        ),
    );
}

fn fingerprint(metadata: &Metadata) -> (u64, u64, u64, i64, i64) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    )
}

pub(crate) fn provenance(
    path: &Path,
    page_count: usize,
    initial: &Metadata,
) -> Result<Value, GoatError> {
    let expected = fingerprint(initial);
    let mut source = File::open(path).map_err(|e| GoatError::os(&e, path))?;
    let opened = source.metadata().map_err(|e| GoatError::os(&e, path))?;
    let mut sha = Sha256::new();
    let mut chunk = vec![0_u8; 1024 * 1024];
    loop {
        let size = source
            .read(&mut chunk)
            .map_err(|e| GoatError::os(&e, path))?;
        if size == 0 {
            break;
        }
        sha.update(&chunk[..size]);
    }
    let closed = source.metadata().map_err(|e| GoatError::os(&e, path))?;
    let current = fs::metadata(path).map_err(|e| GoatError::os(&e, path))?;
    if fingerprint(&opened) != expected
        || fingerprint(&closed) != expected
        || fingerprint(&current) != expected
    {
        return Err(GoatError::exception(
            "RuntimeError",
            "source changed during transcript read",
        ));
    }
    // Python exposes st_mtime as a double, then datetime rounds its
    // fractional seconds to microseconds. Do the same before formatting.
    let seconds = initial.mtime() as f64 + initial.mtime_nsec() as f64 / 1_000_000_000.0;
    let mut whole = seconds.floor() as i64;
    let mut micros = ((seconds - seconds.floor()) * 1_000_000.0).round_ties_even() as u32;
    if micros >= 1_000_000 {
        whole += 1;
        micros -= 1_000_000;
    }
    let time = DateTime::<Utc>::from_timestamp(whole, micros * 1000)
        .ok_or_else(|| GoatError::value_error("timestamp out of range"))?;
    let modified = time.to_rfc3339_opts(
        if micros == 0 {
            SecondsFormat::Secs
        } else {
            SecondsFormat::Micros
        },
        false,
    );
    let digest = sha.finalize();
    let sha256 = format!("{digest:x}");
    Ok(
        json!({"path":path_value(path),"sha256":sha256,"byte_size":initial.len(),"modified_time":modified,"page_count":page_count}),
    )
}

fn read(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let source = resolve(required::<String>(matches, "file")?)?;
    let output = optional::<String>(matches, "output")?
        .filter(|s| !s.is_empty())
        .map(|s| resolve_lenient(&expanduser(s)))
        .transpose()?;
    if let Some(out) = &output {
        let same = match (fs::metadata(out), fs::metadata(&source)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        };
        if out == &source || same {
            return Err(GoatError::message("output must differ from input"));
        }
    }
    let conferred = optional::<String>(matches, "conferred")?.map(String::as_str);
    let mut parsed = crate::transcript::parse_transcript(&source, conferred)?;
    if let Some(map) = parsed.as_object_mut() {
        map.remove("layout");
    }
    let mut result = json!({"verb":"transcript-read","inputs":[path_value(&source)],"outputs":[]});
    if let (Some(to), Some(from)) = (result.as_object_mut(), parsed.as_object()) {
        to.extend(from.clone());
    }
    if let Some(out) = output {
        let out = ensure_parent(&out.to_string_lossy())?;
        fs::write(&out, goat_common::py::json_pretty(&parsed))
            .map_err(|e| GoatError::os(&e, &out))?;
        result["outputs"] = json!([path_value(&out)]);
    }
    object(result)
}

fn discover(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let roots = many::<String>(matches, "root")?;
    let patterns = many::<String>(matches, "glob")?;
    let patterns: Vec<&str> = if patterns.is_empty() {
        vec!["*.pdf"]
    } else {
        patterns.into_iter().map(String::as_str).collect()
    };
    let roots: Vec<PathBuf> = roots
        .into_iter()
        .map(|s| resolve_lenient(&expanduser(s)))
        .collect::<Result<_, _>>()?;
    let rows = discover_transcripts(&roots, &patterns)?;
    let paths: Vec<Value> = roots.iter().map(|p| path_value(p)).collect();
    object(
        json!({"verb":"transcript-resolve","inputs":paths,"outputs":[],"roots":paths,"candidates":rows,"candidate_count":rows.len()}),
    )
}

/// Read only files in the explicitly named roots, one directory level deep.
pub(crate) fn discover_transcripts(
    roots: &[PathBuf],
    patterns: &[&str],
) -> Result<Vec<Value>, GoatError> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        if root.is_file() {
            if seen.insert(root.clone()) {
                candidates.push(root.clone());
            }
            continue;
        }
        if !root.is_dir() {
            continue;
        }
        for entry in fs::read_dir(root).map_err(|e| GoatError::os(&e, root))? {
            let entry = entry.map_err(|e| GoatError::os(&e, root))?;
            let path = entry.path();
            if path.is_file()
                && patterns
                    .iter()
                    .any(|pattern| filename_matches(&entry.file_name().to_string_lossy(), pattern))
                && seen.insert(path.clone())
            {
                candidates.push(path);
            }
        }
    }
    let mut rows = Vec::new();
    for path in candidates {
        match crate::transcript::parse_transcript(&path,None) {
            Ok(parsed)=>rows.push(json!({"path":path_value(&path),"issue_date":parsed["issue_date"],"document_identity":parsed["document_identity"],"degree":parsed["degree"],
                "source_provenance":parsed["source_provenance"],"freshness":parsed["freshness"],"parse_quality":parsed["parse_quality"]})),
            Err(error)=>{
                let message=match error {GoatError::Exception {message,..}|GoatError::Message(message)=>message};
                rows.push(json!({"path":path_value(&path),"parse_error":message}));
            }
        }
    }
    rows.sort_by(|a, b| {
        b["issue_date"]
            .as_str()
            .unwrap_or("0000-00-00")
            .cmp(a["issue_date"].as_str().unwrap_or("0000-00-00"))
            .then(
                b["source_provenance"]["modified_time"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(
                        a["source_provenance"]["modified_time"]
                            .as_str()
                            .unwrap_or(""),
                    ),
            )
    });
    for (index, row) in rows.iter_mut().enumerate() {
        row["rank"] = (index + 1).into();
    }
    Ok(rows)
}

// fnmatch's shell wildcards, not path globs: '/' and leading '.' are
// ordinary characters; malformed '[' is literal, and matching is case-sensitive.
fn filename_matches(name: &str, pattern: &str) -> bool {
    let text: Vec<char> = name.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let mut matched = vec![false; text.len() + 1];
    matched[0] = true;
    let mut at = 0;
    while at < pattern.len() {
        let token = pattern[at];
        let mut next = vec![false; text.len() + 1];
        if token == '*' {
            next[0] = matched[0];
            for j in 1..=text.len() {
                next[j] = matched[j] || next[j - 1];
            }
        } else if token == '?' {
            next[1..].copy_from_slice(&matched[..text.len()]);
        } else if token == '[' {
            let start = at + 1;
            let mut end = start;
            if pattern.get(end) == Some(&'!') {
                end += 1;
            }
            if pattern.get(end) == Some(&']') {
                end += 1;
            }
            while end < pattern.len() && pattern[end] != ']' {
                end += 1;
            }
            if end == pattern.len() {
                for j in 1..=text.len() {
                    next[j] = matched[j - 1] && text[j - 1] == '[';
                }
            } else {
                let negate = pattern.get(start) == Some(&'!');
                for j in 1..=text.len() {
                    let mut member = false;
                    let mut k = start + usize::from(negate);
                    while k < end {
                        if k + 2 < end && pattern[k + 1] == '-' {
                            member |= pattern[k] <= text[j - 1] && text[j - 1] <= pattern[k + 2];
                            k += 3;
                        } else {
                            member |= pattern[k] == text[j - 1];
                            k += 1;
                        }
                    }
                    next[j] = matched[j - 1] && (member != negate);
                }
                at = end;
            }
        } else {
            for j in 1..=text.len() {
                next[j] = matched[j - 1] && text[j - 1] == token;
            }
        }
        matched = next;
        at += 1;
    }
    matched[text.len()]
}
