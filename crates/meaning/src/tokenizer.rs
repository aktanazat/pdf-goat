//! The pinned model's tokenizer, read from its `tokenizer.json`.
//!
//! That file declares a `BertNormalizer`, a `BertPreTokenizer`, a `WordPiece`
//! model and a handful of special added tokens. Each is implemented here the
//! way the Hugging Face `tokenizers` library runs it, so the same text yields
//! the same ids. Any other declaration is rejected when the file is read
//! rather than tokenized differently.

use std::collections::HashMap;

use serde_json::Value;
use unicode_categories::UnicodeCategories;
use unicode_normalization_alignments::UnicodeNormalization;

use crate::MeaningError;

/// A special token matched in the raw input before normalization.
struct AddedToken {
    content: String,
    id: u32,
}

struct BertNormalizer {
    clean_text: bool,
    handle_chinese_chars: bool,
    strip_accents: bool,
    lowercase: bool,
}

pub(crate) struct Tokenizer {
    vocab: HashMap<String, u32>,
    unk_id: u32,
    continuing_prefix: String,
    max_input_chars_per_word: usize,
    normalizer: BertNormalizer,
    added: Vec<AddedToken>,
}

fn invalid(detail: &str) -> MeaningError {
    MeaningError::new(format!(
        "{} tokenizer.json is not supported: {detail}",
        crate::MODEL_ID
    ))
}

fn field<'a>(object: &'a Value, key: &str, context: &str) -> Result<&'a Value, MeaningError> {
    object
        .get(key)
        .ok_or_else(|| invalid(&format!("{context} has no '{key}'")))
}

fn flag(object: &Value, key: &str, context: &str) -> Result<bool, MeaningError> {
    field(object, key, context)?
        .as_bool()
        .ok_or_else(|| invalid(&format!("{context} '{key}' is not a boolean")))
}

fn declared_type<'a>(object: &'a Value, context: &str) -> Result<&'a str, MeaningError> {
    field(object, "type", context)?
        .as_str()
        .ok_or_else(|| invalid(&format!("{context} type is not a string")))
}

impl Tokenizer {
    pub(crate) fn from_json(text: &str) -> Result<Self, MeaningError> {
        let root: Value =
            serde_json::from_str(text).map_err(|error| invalid(&error.to_string()))?;

        let normalizer = field(&root, "normalizer", "tokenizer")?;
        if declared_type(normalizer, "normalizer")? != "BertNormalizer" {
            return Err(invalid("normalizer is not BertNormalizer"));
        }
        let lowercase = flag(normalizer, "lowercase", "normalizer")?;
        // `strip_accents: null` means "follow lowercase".
        let strip_accents = match field(normalizer, "strip_accents", "normalizer")? {
            Value::Null => lowercase,
            Value::Bool(value) => *value,
            _ => return Err(invalid("normalizer 'strip_accents' is not a boolean")),
        };
        let normalizer = BertNormalizer {
            clean_text: flag(normalizer, "clean_text", "normalizer")?,
            handle_chinese_chars: flag(normalizer, "handle_chinese_chars", "normalizer")?,
            strip_accents,
            lowercase,
        };

        let pre_tokenizer = field(&root, "pre_tokenizer", "tokenizer")?;
        if declared_type(pre_tokenizer, "pre_tokenizer")? != "BertPreTokenizer" {
            return Err(invalid("pre_tokenizer is not BertPreTokenizer"));
        }

        let model = field(&root, "model", "tokenizer")?;
        if declared_type(model, "model")? != "WordPiece" {
            return Err(invalid("model is not WordPiece"));
        }
        let unk_token = field(model, "unk_token", "model")?
            .as_str()
            .ok_or_else(|| invalid("model 'unk_token' is not a string"))?;
        let continuing_prefix = field(model, "continuing_subword_prefix", "model")?
            .as_str()
            .ok_or_else(|| invalid("model 'continuing_subword_prefix' is not a string"))?
            .to_owned();
        let max_input_chars_per_word = field(model, "max_input_chars_per_word", "model")?
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| invalid("model 'max_input_chars_per_word' is not a count"))?;
        let entries = field(model, "vocab", "model")?
            .as_object()
            .ok_or_else(|| invalid("model 'vocab' is not an object"))?;
        let mut vocab = HashMap::with_capacity(entries.len());
        for (token, id) in entries {
            let id = id
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(|| invalid(&format!("vocab id for {token:?} is not a u32")))?;
            vocab.insert(token.clone(), id);
        }
        let unk_id = *vocab
            .get(unk_token)
            .ok_or_else(|| invalid("unk_token is not in the vocabulary"))?;

        let mut added = Vec::new();
        if let Some(tokens) = root.get("added_tokens") {
            let tokens = tokens
                .as_array()
                .ok_or_else(|| invalid("'added_tokens' is not a list"))?;
            for token in tokens {
                let context = "added token";
                // Only the shape the pinned file uses: special tokens matched
                // literally on the raw input.
                if !flag(token, "special", context)?
                    || flag(token, "normalized", context)?
                    || flag(token, "single_word", context)?
                    || flag(token, "lstrip", context)?
                    || flag(token, "rstrip", context)?
                {
                    return Err(invalid("an added token is not a plain special token"));
                }
                let content = field(token, "content", context)?
                    .as_str()
                    .filter(|content| !content.is_empty())
                    .ok_or_else(|| invalid("added token content is not a string"))?
                    .to_owned();
                let id = field(token, "id", context)?
                    .as_u64()
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or_else(|| invalid("added token id is not a u32"))?;
                added.push(AddedToken { content, id });
            }
        }

