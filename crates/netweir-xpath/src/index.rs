//! A flat index of a document's nodes in document order. A node's
//! descendants are the contiguous range after it, so scanning them is a
//! pass over small arrays rather than a walk through the tree's links.

use std::collections::HashMap;
use std::sync::OnceLock;

use netweir_html::{self as html, NodeId};

/// The node kinds, as the index stores them: the DOM's nodeType numbers.
pub const ELEMENT: u8 = 0x01;
pub const TEXT: u8 = 0x03;
pub const CDATA: u8 = 0x04;
pub const PI: u8 = 0x07;
pub const COMMENT: u8 = 0x08;
pub const DOCUMENT: u8 = 0x09;
pub const DOCTYPE: u8 = 0x0A;
const OTHER: u8 = 0x0B;

/// Tag ids of names the parser doesn't know start here; below it, a known
/// tag's number plus one.
const FIRST_NAMED: usize = 1000;

/// The index of one version of a tree. Its owner builds it once and keeps
/// it until the tree changes: every query reads it, none builds it.
pub struct Index {
    raw: Vec<u32>,
    /// Each node's parent's position; u32::MAX for the root.
    parent: Vec<u32>,
    /// One past the last descendant of each node.
    end: Vec<u32>,
    kind: Vec<u8>,
    tag: Vec<usize>,
    /// Each node's position, by node id; u32::MAX for one not indexed.
    order: Vec<u32>,
    /// Tag ids of the lowercase names the parser doesn't know.
    names: HashMap<String, usize>,
    /// Positions of the elements carrying each `id`, built on first use.
    ids: OnceLock<HashMap<String, Vec<u32>>>,
}

fn kind_of(tree: &html::Document, id: u32) -> u8 {
    match tree.kind(id) {
        html::Kind::Element { .. } => ELEMENT,
        html::Kind::Text(_) => TEXT,
        html::Kind::Cdata(_) => CDATA,
        html::Kind::ProcessingInstruction { .. } => PI,
        html::Kind::Comment(_) => COMMENT,
        html::Kind::Document => DOCUMENT,
        html::Kind::Doctype { .. } => DOCTYPE,
        html::Kind::Fragment | html::Kind::ShadowRoot(_) => OTHER,
    }
}

/// An element's name for tag ids: lowercase, without an XML prefix.
fn tag_key(tree: &html::Document, id: u32) -> &str {
    let name = tree.lower_name(id).unwrap_or("");
    // Only XML has prefixes: in HTML, <o:p> is an element called "o:p".
    match tree.kind(id) {
        html::Kind::Element {
            ns: html::Ns::Xml, ..
        } => name.split_once(':').map_or(name, |(_, local)| local),
        _ => name,
    }
}

impl Index {
    /// Indexes the tree under each of `roots` in turn: the document, then
    /// any subtrees removed from it or built apart from it, so every node
    /// a caller can hold has a position.
    pub fn build(tree: &html::Document, roots: &[NodeId]) -> Index {
        let mut raw = Vec::with_capacity(tree.len());
        let mut parent = Vec::with_capacity(tree.len());
        let mut kind = Vec::with_capacity(tree.len());
        let mut tag = Vec::with_capacity(tree.len());
        let mut order = vec![u32::MAX; tree.len()];
        let mut names: HashMap<String, usize> = HashMap::new();
        for &root in roots {
            // Preorder by the tree's links, keeping each node's parent's
            // position on a stack of the open ancestors.
            let mut ancestors: Vec<u32> = Vec::new();
            let mut at = Some(root);
            while let Some(n) = at {
                let pos = raw.len() as u32;
                order[n as usize] = pos;
                raw.push(n);
                parent.push(ancestors.last().copied().unwrap_or(u32::MAX));
                let k = kind_of(tree, n);
                kind.push(k);
                // From the lowercase name alone, as tag_id_named works it
                // out: an XML <title> shares HTML title's id.
                tag.push(if k != ELEMENT {
                    0
                } else if let Some(t) = tree.known_tag(n) {
                    t as usize + 1
                } else {
                    let key = tag_key(tree, n).to_ascii_lowercase();
                    match html::tag_number(&key) {
                        Some(t) => t as usize + 1,
                        None => {
                            let next = FIRST_NAMED + names.len();
                            *names.entry(key).or_insert(next)
                        }
                    }
                });
                at = match tree.first_child(n) {
                    Some(child) => {
                        ancestors.push(pos);
                        Some(child)
                    }
                    None => {
                        let mut up = n;
                        loop {
                            if up == root {
                                break None;
                            }
                            if let Some(s) = tree.next_sibling(up) {
                                break Some(s);
                            }
                            up = tree
                                .parent(up)
                                .expect("a node inside the root has a parent");
                            ancestors.pop();
                        }
                    }
                };
            }
        }
        let n = raw.len();
        // Children come after their parent, so one backward pass carries
        // each subtree's end up to its parent.
        let mut end: Vec<u32> = (1..=n as u32).collect();
        for i in (1..n).rev() {
            let p = parent[i] as usize;
            if p < n && end[i] > end[p] {
                end[p] = end[i];
            }
        }
        Index {
            raw,
            parent,
            end,
            kind,
            tag,
            order,
            names,
            ids: OnceLock::new(),
        }
    }

