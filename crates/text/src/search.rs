use crate::{Block, Char, Line, Quad, TextPage};

impl TextPage {
    /// PyMuPDF's `JM_search_stext_page`: case-insensitive ASCII matching,
    /// collapsed whitespace, and adjacent character quads joined into runs.
    /// Build the page with [`crate::TextFlags::SEARCH_FOR`] for `search_for`.
    pub fn search(&self, needle: &str) -> Vec<Quad> {
        if needle.is_empty() || needle.starts_with('\0') {
            return Vec::new();
        }
        let mut text = String::new();
        let mut chars: Vec<(usize, &Char, &Line)> = Vec::new();
        for block in &self.blocks {
            let Block::Text(block) = block else {
                continue;
            };
            for line in &block.lines {
                for (index, ch) in line.chars.iter().enumerate() {
                    if line.joined
                        && index + 1 == line.chars.len()
                        && matches!(ch.c, '-' | '\u{ad}' | '\u{2010}' | '\u{2011}')
                    {
                        continue;
                    }
                    if self.includes(&ch.bbox(line)) {
                        chars.push((text.len(), ch, line));
                        text.push(ch.c);
                    }
                }
                if !line.joined {
                    text.push('\n');
                }
            }
            text.push('\n');
        }
        let hay = text.as_bytes();
        let mut out = Vec::new();
        let mut at = 0;
        let mut char_at = 0;
        while let Some((begin, end)) = find(hay, needle.as_bytes(), at) {
            while char_at < chars.len() && chars[char_at].0 < begin {
                char_at += 1;
            }
            while char_at < chars.len() && chars[char_at].0 < end {
                let (_, ch, line) = chars[char_at];
                highlight(&mut out, ch, line);
                char_at += 1;
            }
            if end <= at {
                break;
            }
            at = end;
        }
        out
    }
}

fn highlight(quads: &mut Vec<Quad>, ch: &Char, line: &Line) {
    let q = ch.quad;
    if let Some(last) = quads.last_mut() {
        let close = |a: crate::Point, b: crate::Point| {
            let dx = (a.x - b.x) as f32;
            let dy = (a.y - b.y) as f32;
            let x = line.dir.x as f32;
            let y = line.dir.y as f32;
            (dx * x + dy * y).abs() < ch.size as f32 * 0.2
                && (dx * y + dy * x).abs() < ch.size as f32 * 0.1
        };
        if close(last.lr, q.ll) && close(last.ur, q.ul) {
            last.ur = q.ur;
            last.lr = q.lr;
            return;
        }
    }
    quads.push(q);
}

fn canon(c: u32) -> u32 {
    match c {
        0xa0 | 0x2028 | 0x2029 | 10 | 13 | 9 => 32,
        65..=90 => c + 32,
        _ => c,
    }
}

// MuPDF advances byte-by-byte when looking for a match. A continuation
// byte reached that way decodes as a replacement character, not a panic.
fn rune(text: &[u8], at: usize) -> (u32, usize) {
    let Some(&first) = text.get(at) else {
        return (0, 0);
    };
    if first < 0x80 {
        return (u32::from(first), 1);
    }
    let count = match first {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return (0xfffd, 1),
    };
    let Some(bytes) = text.get(at..at.saturating_add(count)) else {
        return (0xfffd, 1);
    };
    if let Ok(s) = std::str::from_utf8(bytes)
        && let Some(c) = s.chars().next()
    {
        return (u32::from(c), count);
    }
    (0xfffd, 1)
}

fn match_at(hay: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    let mut h = start;
    let mut n = 0;
    let (mut hc, mut hn) = rune(hay, h);
    let (mut nc, mut nn) = rune(needle, n);
    hc = canon(hc);
    nc = canon(nc);
    while hc == nc && nc != 0 {
        let end = h + hn;
        h = end;
        n += nn;
        if hc == 32 {
            while canon(rune(hay, h).0) == 32 {
                h += rune(hay, h).1;
            }
            while canon(rune(needle, n).0) == 32 {
                n += rune(needle, n).1;
            }
        }
        let hr = rune(hay, h);
        let nr = rune(needle, n);
        hc = canon(hr.0);
        hn = hr.1;
        nc = canon(nr.0);
        nn = nr.1;
        if nc == 0 {
            return Some(end);
        }
    }
    None
}

fn find(hay: &[u8], needle: &[u8], start: usize) -> Option<(usize, usize)> {
    for at in start..hay.len() {
        if hay[at] == 0 {
            break;
        }
        if let Some(end) = match_at(hay, needle, at) {
            return Some((at, end));
        }
    }
    None
}
