//! Optional content, as MuPDF's `pdf-layer.c` decides it: the default
//! configuration's group states and whether an /OC entry hides content.

use std::collections::HashMap;

use pdf_core::{Dict, Document, ObjRef, Object};

/// Nested membership dictionaries deeper than this count as visible.
const MAX_DEPTH: usize = 32;

/// The /Intent of the default configuration.
#[derive(Debug)]
enum Intents {
    Absent,
    Name(Vec<u8>),
    Array(Vec<Vec<u8>>),
    Other,
}

/// The document's optional content groups and their default states.
#[derive(Debug)]
pub(crate) struct OptionalContent {
    /// Entries of /OCProperties /OCGs (0 means everything is visible).
    count: usize,
    /// On (true) or off, by the group's reference.
    states: HashMap<ObjRef, bool>,
    intents: Intents,
}

impl OptionalContent {
    /// Reads /Root /OCProperties with its /D configuration applied. A
    /// broken or missing configuration shows everything.
    pub(crate) fn load(doc: &Document) -> OptionalContent {
        let empty = OptionalContent {
            count: 0,
            states: HashMap::new(),
            intents: Intents::Absent,
        };
        let Some(props) = doc
            .catalog()
            .ok()
            .and_then(|c| c.get(b"OCProperties").cloned())
        else {
            return empty;
        };
        let Some(props) = doc.resolve_dict(&props).ok().flatten() else {
            return empty;
        };
        let groups = props
            .get(b"OCGs")
            .and_then(|o| doc.resolve_array(o).ok().flatten())
            .unwrap_or_default();
        let Some(config) = props
            .get(b"D")
            .and_then(|o| doc.resolve_dict(o).ok().flatten())
        else {
            return empty;
        };
        let base = config
            .get(b"BaseState")
            .and_then(|o| doc.resolve_name(o).ok().flatten());
        let base_state = match base.as_ref().map(|n| n.as_bytes()) {
            Some(b"OFF") => Some(false),
            Some(b"Unchanged") => None,
            _ => Some(true),
        };
        let mut states = HashMap::new();
        for group in &groups {
            if let Object::Reference(id) = group {
                states.insert(*id, base_state.unwrap_or(true));
            }
        }
        for (key, on) in [(b"ON".as_slice(), true), (b"OFF".as_slice(), false)] {
            let list = config
                .get(key)
                .and_then(|o| doc.resolve_array(o).ok().flatten())
                .unwrap_or_default();
            for item in &list {
                if let Object::Reference(id) = item
                    && let Some(state) = states.get_mut(id)
                {
                    *state = on;
                }
            }
        }
        let intents = match config.get(b"Intent").map(|o| doc.resolve(o)) {
            None => Intents::Absent,
            Some(Ok(Object::Name(name))) => Intents::Name(name.as_bytes().to_vec()),
            Some(Ok(Object::Array(items))) => Intents::Array(
                items
                    .iter()
                    .map(|o| {
                        doc.resolve_name(o)
                            .ok()
                            .flatten()
                            .map(|n| n.as_bytes().to_vec())
                            .unwrap_or_default()
                    })
                    .collect(),
            ),
            Some(_) => Intents::Other,
        };
        OptionalContent {
            count: groups.len(),
            states,
            intents,
        }
    }

    /// Whether the optional content `ocg` (a group or membership
    /// dictionary, or a reference to one) hides content for `usage`
    /// (`View` or `Print`).
    pub(crate) fn is_hidden(&self, doc: &Document, usage: &str, ocg: &Object) -> bool {
        if self.count == 0 {
            return false;
        }
        let mut chain = Vec::new();
        self.hidden(doc, usage, ocg, &mut chain)
    }