    /// How many nodes are indexed.
    pub fn len(&self) -> usize {
        self.raw.len()
    }

    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }

    /// A node's position; u32::MAX if it was made after the index was.
    pub fn order(&self, id: NodeId) -> u32 {
        self.order.get(id as usize).copied().unwrap_or(u32::MAX)
    }

    /// The node at position `at`.
    pub fn node_at(&self, at: u32) -> NodeId {
        self.raw[at as usize]
    }

    /// The kind ([`ELEMENT`], [`TEXT`] and so on) at position `at`.
    pub fn kind_at(&self, at: u32) -> u8 {
        self.kind[at as usize]
    }

    /// The tag id at position `at`.
    pub fn tag_at(&self, at: u32) -> usize {
        self.tag.get(at as usize).copied().unwrap_or(0)
    }

    /// The tag id elements named `name` (compared lowercased) carry, if
    /// any element could: a known tag's always, another name's only when
    /// some element in the document has it.
    pub fn tag_id_named(&self, name: &str) -> Option<usize> {
        let lower = name.to_ascii_lowercase();
        match html::tag_number(&lower) {
            Some(t) => Some(t as usize + 1),
            None => self.names.get(&lower).copied(),
        }
    }

    /// The elements whose `id` is `id`, in document order. `id_of` reads a
    /// node's `id` attribute; the first call's is used for every call.
    pub fn with_id<'t>(&self, id: &str, id_of: impl Fn(NodeId) -> Option<&'t str>) -> Vec<NodeId> {
        let ids = self.ids.get_or_init(|| {
            let mut ids: HashMap<String, Vec<u32>> = HashMap::new();
            for (j, &raw) in self.raw.iter().enumerate() {
                if let Some(v) = id_of(raw) {
                    ids.entry(v.to_string()).or_default().push(j as u32);
                }
            }
            ids
        });
        ids.get(id)
            .map(|at| at.iter().map(|&j| self.raw[j as usize]).collect())
            .unwrap_or_default()
    }

    /// Appends to `out` the nodes below `at` (from `at` itself if
    /// `include_self`) that pass `keep(kind, tag)`, in document order.
    pub fn descendants(
        &self,
        at: u32,
        include_self: bool,
        keep: impl Fn(u8, usize) -> bool,
        out: &mut Vec<NodeId>,
    ) {
        let start = if include_self { at } else { at + 1 } as usize;
        for j in start..self.end[at as usize] as usize {
            if keep(self.kind[j], self.tag[j]) {
                out.push(self.raw[j]);
            }
        }
    }

    /// One past the last descendant of `at`.
    pub fn end_of(&self, at: u32) -> u32 {
        self.end[at as usize]
    }

    /// The position of the parent of the node at `at`; u32::MAX for a
    /// root.
    pub fn parent_of(&self, at: u32) -> u32 {
        self.parent[at as usize]
    }

    /// Appends to `out` the children of `at` that pass `keep(kind, tag)`.
    pub fn children(&self, at: u32, keep: impl Fn(u8, usize) -> bool, out: &mut Vec<NodeId>) {
        let stop = self.end[at as usize] as usize;
        let mut c = at as usize + 1;
        while c < stop {
            if keep(self.kind[c], self.tag[c]) {
                out.push(self.raw[c]);
            }
            c = self.end[c] as usize;
        }
    }
}
