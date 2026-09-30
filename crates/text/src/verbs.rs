use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Arg, ArgAction, ArgMatches, Command};
use goat_common::args::{flag, int_value, optional, required};
use goat_common::paths::{ensure_parent, resolve};
use goat_common::pool::PageTask;
use goat_common::textcache::{CachedTask, Form, PageEntry, cached_page_entries};
use goat_common::{Ctx, GoatError, Registry, Verb};
use pdf_core::Document;
use serde_json::{Map, Value, json};

use crate::{TextFlags, extract_page, hit_pattern, mask_text, page_text_and_words, page_words};

pub fn register(registry: &mut Registry) {
    registry.command(Verb::new(
        Command::new("text")
            .about("extract text")
            .arg(file())
            .arg(
                Arg::new("output")
                    .short('o')
                    .long("output")
                    .help("write text to a file"),
            )
            .arg(toggle("layout").help("preserve positioned columns and lines"))
            .arg(
                Arg::new("mask")
                    .long("mask")
                    .help("replace matches of this case-insensitive regex with [REDACTED]"),
            )
            .arg(no_cache()),
        text,
    ));
    registry.command(Verb::new(
        Command::new("count")
            .about("count pages, words, and characters")
            .arg(file())
            .arg(no_cache()),
        count,
    ));
    registry.family_verb(
        "get",
        Verb::new(
            Command::new("text-blocks")
                .about("extract bounded text with page geometry")
                .arg(file())
                .arg(
                    Arg::new("pages")
                        .long("pages")
                        .help("default: first 25 pages"),
                )
                .arg(
                    Arg::new("max_blocks")
                        .long("max-blocks")
                        .value_parser(int_value)
                        .default_value("200"),
                )
                .arg(
                    Arg::new("start_block")
                        .long("start-block")
                        .value_parser(int_value)
                        .default_value("0"),
                ),
            text_blocks,
        ),
    );
    registry.family_verb(
        "setup",
        Verb::new(
            Command::new("meaning")
                .about("download the model meaning search needs")
                .arg(toggle("force").help("download again over the installed files")),
            setup_meaning,
        ),
    );
    registry.family_verb(
        "setup",
        Verb::new(
            Command::new("status").about("report whether the meaning model is installed"),
            setup_status,
        ),
    );
    crate::search_verb::register(registry);
    crate::compare::register(registry);
    crate::convert::register(registry);
    crate::transcript_io::register(registry);
}

pub(crate) fn file() -> Arg {
    Arg::new("file").required(true)
}
pub(crate) fn toggle(name: &'static str) -> Arg {
    Arg::new(name).long(name).action(ArgAction::SetTrue)
}
pub(crate) fn no_cache() -> Arg {
    Arg::new("no_cache")
        .long("no-cache")
        .action(ArgAction::SetTrue)
        .help("skip the text cache")
}
pub(crate) fn path_value(path: &Path) -> Value {
    path.to_string_lossy().as_ref().into()
}
pub(crate) fn object(value: Value) -> Result<Map<String, Value>, GoatError> {
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(GoatError::message("verb result must be an object")),
    }
}

pub(crate) struct OpenDoc {
    pub doc: Document,
    pub count: usize,
}

pub(crate) fn open(source: &Path, command: Option<&str>) -> Result<OpenDoc, GoatError> {
    let bytes = fs::read(source).map_err(|e| GoatError::os(&e, source))?;
    let doc = Document::load(bytes).map_err(|_| {
        GoatError::exception(
            "FileDataError",
            format!("Failed to open file '{}'.", source.display()),
        )
    })?;
    if doc.needs_password() {
        return Err(command.map_or_else(
            || GoatError::value_error("document closed or encrypted"),
            GoatError::needs_password,
        ));
    }
    let count = doc
        .page_count()
        .map_err(|e| GoatError::message(e.to_string()))?;
    Ok(OpenDoc { doc, count })
}

