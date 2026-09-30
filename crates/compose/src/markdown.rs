//! Markdown to HTML in the shape Python-markdown gives `from-md` with the `tables`,
//! `fenced_code`, `sane_lists`, and `toc` extensions: slugged heading ids, a `[TOC]`
//! paragraph replaced by the contents list, `language-` classes on fenced code, and
//! `text-align` styles on aligned table cells.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt::Write;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use unicode_normalization::UnicodeNormalization;

/// One heading as the `toc` extension records it.
struct Heading {
    level: u8,
    id: String,
    name: String,
}

/// Renders `text` as the HTML body Python-markdown produces.
pub fn to_html(text: &str) -> String {
    let source = python_lists(text);
    let events: Vec<Event<'_>> = Parser::new_ext(&source, Options::ENABLE_TABLES).collect();
    let headings = collect_headings(&events);
    let mut writer = Writer {
        out: String::with_capacity(text.len() * 2),
        headings: &headings,
        next_heading: 0,
    };
    writer.run(&events);
    writer.out
}

/// Python's four-space list indentation and sane-list type boundaries differ from
/// CommonMark. Preserve the source outside list prefixes, including fenced code.
fn python_lists(text: &str) -> Cow<'_, str> {
    let mut output = String::new();
    let mut copied = 0;
    let mut offset = 0;
    let mut levels = Vec::new();
    let mut blank = true;
    let mut fence: Option<(u8, usize)> = None;
    for line in text.split_inclusive('\n') {
        let indent = line.bytes().take_while(|&byte| byte == b' ').count();
        let body = &line[indent..];
        let first = body.as_bytes().first().copied();
        let ticks = first
            .filter(|&byte| matches!(byte, b'`' | b'~'))
            .map_or(0, |byte| {
                body.bytes().take_while(|&value| value == byte).count()
            });
        if indent < 4 && ticks >= 3 {
            if let Some((byte, length)) = fence {
                if first == Some(byte) && ticks >= length {
                    fence = None;
                }
            } else if let Some(byte) = first {
                fence = Some((byte, ticks));
            }
            offset += line.len();
            continue;
        }
        if fence.is_some() {
            offset += line.len();
            continue;
        }
        if body.trim().is_empty() {
            blank = true;
            offset += line.len();
            continue;
        }
        let digits = body.bytes().take_while(u8::is_ascii_digit).count();
        let ordered = digits > 0 && body.as_bytes().get(digits) == Some(&b'.');
        let marker = if ordered {
            digits + 1
        } else if matches!(first, Some(b'-' | b'+' | b'*')) {
            1
        } else {
            0
        };
        let is_item = marker > 0
            && body
                .as_bytes()
                .get(marker)
                .is_some_and(u8::is_ascii_whitespace);
        if is_item && (indent < 4 || !levels.is_empty()) {
            let level = indent / 4;
            let literal = !blank && levels.get(level).is_some_and(|&kind| kind != ordered);
            let spaces = level * 4;
            if literal || spaces != indent {
                if output.is_empty() {
                    output.reserve(text.len());
                }
                output.push_str(&text[copied..offset]);
                output.extend(std::iter::repeat_n(' ', spaces));
                if literal {
                    output.push_str(&body[..marker - 1]);
                    output.push('\\');
                    output.push_str(&body[marker - 1..]);
                } else {
                    output.push_str(body);
                }
                copied = offset + line.len();
            }
            if !literal {
                levels.resize(level + 1, ordered);
                levels[level] = ordered;
            }
        } else if blank && indent == 0 {
            levels.clear();
        }
        blank = false;
        offset += line.len();
    }
    if copied == 0 {
        Cow::Borrowed(text)
    } else {
        output.push_str(&text[copied..]);
        Cow::Owned(output)
    }
}

fn collect_headings(events: &[Event<'_>]) -> Vec<Heading> {
    let mut used = HashSet::new();
    let mut headings = Vec::new();
    let mut current: Option<(u8, String)> = None;
    for event in events {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                current = Some((*level as u8, String::new()))
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, name)) = current.take() {
                    let name = name.trim().to_owned();
                    let id = unique(slugify(&name), &mut used);
                    headings.push(Heading { level, id, name });
                }
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some((_, name)) = current.as_mut() {
                    name.push_str(text);
                }
            }
            _ => {}
        }
    }
    headings
}

/// `markdown.extensions.toc.slugify` with the `-` separator.
fn slugify(value: &str) -> String {
    let ascii: String = value.nfkd().filter(char::is_ascii).collect();
    let kept: String = ascii
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-') || c.is_ascii_whitespace())
        .collect();
    let lowered = kept.trim().to_ascii_lowercase();
    let mut slug = String::with_capacity(lowered.len());
    let mut in_run = false;
    for c in lowered.chars() {
        if c == '-' || c.is_ascii_whitespace() {
            if !in_run {
                slug.push('-');
            }
            in_run = true;
        } else {
            slug.push(c);
            in_run = false;
        }
    }
    slug
}

