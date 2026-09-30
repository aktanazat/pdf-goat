//! The document tree html5ever builds: an arena of nodes addressed by index.

use std::borrow::Cow;
use std::cell::RefCell;

use html5ever::interface::tree_builder::{
    ElemName, ElementFlags, NodeOrText, QuirksMode, TreeSink,
};
use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::{
    Attribute, LocalName, Namespace, ParseOpts, QualName, local_name, ns, parse_document,
};

pub type NodeId = usize;

/// The document node.
pub const DOCUMENT: NodeId = 0;

pub enum NodeData {
    Document,
    Element {
        attrs: Vec<(String, String)>,
        template: Option<NodeId>,
    },
    Text(String),
    /// Comments, processing instructions, and the doctype: never rendered.
    Other,
}

pub struct Node {
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    /// The element name; empty for other nodes.
    pub name: QualName,
    pub data: NodeData,
}

pub struct Dom {
    pub nodes: Vec<Node>,
}

impl Dom {
    /// Parses a whole HTML document, scripting disabled as in a print renderer.
    pub fn parse(text: &str) -> Dom {
        let opts = ParseOpts {
            tree_builder: TreeBuilderOpts {
                scripting_enabled: false,
                ..TreeBuilderOpts::default()
            },
            ..ParseOpts::default()
        };
        parse_document(Sink::new(), opts).one(text)
    }

    /// The element's local name in the HTML namespace, or `""`.
    pub fn tag(&self, id: NodeId) -> &str {
        match self.nodes.get(id) {
            Some(node)
                if matches!(node.data, NodeData::Element { .. }) && node.name.ns == ns!(html) =>
            {
                &node.name.local
            }
            Some(node) if matches!(node.data, NodeData::Element { .. }) => &node.name.local,
            _ => "",
        }
    }

    pub fn is_element(&self, id: NodeId) -> bool {
        matches!(
            self.nodes.get(id).map(|node| &node.data),
            Some(NodeData::Element { .. })
        )
    }

    pub fn attr(&self, id: NodeId, name: &str) -> Option<&str> {
        match self.nodes.get(id).map(|node| &node.data) {
            Some(NodeData::Element { attrs, .. }) => attrs
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str()),
            _ => None,
        }
    }

    pub fn attrs(&self, id: NodeId) -> &[(String, String)] {
        match self.nodes.get(id).map(|node| &node.data) {
            Some(NodeData::Element { attrs, .. }) => attrs,
            _ => &[],
        }
    }

    pub fn children(&self, id: NodeId) -> &[NodeId] {
        self.nodes.get(id).map_or(&[], |node| &node.children)
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.nodes.get(id).and_then(|node| node.parent)
    }

    /// The element children of `id`.
    pub fn element_children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.children(id)
            .iter()
            .copied()
            .filter(|&child| self.is_element(child))
    }

    /// The first element child named `tag`.
    pub fn child_named(&self, id: NodeId, tag: &str) -> Option<NodeId> {
        self.element_children(id)
            .find(|&child| self.tag(child) == tag)
    }

    /// The concatenated text of the subtree.
    pub fn text_content(&self, id: NodeId) -> String {
        let mut out = String::new();
        let mut stack = vec![id];
        while let Some(node) = stack.pop() {
            match self.nodes.get(node).map(|n| &n.data) {
                Some(NodeData::Text(text)) => out.push_str(text),
                Some(NodeData::Element { .. } | NodeData::Document) => {
                    stack.extend(self.children(node).iter().rev());
                }
                _ => {}
            }
        }
        out
    }

    /// Serializes an inline SVG subtree as XML for the SVG parser.
    pub fn svg(&self, root: NodeId) -> String {
        fn escape(output: &mut String, text: &str) {
            for ch in text.chars() {
                match ch {
                    '&' => output.push_str("&amp;"),
                    '<' => output.push_str("&lt;"),
                    '>' => output.push_str("&gt;"),
                    '"' => output.push_str("&quot;"),
                    _ => output.push(ch),
                }
            }
        }
        let mut output = String::new();
        let mut stack = vec![(root, false)];
        while let Some((id, closing)) = stack.pop() {
            match &self.nodes[id].data {
                NodeData::Text(text) => escape(&mut output, text),
                NodeData::Element { attrs, .. } => {
                    let name = self.tag(id);
                    if closing {
                        output.push_str("</");
                        output.push_str(name);
                        output.push('>');
                        continue;
                    }
                    output.push('<');
                    output.push_str(name);
                    if id == root {
                        output.push_str(" xmlns=\"http://www.w3.org/2000/svg\"");
                    }
                    for (key, value) in attrs {
                        if key == "xmlns" {
                            continue;
                        }
                        output.push(' ');
                        output.push_str(key);
                        output.push_str("=\"");
                        escape(&mut output, value);
                        output.push('"');
                    }
                    output.push('>');
                    stack.push((id, true));
                    stack.extend(self.children(id).iter().rev().map(|&child| (child, false)));
                }
                _ => {}
            }
        }
        output
    }

    /// The `<html>` element.
    pub fn root_element(&self) -> Option<NodeId> {
        self.element_children(DOCUMENT).next()
    }
}

