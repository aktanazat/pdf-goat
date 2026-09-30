//! Name trees and number trees (ISO 32000-2 7.9.6, 7.9.7): reading every
//! entry, and writing a tree back as one sorted leaf.

use std::collections::{BTreeMap, HashSet};

use crate::document::Document;
use crate::error::{Error, Result};
use crate::object::{Dict, Object, PdfString};

/// Deepest tree accepted.
const MAX_TREE_DEPTH: usize = 64;

/// A name tree holding `entries` in one node: `<< /Names [key value ...] >>`
/// sorted by key bytes. A repeated key keeps its last value.
pub fn build_name_tree(entries: impl IntoIterator<Item = (Vec<u8>, Object)>) -> Dict {
    let sorted: BTreeMap<Vec<u8>, Object> = entries.into_iter().collect();
    let mut names = Vec::with_capacity(sorted.len() * 2);
    for (key, value) in sorted {
        names.push(Object::String(PdfString::literal(key)));
        names.push(value);
    }
    let mut dict = Dict::new();
    dict.insert("Names", names);
    dict
}

/// A number tree holding `entries` in one node: `<< /Nums [key value ...] >>`
/// sorted by key. A repeated key keeps its last value.
pub fn build_number_tree(entries: impl IntoIterator<Item = (i64, Object)>) -> Dict {
    let sorted: BTreeMap<i64, Object> = entries.into_iter().collect();
    let mut nums = Vec::with_capacity(sorted.len() * 2);
    for (key, value) in sorted {
        nums.push(Object::Integer(key));
        nums.push(value);
    }
    let mut dict = Dict::new();
    dict.insert("Nums", nums);
    dict
}

impl Document {
    /// Every entry of the name tree rooted at `root`, in tree order. Values
    /// are returned as stored (possibly references). Kids already visited
    /// are skipped; a key seen twice keeps its first value.
    pub fn name_tree(&self, root: &Object) -> Result<Vec<(Vec<u8>, Object)>> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut visited = HashSet::new();
        self.walk_tree(root, b"Names", &mut visited, 0, &mut |key, value| {
            let key = match key {
                Object::String(s) => s.bytes.clone(),
                Object::Name(n) => n.as_bytes().to_vec(),
                _ => return,
            };
            if seen.insert(key.clone()) {
                out.push((key, value.clone()));
            }
        })?;
        Ok(out)
    }

    /// Every entry of the number tree rooted at `root`, in tree order.
    pub fn number_tree(&self, root: &Object) -> Result<Vec<(i64, Object)>> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut visited = HashSet::new();
        self.walk_tree(root, b"Nums", &mut visited, 0, &mut |key, value| {
            if let Some(key) = key.as_i64()
                && seen.insert(key)
            {
                out.push((key, value.clone()));
            }
        })?;
        Ok(out)
    }

    fn walk_tree(
        &self,
        node: &Object,
        leaf_key: &[u8],
        visited: &mut HashSet<u32>,
        depth: usize,
        visit: &mut dyn FnMut(&Object, &Object),
    ) -> Result<()> {
        if depth > MAX_TREE_DEPTH {
            return Err(Error::LimitExceeded(format!(
                "name or number tree deeper than {MAX_TREE_DEPTH} levels"
            )));
        }
        if let Object::Reference(id) = node
            && !visited.insert(id.num)
        {
            return Ok(());
        }
        let Some(dict) = self.resolve_dict(node)? else {
            return Ok(());
        };
        if let Some(leaf) = dict.get(leaf_key) {
            let items = self.resolve_array(leaf)?.unwrap_or_default();
            for [key, value] in items.as_chunks::<2>().0 {
                let key = self.resolve(key)?;
                visit(&key, value);
            }
        }
        if let Some(kids) = dict.get(b"Kids") {
            for kid in self.resolve_array(kids)?.unwrap_or_default() {
                self.walk_tree(&kid, leaf_key, visited, depth + 1, visit)?;
            }
        }
        Ok(())
    }

    /// Entries of the catalog's `/Names` tree `tree` (such as
    /// `EmbeddedFiles`, `Dests`, or `JavaScript`); empty when absent.
    pub fn names(&self, tree: &[u8]) -> Result<Vec<(Vec<u8>, Object)>> {
        let catalog = self.catalog()?;
        let Some(names) = catalog.get(b"Names") else {
            return Ok(Vec::new());
        };
        let Some(names) = self.resolve_dict(names)? else {
            return Ok(Vec::new());
        };
        match names.get(tree) {
            Some(root) => self.name_tree(root),
            None => Ok(Vec::new()),
        }
    }

    /// Replace the catalog's `/Names` tree `tree` with `entries`, written as
    /// a new object; empty `entries` removes the tree.
    pub fn set_names(&mut self, tree: &[u8], entries: Vec<(Vec<u8>, Object)>) -> Result<()> {
        let root = self.catalog_ref()?;
        let mut catalog = self.catalog()?;
        let stored = catalog.get(b"Names").cloned();
        let mut names = match &stored {
            Some(value) => self.resolve_dict(value)?.unwrap_or_default(),
            None => Dict::new(),
        };
        if entries.is_empty() {
            names.remove(tree);
        } else {
            let node = self.add(build_name_tree(entries));
            names.insert(tree, node);
        }
        match stored {
            Some(Object::Reference(id)) => self.set(id, names),
            _ => {
                if names.is_empty() {
                    catalog.remove(b"Names");
                } else {
                    catalog.insert("Names", names);
                }
                self.set(root, catalog);
            }
        }
        Ok(())
    }

    /// The `/PageLabels` number tree: page index to label dictionary.
    pub fn page_labels(&self) -> Result<Vec<(i64, Object)>> {
        match self.catalog()?.get(b"PageLabels") {
            Some(root) => self.number_tree(root),
            None => Ok(Vec::new()),
        }
    }

    /// Replace `/PageLabels`; empty `entries` removes it.
    pub fn set_page_labels(&mut self, entries: Vec<(i64, Object)>) -> Result<()> {
        let root = self.catalog_ref()?;
        let mut catalog = self.catalog()?;
        if entries.is_empty() {
            catalog.remove(b"PageLabels");
        } else {
            let node = self.add(build_number_tree(entries));
            catalog.insert("PageLabels", node);
        }
        self.set(root, catalog);
        Ok(())
    }
}
