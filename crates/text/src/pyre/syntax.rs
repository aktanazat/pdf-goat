use super::{ASCII, DOTALL, IGNORECASE, MULTILINE, VERBOSE, error};
use fancy_regex::{Expr, LookAround};
use goat_common::GoatError;
use std::collections::HashMap;

struct Atom {
    text: String,
    scalar: Option<char>,
    start: usize,
}
struct Group {
    start: usize,
    flags: u32,
    capture: Option<usize>,
}
struct Parser<'a> {
    source: &'a str,
    chars: Vec<char>,
    at: usize,
    flags: u32,
    out: String,
    groups: Vec<Group>,
    captures: Vec<bool>,
    names: HashMap<String, usize>,
    conditions: Vec<(usize, usize)>,
    atom: bool,
    quantifier: u8,
    prefix: bool,
}

pub(super) fn translate(source: &str, flags: u32) -> Result<String, GoatError> {
    let mut parser = Parser {
        source,
        chars: source.chars().collect(),
        at: 0,
        flags,
        out: engine_flags(flags, false),
        groups: Vec::new(),
        captures: Vec::new(),
        names: HashMap::new(),
        conditions: Vec::new(),
        atom: false,
        quantifier: 0,
        prefix: true,
    };
    while parser.at < parser.chars.len() {
        parser.step()?;
    }
    if let Some(group) = parser.groups.first() {
        return Err(error(
            source,
            "missing ), unterminated subpattern",
            group.start,
        ));
    }
    for (group, position) in parser.conditions {
        if group > parser.captures.len() {
            return Err(error(
                source,
                &format!("invalid group reference {group}"),
                position,
            ));
        }
    }
    Ok(parser.out)
}