/// Element names handed to the tree builder, owned so no borrow of the arena outlives
/// the call.
#[derive(Debug)]
pub struct OwnedName(QualName);

impl ElemName for OwnedName {
    fn ns(&self) -> &Namespace {
        &self.0.ns
    }

    fn local_name(&self) -> &LocalName {
        &self.0.local
    }
}

struct Sink {
    nodes: RefCell<Vec<Node>>,
}

fn empty_name() -> QualName {
    QualName::new(None, ns!(), local_name!(""))
}

impl Sink {
    fn new() -> Sink {
        let document = Node {
            parent: None,
            children: Vec::new(),
            name: empty_name(),
            data: NodeData::Document,
        };
        Sink {
            nodes: RefCell::new(vec![document]),
        }
    }

    fn push(&self, name: QualName, data: NodeData) -> NodeId {
        let mut nodes = self.nodes.borrow_mut();
        nodes.push(Node {
            parent: None,
            children: Vec::new(),
            name,
            data,
        });
        nodes.len() - 1
    }

    fn detach(nodes: &mut [Node], id: NodeId) {
        let Some(parent) = nodes.get_mut(id).and_then(|node| node.parent.take()) else {
            return;
        };
        if let Some(parent) = nodes.get_mut(parent) {
            parent.children.retain(|&child| child != id);
        }
    }

    /// Inserts `child` into `parent` at `position` (the end when `None`), merging text
    /// into an adjacent text node.
    fn insert(&self, parent: NodeId, position: Option<NodeId>, child: NodeOrText<NodeId>) {
        let mut nodes = self.nodes.borrow_mut();
        let Some(siblings) = nodes.get(parent).map(|node| &node.children) else {
            return;
        };
        let index = match position {
            Some(before) => siblings
                .iter()
                .position(|&sibling| sibling == before)
                .unwrap_or(siblings.len()),
            None => siblings.len(),
        };
        let previous = index.checked_sub(1).and_then(|i| siblings.get(i)).copied();
        let child = match child {
            NodeOrText::AppendText(text) => {
                if let Some(previous) = previous
                    && let Some(Node {
                        data: NodeData::Text(existing),
                        ..
                    }) = nodes.get_mut(previous)
                {
                    existing.push_str(&text);
                    return;
                }
                nodes.push(Node {
                    parent: None,
                    children: Vec::new(),
                    name: empty_name(),
                    data: NodeData::Text(text.to_string()),
                });
                nodes.len() - 1
            }
            NodeOrText::AppendNode(node) => {
                Sink::detach(&mut nodes, node);
                node
            }
        };
        // Detaching may have shifted the insertion point.
        let index = match position {
            Some(before) => nodes[parent]
                .children
                .iter()
                .position(|&sibling| sibling == before)
                .unwrap_or(nodes[parent].children.len()),
            None => nodes[parent].children.len(),
        };
        nodes[parent].children.insert(index, child);
        nodes[child].parent = Some(parent);
    }
}

impl TreeSink for Sink {
    type Handle = NodeId;
    type Output = Dom;
    type ElemName<'a> = OwnedName;

    fn finish(self) -> Dom {
        Dom {
            nodes: self.nodes.into_inner(),
        }
    }

