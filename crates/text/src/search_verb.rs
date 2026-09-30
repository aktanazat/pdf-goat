use std::collections::HashMap;
use std::path::Path;

use clap::{Arg, ArgMatches, Command};
use goat_common::args::{flag, int_value, optional, required};
use goat_common::textcache::{Cache, Form, PageEntry, WordColumns, document_key};
use goat_common::{Ctx, GoatError, Registry, Verb};
use serde_json::{Map, Value, json};

use crate::hits::{round, rounded, union};
use crate::verbs::{cached, file, no_cache, object, open, path_value, toggle};
use crate::{HitPattern, Rect, hit_pattern, page_hits, page_words, pyre};

pub fn register(registry: &mut Registry) {
    registry.command(Verb::new(Command::new("search").about("find text and return PDF-point rectangles").arg(file()).arg(Arg::new("query").required(true))
        .arg(toggle("first").overrides_with("limit").help("stop at the first hit"))
        .arg(Arg::new("limit").long("limit").value_parser(int_value).overrides_with("first").help("stop after this many hits"))
        .arg(Arg::new("pages").long("pages").help("default: all pages")).arg(no_cache())
        .arg(toggle("meaning").help("rank passages by meaning instead of matching characters (needs: pdf-goat setup meaning)")),search)
        .schema_name("first","limit").schema_without_default("first"));
}

fn indices(spec: Option<&str>, count: usize) -> Result<Vec<usize>, GoatError> {
    match spec.filter(|s| !s.is_empty()) {
        Some(spec) => goat_common::parse::parse_pages(spec, count),
        None => Ok((0..count).collect()),
    }
}

fn search(matches: &ArgMatches, ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let source = goat_common::paths::resolve(required::<String>(matches, "file")?)?;
    let limit = if optional::<bool>(matches, "first")?
        .copied()
        .unwrap_or(false)
    {
        Some(1)
    } else {
        optional::<i64>(matches, "limit")?.copied()
    };
    if limit.is_some_and(|v| v < 1) {
        return Err(GoatError::message("--limit must be 1 or more"));
    }
    let limit = limit.map(|v| v as usize);
    let query = required::<String>(matches, "query")?;
    let spec = optional::<String>(matches, "pages")?.map(String::as_str);
    let skip_cache = flag(matches, "no_cache")?;
    if flag(matches, "meaning")? {
        return meaning(&source, query, spec, limit, skip_cache, ctx);
    }
    let tokens: Vec<String> = query
        .split(goat_common::py::is_space)
        .filter(|s| !s.is_empty())
        .map(pyre::escape)
        .collect();
    let pattern = if tokens.is_empty() {
        None
    } else {
        Some(hit_pattern(&tokens.join("\\s+"))?)
    };
    let (hits, truncated) = if let Some(limit) = limit {
        limited(&source, spec, pattern.as_ref(), limit, skip_cache, ctx)?
    } else {
        let (count, entries) = cached(&source, Form::Words, spec, skip_cache, ctx)?;
        let mut hits = Vec::new();
        for (index, entry) in indices(spec, count)?.into_iter().zip(entries) {
            append_hits(&mut hits, index, &entry, pattern.as_ref())?;
        }
        (hits, false)
    };
    object(
        json!({"verb":"search","inputs":[path_value(&source)],"outputs":[],"query":query,"mode":"literal","count":hits.len(),"hits":hits,"truncated":truncated}),
    )
}

fn append_hits(
    hits: &mut Vec<Value>,
    index: usize,
    entry: &PageEntry,
    pattern: Option<&HitPattern>,
) -> Result<(), GoatError> {
    if let (PageEntry::Words(words), Some(pattern)) = (entry, pattern) {
        for rect in page_hits(words, pattern)? {
            hits.push(json!({"page":index+1,"rect":rounded(rect)}));
        }
    }
    Ok(())
}