struct CachedPages<'a> {
    source: &'a Path,
    form: Form,
}
impl PageTask for CachedPages<'_> {
    type Doc = OpenDoc;
    type Value = PageEntry;
    fn open(&self) -> Result<OpenDoc, GoatError> {
        open(self.source, None)
    }
    fn page(&self, doc: &mut OpenDoc, index: usize) -> Result<PageEntry, GoatError> {
        match self.form {
            Form::Text => Ok(PageEntry::Text(
                extract_page(&doc.doc, index, TextFlags::TEXT)?.text(),
            )),
            Form::Count => {
                let (text, words) = page_text_and_words(&doc.doc, index)?;
                Ok(PageEntry::Count {
                    chars: i64::try_from(text.chars().count()).unwrap_or(i64::MAX),
                    words: i64::try_from(words.len()).unwrap_or(i64::MAX),
                })
            }
            Form::Words => Ok(PageEntry::Words(page_words(&doc.doc, index)?)),
        }
    }
}
impl CachedTask for CachedPages<'_> {
    fn page_count(&self, doc: &OpenDoc) -> usize {
        doc.count
    }
}

pub(crate) fn cached(
    source: &Path,
    form: Form,
    pages: Option<&str>,
    no_cache: bool,
    ctx: &Ctx,
) -> Result<(usize, Vec<PageEntry>), GoatError> {
    cached_page_entries(
        &CachedPages { source, form },
        source,
        form,
        pages,
        no_cache,
        ctx,
    )
}

pub(crate) fn texts(
    source: &Path,
    no_cache: bool,
    ctx: &Ctx,
) -> Result<(usize, Vec<String>), GoatError> {
    let (count, entries) = cached(source, Form::Text, None, no_cache, ctx)?;
    let values = entries
        .into_iter()
        .map(|entry| match entry {
            PageEntry::Text(text) => Ok(text),
            _ => Err(GoatError::message("unexpected text cache form")),
        })
        .collect::<Result<_, _>>()?;
    Ok((count, values))
}

fn text(matches: &ArgMatches, ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let source = resolve(required::<String>(matches, "file")?)?;
    let output = optional::<String>(matches, "output")?
        .filter(|s| !s.is_empty())
        .map(|s| ensure_parent(s))
        .transpose()?;
    let pattern = optional::<String>(matches, "mask")?
        .filter(|s| !s.is_empty())
        .map(|s| hit_pattern(s))
        .transpose()?;
    let mut result = json!({"verb":"text","inputs":[path_value(&source)],"outputs":output.iter().map(|p|path_value(p)).collect::<Vec<_>>()});
    if flag(matches, "layout")? {
        let task = LayoutTask {
            source: source.clone(),
        };
        let mut doc = task.open()?;
        let indices: Vec<usize> = (0..doc.count).collect();
        let values = goat_common::pool::map_pages(&task, &mut doc, &indices, &ctx.pool()?)?;
        let mut pages = Vec::with_capacity(values.len());
        let mut full = String::new();
        for (index, value) in values.into_iter().enumerate() {
            let mut value = mask_text(value, pattern.as_ref())?;
            value["page"] = (index + 1).into();
            if index != 0 {
                full.push('\n');
            }
            if let Some(text) = value["text"].as_str() {
                full.push_str(text);
            }
            pages.push(value);
        }
        result["char_count"] = full.chars().count().into();
        result["pages"] = pages.into();
        result["mode"] = "layout".into();
        if let Some(out) = &output {
            fs::write(out, full).map_err(|e| GoatError::os(&e, out))?;
        }
    } else {
        let (page_count, mut values) = texts(&source, flag(matches, "no_cache")?, ctx)?;
        if let Some(pattern) = pattern {
            for text in &mut values {
                *text = pattern.replace_all(text, "[REDACTED]")?;
            }
        }
        let count = values.iter().map(|s| s.chars().count()).sum::<usize>()
            + values.len().saturating_sub(1);
        result["char_count"] = count.into();
        if let Some(out) = output {
            let mut handle = fs::File::create(&out).map_err(|e| GoatError::os(&e, &out))?;
            for (i, value) in values.iter().enumerate() {
                if i != 0 {
                    handle
                        .write_all(b"\n")
                        .map_err(|e| GoatError::os(&e, &out))?;
                }
                handle
                    .write_all(value.as_bytes())
                    .map_err(|e| GoatError::os(&e, &out))?;
            }
            result["page_count"] = page_count.into();
        } else {
            result["pages"] = values
                .into_iter()
                .enumerate()
                .map(|(i, text)| json!({"page":i+1,"text":text}))
                .collect();
        }
    }
    object(result)
}

