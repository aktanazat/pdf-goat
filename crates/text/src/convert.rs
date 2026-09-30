use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use clap::{Arg, ArgMatches, Command};
use goat_common::args::{optional, required};
use goat_common::paths::{default_out, ensure_parent, resolve};
use goat_common::pool::{PageTask, map_pages};
use goat_common::{Ctx, GoatError, Registry, Verb};
use serde_json::{Map, Value, json};

use crate::verbs::{OpenDoc, file, object, open, path_value, texts};

pub fn register(registry: &mut Registry) {
    registry.family_verb(
        "convert",
        Verb::new(
            Command::new("html")
                .about("convert PDF pages to HTML")
                .arg(file())
                .arg(Arg::new("output").short('o').long("output")),
            html,
        ),
    );
    registry.family_verb(
        "convert",
        Verb::new(
            Command::new("audio")
                .about("export extracted text as AIFF with macOS say")
                .arg(file())
                .arg(Arg::new("voice").long("voice"))
                .arg(Arg::new("output").short('o').long("output")),
            audio,
        ),
    );
}

fn out_path(
    matches: &ArgMatches,
    source: &Path,
    suffix: &str,
    ext: &str,
) -> Result<PathBuf, GoatError> {
    match optional::<String>(matches, "output")?.filter(|s| !s.is_empty()) {
        Some(path) => ensure_parent(path),
        None => ensure_parent(&default_out(&source.to_string_lossy(), suffix, ext)?),
    }
}

struct HtmlTask<'a> {
    source: &'a Path,
}
impl PageTask for HtmlTask<'_> {
    type Doc = OpenDoc;
    type Value = String;
    fn open(&self) -> Result<OpenDoc, GoatError> {
        open(self.source, None)
    }
    fn page(&self, doc: &mut OpenDoc, index: usize) -> Result<String, GoatError> {
        let page = crate::load_page(&doc.doc, index)?;
        let media = page.media_box();
        let crop = page.crop_box();
        let rect = crate::Rect::new(crop.x0, media.y1 - crop.y1, crop.x1, media.y1 - crop.y0);
        Ok(crate::extract(&doc.doc, &page, rect, crate::TextFlags::HTML)?.html(0))
    }
}

fn html(matches: &ArgMatches, ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let source = resolve(required::<String>(matches, "file")?)?;
    let out = out_path(matches, &source, "from-pdf", "html")?;
    let task = HtmlTask { source: &source };
    let mut doc = task.open()?;
    let indices: Vec<usize> = (0..doc.count).collect();
    let pages = map_pages(&task, &mut doc, &indices, &ctx.pool()?)?;
    let mut html = String::from("<!DOCTYPE html><html><head><meta charset='utf-8'></head><body>");
    for (i, page) in pages.iter().enumerate() {
        if i != 0 {
            html.push_str("\n<hr/>\n");
        }
        html.push_str(page);
    }
    html.push_str("</body></html>");
    fs::write(&out, html).map_err(|e| GoatError::os(&e, &out))?;
    object(
        json!({"verb":"convert-html","inputs":[path_value(&source)],"outputs":[path_value(&out)]}),
    )
}

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|p| {
                p.is_file() && fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
            })
    })
}

fn audio(matches: &ArgMatches, ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let source = resolve(required::<String>(matches, "file")?)?;
    let out = out_path(matches, &source, "audio", "aiff")?;
    let say = on_path("say")
        .ok_or_else(|| GoatError::message("convert audio requires macOS say on PATH"))?;
    let (_, pages) = texts(&source, true, ctx)?;
    let joined = pages.join("\n");
    let text = goat_common::py::strip(&joined);
    if text.is_empty() {
        return Err(GoatError::message("no extractable text to read aloud"));
    }
    let mut input = tempfile::Builder::new()
        .suffix(".txt")
        .tempfile()
        .map_err(|e| GoatError::os_unnamed(&e))?;
    input
        .write_all(text.as_bytes())
        .map_err(|e| GoatError::os(&e, input.path()))?;
    input.flush().map_err(|e| GoatError::os(&e, input.path()))?;
    let voice = optional::<String>(matches, "voice")?;
    let mut command = Process::new(say);
    command.arg("-o").arg(&out).arg("-f").arg(input.path());
    if let Some(voice) = voice.filter(|s| !s.is_empty()) {
        command.arg("-v").arg(voice);
    }
    let result = command.output().map_err(|e| GoatError::os_unnamed(&e))?;
    drop(input);
    if !result.status.success() || !out.exists() {
        let stderr = String::from_utf8_lossy(&result.stderr);
        return Err(GoatError::message(format!(
            "say failed: {}",
            goat_common::py::strip(&stderr)
                .chars()
                .take(200)
                .collect::<String>()
        )));
    }
    let mut duration = None;
    if let Some(afinfo) = on_path("afinfo") {
        let result = Process::new(afinfo)
            .arg(&out)
            .output()
            .map_err(|e| GoatError::os_unnamed(&e))?;
        let info = String::from_utf8_lossy(&result.stdout);
        if let Some((_, after)) = info.split_once("estimated duration:") {
            let number: String = after
                .trim_start_matches(goat_common::py::is_space)
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if !number.is_empty() {
                duration = Some(crate::hits::round(
                    goat_common::py::parse_float(&number)?,
                    2,
                ));
            }
        }
    }
    object(
        json!({"verb":"convert-audio","inputs":[path_value(&source)],"outputs":[path_value(&out)],"chars":text.chars().count(),"voice":voice,"duration_sec":duration}),
    )
}