fn limited(
    source: &Path,
    spec: Option<&str>,
    pattern: Option<&HitPattern>,
    limit: usize,
    no_cache: bool,
    ctx: &Ctx,
) -> Result<(Vec<Value>, bool), GoatError> {
    let cache = Cache::open(
        ctx.cache_path(),
        if no_cache { 0 } else { ctx.cache_cap_bytes() },
    );
    let key = if cache.enabled() {
        document_key(source)
    } else {
        None
    };
    let stored = key.as_ref().and_then(|key| cache.document(key));
    let mut doc = if stored.is_none() {
        Some(open(source, None)?)
    } else {
        None
    };
    let mut count = stored.map_or_else(
        || doc.as_ref().map_or(0, |d| d.count),
        |info| info.page_count,
    );
    let mut pending = HashMap::new();
    // One restart is enough: a contradicted cache count is replaced by the
    // already-open document, whose count cannot change during this run.
    for attempt in 0..2 {
        let selected = indices(spec, count)?;
        let mut hits = Vec::new();
        let mut restart = false;
        let mut truncated = false;
        let run = (|| -> Result<(), GoatError> {
            'pages: for (offset, batch) in selected.chunks(64).enumerate() {
                let mut entries = key
                    .as_ref()
                    .map(|key| cache.lookup(key, Form::Words, batch))
                    .unwrap_or_default();
                for (position, &index) in batch.iter().enumerate() {
                    let entry = if let Some(value) = entries.remove(&index) {
                        value
                    } else {
                        if doc.is_none() {
                            let opened = open(source, None)?;
                            if opened.count != count {
                                if let Some(key) = &key {
                                    cache.discard_document(key);
                                }
                                count = opened.count;
                                pending.clear();
                                restart = true;
                            }
                            doc = Some(opened);
                        }
                        if restart {
                            break 'pages;
                        }
                        let opened = doc
                            .as_ref()
                            .ok_or_else(|| GoatError::message("search document was not opened"))?;
                        let value = PageEntry::Words(page_words(&opened.doc, index)?);
                        pending.insert(index, value.clone());
                        if pending.len() >= 64 {
                            if let Some(key) = &key {
                                cache.write(
                                    key,
                                    source,
                                    count,
                                    Form::Words,
                                    pending.iter().map(|(&i, v)| (i, v)),
                                );
                            }
                            pending.clear();
                        }
                        value
                    };
                    append_hits(&mut hits, index, &entry, pattern)?;
                    if hits.len() >= limit {
                        truncated =
                            hits.len() > limit || offset * 64 + position + 1 < selected.len();
                        hits.truncate(limit);
                        break 'pages;
                    }
                }
            }
            Ok(())
        })();
        if let Some(key) = &key {
            cache.write(
                key,
                source,
                count,
                Form::Words,
                pending.iter().map(|(&i, v)| (i, v)),
            );
        }
        pending.clear();
        run?;
        if !restart {
            return Ok((hits, truncated));
        }
        if attempt == 1 {
            return Err(GoatError::message(
                "document page count changed during search",
            ));
        }
    }
    Err(GoatError::message(
        "document page count changed during search",
    ))
}

#[derive(Debug)]
struct Passage {
    block: i32,
    rect: Rect,
    text: String,
}

fn passages(words: &WordColumns) -> Vec<Passage> {
    let mut out: Vec<Passage> = Vec::new();
    let mut prior_line = None;
    if words.text.is_empty() {
        return out;
    }
    for (i, text) in words.text.split('\n').enumerate() {
        let Some(rect) = words.rects.get(i * 4..i * 4 + 4) else {
            break;
        };
        let Some(line) = words.lines.get(i * 2..i * 2 + 2) else {
            break;
        };
        let rect = Rect::new(rect[0], rect[1], rect[2], rect[3]);
        if let Some(passage) = out.last_mut().filter(|p| p.block == line[0]) {
            passage.rect = union(passage.rect, rect);
            passage.text.push(if prior_line == Some(line[1]) {
                ' '
            } else {
                '\n'
            });
            passage.text.push_str(text);
        } else {
            out.push(Passage {
                block: line[0],
                rect,
                text: text.to_owned(),
            });
        }
        prior_line = Some(line[1]);
    }
    out
}

fn meaning(
    source: &Path,
    query: &str,
    spec: Option<&str>,
    limit: Option<usize>,
    no_cache: bool,
    ctx: &Ctx,
) -> Result<Map<String, Value>, GoatError> {
    let model = pdf_meaning::load(ctx.home()).map_err(|e| GoatError::message(e.to_string()))?;
    let (count, entries) = cached(source, Form::Words, spec, no_cache, ctx)?;
    let mut candidates = Vec::new();
    for (index, entry) in indices(spec, count)?.into_iter().zip(entries) {
        if let PageEntry::Words(words) = entry {
            for passage in passages(&words) {
                if !goat_common::py::strip(&passage.text).is_empty() {
                    candidates.push((index, passage));
                }
            }
        }
    }
    let texts: Vec<&str> = candidates.iter().map(|(_, p)| p.text.as_str()).collect();
    let scores = model.score(query, &texts);
    let mut ranked: Vec<_> = candidates
        .into_iter()
        .zip(scores)
        .map(|((page, p), score)| (page, p, round(score, 4)))
        .collect();
    ranked.sort_by(|a, b| {
        b.2.total_cmp(&a.2)
            .then(a.0.cmp(&b.0))
            .then(a.1.block.cmp(&b.1.block))
    });
    let candidates = ranked.len();
    if let Some(limit) = limit {
        ranked.truncate(limit);
    }
    let hits:Vec<Value>=ranked.into_iter().map(|(page,p,score)|json!({"page":page+1,"block":p.block,"rect":rounded(p.rect),"text":p.text,"score":score})).collect();
    object(
        json!({"verb":"search","inputs":[path_value(source)],"outputs":[],"query":query,"mode":"meaning","count":hits.len(),"candidates":candidates,
        "hits":hits,"truncated":limit.is_some_and(|limit|candidates>limit),"model":{"id":pdf_meaning::MODEL_ID,"revision":pdf_meaning::MODEL_REVISION,"dim":model.dim()}}),
    )
}