    fn hidden(&self, doc: &Document, usage: &str, ocg: &Object, chain: &mut Vec<ObjRef>) -> bool {
        if chain.len() > MAX_DEPTH {
            return false;
        }
        let id = match ocg {
            Object::Reference(id) => {
                if chain.contains(id) {
                    return false;
                }
                Some(*id)
            }
            _ => None,
        };
        let Some(dict) = doc.resolve_dict(ocg).ok().flatten() else {
            return false;
        };
        let kind = dict
            .get(b"Type")
            .and_then(|o| doc.resolve_name(o).ok().flatten());
        match kind.as_ref().map(|n| n.as_bytes()) {
            Some(b"OCG") => self.group_hidden(doc, usage, id, &dict),
            Some(b"OCMD") => {
                if let Some(id) = id {
                    chain.push(id);
                }
                let hidden = self.membership_hidden(doc, usage, &dict, chain);
                if id.is_some() {
                    chain.pop();
                }
                hidden
            }
            _ => false,
        }
    }

    fn group_hidden(&self, doc: &Document, usage: &str, id: Option<ObjRef>, dict: &Dict) -> bool {
        let default = id.and_then(|id| self.states.get(&id)).is_some_and(|on| !on);
        match dict.get(b"Intent").map(|o| doc.resolve(o)) {
            Some(Ok(Object::Name(name))) => {
                if !self.intent_included(name.as_bytes()) {
                    return true;
                }
            }
            Some(Ok(Object::Array(items))) => {
                let any = items.iter().any(|item| {
                    let name = doc.resolve_name(item).ok().flatten();
                    self.intent_included(name.as_ref().map_or(b"".as_slice(), |n| n.as_bytes()))
                });
                if !any {
                    return true;
                }
            }
            _ => {
                if !self.intent_included(b"View") {
                    return true;
                }
            }
        }
        let Some(usage_dict) = dict
            .get(b"Usage")
            .and_then(|o| doc.resolve_dict(o).ok().flatten())
        else {
            return default;
        };
        let state_key = format!("{usage}State");
        let state = usage_dict
            .get(usage.as_bytes())
            .and_then(|o| doc.resolve_dict(o).ok().flatten())
            .and_then(|d| {
                d.get(state_key.as_bytes())
                    .and_then(|o| doc.resolve_name(o).ok().flatten())
            });
        if state.is_some_and(|s| s.as_bytes() == b"OFF") {
            return true;
        }
        default
    }

    fn membership_hidden(
        &self,
        doc: &Document,
        usage: &str,
        dict: &Dict,
        chain: &mut Vec<ObjRef>,
    ) -> bool {
        if matches!(
            dict.get(b"VE").map(|o| doc.resolve(o)),
            Some(Ok(Object::Array(_)))
        ) {
            return false;
        }
        let policy = dict
            .get(b"P")
            .and_then(|o| doc.resolve_name(o).ok().flatten());
        let combine: u8 = match policy.as_ref().map(|n| n.as_bytes()) {
            Some(b"AllOn") => 1,
            Some(b"AnyOff") => 2,
            Some(b"AllOff") => 3,
            _ => 0,
        };
        let groups = dict.get(b"OCGs").cloned().unwrap_or(Object::Null);
        let mut on = combine & 1 == 1;
        match doc.resolve(&groups) {
            Ok(Object::Array(items)) => {
                for item in &items {
                    let mut hidden = self.hidden(doc, usage, item, chain);
                    if combine & 1 == 0 {
                        hidden = !hidden;
                    }
                    if combine & 2 != 0 {
                        on &= hidden;
                    } else {
                        on |= hidden;
                    }
                }
            }
            _ => {
                on = self.hidden(doc, usage, &groups, chain);
                if combine & 1 == 0 {
                    on = !on;
                }
            }
        }
        !on
    }

    /// MuPDF's `ocg_intents_include`.
    fn intent_included(&self, name: &[u8]) -> bool {
        if name == b"All" {
            return true;
        }
        match &self.intents {
            Intents::Absent => name == b"View",
            Intents::Name(intent) => intent == b"All" || intent == name,
            Intents::Array(items) => items.iter().any(|i| i == b"All" || i == name),
            Intents::Other => false,
        }
    }
}