impl Parser<'_> {
    fn step(&mut self) -> Result<(), GoatError> {
        let c = self.chars[self.at];
        if self.flags & VERBOSE != 0 {
            if " \t\n\r\u{b}\u{c}".contains(c) {
                self.at += 1;
                return Ok(());
            }
            if c == '#' {
                while self.at < self.chars.len() && self.chars[self.at] != '\n' {
                    self.at += 1;
                }
                return Ok(());
            }
        }
        match c {
            '\\' => {
                let atom = self.escape(false)?;
                self.out.push_str(&atom.text);
                self.atom = true;
                self.quantifier = 0;
                self.prefix = false;
            }
            '[' => {
                let class = self.class()?;
                self.out.push_str(&class);
                self.atom = true;
                self.quantifier = 0;
                self.prefix = false;
            }
            '(' => self.group()?,
            ')' => {
                let group = self
                    .groups
                    .pop()
                    .ok_or_else(|| error(self.source, "unbalanced parenthesis", self.at))?;
                self.flags = group.flags;
                if let Some(index) = group.capture {
                    self.captures[index - 1] = true;
                }
                self.out.push(')');
                self.at += 1;
                self.atom = true;
                self.quantifier = 0;
            }
            '*' | '+' | '?' => {
                if !self.atom {
                    return Err(error(self.source, "nothing to repeat", self.at));
                }
                if self.quantifier != 0 {
                    if self.quantifier != 1 || !matches!(c, '?' | '+') {
                        return Err(error(self.source, "multiple repeat", self.at));
                    }
                    self.quantifier = 2;
                } else {
                    self.quantifier = 1;
                }
                self.out.push(c);
                self.at += 1;
                self.prefix = false;
            }
            '{' => self.repeat()?,
            '|' => {
                self.out.push('|');
                self.at += 1;
                self.atom = false;
                self.quantifier = 0;
                self.prefix = false;
            }
            '^' | '$' => {
                if c == '$' && self.flags & MULTILINE == 0 {
                    self.out.push_str("(?=\\n?\\z)");
                } else {
                    self.out.push(c);
                }
                self.at += 1;
                self.atom = false;
                self.quantifier = 0;
                self.prefix = false;
            }
            '.' => {
                self.out.push('.');
                self.at += 1;
                self.atom = true;
                self.quantifier = 0;
                self.prefix = false;
            }
            _ => {
                self.out.push_str(&literal(c, self.flags));
                self.at += 1;
                self.atom = true;
                self.quantifier = 0;
                self.prefix = false;
            }
        }
        Ok(())
    }

    fn escape(&mut self, in_class: bool) -> Result<Atom, GoatError> {
        let start = self.at;
        self.at += 1;
        let e = *self
            .chars
            .get(self.at)
            .ok_or_else(|| error(self.source, "bad escape (end of pattern)", start))?;
        self.at += 1;
        let word = if self.flags & ASCII != 0 {
            "a-zA-Z0-9_"
        } else {
            "\\p{L}\\p{N}_"
        };
        let scalar = match e {
            'a' => Some('\u{7}'),
            'b' if in_class => Some('\u{8}'),
            'f' => Some('\u{c}'),
            'n' => Some('\n'),
            'r' => Some('\r'),
            't' => Some('\t'),
            'v' => Some('\u{b}'),
            'u' | 'U' | 'x' => {
                let n = match e {
                    'u' => 4,
                    'U' => 8,
                    _ => 2,
                };
                let mut hex = String::new();
                for _ in 0..n {
                    let c = self
                        .chars
                        .get(self.at)
                        .copied()
                        .filter(char::is_ascii_hexdigit)
                        .ok_or_else(|| {
                            error(self.source, &format!("incomplete escape \\{e}{hex}"), start)
                        })?;
                    hex.push(c);
                    self.at += 1;
                }
                let value = u32::from_str_radix(&hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| error(self.source, &format!("bad escape \\{e}{hex}"), start))?;
                Some(value)
            }
            'N' => {
                if self.chars.get(self.at) != Some(&'{') {
                    return Err(error(self.source, "missing {", self.at));
                }
                self.at += 1;
                let name_start = self.at;
                while self.at < self.chars.len() && self.chars[self.at] != '}' {
                    self.at += 1;
                }
                if self.at == self.chars.len() {
                    return Err(error(
                        self.source,
                        "missing }, unterminated name",
                        name_start,
                    ));
                }
                let name: String = self.chars[name_start..self.at].iter().collect();
                self.at += 1;
                Some(unicode_names2::character(&name).ok_or_else(|| {
                    error(
                        self.source,
                        &format!("undefined character name {name:?}").replace('"', "'"),
                        start,
                    )
                })?)
            }
            '0'..='7'
                if e == '0'
                    || in_class
                    || (self
                        .chars
                        .get(self.at)
                        .is_some_and(|c| ('0'..='7').contains(c))
                        && self
                            .chars
                            .get(self.at + 1)
                            .is_some_and(|c| ('0'..='7').contains(c))) =>
            {
                let mut octal = String::from(e);
                while octal.len() < 3
                    && self
                        .chars
                        .get(self.at)
                        .is_some_and(|c| ('0'..='7').contains(c))
                {
                    octal.push(self.chars[self.at]);
                    self.at += 1;
                }
                let value = u32::from_str_radix(&octal, 8)
                    .map_err(|_| error(self.source, "bad escape", start))?;
                if value > 255 {
                    return Err(error(
                        self.source,
                        &format!("octal escape value \\{octal} outside of range 0-0o377"),
                        start,
                    ));
                }
                Some(char::from(value as u8))
            }
            '1'..='9' if !in_class => {
                let mut digits = String::from(e);
                if self.chars.get(self.at).is_some_and(char::is_ascii_digit) {
                    digits.push(self.chars[self.at]);
                    self.at += 1;
                }
                let number = digits
                    .parse::<usize>()
                    .map_err(|_| error(self.source, "invalid group reference", start + 1))?;
                if number > self.captures.len() {
                    return Err(error(
                        self.source,
                        &format!("invalid group reference {number}"),
                        start + 1,
                    ));
                }
                if !self.captures[number - 1] {
                    return Err(error(
                        self.source,
                        "cannot refer to an open group",
                        start + 1,
                    ));
                }
                return Ok(Atom {
                    text: format!("\\{number}"),
                    scalar: None,
                    start,
                });
            }
            _ => None,
        };
        if let Some(c) = scalar {
            return Ok(Atom {
                text: if in_class {
                    hex(c)
                } else {
                    literal(c, self.flags)
                },
                scalar: Some(c),
                start,
            });
        }
        let text = match e {
            'w' => format!("[{word}]"),
            'W' => format!("[^{word}]"),
            'd' => if self.flags & ASCII != 0 {
                "[0-9]"
            } else {
                "\\d"
            }
            .to_owned(),
            'D' => if self.flags & ASCII != 0 {
                "[^0-9]"
            } else {
                "\\D"
            }
            .to_owned(),
            's' => if self.flags & ASCII != 0 {
                "[ \\t\\n\\r\\x0b\\x0c]"
            } else {
                "[\\s\\x1c-\\x1f]"
            }
            .to_owned(),
            'S' => if self.flags & ASCII != 0 {
                "[^ \\t\\n\\r\\x0b\\x0c]"
            } else {
                "[^\\s\\x1c-\\x1f]"
            }
            .to_owned(),
            'b' if !in_class => format!("(?:(?<![{word}])(?=[{word}])|(?<=[{word}])(?![{word}]))"),
            'B' if !in_class => {
                format!("(?:(?<=[{word}])(?=[{word}])|(?<![{word}])(?![{word}]))(?!\\A\\z)")
            }
            'A' if !in_class => "\\A".to_owned(),
            'Z' if !in_class => "\\z".to_owned(),
            c if c.is_ascii_alphabetic() || (in_class && c.is_ascii_digit()) => {
                return Err(error(self.source, &format!("bad escape \\{c}"), start));
            }
            c => {
                return Ok(Atom {
                    text: if in_class {
                        hex(c)
                    } else {
                        literal(c, self.flags)
                    },
                    scalar: Some(c),
                    start,
                });
            }
        };
        Ok(Atom {
            text,
            scalar: None,
            start,
        })
    }

    fn class_atom(&mut self) -> Result<Atom, GoatError> {
        if self.chars[self.at] == '\\' {
            return self.escape(true);
        }
        let start = self.at;
        let scalar = self.chars[self.at];
        self.at += 1;
        Ok(Atom {
            text: hex(scalar),
            scalar: Some(scalar),
            start,
        })
    }
    fn class(&mut self) -> Result<String, GoatError> {
        let start = self.at;
        self.at += 1;
        let negate = self.chars.get(self.at) == Some(&'^');
        if negate {
            self.at += 1;
        }
        let mut text = String::from(if negate { "[^" } else { "[" });
        let mut first = true;
        loop {
            if self.at >= self.chars.len() {
                return Err(error(self.source, "unterminated character set", start));
            }
            if self.chars[self.at] == ']' && !first {
                self.at += 1;
                break;
            }
            first = false;
            let left = self.class_atom()?;
            if self.chars.get(self.at) == Some(&'-')
                && self.chars.get(self.at + 1).is_some_and(|&c| c != ']')
            {
                self.at += 1;
                let right = self.class_atom()?;
                let (Some(a), Some(b)) = (left.scalar, right.scalar) else {
                    return Err(self.bad_range(left.start));
                };
                if a > b {
                    return Err(self.bad_range(left.start));
                }
                text.push_str(&left.text);
                text.push('-');
                text.push_str(&right.text);
                fold_range(&mut text, a, b, self.flags);
            } else {
                text.push_str(&left.text);
                if let Some(c) = left.scalar {
                    fold_range(&mut text, c, c, self.flags);
                }
            }
        }
        text.push(']');
        if self.flags & (ASCII | IGNORECASE) == ASCII | IGNORECASE {
            Ok(format!("(?-i:{text})"))
        } else {
            Ok(text)
        }
    }
    fn bad_range(&self, start: usize) -> GoatError {
        let range: String = self.chars[start..self.at].iter().collect();
        error(self.source, &format!("bad character range {range}"), start)
    }

    fn name(&mut self, terminator: char) -> Result<(String, usize), GoatError> {
        let start = self.at;
        while self.at < self.chars.len() && self.chars[self.at] != terminator {
            self.at += 1;
        }
        if self.at == self.chars.len() {
            return Err(error(
                self.source,
                &format!("missing {terminator}, unterminated name"),
                start,
            ));
        }
        let name: String = self.chars[start..self.at].iter().collect();
        self.at += 1;
        if name.is_empty() {
            return Err(error(self.source, "missing group name", start));
        }
        let mut chars = name.chars();
        let valid = chars
            .next()
            .is_some_and(|c| c == '_' || unicode_ident::is_xid_start(c))
            && chars.all(unicode_ident::is_xid_continue);
        if !valid {
            return Err(error(
                self.source,
                &format!("bad character in group name '{name}'"),
                start,
            ));
        }
        Ok((name, start))
    }
    fn group(&mut self) -> Result<(), GoatError> {
        let start = self.at;
        let prior = self.flags;
        self.at += 1;
        let mut capture = None;
        if self.chars.get(self.at) != Some(&'?') {
            self.captures.push(false);
            capture = Some(self.captures.len());
            self.out.push('(');
        } else {
            self.at += 1;
            let Some(&kind) = self.chars.get(self.at) else {
                return Err(error(self.source, "unexpected end of pattern", self.at));
            };
            match kind {
                '#' => {
                    self.at += 1;
                    while self.at < self.chars.len() && self.chars[self.at] != ')' {
                        if self.chars[self.at] == '\\' {
                            self.at += 1;
                        }
                        self.at += 1;
                    }
                    if self.at >= self.chars.len() {
                        return Err(error(self.source, "missing ), unterminated comment", start));
                    }
                    self.at += 1;
                    return Ok(());
                }
                'P' if self.chars.get(self.at + 1) == Some(&'<') => {
                    self.at += 2;
                    let (name, position) = self.name('>')?;
                    let number = self.captures.len() + 1;
                    if let Some(old) = self.names.insert(name.clone(), number) {
                        return Err(error(
                            self.source,
                            &format!(
                                "redefinition of group name '{name}' as group {number}; was group {old}"
                            ),
                            position,
                        ));
                    }
                    self.captures.push(false);
                    capture = Some(number);
                    self.out.push_str(&format!("(?P<{name}>"));
                }
                'P' if self.chars.get(self.at + 1) == Some(&'=') => {
                    self.at += 2;
                    let (name, position) = self.name(')')?;
                    let number = *self.names.get(&name).ok_or_else(|| {
                        error(
                            self.source,
                            &format!("unknown group name '{name}'"),
                            position,
                        )
                    })?;
                    if !self.captures[number - 1] {
                        return Err(error(
                            self.source,
                            "cannot refer to an open group",
                            position,
                        ));
                    }
                    self.out.push_str(&format!("(?P={name})"));
                    self.atom = true;
                    self.quantifier = 0;
                    self.prefix = false;
                    return Ok(());
                }
                ':' | '=' | '!' | '>' => {
                    self.out.push_str("(?");
                    self.out.push(kind);
                    self.at += 1;
                }
                '<' if self
                    .chars
                    .get(self.at + 1)
                    .is_some_and(|c| matches!(c, '=' | '!')) =>
                {
                    self.out.push_str("(?<");
                    self.out.push(self.chars[self.at + 1]);
                    self.at += 2;
                }
                '(' => {
                    self.at += 1;
                    let position = self.at;
                    while self.at < self.chars.len() && self.chars[self.at] != ')' {
                        self.at += 1;
                    }
                    if self.at == self.chars.len() {
                        return Err(error(self.source, "missing ), unterminated name", position));
                    }
                    let name: String = self.chars[position..self.at].iter().collect();
                    self.at += 1;
                    let number = if let Ok(number) = name.parse::<usize>() {
                        number
                    } else {
                        *self.names.get(&name).ok_or_else(|| {
                            error(
                                self.source,
                                &format!("unknown group name '{name}'"),
                                position,
                            )
                        })?
                    };
                    self.conditions.push((number, position));
                    self.out.push_str(&format!("(?({number})"));
                }
                'a' | 'i' | 'L' | 'm' | 's' | 'u' | 'x' | '-' => {
                    let mut enabled = 0;
                    let mut disabled = 0;
                    let mut minus = false;
                    let mut mode = None;
                    while let Some(&c) = self.chars.get(self.at) {
                        if matches!(c, ':' | ')') {
                            break;
                        }
                        if c == '-' && !minus {
                            minus = true;
                            self.at += 1;
                            continue;
                        }
                        let bit = match c {
                            'a' => ASCII,
                            'i' => IGNORECASE,
                            'm' => MULTILINE,
                            's' => DOTALL,
                            'u' => 32,
                            'x' => VERBOSE,
                            'L' => {
                                return Err(error(
                                    self.source,
                                    "bad inline flags: cannot use 'L' flag with a str pattern",
                                    self.at,
                                ));
                            }
                            _ => return Err(error(self.source, "unknown flag", self.at)),
                        };
                        if matches!(c, 'a' | 'u') {
                            if minus {
                                return Err(error(
                                    self.source,
                                    "bad inline flags: cannot turn off flags 'a', 'u' and 'L'",
                                    self.at,
                                ));
                            }
                            if mode.is_some_and(|m| m != c) {
                                return Err(error(
                                    self.source,
                                    "bad inline flags: flags 'a', 'u' and 'L' are incompatible",
                                    self.at,
                                ));
                            }
                            mode = Some(c);
                        }
                        if minus {
                            disabled |= bit;
                        } else {
                            enabled |= bit;
                        }
                        self.at += 1;
                    }
                    let end = *self
                        .chars
                        .get(self.at)
                        .ok_or_else(|| error(self.source, "missing -, : or )", self.at))?;
                    if end == ')' && minus {
                        return Err(error(self.source, "missing :", self.at));
                    }
                    if enabled & disabled != 0 {
                        return Err(error(
                            self.source,
                            "bad inline flags: flag turned on and off",
                            self.at,
                        ));
                    }
                    self.flags = (self.flags | enabled) & !disabled;
                    if mode == Some('u') {
                        self.flags &= !ASCII;
                    }
                    self.at += 1;
                    if end == ')' {
                        if !self.prefix || !self.groups.is_empty() {
                            return Err(error(
                                self.source,
                                "global flags not at the start of the expression",
                                start,
                            ));
                        }
                        self.out.push_str(&engine_flags(self.flags, false));
                        return Ok(());
                    }
                    self.out.push_str(&engine_flags(self.flags, true));
                }
                _ => {
                    return Err(error(
                        self.source,
                        &format!("unknown extension ?{kind}"),
                        start + 1,
                    ));
                }
            }
        }
        self.groups.push(Group {
            start,
            flags: prior,
            capture,
        });
        self.atom = false;
        self.quantifier = 0;
        self.prefix = false;
        Ok(())
    }
    fn repeat(&mut self) -> Result<(), GoatError> {
        let start = self.at;
        let mut end = start + 1;
        while end < self.chars.len() && (self.chars[end].is_ascii_digit() || self.chars[end] == ',')
        {
            end += 1;
        }
        let range: String = self.chars[start + 1..end].iter().collect();
        let parts: Vec<&str> = range.split(',').collect();
        if self.chars.get(end) != Some(&'}') || range.is_empty() || parts.len() > 2 {
            self.out.push_str("\\{");
            self.at += 1;
            self.atom = true;
            self.quantifier = 0;
            self.prefix = false;
            return Ok(());
        }
        if !self.atom {
            return Err(error(self.source, "nothing to repeat", start));
        }
        if self.quantifier != 0 {
            return Err(error(self.source, "multiple repeat", start));
        }
        let mut counts = Vec::new();
        for part in &parts {
            let value = if part.is_empty() {
                0
            } else {
                part.parse::<u64>().map_err(|_| {
                    GoatError::exception("OverflowError", "the repetition number is too large")
                })?
            };
            if value >= u64::from(u32::MAX) {
                return Err(GoatError::exception(
                    "OverflowError",
                    "the repetition number is too large",
                ));
            }
            counts.push(value);
        }
        if parts.len() == 2 && !parts[1].is_empty() && counts[0] > counts[1] {
            return Err(error(
                self.source,
                "min repeat greater than max repeat",
                start + 1,
            ));
        }
        self.out.push('{');
        if range.starts_with(',') {
            self.out.push('0');
        }
        self.out.push_str(&range);
        self.out.push('}');
        self.at = end + 1;
        self.quantifier = 1;
        self.prefix = false;
        Ok(())
    }
}