        Ok(Self {
            vocab,
            unk_id,
            continuing_prefix,
            max_input_chars_per_word,
            normalizer,
            added,
        })
    }

    pub(crate) fn token_to_id(&self, token: &str) -> Option<u32> {
        if let Some(added) = self.added.iter().find(|added| added.content == token) {
            return Some(added.id);
        }
        self.vocab.get(token).copied()
    }

    /// The largest id the tokenizer can produce.
    pub(crate) fn max_id(&self) -> Option<u32> {
        let added = self.added.iter().map(|token| token.id);
        self.vocab.values().copied().chain(added).max()
    }

    /// Token ids for `text` without special tokens added around it, which is
    /// `encode(text, add_special_tokens=False)` in the Python library.
    pub(crate) fn encode(&self, text: &str) -> Vec<u32> {
        let mut ids = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            match self.next_added(rest) {
                Some((start, token)) => {
                    self.encode_segment(&rest[..start], &mut ids);
                    ids.push(token.id);
                    rest = &rest[start + token.content.len()..];
                }
                None => {
                    self.encode_segment(rest, &mut ids);
                    break;
                }
            }
        }
        ids
    }

    /// The leftmost special token in `text`, the longest one at that place.
    fn next_added(&self, text: &str) -> Option<(usize, &AddedToken)> {
        for (start, _) in text.char_indices() {
            let tail = &text[start..];
            let longest = self
                .added
                .iter()
                .filter(|token| tail.starts_with(token.content.as_str()))
                .max_by_key(|token| token.content.len());
            if let Some(token) = longest {
                return Some((start, token));
            }
        }
        None
    }

    fn encode_segment(&self, segment: &str, ids: &mut Vec<u32>) {
        if segment.is_empty() {
            return;
        }
        let normalized = self.normalize(segment);
        let mut word = String::new();
        for c in normalized.chars() {
            if c.is_whitespace() {
                self.word_piece(&word, ids);
                word.clear();
            } else if is_bert_punctuation(c) {
                self.word_piece(&word, ids);
                word.clear();
                let mut buffer = [0; 4];
                self.word_piece(c.encode_utf8(&mut buffer), ids);
            } else {
                word.push(c);
            }
        }
        self.word_piece(&word, ids);
    }

    fn normalize(&self, text: &str) -> String {
        let options = &self.normalizer;
        let mut out = String::with_capacity(text.len());
        for c in text.chars() {
            if options.clean_text {
                if c == '\0' || c == '\u{fffd}' || is_control(c) {
                    continue;
                }
                if is_bert_whitespace(c) {
                    out.push(' ');
                    continue;
                }
            }
            if options.handle_chinese_chars && is_chinese_char(c) {
                out.push(' ');
                out.push(c);
                out.push(' ');
            } else {
                out.push(c);
            }
        }
        if options.strip_accents {
            out = out
                .as_str()
                .nfd()
                .map(|(c, _)| c)
                .filter(|c| !c.is_mark_nonspacing())
                .collect();
        }
        if options.lowercase {
            out = out.chars().flat_map(char::to_lowercase).collect();
        }
        out
    }

    fn word_piece(&self, word: &str, ids: &mut Vec<u32>) {
        if word.is_empty() {
            return;
        }
        if word.chars().count() > self.max_input_chars_per_word {
            ids.push(self.unk_id);
            return;
        }
        let first = ids.len();
        let mut key = String::with_capacity(self.continuing_prefix.len() + word.len());
        let mut start = 0;
        while start < word.len() {
            let mut end = word.len();
            let mut found = None;
            while start < end {
                key.clear();
                if start > 0 {
                    key.push_str(&self.continuing_prefix);
                }
                key.push_str(&word[start..end]);
                if let Some(&id) = self.vocab.get(&key) {
                    found = Some(id);
                    break;
                }
                end = word[start..end]
                    .char_indices()
                    .next_back()
                    .map_or(start, |(offset, _)| start + offset);
            }
            match found {
                Some(id) => {
                    ids.push(id);
                    start = end;
                }
                None => {
                    ids.truncate(first);
                    ids.push(self.unk_id);
                    return;
                }
            }
        }
    }
}

fn is_bert_whitespace(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r') || c.is_whitespace()
}

fn is_control(c: char) -> bool {
    !matches!(c, '\t' | '\n' | '\r') && c.is_other()
}

fn is_chinese_char(c: char) -> bool {
    matches!(
        u32::from(c),
        0x4E00..=0x9FFF
            | 0x3400..=0x4DBF
            | 0x20000..=0x2A6DF
            | 0x2A700..=0x2B73F
            | 0x2B740..=0x2B81F
            | 0x2B920..=0x2CEAF
            | 0xF900..=0xFAFF
            | 0x2F800..=0x2FA1F
    )
}

fn is_bert_punctuation(c: char) -> bool {
    c.is_ascii_punctuation() || c.is_punctuation()
}