struct LayoutTask {
    source: PathBuf,
}
impl PageTask for LayoutTask {
    type Doc = OpenDoc;
    type Value = Value;
    fn open(&self) -> Result<OpenDoc, GoatError> {
        open(&self.source, None)
    }
    fn page(&self, doc: &mut OpenDoc, index: usize) -> Result<Value, GoatError> {
        Ok(crate::layout::extract_page_layout(&doc.doc, index)?)
    }
}

fn count(matches: &ArgMatches, ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let source = resolve(required::<String>(matches, "file")?)?;
    let (pages, entries) = cached(&source, Form::Count, None, flag(matches, "no_cache")?, ctx)?;
    let (mut chars, mut words) = (0_i64, 0_i64);
    for entry in entries {
        if let PageEntry::Count { chars: c, words: w } = entry {
            chars += c;
            words += w;
        }
    }
    object(
        json!({"verb":"count","inputs":[path_value(&source)],"outputs":[],"pages":pages,"words":words,"chars":chars}),
    )
}

fn text_blocks(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let max = *required::<i64>(matches, "max_blocks")?;
    let start = *required::<i64>(matches, "start_block")?;
    if !(1..=1000).contains(&max) {
        return Err(GoatError::message(
            "--max-blocks must be between 1 and 1000",
        ));
    }
    if start < 0 {
        return Err(GoatError::message("--start-block must be zero or greater"));
    }
    let source = resolve(required::<String>(matches, "file")?)?;
    let doc = open(&source, None)?;
    let spec = optional::<String>(matches, "pages")?.filter(|s| !s.is_empty());
    let page_truncated = spec.is_none() && doc.count > 25;
    let indices = match spec {
        Some(spec) => goat_common::parse::parse_pages(spec, doc.count)?,
        None => (0..doc.count.min(25)).collect(),
    };
    let mut blocks = Vec::new();
    let mut skip = start;
    'pages: for index in indices {
        for block in extract_page(&doc.doc, index, TextFlags::BLOCKS)?.blocks(true) {
            if block.kind != 0 || goat_common::py::strip(&block.text).is_empty() {
                continue;
            }
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let rect = [block.rect.x0, block.rect.y0, block.rect.x1, block.rect.y1]
                .map(|v| crate::hits::round(v, 2));
            blocks.push(json!({"page":index+1,"block":block.block,"rect":rect,"text":block.text.trim_end_matches(goat_common::py::is_space)}));
            if blocks.len() > max as usize {
                break 'pages;
            }
        }
    }
    let block_truncated = blocks.len() > max as usize;
    blocks.truncate(max as usize);
    object(
        json!({"verb":"get-text-blocks","inputs":[path_value(&source)],"outputs":[],"total_pages":doc.count,"count":blocks.len(),"blocks":blocks,"start_block":start,
        "page_truncated":page_truncated,"block_truncated":block_truncated,"next_block":if block_truncated { Some(start.saturating_add(max)) } else { None },
        "next_page":if page_truncated && !block_truncated {Some(26)} else {None},"truncated":page_truncated || block_truncated}),
    )
}

fn setup_meaning(matches: &ArgMatches, ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let installed = pdf_meaning::install(ctx.home(), flag(matches, "force")?, &mut |s| {
        eprintln!("pdf-goat: {s}")
    })
    .map_err(|e| GoatError::message(e.to_string()))?;
    object(pdf_meaning::setup_meaning_result(&installed))
}
fn setup_status(_matches: &ArgMatches, ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    object(pdf_meaning::setup_status_result(&pdf_meaning::status(
        ctx.home(),
    )))
}