fn engine_flags(flags: u32, scoped: bool) -> String {
    let mut enabled = String::new();
    let mut disabled = String::new();
    for (c, bit) in [('i', IGNORECASE), ('m', MULTILINE), ('s', DOTALL)] {
        if flags & bit != 0 {
            enabled.push(c);
        } else {
            disabled.push(c);
        }
    }
    let disabled = if disabled.is_empty() {
        disabled
    } else {
        format!("-{disabled}")
    };
    format!(
        "(?{enabled}{disabled}{end}",
        end = if scoped { ":" } else { ")" }
    )
}
fn hex(c: char) -> String {
    format!("\\x{{{:x}}}", u32::from(c))
}
fn literal(c: char, flags: u32) -> String {
    if flags & IGNORECASE != 0 {
        if flags & ASCII != 0 {
            if c.is_ascii_alphabetic() {
                return format!(
                    "(?-i:[{}{}])",
                    c.to_ascii_lowercase(),
                    c.to_ascii_uppercase()
                );
            }
            return format!("(?-i:{})", hex(c));
        }
        if matches!(c, 'i' | 'I' | 'İ' | 'ı') {
            return "[iIİı]".to_owned();
        }
    }
    hex(c)
}
fn fold_range(out: &mut String, a: char, b: char, flags: u32) {
    if flags & IGNORECASE == 0 {
        return;
    }
    if flags & ASCII != 0 {
        for (first, last, offset) in [('a', 'z', -32), ('A', 'Z', 32)] {
            let left = a.max(first);
            let right = b.min(last);
            if left <= right {
                out.push_str(&format!(
                    "\\x{{{:x}}}-\\x{{{:x}}}",
                    u32::from(left).wrapping_add_signed(offset),
                    u32::from(right).wrapping_add_signed(offset)
                ));
            }
        }
    } else if ['i', 'I', 'İ', 'ı'].iter().any(|&c| a <= c && c <= b) {
        out.push_str("iIİı");
    }
}

