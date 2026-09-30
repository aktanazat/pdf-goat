use super::error;
use fancy_regex::{Captures, Regex};
use goat_common::GoatError;

pub(super) enum Part {
    Literal(String),
    Group(usize),
}

pub(super) fn parse(source: &str, regex: &Regex) -> Result<Vec<Part>, GoatError> {
    let chars: Vec<char> = source.chars().collect();
    let mut at = 0;
    let mut literal = String::new();
    let mut parts = Vec::new();
    while at < chars.len() {
        if chars[at] != '\\' {
            literal.push(chars[at]);
            at += 1;
            continue;
        }
        let start = at;
        at += 1;
        let c = *chars
            .get(at)
            .ok_or_else(|| error(source, "bad escape (end of pattern)", start))?;
        at += 1;
        let group = if c == 'g' {
            if chars.get(at) != Some(&'<') {
                return Err(error(source, "missing <", at));
            }
            at += 1;
            let position = at;
            while at < chars.len() && chars[at] != '>' {
                at += 1;
            }
            if at == chars.len() {
                return Err(error(source, "missing >, unterminated name", position));
            }
            let name: String = chars[position..at].iter().collect();
            at += 1;
            if name.is_empty() {
                return Err(error(source, "missing group name", position));
            }
            let number = if let Ok(number) = name.parse::<usize>() {
                number
            } else {
                let mut letters = name.chars();
                if !letters
                    .next()
                    .is_some_and(|c| c == '_' || unicode_ident::is_xid_start(c))
                    || !letters.all(unicode_ident::is_xid_continue)
                {
                    return Err(error(
                        source,
                        &format!("bad character in group name '{name}'"),
                        position,
                    ));
                }
                regex
                    .capture_names()
                    .position(|n| n == Some(name.as_str()))
                    .ok_or_else(|| {
                        GoatError::exception("IndexError", format!("unknown group name '{name}'"))
                    })?
            };
            Some((number, position))
        } else if ('0'..='7').contains(&c)
            && (c == '0'
                || (chars.get(at).is_some_and(|c| ('0'..='7').contains(c))
                    && chars.get(at + 1).is_some_and(|c| ('0'..='7').contains(c))))
        {
            let mut octal = String::from(c);
            while octal.len() < 3 && chars.get(at).is_some_and(|c| ('0'..='7').contains(c)) {
                octal.push(chars[at]);
                at += 1;
            }
            let value =
                u32::from_str_radix(&octal, 8).map_err(|_| error(source, "bad escape", start))?;
            if value > 255 {
                return Err(error(
                    source,
                    &format!("octal escape value \\{octal} outside of range 0-0o377"),
                    start,
                ));
            }
            literal.push(char::from(value as u8));
            None
        } else if ('1'..='9').contains(&c) {
            let mut number = usize::from(c as u8 - b'0');
            if chars.get(at).is_some_and(char::is_ascii_digit) {
                number = number * 10 + usize::from(chars[at] as u8 - b'0');
                at += 1;
            }
            Some((number, start + 1))
        } else {
            let escaped = match c {
                'a' => '\u{7}',
                'b' => '\u{8}',
                'f' => '\u{c}',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                'v' => '\u{b}',
                '\\' => '\\',
                _ => c,
            };
            if c.is_ascii_alphabetic() && escaped == c {
                return Err(error(source, &format!("bad escape \\{c}"), start));
            }
            if escaped == c && c != '\\' {
                literal.push('\\');
            }
            literal.push(escaped);
            None
        };
        if let Some((number, position)) = group {
            if number >= regex.captures_len() {
                return Err(error(
                    source,
                    &format!("invalid group reference {number}"),
                    position,
                ));
            }
            if !literal.is_empty() {
                parts.push(Part::Literal(std::mem::take(&mut literal)));
            }
            parts.push(Part::Group(number));
        }
    }
    if !literal.is_empty() {
        parts.push(Part::Literal(literal));
    }
    Ok(parts)
}

pub(super) fn append(parts: &[Part], captures: &Captures<'_, str>, out: &mut String) {
    for part in parts {
        match part {
            Part::Literal(text) => out.push_str(text),
            Part::Group(index) => {
                if let Some(group) = captures.get(*index) {
                    out.push_str(group.as_str());
                }
            }
        }
    }
}
