//! CSS syntax: a tokenizer after CSS Syntax Level 3, stylesheet and declaration parsing,
//! and selectors (Selectors Level 3 plus `:is`, `:where`, and `:not` over compounds).

use super::dom::{Dom, NodeId};

#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    Ident(String),
    Function(String),
    AtKeyword(String),
    Hash(String),
    Str(String),
    Url(String),
    Number(f32),
    Percentage(f32),
    Dimension(f32, String),
    Delim(char),
    Whitespace,
    Colon,
    Semicolon,
    Comma,
    Open(char),
    Close(char),
    /// A bad string or bad url: it invalidates whatever contains it.
    Bad,
}

/// A component value: a token, a block, or a function with its arguments.
#[derive(Clone, Debug, PartialEq)]
pub enum Cv {
    T(Token),
    Block(char, Vec<Cv>),
    Func(String, Vec<Cv>),
}

impl Cv {
    pub fn is_whitespace(&self) -> bool {
        matches!(self, Cv::T(Token::Whitespace))
    }

    pub fn ident(&self) -> Option<&str> {
        match self {
            Cv::T(Token::Ident(name)) => Some(name),
            _ => None,
        }
    }
}

/// Values nest at most this deep; deeper blocks are dropped.
const MAX_NESTING: usize = 64;

struct Tokenizer {
    chars: Vec<char>,
    pos: usize,
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || !c.is_ascii()
}

fn is_name(c: char) -> bool {
    is_name_start(c) || c.is_ascii_digit() || c == '-'
}

