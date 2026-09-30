use std::collections::{HashMap, HashSet};
use std::path::Path;

use clap::{Arg, ArgMatches, Command};
use goat_common::args::{flag, int_value, many, optional, required};
use goat_common::{Ctx, GoatError, Registry, Verb};
use serde_json::{Map, Value, json};

use crate::difflib::{Matcher, unified};
use crate::verbs::{file, no_cache, object, path_value, texts};

pub fn register(registry: &mut Registry) {
    registry.family_verb(
        "compare",
        Verb::new(
            Command::new("text")
                .about("map pages by text and show line changes")
                .arg(file())
                .arg(Arg::new("others").required(true).num_args(1..))
                .arg(
                    Arg::new("context")
                        .long("context")
                        .value_parser(int_value)
                        .default_value("1")
                        .help("unchanged lines around each change"),
                )
                .arg(
                    Arg::new("max_lines")
                        .long("max-lines")
                        .value_parser(int_value)
                        .default_value("20")
                        .help("diff lines returned per page"),
                )
                .arg(
                    Arg::new("mask")
                        .long("mask")
                        .help("replace matches of this case-insensitive regex with [REDACTED]"),
                )
                .arg(no_cache()),
            compare,
        ),
    );
}

#[derive(Clone, Copy)]
struct Candidate<'a> {
    file: &'a Path,
    index: usize,
    lines: &'a [String],
}

fn mapped<'a>(
    index: usize,
    lines: &[String],
    first: &[Candidate<'a>],
    candidates: &[Candidate<'a>],
    exact: &HashMap<&'a [String], Candidate<'a>>,
) -> Option<(Candidate<'a>, f64)> {
    if lines.is_empty() {
        return None;
    }
    let positional = first.get(index).copied().filter(|c| !c.lines.is_empty());
    if let Some(candidate) = positional.filter(|c| c.lines == lines) {
        return Some((candidate, 1.0));
    }
    if let Some(&candidate) = exact.get(lines) {
        return Some((candidate, 1.0));
    }
    let matcher = Matcher::new(lines, false);
    let mut best = None;
    let mut best_ratio = -1.0;
    for candidate in positional.into_iter().chain(candidates.iter().copied()) {
        if matcher.real_quick_ratio(candidate.lines) <= best_ratio
            || matcher.quick_ratio(candidate.lines) <= best_ratio
        {
            continue;
        }
        let ratio = matcher.ratio(candidate.lines);
        if ratio > best_ratio {
            best = Some((candidate, ratio));
            best_ratio = ratio;
        }
    }
    best
}

fn text_lines(text: &str) -> Vec<String> {
    text.split([
        '\n', '\r', '\u{b}', '\u{c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}',
        '\u{2029}',
    ])
    .map(goat_common::py::strip)
    .filter(|s| !s.is_empty())
    .map(str::to_owned)
    .collect()
}

fn compare(matches: &ArgMatches, ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let context = *required::<i64>(matches, "context")?;
    let max_lines = *required::<i64>(matches, "max_lines")?;
    if context < 0 {
        return Err(GoatError::message("--context must be zero or greater"));
    }
    if max_lines < 0 {
        return Err(GoatError::message("--max-lines must be zero or greater"));
    }
    let mask = optional::<String>(matches, "mask")?
        .filter(|s| !s.is_empty())
        .map(|s| crate::hit_pattern(s))
        .transpose()?;
    let left = goat_common::paths::resolve(required::<String>(matches, "file")?)?;
    let mut others = Vec::new();
    for raw in many::<String>(matches, "others")? {
        let path = goat_common::paths::resolve(raw)?;
        if !others.contains(&path) {
            others.push(path);
        }
    }
    let no_cache = flag(matches, "no_cache")?;
    let mut all = HashMap::new();
    for path in std::iter::once(&left).chain(&others) {
        if !all.contains_key(path) {
            let (_, values) = texts(path, no_cache, ctx)?;
            all.insert(
                path.clone(),
                values.iter().map(|s| text_lines(s)).collect::<Vec<_>>(),
            );
        }
    }
    let first_path = others
        .first()
        .ok_or_else(|| GoatError::message("compare text requires another file"))?;
    let first: Vec<Candidate<'_>> = all[first_path]
        .iter()
        .enumerate()
        .map(|(index, lines)| Candidate {
            file: first_path,
            index,
            lines,
        })
        .collect();
    let mut candidates = Vec::new();
    for file in &others {
        for (index, lines) in all[file].iter().enumerate() {
            if !lines.is_empty() {
                candidates.push(Candidate { file, index, lines });
            }
        }
    }
    let mut exact = HashMap::new();
    for &candidate in &candidates {
        exact.entry(candidate.lines).or_insert(candidate);
    }
    let mut pages = Vec::new();
    let mut matched = HashSet::new();
    let mut identical = true;
    let (mut total_added, mut total_removed) = (0, 0);
    for (index, lines) in all[&left].iter().enumerate() {
        let mut entry = json!({"page":index+1,"match":null,"added":0,"removed":0,"diff":[],"diff_truncated":false});
        if let Some((candidate, ratio)) = mapped(index, lines, &first, &candidates, &exact) {
            matched.insert((candidate.file, candidate.index));
            let mut diff = unified(lines, candidate.lines, context as usize);
            let added = diff.iter().filter(|s| s.starts_with('+')).count();
            let removed = diff.iter().filter(|s| s.starts_with('-')).count();
            identical = identical && diff.is_empty();
            total_added += added;
            total_removed += removed;
            entry["match"] = json!({"file":path_value(candidate.file),"page":candidate.index+1,"ratio":crate::hits::round(ratio,3)});
            entry["added"] = added.into();
            entry["removed"] = removed.into();
            entry["diff_truncated"] = (diff.len() > max_lines as usize).into();
            diff.truncate(max_lines as usize);
            entry["diff"] = crate::mask_text(json!(diff), mask.as_ref())?;
        } else {
            identical = identical && lines.is_empty();
        }
        pages.push(entry);
    }
    let unmatched: Vec<Value> = candidates
        .iter()
        .filter(|c| !matched.contains(&(c.file, c.index)))
        .map(|c| json!({"file":path_value(c.file),"page":c.index+1}))
        .collect();
    let inputs: Vec<Value> = std::iter::once(&left)
        .chain(&others)
        .map(|p| path_value(p))
        .collect();
    object(
        json!({"verb":"compare-text","inputs":inputs,"outputs":[],"identical":identical && unmatched.is_empty(),"added":total_added,"removed":total_removed,"pages":pages,"unmatched":unmatched}),
    )
}
