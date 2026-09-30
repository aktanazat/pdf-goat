use pdf_core::{Dict, ObjRef, Object};
use pdf_interp::MarkedContentEvent;

use crate::device::{Item, Last, StextDevice};
use crate::geom::{M, R};
use crate::{FontInfo, GlyphSource, Rect};

pub(crate) struct MetaText {
    actual: bool,
    text: String,
    bounds: R,
    source: Option<GlyphSource>,
}

impl StextDevice<'_> {
    pub fn actual_index(&self) -> Option<usize> {
        self.meta.iter().rposition(|m| m.actual)
    }

    pub fn actual_bounds(&mut self, bbox: R, source: &GlyphSource) {
        if let Some(index) = self.actual_index() {
            let meta = &mut self.meta[index];
            meta.bounds = meta.bounds.union(bbox);
            meta.source = Some(source.clone());
        }
    }

    pub fn push_parent(&mut self, reference: ObjRef) {
        let parent = self
            .doc
            .get(reference)
            .ok()
            .and_then(|object| {
                object
                    .as_dict()
                    .or_else(|| object.as_stream().map(|s| &s.dict))
                    .and_then(|dict| dict.get_i64(b"StructParent"))
            })
            .unwrap_or(-1);
        self.parents.push(parent);
    }

    fn mcid_dict(&mut self, props: &Dict) -> Option<Dict> {
        let mcid = self.doc.resolve_key(props, b"MCID").ok()?.as_i64()?;
        let parent = *self.parents.last()?;
        if parent < 0 {
            return None;
        }
        if self.parent_tree.is_none() {
            let catalog = self.doc.catalog().ok()?;
            let root = self.doc.resolve_key(&catalog, b"StructTreeRoot").ok()?;
            let tree = self.doc.resolve_key(root.as_dict()?, b"ParentTree").ok()?;
            self.parent_tree = Some(self.doc.number_tree(&tree).ok()?);
        }
        let value = &self
            .parent_tree
            .as_ref()?
            .iter()
            .find(|(key, _)| *key == parent)?
            .1;
        let array = self.doc.resolve(value).ok()?;
        let array = array.as_array()?;
        let is_mcid = |object: &Object| -> Option<Dict> {
            let dict = self.doc.resolve_dict(object).ok()??;
            let k = self.doc.resolve_key(&dict, b"K").ok()?;
            let matches = k.as_i64() == Some(mcid)
                || k.as_array()
                    .is_some_and(|values| values.iter().any(|k| k.as_i64() == Some(mcid)));
            matches.then_some(dict)
        };
        if let Ok(index) = usize::try_from(mcid)
            && let Some(dict) = array.get(index).and_then(is_mcid)
        {
            return Some(dict);
        }
        array.iter().find_map(is_mcid)
    }

    pub fn begin_marked(&mut self, event: &MarkedContentEvent<'_>) {
        let mut count = 0;
        if let Some(props) = event.properties {
            let mcid = self.mcid_dict(props);
            for (key, actual) in [
                (b"ActualText".as_slice(), true),
                (b"Alt", false),
                (b"E", false),
                (b"T", false),
            ] {
                let value = self
                    .doc
                    .resolve_key(props, key)
                    .ok()
                    .filter(|v| !v.is_null())
                    .or_else(|| {
                        mcid.as_ref()
                            .and_then(|d| self.doc.resolve_key(d, key).ok())
                            .filter(|v| !v.is_null())
                    });
                if let Some(value) = value {
                    let text = value.as_string().map_or_else(String::new, |s| s.to_text());
                    if actual {
                        if let Some(index) = self.actual_index() {
                            let prior = self.meta[index].text.clone();
                            self.flush_actual(&prior, 0.0);
                        }
                        if let Some(last) = &mut self.last {
                            last.valid = false;
                        }
                    }
                    self.meta.push(MetaText {
                        actual,
                        text,
                        bounds: R::EMPTY,
                        source: None,
                    });
                    count += 1;
                }
            }
        }
        self.meta_counts.push(count);
    }

    pub fn end_marked(&mut self) {
        let count = self.meta_counts.pop().unwrap_or(0);
        for _ in 0..count {
            self.end_meta();
        }
    }

    fn end_meta(&mut self) {
        let Some(meta) = self.meta.pop() else {
            return;
        };
        if meta.actual {
            if self.last.as_ref().is_some_and(|last| last.valid) {
                self.flush_actual(&meta.text, 0.0);
                if let Some(last) = &mut self.last {
                    last.valid = false;
                }
            } else if !meta.bounds.is_empty()
                && let Some(source) = meta.source.clone()
            {
                let font = self.last.as_ref().map(|last| last.font).unwrap_or_else(|| {
                    let index = self.fonts.len();
                    self.fonts.push(FontInfo {
                        name: "Helvetica".to_owned(),
                        full_name: "Helvetica".to_owned(),
                        ascender: f64::from(1.075_f32),
                        descender: f64::from(-0.299_f32),
                        bbox: Rect::new(-0.21, -0.299, 1.032, 1.075),
                        bold: false,
                        italic: false,
                        serif: false,
                        mono: false,
                    });
                    index
                });
                let bbox = meta.bounds;
                let trm = M::new(
                    bbox.x1 - bbox.x0,
                    0.0,
                    0.0,
                    bbox.y0 - bbox.y1,
                    bbox.x0,
                    bbox.y1,
                );
                let (wmode, bidi, flags) = self
                    .last
                    .as_ref()
                    .map_or((0, 0, 0), |last| (last.wmode, last.item.bidi, last.flags));
                self.last = Some(Last {
                    item: Item {
                        c: None,
                        glyph: -2,
                        adv: 1.0,
                        trm,
                        clip_bbox: bbox,
                        cid: 0,
                        bidi,
                        source,
                    },
                    font,
                    wmode,
                    flags,
                    clipped: false,
                    valid: true,
                });
                self.flush_actual(&meta.text, 1.0);
            }
        }
        if let Some(parent) = self.meta.last_mut() {
            parent.bounds = parent.bounds.union(meta.bounds);
            if meta.source.is_some() {
                parent.source = meta.source;
            }
        }
    }

    pub fn extract_actual(
        &mut self,
        index: usize,
        items: &[Item],
        font: usize,
        wmode: u8,
        flags: &mut u32,
    ) {
        let text = std::mem::take(&mut self.meta[index].text);
        let chars: Vec<char> = text.chars().collect();
        if chars.is_empty() {
            return;
        }
        let mut start = 0;
        while start < items.len() && start < chars.len() && items[start].c == Some(chars[start]) {
            start += 1;
        }
        self.extract_items(&items[..start], font, wmode, flags);
        if start == items.len() {
            self.meta[index].text = chars[start..].iter().collect();
            return;
        }
        let mut end = items.len();
        let mut z = chars.len();
        while end > start && z > start && items[end - 1].c == Some(chars[z - 1]) {
            end -= 1;
            z -= 1;
        }
        let mut consumed = start;
        for (i, item) in items.iter().enumerate().take(end).skip(start) {
            let mut item = item.clone();
            item.c = if i < z {
                consumed += 1;
                Some(chars[i])
            } else {
                None
            };
            self.extract_item(&item, font, wmode, flags, i == 0);
        }
        if end == items.len() {
            self.meta[index].text = chars[consumed..].iter().collect();
        } else {
            let rest: String = chars[consumed..z].iter().collect();
            self.flush_actual(&rest, 0.0);
            self.extract_items(&items[end..], font, wmode, flags);
        }
    }
}
