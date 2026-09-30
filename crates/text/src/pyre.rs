//! Python `re` syntax at the CLI boundary; fancy-regex owns execution.
//! Public match positions are Unicode character offsets, not UTF-8 bytes.

mod syntax;
mod template;

use fancy_regex::{Captures, Regex, RegexBuilder};
use goat_common::GoatError;

pub const IGNORECASE: u32 = 2;
pub const MULTILINE: u32 = 8;
pub const DOTALL: u32 = 16;
pub const VERBOSE: u32 = 64;
pub const ASCII: u32 = 256;

#[derive(Clone, Debug)]
pub struct Pattern {
    regex: Regex,
    nonempty: Option<Regex>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match<'a> {
    pub start: usize,
    pub end: usize,
    pub text: &'a str,
}

pub fn compile(pattern: &str, flags: u32) -> Result<Pattern, GoatError> {
    let translated = syntax::translate(pattern, flags)?;
    let tree = fancy_regex::Expr::parse_tree(&translated)
        .map_err(|e| syntax::compile_error(pattern, e))?;
    let minimum = syntax::validate_widths(&tree.expr)?;
    let regex = RegexBuilder::new(&translated)
        .build()
        .map_err(|e| syntax::compile_error(pattern, e))?;
    let nonempty = if minimum == 0 {
        match RegexBuilder::new(&format!("\\G(?:{translated})"))
            .find_not_empty(true)
            .build()
        {
            Ok(regex) => Some(regex),
            Err(fancy_regex::Error::CompileError(e))
                if matches!(*e, fancy_regex::CompileError::PatternCanNeverMatch) =>
            {
                None
            }
            Err(error) => return Err(runtime_error(error)),
        }
    } else {
        None
    };
    Ok(Pattern { regex, nonempty })
}

impl Pattern {
    pub fn is_match(&self, text: &str) -> Result<bool, GoatError> {
        self.regex.is_match(text).map_err(runtime_error)
    }
    pub(crate) fn captures<'a>(
        &self,
        text: &'a str,
    ) -> Result<Option<Captures<'a, str>>, GoatError> {
        self.regex.captures(text).map_err(runtime_error)
    }

    pub fn finditer<'a>(&self, text: &'a str) -> Result<Vec<Match<'a>>, GoatError> {
        let mut out = Vec::new();
        let mut byte = 0;
        let mut chars = 0;
        self.visit(
            text,
            find_at,
            |m| (m.start(), m.end()),
            |found| {
                chars += text[byte..found.start()].chars().count();
                let start = chars;
                chars += found.as_str().chars().count();
                byte = found.end();
                out.push(Match {
                    start,
                    end: chars,
                    text: found.as_str(),
                });
                Ok(())
            },
        )?;
        Ok(out)
    }

    /// Literal replacement, without interpreting backslashes in the value.
    pub fn sub_literal(&self, text: &str, replacement: &str) -> Result<String, GoatError> {
        let mut out = String::new();
        let mut end = 0;
        self.visit(
            text,
            find_at,
            |m| (m.start(), m.end()),
            |found| {
                out.push_str(&text[end..found.start()]);
                out.push_str(replacement);
                end = found.end();
                Ok(())
            },
        )?;
        out.push_str(&text[end..]);
        Ok(out)
    }

    /// Python replacement templates, including named and numbered groups.
    pub fn sub(&self, replacement: &str, text: &str) -> Result<String, GoatError> {
        let template = template::parse(replacement, &self.regex)?;
        let mut out = String::new();
        let mut end = 0;
        self.visit(text, Regex::captures_from_pos, capture_bounds, |captures| {
            let (start, stop) = capture_bounds(&captures);
            out.push_str(&text[end..start]);
            template::append(&template, &captures, &mut out);
            end = stop;
            Ok(())
        })?;
        out.push_str(&text[end..]);
        Ok(out)
    }

    // Python accepts an empty match after a nonempty match. After an empty
    // match it first retries the same position with empty results forbidden.
    // The engine still chooses every alternative and performs all backtracking.
    fn visit<'a, T>(
        &self,
        text: &'a str,
        search: fn(&Regex, &'a str, usize) -> fancy_regex::Result<Option<T>>,
        bounds: fn(&T) -> (usize, usize),
        mut consume: impl FnMut(T) -> Result<(), GoatError>,
    ) -> Result<(), GoatError> {
        let mut position = 0;
        let mut retry_nonempty = false;
        loop {
            let regex = if retry_nonempty {
                self.nonempty.as_ref()
            } else {
                Some(&self.regex)
            };
            let found = regex
                .map(|regex| search(regex, text, position))
                .transpose()
                .map_err(runtime_error)?
                .flatten();
            if let Some(found) = found {
                let (start, end) = bounds(&found);
                consume(found)?;
                position = end;
                retry_nonempty = start == end;
            } else if retry_nonempty {
                let Some(next) = text[position..].chars().next() else {
                    break;
                };
                position += next.len_utf8();
                retry_nonempty = false;
            } else {
                break;
            }
        }
        Ok(())
    }
}
fn capture_bounds(captures: &Captures<'_, str>) -> (usize, usize) {
    captures.get(0).map_or((0, 0), |m| (m.start(), m.end()))
}

fn find_at<'a>(
    regex: &Regex,
    text: &'a str,
    position: usize,
) -> fancy_regex::Result<Option<fancy_regex::Match<'a>>> {
    regex.find_from_pos(text, position)
}

pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "()[]{}?*+-|^$\\.&~# \t\n\r\u{b}\u{c}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
fn runtime_error(error: fancy_regex::Error) -> GoatError {
    GoatError::exception("error", error.to_string())
}
fn error(pattern: &str, message: &str, position: usize) -> GoatError {
    let mut text = format!("{message} at position {position}");
    if pattern.contains('\n') {
        let before: String = pattern.chars().take(position).collect();
        let line = before.chars().filter(|&c| c == '\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, |s| s.chars().count()) + 1;
        text.push_str(&format!(" (line {line}, column {column})"));
    }
    GoatError::exception("error", text)
}