/// `markdown.extensions.toc.unique`: `_1`, `_2`, ... until the id is unused and not empty.
fn unique(mut id: String, used: &mut HashSet<String>) -> String {
    while id.is_empty() || used.contains(&id) {
        let counted = id
            .rsplit_once('_')
            .filter(|(_, count)| !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|(stem, count)| Some((stem.to_owned(), count.parse::<u64>().ok()?)));
        id = match counted {
            Some((stem, count)) => count
                .checked_add(1)
                .map_or_else(|| format!("{id}_1"), |next| format!("{stem}_{next}")),
            None => format!("{id}_1"),
        };
    }
    used.insert(id.clone());
    id
}

fn escape_text(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

fn escape_attr(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '"' => out.push_str("&quot;"),
            _ => escape_text(out, c.encode_utf8(&mut [0; 4])),
        }
    }
}

/// `nest_toc_tokens`: each heading goes under the closest earlier heading of a lower
/// level. Returns the top-level indices and the children of every heading.
fn nest(headings: &[Heading]) -> (Vec<usize>, Vec<Vec<usize>>) {
    let mut children = vec![Vec::new(); headings.len()];
    let mut top = Vec::new();
    let Some(first) = headings.first() else {
        return (top, children);
    };
    top.push(0);
    let mut last = 0usize;
    let mut levels = vec![first.level];
    let mut parents: Vec<usize> = Vec::new();
    for (index, heading) in headings.iter().enumerate().skip(1) {
        let level = heading.level;
        if levels.last().is_some_and(|&previous| level < previous) {
            levels.pop();
            let to_pop = parents
                .iter()
                .rev()
                .take_while(|&&parent| level <= headings[parent].level)
                .count();
            levels.truncate(levels.len().saturating_sub(to_pop));
            parents.truncate(parents.len() - to_pop);
            levels.push(level);
        }
        if levels.last() == Some(&level) {
            match parents.last() {
                Some(&parent) => children[parent].push(index),
                None => top.push(index),
            }
        } else {
            children[last].push(index);
            parents.push(last);
            levels.push(level);
        }
        last = index;
    }
    (top, children)
}

/// `build_toc_div`: the nested contents list.
fn write_toc(out: &mut String, headings: &[Heading]) {
    fn list(out: &mut String, headings: &[Heading], children: &[Vec<usize>], items: &[usize]) {
        out.push_str("<ul>\n");
        for &item in items {
            let heading = &headings[item];
            out.push_str("<li><a href=\"#");
            escape_attr(out, &heading.id);
            out.push_str("\">");
            escape_text(out, &heading.name);
            out.push_str("</a>");
            if !children[item].is_empty() {
                list(out, headings, children, &children[item]);
            }
            out.push_str("</li>\n");
        }
        out.push_str("</ul>\n");
    }
    let (top, children) = nest(headings);
    out.push_str("<div class=\"toc\">\n");
    list(out, headings, &children, &top);
    out.push_str("</div>\n");
}

/// The number of events after a paragraph start that make up a `[TOC]` marker paragraph,
/// its end included.
fn toc_marker(events: &[Event<'_>]) -> Option<usize> {
    let mut text = String::new();
    for (index, event) in events.iter().enumerate() {
        match event {
            Event::Text(part) => text.push_str(part),
            Event::End(TagEnd::Paragraph) => return (text.trim() == "[TOC]").then_some(index + 1),
            _ => return None,
        }
    }
    None
}

struct Writer<'h> {
    out: String,
    headings: &'h [Heading],
    next_heading: usize,
}