impl Tokenizer {
    fn peek(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn valid_escape(&self, offset: usize) -> bool {
        self.peek(offset) == Some('\\') && !matches!(self.peek(offset + 1), Some('\n') | None)
    }

    fn starts_ident(&self, offset: usize) -> bool {
        match self.peek(offset) {
            Some('-') => {
                matches!(self.peek(offset + 1), Some(c) if is_name_start(c) || c == '-')
                    || self.valid_escape(offset + 1)
            }
            Some('\\') => self.valid_escape(offset),
            Some(c) => is_name_start(c),
            None => false,
        }
    }

    fn starts_number(&self) -> bool {
        match self.peek(0) {
            Some('+' | '-') => {
                self.peek(1).is_some_and(|c| c.is_ascii_digit())
                    || (self.peek(1) == Some('.')
                        && self.peek(2).is_some_and(|c| c.is_ascii_digit()))
            }
            Some('.') => self.peek(1).is_some_and(|c| c.is_ascii_digit()),
            Some(c) => c.is_ascii_digit(),
            None => false,
        }
    }

    /// Consumes an escape after the backslash.
    fn escape(&mut self) -> char {
        let Some(first) = self.peek(0) else {
            return '\u{FFFD}';
        };
        if !first.is_ascii_hexdigit() {
            self.pos += 1;
            return first;
        }
        let mut value = 0u32;
        let mut digits = 0;
        while digits < 6
            && let Some(c) = self.peek(0).filter(char::is_ascii_hexdigit)
        {
            value = value * 16 + c.to_digit(16).unwrap_or(0);
            self.pos += 1;
            digits += 1;
        }
        if self.peek(0).is_some_and(|c| c.is_ascii_whitespace()) {
            self.pos += 1;
        }
        match char::from_u32(value) {
            Some(c) if value != 0 => c,
            _ => '\u{FFFD}',
        }
    }

    fn name(&mut self) -> String {
        let mut out = String::new();
        loop {
            match self.peek(0) {
                Some(c) if is_name(c) => {
                    out.push(c);
                    self.pos += 1;
                }
                Some('\\') if self.valid_escape(0) => {
                    self.pos += 1;
                    out.push(self.escape());
                }
                _ => return out,
            }
        }
    }

    fn number(&mut self) -> f32 {
        let start = self.pos;
        if matches!(self.peek(0), Some('+' | '-')) {
            self.pos += 1;
        }
        while self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.peek(0) == Some('.') && self.peek(1).is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
            while self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(0), Some('e' | 'E')) {
            let digit_at = if matches!(self.peek(1), Some('+' | '-')) {
                2
            } else {
                1
            };
            if self.peek(digit_at).is_some_and(|c| c.is_ascii_digit()) {
                self.pos += digit_at;
                while self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        text.parse::<f32>().unwrap_or(0.0)
    }

    fn string(&mut self, quote: char) -> Token {
        let mut out = String::new();
        loop {
            let Some(c) = self.peek(0) else {
                return Token::Str(out);
            };
            self.pos += 1;
            match c {
                _ if c == quote => return Token::Str(out),
                '\n' => {
                    self.pos -= 1;
                    return Token::Bad;
                }
                '\\' => match self.peek(0) {
                    None => {}
                    Some('\n') => self.pos += 1,
                    Some(_) => out.push(self.escape()),
                },
                _ => out.push(c),
            }
        }
    }

    fn url(&mut self) -> Token {
        while self.peek(0).is_some_and(|c| c.is_ascii_whitespace()) {
            self.pos += 1;
        }
        let mut out = String::new();
        loop {
            let Some(c) = self.peek(0) else {
                return Token::Url(out);
            };
            self.pos += 1;
            match c {
                ')' => return Token::Url(out),
                c if c.is_ascii_whitespace() => {
                    while self.peek(0).is_some_and(|c| c.is_ascii_whitespace()) {
                        self.pos += 1;
                    }
                    if matches!(self.peek(0), Some(')') | None) {
                        self.pos += 1;
                        return Token::Url(out);
                    }
                    return self.bad_url();
                }
                '"' | '\'' | '(' => return self.bad_url(),
                '\\' if !matches!(self.peek(0), Some('\n') | None) => out.push(self.escape()),
                '\\' => return self.bad_url(),
                _ => out.push(c),
            }
        }
    }

    fn bad_url(&mut self) -> Token {
        while let Some(c) = self.peek(0) {
            self.pos += 1;
            if c == ')' {
                break;
            }
        }
        Token::Bad
    }

    fn ident_like(&mut self) -> Token {
        let name = self.name();
        if self.peek(0) == Some('(') {
            self.pos += 1;
            if name.eq_ignore_ascii_case("url") {
                let mut look = 0;
                while self.peek(look).is_some_and(|c| c.is_ascii_whitespace()) {
                    look += 1;
                }
                if !matches!(self.peek(look), Some('"' | '\'')) {
                    return self.url();
                }
            }
            return Token::Function(name);
        }
        Token::Ident(name)
    }

    fn numeric(&mut self) -> Token {
        let value = self.number();
        if self.starts_ident(0) {
            return Token::Dimension(value, self.name());
        }
        if self.peek(0) == Some('%') {
            self.pos += 1;
            return Token::Percentage(value);
        }
        Token::Number(value)
    }

    fn next(&mut self) -> Option<Token> {
        loop {
            let c = self.peek(0)?;
            if c == '/' && self.peek(1) == Some('*') {
                self.pos += 2;
                while self.pos < self.chars.len()
                    && !(self.peek(0) == Some('*') && self.peek(1) == Some('/'))
                {
                    self.pos += 1;
                }
                self.pos = (self.pos + 2).min(self.chars.len());
                continue;
            }
            break;
        }
        let c = self.peek(0)?;
        if c.is_ascii_whitespace() {
            while self.peek(0).is_some_and(|c| c.is_ascii_whitespace()) {
                self.pos += 1;
            }
            return Some(Token::Whitespace);
        }
        if c == '"' || c == '\'' {
            self.pos += 1;
            return Some(self.string(c));
        }
        if self.starts_number() {
            return Some(self.numeric());
        }
        if self.starts_ident(0) {
            return Some(self.ident_like());
        }
        self.pos += 1;
        Some(match c {
            '#' if self.peek(0).is_some_and(is_name) || self.valid_escape(0) => {
                Token::Hash(self.name())
            }
            '@' if self.starts_ident(0) => Token::AtKeyword(self.name()),
            '(' | '[' | '{' => Token::Open(c),
            ')' | ']' | '}' => Token::Close(c),
            ',' => Token::Comma,
            ':' => Token::Colon,
            ';' => Token::Semicolon,
            '<' if self.peek(0) == Some('!')
                && self.peek(1) == Some('-')
                && self.peek(2) == Some('-') =>
            {
                self.pos += 3;
                Token::Whitespace
            }
            '-' if self.peek(0) == Some('-') && self.peek(1) == Some('>') => {
                self.pos += 2;
                Token::Whitespace
            }
            _ => Token::Delim(c),
        })
    }
}

/// Tokenizes `text` into component values.
pub fn parse_component_values(text: &str) -> Vec<Cv> {
    let mut tokenizer = Tokenizer {
        chars: text.chars().collect(),
        pos: 0,
    };
    let mut tokens = Vec::new();
    while let Some(token) = tokenizer.next() {
        tokens.push(token);
    }
    let mut iter = tokens.into_iter();
    nest(&mut iter, None, 0)
}

fn closing(open: char) -> char {
    match open {
        '(' => ')',
        '[' => ']',
        _ => '}',
    }
}

fn nest(tokens: &mut impl Iterator<Item = Token>, close: Option<char>, depth: usize) -> Vec<Cv> {
    if depth >= MAX_NESTING {
        let mut open = 1usize;
        for token in tokens.by_ref() {
            match token {
                Token::Open(_) | Token::Function(_) => open += 1,
                Token::Close(_) => {
                    open -= 1;
                    if open == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        return Vec::new();
    }
    let mut out = Vec::new();
    while let Some(token) = tokens.next() {
        match token {
            Token::Close(c) if Some(c) == close => return out,
            Token::Open(open) => {
                let inner = nest(tokens, Some(closing(open)), depth + 1);
                if depth < MAX_NESTING {
                    out.push(Cv::Block(open, inner));
                }
            }
            Token::Function(name) => {
                let inner = nest(tokens, Some(')'), depth + 1);
                if depth < MAX_NESTING {
                    out.push(Cv::Func(name, inner));
                }
            }
            other => out.push(Cv::T(other)),
        }
    }
    out
}

#[derive(Clone, Debug)]
pub struct Declaration {
    /// Lowercased property name.
    pub name: String,
    pub value: Vec<Cv>,
    pub important: bool,
}

/// A rule of a stylesheet after `@media` filtering.
pub enum Rule {
    Style {
        selectors: Vec<Selector>,
        declarations: Vec<Declaration>,
    },
    /// An `@page` rule without a page selector.
    Page(Vec<Declaration>),
    /// `@import` of a stylesheet by URL.
    Import(String),
}

/// Parses a stylesheet, keeping rules for print media.
pub fn parse_stylesheet(text: &str) -> Vec<Rule> {
    let values = parse_component_values(text);
    let mut rules = Vec::new();
    rule_list(&values, &mut rules, true, 0);
    rules
}

fn rule_list(values: &[Cv], out: &mut Vec<Rule>, top_level: bool, depth: usize) {
    let mut index = 0;
    while index < values.len() {
        match &values[index] {
            Cv::T(Token::Whitespace) => index += 1,
            Cv::T(Token::AtKeyword(name)) => {
                let name = name.to_ascii_lowercase();
                let start = index + 1;
                let mut end = start;
                let mut block = None;
                while end < values.len() {
                    match &values[end] {
                        Cv::T(Token::Semicolon) => break,
                        Cv::Block('{', inner) => {
                            block = Some(inner);
                            break;
                        }
                        _ => end += 1,
                    }
                }
                let prelude = &values[start..end.min(values.len())];
                index = end + 1;
                at_rule(&name, prelude, block, out, top_level, depth);
            }
            _ => {
                let start = index;
                let mut end = start;
                while end < values.len() && !matches!(values[end], Cv::Block('{', _)) {
                    end += 1;
                }
                let Some(Cv::Block('{', block)) = values.get(end) else {
                    return;
                };
                index = end + 1;
                if let Some(selectors) = parse_selector_list(&values[start..end]) {
                    out.push(Rule::Style {
                        selectors,
                        declarations: parse_declarations(block),
                    });
                }
            }
        }
    }
}

fn at_rule(
    name: &str,
    prelude: &[Cv],
    block: Option<&Vec<Cv>>,
    out: &mut Vec<Rule>,
    top_level: bool,
    depth: usize,
) {
    match (name, block) {
        ("media", Some(block)) if depth < MAX_NESTING => {
            if media_matches(prelude) {
                rule_list(block, out, false, depth + 1);
            }
        }
        ("page", Some(block)) => {
            if prelude.iter().all(Cv::is_whitespace) {
                out.push(Rule::Page(parse_declarations(block)));
            }
        }
        ("import", None) if top_level => {
            let mut parts = prelude.iter().filter(|value| !value.is_whitespace());
            let url = match parts.next() {
                Some(Cv::T(Token::Str(url) | Token::Url(url))) => url.clone(),
                Some(Cv::Func(name, args)) if name.eq_ignore_ascii_case("url") => {
                    match args.iter().find(|a| !a.is_whitespace()) {
                        Some(Cv::T(Token::Str(url))) => url.clone(),
                        _ => return,
                    }
                }
                _ => return,
            };
            let rest: Vec<Cv> = parts.cloned().collect();
            if rest.is_empty() || media_matches(&rest) {
                out.push(Rule::Import(url));
            }
        }
        _ => {}
    }
}

/// A media query list evaluated for print: a query matches when its type is `print` or
/// `all` (or absent); media features are not evaluated.
fn media_matches(prelude: &[Cv]) -> bool {
    let mut queries: Vec<Vec<&Cv>> = vec![Vec::new()];
    for value in prelude {
        match value {
            Cv::T(Token::Comma) => queries.push(Vec::new()),
            Cv::T(Token::Whitespace) => {}
            other => {
                if let Some(query) = queries.last_mut() {
                    query.push(other);
                }
            }
        }
    }
    if prelude.iter().all(Cv::is_whitespace) {
        return true;
    }
    queries.iter().any(|query| {
        let mut negate = false;
        let mut media_type = None;
        for value in query {
            match value.ident().map(str::to_ascii_lowercase).as_deref() {
                Some("not") => negate = true,
                Some("only" | "and") => {}
                Some(other) if media_type.is_none() => media_type = Some(other.to_owned()),
                _ => {}
            }
        }
        let matched = matches!(media_type.as_deref(), None | Some("print" | "all"));
        matched != negate
    })
}

/// Parses the declarations of a block (or a `style` attribute).
pub fn parse_declarations(values: &[Cv]) -> Vec<Declaration> {
    let mut out = Vec::new();
    for part in values.split(|value| matches!(value, Cv::T(Token::Semicolon))) {
        let mut iter = part.iter().skip_while(|value| value.is_whitespace());
        let Some(name) = iter.next().and_then(Cv::ident) else {
            continue;
        };
        let mut rest: Vec<&Cv> = iter.skip_while(|value| value.is_whitespace()).collect();
        if !matches!(rest.first(), Some(Cv::T(Token::Colon))) {
            continue;
        }
        rest.remove(0);
        let mut value: Vec<Cv> = rest.into_iter().cloned().collect();
        while value.last().is_some_and(Cv::is_whitespace) {
            value.pop();
        }
        let mut important = false;
        let significant: Vec<usize> = (0..value.len())
            .filter(|&i| !value[i].is_whitespace())
            .collect();
        if let [.., bang, word] = significant[..]
            && matches!(value[bang], Cv::T(Token::Delim('!')))
            && value[word]
                .ident()
                .is_some_and(|w| w.eq_ignore_ascii_case("important"))
        {
            important = true;
            value.truncate(bang);
            while value.last().is_some_and(Cv::is_whitespace) {
                value.pop();
            }
        }
        while value.first().is_some_and(Cv::is_whitespace) {
            value.remove(0);
        }
        if value.is_empty() || value.iter().any(|v| matches!(v, Cv::T(Token::Bad))) {
            continue;
        }
        out.push(Declaration {
            name: name.to_ascii_lowercase(),
            value,
            important,
        });
    }
    out
}

/// Parses the text of a `style` attribute.
pub fn parse_style_attribute(text: &str) -> Vec<Declaration> {
    parse_declarations(&parse_component_values(text))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Combinator {
    Descendant,
    Child,
    Next,
    Subsequent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttrOp {
    Exists,
    Equals,
    Includes,
    DashMatch,
    Prefix,
    Suffix,
    Substring,
}

#[derive(Clone, Debug)]
pub enum Simple {
    Id(String),
    Class(String),
    Attr {
        name: String,
        op: AttrOp,
        value: String,
        ignore_case: bool,
    },
    /// `:nth-child(an+b)` and relatives; `first-child` is `a = 0, b = 1`.
    Nth {
        a: i32,
        b: i32,
        from_end: bool,
        of_type: bool,
    },
    Only {
        of_type: bool,
    },
    Root,
    Empty,
    Link,
    /// `:is()` and `:where()`: any of the compounds.
    Any(Vec<Compound>),
    Not(Vec<Compound>),
    /// Pseudo-classes that never match in print (`:hover`, `:visited`, ...).
    Never,
}

#[derive(Clone, Debug, Default)]
pub struct Compound {
    /// Lowercased element name; `None` for `*` or no type selector.
    pub tag: Option<String>,
    pub simple: Vec<Simple>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PseudoElement {
    Before,
    After,
    /// Pseudo-elements this renderer does not generate (`::marker`, `::first-line`, ...).
    Unsupported,
}

#[derive(Clone, Debug)]
pub struct Selector {
    /// Compounds from the subject leftwards.
    pub compounds: Vec<Compound>,
    /// `combinators[i]` joins `compounds[i]` to `compounds[i + 1]` on its left.
    pub combinators: Vec<Combinator>,
    pub specificity: u32,
    pub pseudo: Option<PseudoElement>,
}

fn specificity(compound: &Compound) -> (u32, u32, u32) {
    let mut total = (0, 0, u32::from(compound.tag.is_some()));
    for simple in &compound.simple {
        let (a, b, c) = match simple {
            Simple::Id(_) => (1, 0, 0),
            Simple::Any(list) | Simple::Not(list) => {
                list.iter().map(specificity).max().unwrap_or((0, 0, 0))
            }
            _ => (0, 1, 0),
        };
        total = (total.0 + a, total.1 + b, total.2 + c);
    }
    total
}

/// Parses a comma-separated selector list; `None` when any selector is invalid.
pub fn parse_selector_list(prelude: &[Cv]) -> Option<Vec<Selector>> {
    prelude
        .split(|value| matches!(value, Cv::T(Token::Comma)))
        .map(parse_selector)
        .collect()
}

fn parse_selector(values: &[Cv]) -> Option<Selector> {
    let values: Vec<&Cv> = {
        let start = values.iter().position(|v| !v.is_whitespace())?;
        let end = values.iter().rposition(|v| !v.is_whitespace())?;
        values[start..=end].iter().collect()
    };
    let mut compounds = Vec::new();
    let mut combinators = Vec::new();
    let mut pseudo = None;
    let mut index = 0;
    let mut pending: Option<Combinator> = None;
    while index < values.len() {
        if pseudo.is_some() {
            return None;
        }
        if compounds.len() >= MAX_NESTING {
            return None;
        }
        let (compound, element, next) = parse_compound(&values, index)?;
        if next == index {
            return None;
        }
        if !compounds.is_empty() {
            combinators.push(pending.take().unwrap_or(Combinator::Descendant));
        }
        compounds.push(compound);
        pseudo = element;
        index = next;
        let mut saw_space = false;
        while let Some(value) = values.get(index) {
            match value {
                Cv::T(Token::Whitespace) => saw_space = true,
                Cv::T(Token::Delim('>')) => pending = Some(Combinator::Child),
                Cv::T(Token::Delim('+')) => pending = Some(Combinator::Next),
                Cv::T(Token::Delim('~')) => pending = Some(Combinator::Subsequent),
                _ => break,
            }
            index += 1;
        }
        if pending.is_none() && saw_space {
            pending = Some(Combinator::Descendant);
        }
        if index >= values.len() && pending.is_some_and(|c| c != Combinator::Descendant) {
            return None;
        }
    }
    compounds.reverse();
    combinators.reverse();
    let (a, b, c) = compounds
        .iter()
        .map(specificity)
        .fold((0, 0, 0), |acc, s| (acc.0 + s.0, acc.1 + s.1, acc.2 + s.2));
    let c = c + u32::from(pseudo.is_some());
    Some(Selector {
        compounds,
        combinators,
        specificity: (a.min(1023) << 20) | (b.min(1023) << 10) | c.min(1023),
        pseudo,
    })
}

/// Parses one compound starting at `index`: the compound, a trailing pseudo-element, and
/// the index after it.
fn parse_compound(
    values: &[&Cv],
    mut index: usize,
) -> Option<(Compound, Option<PseudoElement>, usize)> {
    let mut compound = Compound::default();
    match values.get(index) {
        Some(Cv::T(Token::Ident(name))) => {
            compound.tag = Some(name.to_ascii_lowercase());
            index += 1;
        }
        Some(Cv::T(Token::Delim('*'))) => index += 1,
        _ => {}
    }
    if matches!(values.get(index), Some(Cv::T(Token::Delim('|')))) {
        return None;
    }
    let mut pseudo = None;
    while let Some(value) = values.get(index) {
        match value {
            Cv::T(Token::Hash(id)) => compound.simple.push(Simple::Id(id.clone())),
            Cv::T(Token::Delim('.')) => {
                let Some(Cv::T(Token::Ident(class))) = values.get(index + 1) else {
                    return None;
                };
                compound.simple.push(Simple::Class(class.clone()));
                index += 1;
            }
            Cv::Block('[', inner) => compound.simple.push(parse_attribute(inner)?),
            Cv::T(Token::Colon) => {
                if matches!(values.get(index + 1), Some(Cv::T(Token::Colon))) {
                    let name = values.get(index + 2)?.ident()?.to_ascii_lowercase();
                    pseudo = Some(match name.as_str() {
                        "before" => PseudoElement::Before,
                        "after" => PseudoElement::After,
                        "marker" | "first-line" | "first-letter" | "selection" | "placeholder"
                        | "footnote-call" | "footnote-marker" => PseudoElement::Unsupported,
                        _ => return None,
                    });
                    index += 3;
                    break;
                }
                match values.get(index + 1)? {
                    Cv::T(Token::Ident(name)) => {
                        let name = name.to_ascii_lowercase();
                        if matches!(
                            name.as_str(),
                            "before" | "after" | "first-line" | "first-letter"
                        ) {
                            pseudo = Some(match name.as_str() {
                                "before" => PseudoElement::Before,
                                "after" => PseudoElement::After,
                                _ => PseudoElement::Unsupported,
                            });
                            index += 2;
                            break;
                        }
                        compound.simple.push(pseudo_class(&name)?);
                    }
                    Cv::Func(name, args) => compound
                        .simple
                        .push(pseudo_function(&name.to_ascii_lowercase(), args)?),
                    _ => return None,
                }
                index += 1;
            }
            _ => break,
        }
        index += 1;
    }
    Some((compound, pseudo, index))
}

fn pseudo_class(name: &str) -> Option<Simple> {
    Some(match name {
        "first-child" => Simple::Nth {
            a: 0,
            b: 1,
            from_end: false,
            of_type: false,
        },
        "last-child" => Simple::Nth {
            a: 0,
            b: 1,
            from_end: true,
            of_type: false,
        },
        "first-of-type" => Simple::Nth {
            a: 0,
            b: 1,
            from_end: false,
            of_type: true,
        },
        "last-of-type" => Simple::Nth {
            a: 0,
            b: 1,
            from_end: true,
            of_type: true,
        },
        "only-child" => Simple::Only { of_type: false },
        "only-of-type" => Simple::Only { of_type: true },
        "root" => Simple::Root,
        "empty" => Simple::Empty,
        "link" | "any-link" => Simple::Link,
        "visited" | "hover" | "active" | "focus" | "focus-within" | "focus-visible" | "target"
        | "checked" | "disabled" | "enabled" | "indeterminate" | "default" | "required"
        | "optional" | "invalid" | "valid" | "read-only" | "read-write" | "placeholder-shown"
        | "first" | "left" | "right" | "blank" => Simple::Never,
        _ => return None,
    })
}

fn pseudo_function(name: &str, args: &[Cv]) -> Option<Simple> {
    match name {
        "is" | "where" | "matches" | "not" => {
            let mut compounds = Vec::new();
            for part in args.split(|value| matches!(value, Cv::T(Token::Comma))) {
                let part: Vec<&Cv> = part.iter().filter(|value| !value.is_whitespace()).collect();
                let (compound, pseudo, end) = parse_compound(&part, 0)?;
                if pseudo.is_some() || end != part.len() || end == 0 {
                    return None;
                }
                compounds.push(compound);
            }
            Some(if name == "not" {
                Simple::Not(compounds)
            } else {
                Simple::Any(compounds)
            })
        }
        "nth-child" | "nth-last-child" | "nth-of-type" | "nth-last-of-type" => {
            let (a, b) = parse_nth(args)?;
            Some(Simple::Nth {
                a,
                b,
                from_end: name.contains("last"),
                of_type: name.ends_with("of-type"),
            })
        }
        "lang" | "dir" | "has" | "host" | "contains" => Some(Simple::Never),
        _ => None,
    }
}

/// `an+b`, `odd`, `even`, or an integer.
fn parse_nth(args: &[Cv]) -> Option<(i32, i32)> {
    let mut text = String::new();
    for value in args {
        match value {
            Cv::T(Token::Whitespace) => {}
            Cv::T(Token::Ident(name)) => text.push_str(name),
            Cv::T(Token::Number(n)) => text.push_str(&format!(
                "{}{}",
                if *n >= 0.0 && text.ends_with('n') {
                    "+"
                } else {
                    ""
                },
                *n as i32
            )),
            Cv::T(Token::Dimension(n, unit)) => text.push_str(&format!("{}{unit}", *n as i32)),
            Cv::T(Token::Delim(c)) => text.push(*c),
            _ => return None,
        }
    }
    let text = text.to_ascii_lowercase();
    match text.as_str() {
        "odd" => return Some((2, 1)),
        "even" => return Some((2, 0)),
        _ => {}
    }
    let Some(n_at) = text.find('n') else {
        return text.parse::<i32>().ok().map(|b| (0, b));
    };
    let a = match &text[..n_at] {
        "" | "+" => 1,
        "-" => -1,
        other => other.parse::<i32>().ok()?,
    };
    let rest = &text[n_at + 1..];
    let b = if rest.is_empty() {
        0
    } else {
        rest.trim_start_matches('+').parse::<i32>().ok()?
    };
    Some((a, b))
}

fn parse_attribute(inner: &[Cv]) -> Option<Simple> {
    let parts: Vec<&Cv> = inner
        .iter()
        .filter(|value| !value.is_whitespace())
        .collect();
    let name = parts.first()?.ident()?.to_ascii_lowercase();
    if parts.len() == 1 {
        return Some(Simple::Attr {
            name,
            op: AttrOp::Exists,
            value: String::new(),
            ignore_case: false,
        });
    }
    let (op, rest) = match (parts.get(1)?, parts.get(2)) {
        (Cv::T(Token::Delim('=')), _) => (AttrOp::Equals, &parts[2..]),
        (Cv::T(Token::Delim(c)), Some(Cv::T(Token::Delim('=')))) => {
            let op = match c {
                '~' => AttrOp::Includes,
                '|' => AttrOp::DashMatch,
                '^' => AttrOp::Prefix,
                '$' => AttrOp::Suffix,
                '*' => AttrOp::Substring,
                _ => return None,
            };
            (op, &parts[3..])
        }
        _ => return None,
    };
    let value = match rest.first()? {
        Cv::T(Token::Ident(value) | Token::Str(value)) => value.clone(),
        _ => return None,
    };
    let ignore_case = match rest.get(1) {
        None => false,
        Some(flag) if flag.ident().is_some_and(|f| f.eq_ignore_ascii_case("i")) => true,
        Some(flag) if flag.ident().is_some_and(|f| f.eq_ignore_ascii_case("s")) => false,
        Some(_) => return None,
    };
    Some(Simple::Attr {
        name,
        op,
        value,
        ignore_case,
    })
}

/// Does `selector` match element `id`?
pub fn matches(dom: &Dom, selector: &Selector, id: NodeId) -> bool {
    match_from(dom, selector, 0, id)
}

fn match_from(dom: &Dom, selector: &Selector, index: usize, id: NodeId) -> bool {
    let Some(compound) = selector.compounds.get(index) else {
        return true;
    };
    if !compound_matches(dom, compound, id) {
        return false;
    }
    let Some(combinator) = selector.combinators.get(index) else {
        return true;
    };
    match combinator {
        Combinator::Child => parent_element(dom, id)
            .is_some_and(|parent| match_from(dom, selector, index + 1, parent)),
        Combinator::Descendant => {
            let mut current = parent_element(dom, id);
            while let Some(ancestor) = current {
                if match_from(dom, selector, index + 1, ancestor) {
                    return true;
                }
                current = parent_element(dom, ancestor);
            }
            false
        }
        Combinator::Next => previous_element(dom, id)
            .is_some_and(|previous| match_from(dom, selector, index + 1, previous)),
        Combinator::Subsequent => {
            let mut current = previous_element(dom, id);
            while let Some(previous) = current {
                if match_from(dom, selector, index + 1, previous) {
                    return true;
                }
                current = previous_element(dom, previous);
            }
            false
        }
    }
}

fn parent_element(dom: &Dom, id: NodeId) -> Option<NodeId> {
    dom.parent(id).filter(|&parent| dom.is_element(parent))
}

fn previous_element(dom: &Dom, id: NodeId) -> Option<NodeId> {
    let parent = dom.parent(id)?;
    let siblings = dom.children(parent);
    let position = siblings.iter().position(|&sibling| sibling == id)?;
    siblings[..position]
        .iter()
        .rev()
        .copied()
        .find(|&sibling| dom.is_element(sibling))
}

fn compound_matches(dom: &Dom, compound: &Compound, id: NodeId) -> bool {
    if !dom.is_element(id) {
        return false;
    }
    if let Some(tag) = &compound.tag
        && !dom.tag(id).eq_ignore_ascii_case(tag)
    {
        return false;
    }
    compound
        .simple
        .iter()
        .all(|simple| simple_matches(dom, simple, id))
}

fn simple_matches(dom: &Dom, simple: &Simple, id: NodeId) -> bool {
    match simple {
        Simple::Id(want) => dom.attr(id, "id") == Some(want.as_str()),
        Simple::Class(want) => dom
            .attr(id, "class")
            .is_some_and(|classes| classes.split_ascii_whitespace().any(|c| c == want)),
        Simple::Attr {
            name,
            op,
            value,
            ignore_case,
        } => {
            let Some(actual) = dom
                .attrs(id)
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
            else {
                return false;
            };
            let (actual, value) = if *ignore_case {
                (actual.to_lowercase(), value.to_lowercase())
            } else {
                (actual.to_owned(), value.clone())
            };
            match op {
                AttrOp::Exists => true,
                AttrOp::Equals => actual == value,
                AttrOp::Includes => {
                    !value.is_empty() && actual.split_ascii_whitespace().any(|word| word == value)
                }
                AttrOp::DashMatch => actual == value || actual.starts_with(&format!("{value}-")),
                AttrOp::Prefix => !value.is_empty() && actual.starts_with(&value),
                AttrOp::Suffix => !value.is_empty() && actual.ends_with(&value),
                AttrOp::Substring => !value.is_empty() && actual.contains(&value),
            }
        }
        Simple::Nth {
            a,
            b,
            from_end,
            of_type,
        } => {
            let Some(parent) = dom.parent(id) else {
                return false;
            };
            let tag = dom.tag(id);
            let siblings: Vec<NodeId> = dom
                .element_children(parent)
                .filter(|&sibling| !*of_type || dom.tag(sibling) == tag)
                .collect();
            let Some(position) = siblings.iter().position(|&sibling| sibling == id) else {
                return false;
            };
            let n = if *from_end {
                siblings.len() - position
            } else {
                position + 1
            };
            let n = i64::try_from(n).unwrap_or(i64::MAX);
            let (a, b) = (i64::from(*a), i64::from(*b));
            if a == 0 {
                n == b
            } else {
                let diff = n - b;
                diff % a == 0 && diff / a >= 0
            }
        }
        Simple::Only { of_type } => {
            let Some(parent) = dom.parent(id) else {
                return false;
            };
            let tag = dom.tag(id);
            dom.element_children(parent)
                .filter(|&sibling| !*of_type || dom.tag(sibling) == tag)
                .count()
                == 1
        }
        Simple::Root => dom.parent(id).is_some_and(|parent| !dom.is_element(parent)),
        Simple::Empty => dom
            .children(id)
            .iter()
            .all(|&child| !dom.is_element(child) && dom.text_content(child).is_empty()),
        Simple::Link => {
            matches!(dom.tag(id), "a" | "area" | "link") && dom.attr(id, "href").is_some()
        }
        Simple::Any(list) => list
            .iter()
            .any(|compound| compound_matches(dom, compound, id)),
        Simple::Not(list) => !list
            .iter()
            .any(|compound| compound_matches(dom, compound, id)),
        Simple::Never => false,
    }
}