    fn parse_error(&self, _msg: Cow<'static, str>) {}

    fn get_document(&self) -> NodeId {
        DOCUMENT
    }

    fn elem_name<'a>(&'a self, target: &'a NodeId) -> OwnedName {
        OwnedName(
            self.nodes
                .borrow()
                .get(*target)
                .map_or_else(empty_name, |node| node.name.clone()),
        )
    }

    fn create_element(&self, name: QualName, attrs: Vec<Attribute>, flags: ElementFlags) -> NodeId {
        let template = flags
            .template
            .then(|| self.push(empty_name(), NodeData::Document));
        let attrs = attrs
            .into_iter()
            .map(|attr| (attr.name.local.to_string(), attr.value.to_string()))
            .collect();
        self.push(name, NodeData::Element { attrs, template })
    }

    fn create_comment(&self, _text: StrTendril) -> NodeId {
        self.push(empty_name(), NodeData::Other)
    }

    fn create_pi(&self, _target: StrTendril, _data: StrTendril) -> NodeId {
        self.push(empty_name(), NodeData::Other)
    }

    fn append(&self, parent: &NodeId, child: NodeOrText<NodeId>) {
        self.insert(*parent, None, child);
    }

    fn append_based_on_parent_node(
        &self,
        element: &NodeId,
        prev_element: &NodeId,
        child: NodeOrText<NodeId>,
    ) {
        let parent = self
            .nodes
            .borrow()
            .get(*element)
            .and_then(|node| node.parent);
        match parent {
            Some(parent) => self.insert(parent, Some(*element), child),
            None => self.insert(*prev_element, None, child),
        }
    }

    fn append_doctype_to_document(
        &self,
        _name: StrTendril,
        _public_id: StrTendril,
        _system_id: StrTendril,
    ) {
    }

    fn get_template_contents(&self, target: &NodeId) -> NodeId {
        match self.nodes.borrow().get(*target).map(|node| &node.data) {
            Some(NodeData::Element {
                template: Some(contents),
                ..
            }) => *contents,
            _ => *target,
        }
    }

    fn same_node(&self, x: &NodeId, y: &NodeId) -> bool {
        x == y
    }

    fn set_quirks_mode(&self, _mode: QuirksMode) {}

    fn append_before_sibling(&self, sibling: &NodeId, new_node: NodeOrText<NodeId>) {
        let parent = self
            .nodes
            .borrow()
            .get(*sibling)
            .and_then(|node| node.parent);
        if let Some(parent) = parent {
            self.insert(parent, Some(*sibling), new_node);
        }
    }

    fn add_attrs_if_missing(&self, target: &NodeId, attrs: Vec<Attribute>) {
        let mut nodes = self.nodes.borrow_mut();
        if let Some(Node {
            data: NodeData::Element {
                attrs: existing, ..
            },
            ..
        }) = nodes.get_mut(*target)
        {
            for attr in attrs {
                let name = attr.name.local.to_string();
                if !existing.iter().any(|(key, _)| *key == name) {
                    existing.push((name, attr.value.to_string()));
                }
            }
        }
    }

    fn remove_from_parent(&self, target: &NodeId) {
        Sink::detach(&mut self.nodes.borrow_mut(), *target);
    }

    fn reparent_children(&self, node: &NodeId, new_parent: &NodeId) {
        let mut nodes = self.nodes.borrow_mut();
        let Some(children) = nodes
            .get_mut(*node)
            .map(|n| std::mem::take(&mut n.children))
        else {
            return;
        };
        for &child in &children {
            nodes[child].parent = Some(*new_parent);
        }
        if let Some(parent) = nodes.get_mut(*new_parent) {
            parent.children.extend(children);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Dom;
    use crate::markdown::to_html;

    #[test]
    fn sane_lists_keep_mixed_markers_in_the_item_and_require_four_space_nesting() {
        let dom = Dom::parse(&to_html(
            "- bullet\n1. literal continuation\n\n- outer\n  - same level\n",
        ));
        let lists: Vec<_> = (0..dom.nodes.len())
            .filter(|&id| matches!(dom.tag(id), "ul" | "ol"))
            .collect();
        assert_eq!(
            lists.len(),
            1,
            "a marker of another type without a blank line remains literal text"
        );
        let items: Vec<_> = dom
            .element_children(lists[0])
            .map(|id| {
                dom.text_content(id)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        assert_eq!(
            items,
            ["bullet 1. literal continuation", "outer", "same level"]
        );
    }
}