impl Writer<'_> {
    fn run(&mut self, events: &[Event<'_>]) {
        let mut alignments: Vec<Alignment> = Vec::new();
        let mut cell = 0usize;
        let mut in_head = false;
        // An image's alt text collects until its end, then the tag is written whole.
        let mut image: Option<(String, String)> = None;
        let mut index = 0;
        while let Some(event) = events.get(index) {
            index += 1;
            if let Some((alt, _)) = image.as_mut() {
                match event {
                    Event::End(TagEnd::Image) => {
                        if let Some((alt, tail)) = image.take() {
                            self.out.push_str("<img alt=\"");
                            escape_attr(&mut self.out, &alt);
                            self.out.push('"');
                            self.out.push_str(&tail);
                        }
                    }
                    Event::Text(text) | Event::Code(text) => alt.push_str(text),
                    _ => {}
                }
                continue;
            }
            match event {
                Event::Start(Tag::Paragraph) => match toc_marker(&events[index..]) {
                    Some(skip) => {
                        index += skip;
                        write_toc(&mut self.out, self.headings);
                    }
                    None => self.out.push_str("<p>"),
                },
                Event::End(TagEnd::Paragraph) => self.out.push_str("</p>\n"),
                Event::Start(Tag::Heading { level, .. }) => {
                    let id = self
                        .headings
                        .get(self.next_heading)
                        .map(|heading| heading.id.as_str())
                        .unwrap_or_default();
                    self.next_heading += 1;
                    let _ = write!(self.out, "<h{} id=\"", *level as u8);
                    escape_attr(&mut self.out, id);
                    self.out.push_str("\">");
                }
                Event::End(TagEnd::Heading(level)) => {
                    let _ = writeln!(self.out, "</h{}>", *level as u8);
                }
                Event::Start(Tag::BlockQuote(_)) => self.out.push_str("<blockquote>\n"),
                Event::End(TagEnd::BlockQuote(_)) => self.out.push_str("</blockquote>\n"),
                Event::Start(Tag::CodeBlock(kind)) => {
                    self.out.push_str("<pre><code");
                    if let CodeBlockKind::Fenced(info) = kind
                        && let Some(lang) = info.split_whitespace().next()
                    {
                        self.out.push_str(" class=\"language-");
                        escape_attr(&mut self.out, lang);
                        self.out.push('"');
                    }
                    self.out.push('>');
                }
                Event::End(TagEnd::CodeBlock) => self.out.push_str("</code></pre>\n"),
                Event::Start(Tag::List(Some(1))) => self.out.push_str("<ol>\n"),
                Event::Start(Tag::List(Some(start))) => {
                    let _ = writeln!(self.out, "<ol start=\"{start}\">");
                }
                Event::Start(Tag::List(None)) => self.out.push_str("<ul>\n"),
                Event::End(TagEnd::List(true)) => self.out.push_str("</ol>\n"),
                Event::End(TagEnd::List(false)) => self.out.push_str("</ul>\n"),
                Event::Start(Tag::Item) => self.out.push_str("<li>"),
                Event::End(TagEnd::Item) => self.out.push_str("</li>\n"),
                Event::Start(Tag::Table(aligns)) => {
                    alignments.clone_from(aligns);
                    self.out.push_str("<table>\n");
                }
                Event::End(TagEnd::Table) => self.out.push_str("</tbody>\n</table>\n"),
                Event::Start(Tag::TableHead) => {
                    in_head = true;
                    cell = 0;
                    self.out.push_str("<thead>\n<tr>\n");
                }
                Event::End(TagEnd::TableHead) => {
                    in_head = false;
                    self.out.push_str("</tr>\n</thead>\n<tbody>\n");
                }
                Event::Start(Tag::TableRow) => {
                    cell = 0;
                    self.out.push_str("<tr>\n");
                }
                Event::End(TagEnd::TableRow) => self.out.push_str("</tr>\n"),
                Event::Start(Tag::TableCell) => {
                    self.out.push_str(if in_head { "<th" } else { "<td" });
                    let align = match alignments.get(cell) {
                        Some(Alignment::Left) => Some("left"),
                        Some(Alignment::Center) => Some("center"),
                        Some(Alignment::Right) => Some("right"),
                        Some(Alignment::None) | None => None,
                    };
                    if let Some(align) = align {
                        let _ = write!(self.out, " style=\"text-align: {align};\"");
                    }
                    self.out.push('>');
                }
                Event::End(TagEnd::TableCell) => {
                    self.out
                        .push_str(if in_head { "</th>\n" } else { "</td>\n" });
                    cell += 1;
                }
                Event::Start(Tag::Emphasis) => self.out.push_str("<em>"),
                Event::End(TagEnd::Emphasis) => self.out.push_str("</em>"),
                Event::Start(Tag::Strong) => self.out.push_str("<strong>"),
                Event::End(TagEnd::Strong) => self.out.push_str("</strong>"),
                Event::Start(Tag::Link {
                    dest_url, title, ..
                }) => {
                    self.out.push_str("<a href=\"");
                    escape_attr(&mut self.out, dest_url);
                    self.out.push('"');
                    if !title.is_empty() {
                        self.out.push_str(" title=\"");
                        escape_attr(&mut self.out, title);
                        self.out.push('"');
                    }
                    self.out.push('>');
                }
                Event::End(TagEnd::Link) => self.out.push_str("</a>"),
                Event::Start(Tag::Image {
                    dest_url, title, ..
                }) => {
                    let mut tail = String::from(" src=\"");
                    escape_attr(&mut tail, dest_url);
                    tail.push('"');
                    if !title.is_empty() {
                        tail.push_str(" title=\"");
                        escape_attr(&mut tail, title);
                        tail.push('"');
                    }
                    tail.push_str(" />");
                    image = Some((String::new(), tail));
                }
                Event::Text(text) => escape_text(&mut self.out, text),
                Event::Code(text) => {
                    self.out.push_str("<code>");
                    escape_text(&mut self.out, text);
                    self.out.push_str("</code>");
                }
                Event::Html(html) | Event::InlineHtml(html) => self.out.push_str(html),
                Event::SoftBreak => self.out.push('\n'),
                Event::HardBreak => self.out.push_str("<br />\n"),
                Event::Rule => self.out.push_str("<hr />\n"),
                _ => {}
            }
        }
    }
}