pub(super) fn validate_widths(expr: &Expr) -> Result<usize, GoatError> {
    let mut groups = vec![None];
    let (minimum, _) = width(expr, &mut groups)?;
    Ok(minimum)
}
fn width(
    expr: &Expr,
    groups: &mut Vec<Option<(usize, Option<usize>)>>,
) -> Result<(usize, Option<usize>), GoatError> {
    let fixed = |n| (n, Some(n));
    Ok(match expr {
        Expr::Empty | Expr::Assertion(_) | Expr::BackrefExistsCondition { .. } => fixed(0),
        Expr::Any { .. } | Expr::Delegate { .. } => fixed(1),
        Expr::Literal { val, .. } => fixed(val.chars().count()),
        Expr::Group(child) => {
            let number = groups.len();
            groups.push(None);
            let result = width(child, groups)?;
            groups[number] = Some(result);
            result
        }
        Expr::AtomicGroup(child) => width(child, groups)?,
        Expr::Backref { group, .. } => groups.get(*group).copied().flatten().unwrap_or((0, None)),
        Expr::Repeat { child, lo, hi, .. } => {
            let (min, max) = width(child, groups)?;
            (
                min.saturating_mul(*lo),
                if *hi == usize::MAX {
                    if max == Some(0) { Some(0) } else { None }
                } else {
                    max.and_then(|v| v.checked_mul(*hi))
                },
            )
        }
        Expr::Concat(children) => {
            let mut result = fixed(0);
            for child in children {
                let (min, max) = width(child, groups)?;
                result.0 = result.0.saturating_add(min);
                result.1 = result.1.zip(max).and_then(|(a, b)| a.checked_add(b));
            }
            result
        }
        Expr::Alt(children) => {
            let mut result = None;
            for child in children {
                let (min, max) = width(child, groups)?;
                result = Some(match result {
                    None => (min, max),
                    Some((a, b)) => (min.min(a), max.zip(b).map(|(a, b)| a.max(b))),
                });
            }
            result.unwrap_or(fixed(0))
        }
        Expr::Conditional {
            condition,
            true_branch,
            false_branch,
        } => {
            width(condition, groups)?;
            let (a, b) = width(true_branch, groups)?;
            let (c, d) = width(false_branch, groups)?;
            (a.min(c), b.zip(d).map(|(b, d)| b.max(d)))
        }
        Expr::LookAround(child, kind) => {
            let (min, max) = width(child, groups)?;
            if matches!(kind, LookAround::LookBehind | LookAround::LookBehindNeg)
                && max != Some(min)
            {
                return Err(GoatError::exception(
                    "error",
                    "look-behind requires fixed-width pattern",
                ));
            }
            fixed(0)
        }
        _ => (0, None),
    })
}

pub(super) fn compile_error(pattern: &str, value: fancy_regex::Error) -> GoatError {
    match value {
        fancy_regex::Error::CompileError(e)
            if matches!(
                *e,
                fancy_regex::CompileError::LookBehindNotConst
                    | fancy_regex::CompileError::VariableLookBehindRequiresFeature
            ) =>
        {
            GoatError::exception("error", "look-behind requires fixed-width pattern")
        }
        fancy_regex::Error::ParseError(position, e) => error(
            pattern,
            &e.to_string(),
            position.min(pattern.chars().count()),
        ),
        other => GoatError::exception("error", other.to_string()),
    }
}
