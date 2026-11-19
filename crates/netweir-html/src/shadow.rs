//! Shadow roots, slots and the flat tree, as the DOM Standard has them.
//!
//! A shadow root is a node of its own kind, like a template's contents:
//! nobody's child, reached from its host by [`Document::shadow_root`] and
//! back by [`Document::shadow_host`]. The host's children stay its
//! children; which of them each slot shows is worked out when asked, from
//! the `slot` and `name` attributes as they are then, so edits never leave
//! an assignment stale. The flat tree, where a host's shadow root takes
//! the place of its children and each slot shows its assigned nodes, is
//! walked by [`Document::flat_children`] and [`Document::flat_parent`].
//!
//! Walks that follow the node tree (children, `select` from the document,
//! XPath, `text`) don't go into shadow roots, as a browser's don't.

use crate::dom::{AttrNs, Data, Document, NodeId, Ns, ShadowRoot, SlotAssignment};
use crate::edit::EditError;
use crate::tags::Tag;

/// Whether an element of this name may host a shadow root: one of the
/// HTML Standard's list, or a valid custom element name.
pub(crate) fn valid_host(ns: Ns, tag: Tag, name: &str) -> bool {
    use Tag::*;
    ns == Ns::Html
        && (matches!(
            tag,
            Article
                | Aside
                | Blockquote
                | Body
                | Div
                | Footer
                | H1
                | H2
                | H3
                | H4
                | H5
                | H6
                | Header
                | Main
                | Nav
                | P
                | Section
                | Span
        ) || (tag == Other && crate::css::custom_element_name(name)))
}

impl Document {
    /// The element's shadow root, open or closed.
    pub fn shadow_root(&self, host: NodeId) -> Option<NodeId> {
        if self.shadow_roots.is_empty() || self.is_removed(host) {
            return None;
        }
        self.shadow_roots.get(&host).copied()
    }

    /// A shadow root's host.
    pub fn shadow_host(&self, root: NodeId) -> Option<NodeId> {
        if self.is_removed(root) {
            return None;
        }
        match self.nodes[root as usize].data {
            Data::ShadowRoot { host, .. } => Some(host),
            _ => None,
        }
    }

    /// The node at the top of `id`'s tree: the document, a template's
    /// contents, a shadow root, or a detached node.
    pub fn tree_root(&self, id: NodeId) -> NodeId {
        let mut at = id;
        while let Some(p) = self.parent(at) {
            at = p;
        }
        at
    }

    /// Attaches a shadow root to `host`, as `attachShadow` does: only an
    /// HTML element of a name the standard lists (`div`, `span`,
    /// `section`...) or a custom element may have one, and only one.
    /// [`EditError::NotAShadowHost`] otherwise. The root starts empty. As
    /// in the DOM, a root the parser made from a `<template
    /// shadowrootmode>` is handed back once to a call with its mode,
    /// emptied, keeping the settings it was declared with.
    pub fn attach_shadow(&mut self, host: NodeId, init: ShadowRoot) -> Result<NodeId, EditError> {
        if self.is_removed(host) {
            return Err(EditError::Removed);
        }
        let Some(e) = self.element(host) else {
            return Err(EditError::NotAShadowHost);
        };
        let name = self.element_name(e);
        if !valid_host(e.ns, e.tag, name) {
            return Err(EditError::NotAShadowHost);
        }
        let Some(&root) = self.shadow_roots.get(&host) else {
            return Ok(self.add_shadow_root(host, init));
        };
        // A root the parser made is handed back once, emptied, to the
        // first call asking for its mode; it keeps its other settings.
        let Data::ShadowRoot { init: mut kept, .. } = self.nodes[root as usize].data else {
            unreachable!("a host's shadow root");
        };
        if !kept.declarative || kept.mode != init.mode {
            return Err(EditError::NotAShadowHost);
        }
        while let Some(child) = self.first_child(root) {
            self.detach(child);
        }
        kept.declarative = false;
        self.nodes[root as usize].data = Data::ShadowRoot { host, init: kept };
        Ok(root)
    }

    /// Makes and links a shadow root, the checks done.
    pub(crate) fn add_shadow_root(&mut self, host: NodeId, init: ShadowRoot) -> NodeId {
        debug_assert!(!self.shadow_roots.contains_key(&host), "{host} has a root");
        let root = self.add(Data::ShadowRoot { host, init });
        self.shadow_roots.insert(host, root);
        root
    }

    /// Whether `id` is a `slot` element in a shadow tree, the only kind
    /// that's assigned nodes.
    pub fn is_slot(&self, id: NodeId) -> bool {
        !self.shadow_roots.is_empty()
            && self
                .element(id)
                .is_some_and(|e| e.ns == Ns::Html && self.element_name(e) == "slot")
            && self.shadow_host(self.tree_root(id)).is_some()
    }

    /// An attribute with no namespace.
    fn plain_attr(&self, id: NodeId, name: &str) -> Option<&str> {
        self.attrs(id)
            .find(|&(ns, n, _)| ns == AttrNs::None && n == name)
            .map(|(_, _, v)| v)
    }

    /// Whether `id` can be assigned to a slot: an element or text.
    fn slottable(&self, id: NodeId) -> bool {
        matches!(
            self.nodes[id as usize].data,
            Data::Element(_) | Data::Text(_)
        )
    }

    /// Sets the nodes a slot in a shadow root with
    /// [`SlotAssignment::Manual`] is assigned, as `slot.assign()` does:
    /// in that order, each taken from any slot it was assigned before.
    /// Only those that are children of the host are assigned, when asked;
    /// in a root with named assignment the list is kept and unused.
    pub fn assign(&mut self, slot: NodeId, nodes: &[NodeId]) {
        if self.element(slot).is_none() || self.is_removed(slot) {
            return;
        }
        for list in self.manual_slots.values_mut() {
            list.retain(|n| !nodes.contains(n));
        }
        let mut list: Vec<NodeId> = Vec::with_capacity(nodes.len());
        for &n in nodes {
            if self.slottable(n) && !self.is_removed(n) && !list.contains(&n) {
                list.push(n);
            }
        }
        self.manual_slots.insert(slot, list);
    }

    /// The slot that `node` is assigned to, as the DOM's "find a slot":
    /// `None` unless it's an element or text whose parent hosts a shadow
    /// root (open or closed).
    pub fn assigned_slot(&self, node: NodeId) -> Option<NodeId> {
        if self.shadow_roots.is_empty() || !self.slottable(node) {
            return None;
        }
        let root = self.shadow_root(self.parent(node)?)?;
        let Data::ShadowRoot { init, .. } = self.nodes[root as usize].data else {
            return None;
        };
        if init.slot_assignment == SlotAssignment::Manual {
            return self.slots(root).find(|s| {
                self.manual_slots
                    .get(s)
                    .is_some_and(|list| list.contains(&node))
            });
        }
        let name = self.slot_name(node);
        self.slots(root)
            .find(|&s| self.plain_attr(s, "name").unwrap_or("") == name)
    }

    /// The name a slottable asks for: its `slot` attribute, or "".
    fn slot_name(&self, node: NodeId) -> &str {
        match self.nodes[node as usize].data {
            Data::Element(_) => self.plain_attr(node, "slot").unwrap_or(""),
            _ => "",
        }
    }

    /// The slots in a shadow tree, in tree order.
    fn slots(&self, root: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        self.descendants(root).filter(move |&n| {
            self.element(n)
                .is_some_and(|e| e.ns == Ns::Html && self.element_name(e) == "slot")
        })
    }

    /// Every node under `id`, in tree order, not into template contents
    /// or shadow roots.
    fn descendants(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let mut at = self.first_child(id);
        std::iter::from_fn(move || {
            let n = at?;
            at = self.first_child(n).or_else(|| {
                let mut up = n;
                while up != id {
                    if let Some(s) = self.next_sibling(up) {
                        return Some(s);
                    }
                    up = self.parent(up)?;
                }
                None
            });
            Some(n)
        })
    }

    /// The nodes assigned to a slot, as `slot.assignedNodes()`: the
    /// host's children that [`Document::assigned_slot`] puts there, in
    /// tree order (or the order [`Document::assign`] gave, for manual
    /// assignment). Empty for an element that isn't a slot in a shadow
    /// tree.
    pub fn assigned_nodes(&self, slot: NodeId) -> Vec<NodeId> {
        let Some(host) = self.shadow_host(self.tree_root(slot)) else {
            return Vec::new();
        };
        if !self.is_slot(slot) {
            return Vec::new();
        }
        let root = self.shadow_roots[&host];
        if let Data::ShadowRoot { init, .. } = self.nodes[root as usize].data
            && init.slot_assignment == SlotAssignment::Manual
        {
            return self.manual_slots.get(&slot).map_or(Vec::new(), |list| {
                list.iter()
                    .copied()
                    .filter(|&n| self.parent(n) == Some(host))
                    .collect()
            });
        }
        // Only the first slot of a name is assigned anything.
        let name = self.plain_attr(slot, "name").unwrap_or("");
        if self
            .slots(root)
            .find(|&s| self.plain_attr(s, "name").unwrap_or("") == name)
            != Some(slot)
        {
            return Vec::new();
        }
        self.children(host)
            .filter(|&c| self.slottable(c) && self.slot_name(c) == name)
            .collect()
    }

    /// `slot.assignedNodes({flatten: true})`: a slot assigned another
    /// slot shows what that one shows, and a slot assigned nothing shows
    /// its own children, its fallback content.
    pub fn flattened_assigned_nodes(&self, slot: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        self.flatten_into(slot, &mut out);
        out
    }

    fn flatten_into(&self, slot: NodeId, out: &mut Vec<NodeId>) {
        if !self.is_slot(slot) {
            return;
        }
        let mut nodes = self.assigned_nodes(slot);
        if nodes.is_empty() {
            nodes = self.children(slot).filter(|&c| self.slottable(c)).collect();
        }
        for n in nodes {
            if self.is_slot(n) {
                self.flatten_into(n, out);
            } else {
                out.push(n);
            }
        }
    }

    /// A node's children in the flat tree, as rendering sees them: a
    /// shadow host's are its shadow root's children, a slot's are its
    /// assigned nodes (or its own children when it has none), and every
    /// other node's are its children.
    pub fn flat_children(&self, id: NodeId) -> Vec<NodeId> {
        if let Some(root) = self.shadow_root(id) {
            return self.children(root).collect();
        }
        if self.is_slot(id) {
            let assigned = self.assigned_nodes(id);
            if !assigned.is_empty() {
                return assigned;
            }
        }
        self.children(id).collect()
    }

    /// A node's parent in the flat tree: its slot when it's assigned one,
    /// the host for a shadow root's child, else its parent. `None` for a
    /// node the flat tree leaves out: a host's child no slot takes, or a
    /// slot's fallback content when the slot was assigned nodes.
    pub fn flat_parent(&self, id: NodeId) -> Option<NodeId> {
        let parent = self.parent(id)?;
        if self.shadow_roots.is_empty() {
            return Some(parent);
        }
        if let Some(host) = self.shadow_host(parent) {
            return Some(host);
        }
        if self.shadow_root(parent).is_some() {
            return self.assigned_slot(id);
        }
        if self.is_slot(parent) && !self.assigned_nodes(parent).is_empty() {
            return None;
        }
        Some(parent)
    }

    /// The flat tree under `id`, in tree order, `id` first: what a
    /// renderer walks.
    pub fn flat_tree(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        let mut stack = if self.is_removed(id) {
            vec![]
        } else {
            vec![id]
        };
        std::iter::from_fn(move || {
            let n = stack.pop()?;
            let at = stack.len();
            stack.extend(self.flat_children(n));
            stack[at..].reverse();
            Some(n)
        })
    }

    /// The parent, or for a shadow root its host: the DOM's
    /// shadow-including parent, which `:host-context()` climbs.
    pub(crate) fn shadow_including_parent(&self, id: NodeId) -> Option<NodeId> {
        self.parent(id).or_else(|| self.shadow_host(id))
    }
}

/// A shadow root's settings from a `<template>` start tag's attributes,
/// or `None` when its `shadowrootmode` is missing or neither `open` nor
/// `closed` (in any case).
pub(crate) fn declared<'a>(attrs: impl Iterator<Item = (&'a str, &'a str)>) -> Option<ShadowRoot> {
    let mut init = ShadowRoot::open();
    let mut mode = None;
    for (name, value) in attrs {
        match name {
            "shadowrootmode" if mode.is_none() => {
                mode = Some(if value.eq_ignore_ascii_case("open") {
                    Some(crate::ShadowRootMode::Open)
                } else if value.eq_ignore_ascii_case("closed") {
                    Some(crate::ShadowRootMode::Closed)
                } else {
                    None
                })
            }
            "shadowrootdelegatesfocus" => init.delegates_focus = true,
            "shadowrootclonable" => init.clonable = true,
            "shadowrootserializable" => init.serializable = true,
            // Any other value is the default, named.
            "shadowrootslotassignment" if value.eq_ignore_ascii_case("manual") => {
                init.slot_assignment = SlotAssignment::Manual
            }
            _ => {}
        }
    }
    init.mode = mode??;
    init.declarative = true;
    Some(init)
}

#[cfg(test)]
mod tests {
    use crate::css::SelectorList;
    use crate::{
        Document, EditError, Kind, NodeId, Ns, Options, Parser, SerializeOptions, ShadowRoot,
        SlotAssignment, parse, parse_fragment, parse_fragment_with, parse_with,
    };

    fn by_id(doc: &Document, id: &str) -> NodeId {
        (0..doc.len() as NodeId)
            .find(|&n| doc.attrs(n).any(|a| a.1 == "id" && a.2 == id))
            .unwrap_or_else(|| panic!("no #{id}"))
    }

    fn ids(doc: &Document, nodes: &[NodeId]) -> Vec<String> {
        nodes
            .iter()
            .map(|&n| match doc.kind(n) {
                Kind::Text(t) => format!("'{t}'"),
                _ => doc
                    .attrs(n)
                    .find(|a| a.1 == "id")
                    .map_or("?".into(), |a| a.2.to_string()),
            })
            .collect()
    }

    const SLOTS: &str = "<div id=host><template shadowrootmode=open>\
        <slot id=s-a name=a></slot><slot id=s-a2 name=a></slot><slot id=s-default>fallback</slot>\
        <slot id=s-none name=none><i id=fb>fb</i></slot>\
        </template><span id=x slot=a></span>t<b id=y></b><!--c--><em id=z slot=nope></em></div>";

    #[test]
    fn named_slots_take_the_hosts_children_by_their_slot_attribute() {
        let doc = parse(SLOTS, false);
        #[cfg(debug_assertions)]
        doc.check_tree();
        let host = by_id(&doc, "host");
        let root = doc.shadow_root(host).unwrap();
        assert_eq!(doc.shadow_host(root), Some(host));
        assert!(matches!(doc.kind(root), Kind::ShadowRoot(i) if i.declarative));
        let (a, a2, default) = (
            by_id(&doc, "s-a"),
            by_id(&doc, "s-a2"),
            by_id(&doc, "s-default"),
        );
        assert_eq!(ids(&doc, &doc.assigned_nodes(a)), ["x"]);
        // Only the first slot of a name gets anything.
        assert!(doc.assigned_nodes(a2).is_empty());
        assert_eq!(ids(&doc, &doc.assigned_nodes(default)), ["'t'", "y"]);
        assert_eq!(doc.assigned_slot(by_id(&doc, "x")), Some(a));
        assert_eq!(doc.assigned_slot(by_id(&doc, "z")), None);
        // A slot with nothing assigned shows its fallback content.
        let none = by_id(&doc, "s-none");
        assert_eq!(ids(&doc, &doc.flattened_assigned_nodes(none)), ["fb"]);
        assert!(doc.is_slot(a) && !doc.is_slot(host));
    }

    #[test]
    fn the_flat_tree_puts_slotted_nodes_under_their_slots() {
        let doc = parse(SLOTS, false);
        let host = by_id(&doc, "host");
        let a = by_id(&doc, "s-a");
        let flat: Vec<NodeId> = doc.flat_tree(host).collect();
        assert_eq!(
            ids(&doc, &flat),
            [
                "host",
                "s-a",
                "x",
                "s-a2",
                "s-default",
                "'t'",
                "y",
                "s-none",
                "fb",
                "'fb'"
            ]
        );
        assert_eq!(doc.flat_parent(by_id(&doc, "x")), Some(a));
        assert_eq!(doc.flat_parent(a), Some(host));
        // Unassigned: not in the flat tree.
        assert_eq!(doc.flat_parent(by_id(&doc, "z")), None);
        let fallback = doc.first_child(by_id(&doc, "s-default")).unwrap();
        assert_eq!(doc.flat_parent(fallback), None);
        assert_eq!(doc.flat_children(a), [by_id(&doc, "x")]);
    }

    #[test]
    fn a_slot_slotted_into_another_shows_what_it_shows_flattened() {
        let doc = parse(
            "<div id=outer><template shadowrootmode=open>\
               <div id=inner><template shadowrootmode=open><slot id=deep></slot></template>\
                 <slot id=mid></slot></div>\
             </template><p id=light></p></div>",
            false,
        );
        #[cfg(debug_assertions)]
        doc.check_tree();
        let deep = by_id(&doc, "deep");
        assert_eq!(ids(&doc, &doc.assigned_nodes(deep)), ["mid"]);
        assert_eq!(ids(&doc, &doc.flattened_assigned_nodes(deep)), ["light"]);
        // ::slotted() in either shadow tree sees the light <p>, in the
        // inner one through #mid.
        let inner_root = doc.shadow_root(by_id(&doc, "inner")).unwrap();
        let slotted = SelectorList::parse("::slotted(p)").unwrap();
        assert!(slotted.matches(&doc, by_id(&doc, "light"), Some(inner_root)));
        let outer_root = doc.shadow_root(by_id(&doc, "outer")).unwrap();
        assert!(slotted.matches(&doc, by_id(&doc, "light"), Some(outer_root)));
        assert!(!slotted.matches(&doc, by_id(&doc, "light"), None));
    }

    #[test]
    fn manual_assignment_takes_only_what_assign_gives() {
        let mut doc = parse(
            "<div id=host><span id=x slot=a></span><b id=y></b></div>",
            false,
        );
        let host = by_id(&doc, "host");
        let init = ShadowRoot {
            slot_assignment: SlotAssignment::Manual,
            ..ShadowRoot::open()
        };
        let root = doc.attach_shadow(host, init).unwrap();
        let (s1, s2) = (
            doc.create_element(Ns::Html, "slot"),
            doc.create_element(Ns::Html, "slot"),
        );
        doc.set_attr(s1, "name", "a");
        doc.append(root, s1).unwrap();
        doc.append(root, s2).unwrap();
        let (x, y) = (by_id(&doc, "x"), by_id(&doc, "y"));
        assert!(doc.assigned_nodes(s1).is_empty());
        doc.assign(s1, &[y, x]);
        assert_eq!(doc.assigned_nodes(s1), [y, x]);
        assert_eq!(doc.assigned_slot(x), Some(s1));
        // Assigning to another slot takes them from the first.
        doc.assign(s2, &[x]);
        assert_eq!(doc.assigned_nodes(s1), [y]);
        assert_eq!(doc.assigned_slot(x), Some(s2));
        // Only the host's children count.
        doc.remove(y);
        assert!(doc.assigned_nodes(s1).is_empty());
        #[cfg(debug_assertions)]
        doc.check_tree();
    }

    #[test]
    fn attach_shadow_as_the_dom_has_it() {
        let mut doc = parse(
            "<div id=d><template shadowrootmode=open>x</template></div><p id=p></p>\
             <progress id=g></progress><my-el id=c></my-el>",
            false,
        );
        let (d, p, g, c) = (
            by_id(&doc, "d"),
            by_id(&doc, "p"),
            by_id(&doc, "g"),
            by_id(&doc, "c"),
        );
        assert_eq!(
            doc.attach_shadow(g, ShadowRoot::open()),
            Err(EditError::NotAShadowHost)
        );
        assert!(doc.attach_shadow(c, ShadowRoot::closed()).is_ok());
        let root = doc.attach_shadow(p, ShadowRoot::open()).unwrap();
        assert_eq!(
            doc.attach_shadow(p, ShadowRoot::open()),
            Err(EditError::NotAShadowHost)
        );
        let text = doc.create_text("t");
        assert_eq!(
            doc.attach_shadow(text, ShadowRoot::open()),
            Err(EditError::NotAShadowHost)
        );
        // A declared root: a mismatched mode is refused, a matching one
        // gets it back emptied, once.
        let declared = doc.shadow_root(d).unwrap();
        assert_eq!(
            doc.attach_shadow(d, ShadowRoot::closed()),
            Err(EditError::NotAShadowHost)
        );
        assert_eq!(doc.attach_shadow(d, ShadowRoot::open()), Ok(declared));
        assert_eq!(doc.first_child(declared), None);
        assert_eq!(
            doc.attach_shadow(d, ShadowRoot::open()),
            Err(EditError::NotAShadowHost)
        );
        // Shadow roots are never children, and a host can't go into its
        // own shadow tree.
        let div = doc.create_element(Ns::Html, "div");
        assert_eq!(doc.append(div, root), Err(EditError::NotInsertable));
        doc.append(root, div).unwrap();
        assert_eq!(doc.append(div, p), Err(EditError::Cycle));
        assert_eq!(doc.append(root, p), Err(EditError::Cycle));
        #[cfg(debug_assertions)]
        doc.check_tree();
    }

    #[test]
    fn only_documents_and_asked_for_fragments_get_declarative_roots() {
        let html = "<div><template shadowrootmode=open><b></b></template></div>";
        let hosts = |doc: &Document| {
            (0..doc.len() as NodeId)
                .filter(|&n| doc.shadow_root(n).is_some())
                .count()
        };
        assert_eq!(hosts(&parse(html, false)), 1);
        assert_eq!(hosts(&parse_fragment(html, Ns::Html, "div", false)), 0);
        let on = Options {
            declarative_shadow_roots: Some(true),
            ..Options::default()
        };
        let off = Options {
            declarative_shadow_roots: Some(false),
            ..Options::default()
        };
        assert_eq!(
            hosts(&parse_fragment_with(html, Ns::Html, "div", on).unwrap()),
            1
        );
        assert_eq!(hosts(&parse_with(html, off).unwrap()), 0);
        // The context element is never the host: it isn't in the result.
        let doc = parse_fragment_with(
            "<template shadowrootmode=open></template>",
            Ns::Html,
            "div",
            on,
        )
        .unwrap();
        assert_eq!(hosts(&doc), 0);
        assert_eq!(
            doc.inner_html(0),
            "<template shadowrootmode=\"open\"></template>"
        );
        // Nor the html element.
        let doc = parse("<template shadowrootmode=open></template>", false);
        assert_eq!(hosts(&doc), 0);
    }

    #[test]
    fn a_shadow_root_is_built_alike_in_pieces() {
        let html = "<div id=h><template shadowrootmode=closed shadowrootserializable>\
                    <p>a<b>b</template>c</div><span><template shadowrootmode=open>x</template></span>";
        let options = Options {
            spans: true,
            ..Options::default()
        };
        let whole = parse_with(html, options).unwrap();
        let dump = |doc: &Document| {
            doc.serialize_with(
                0,
                &SerializeOptions {
                    html5test: true,
                    ..SerializeOptions::default()
                },
            )
        };
        assert!(dump(&whole).contains("#shadow-root (closed serializable)"));
        for size in 1..8 {
            let mut parser = Parser::new(options);
            for piece in html.as_bytes().chunks(size) {
                parser.feed(std::str::from_utf8(piece).unwrap());
            }
            let doc = parser.finish().unwrap();
            #[cfg(debug_assertions)]
            doc.check_tree();
            assert_eq!(dump(&doc), dump(&whole));
        }
        let root = whole.shadow_root(by_id(&whole, "h")).unwrap();
        let p = whole.first_child(root).unwrap();
        let at = html.find("<p>").unwrap();
        assert_eq!(whole.span(p), Some(at..at + 3));
    }

    #[test]
    fn get_html_writes_the_roots_asked_for() {
        let doc = parse(
            "<div id=a><template shadowrootmode=open shadowrootserializable>\
               <span id=b><template shadowrootmode=closed>in</template></span>\
             </template>light</div>",
            false,
        );
        let a = by_id(&doc, "a");
        assert_eq!(doc.outer_html(a), "<div id=\"a\">light</div>");
        let serializable = SerializeOptions {
            serializable_shadow_roots: true,
            ..SerializeOptions::default()
        };
        assert_eq!(
            doc.serialize_with(a, &serializable),
            "<div id=\"a\"><template shadowrootmode=\"open\" shadowrootserializable=\"\">\
             <span id=\"b\"></span></template>light</div>"
        );
        let b = by_id(&doc, "b");
        let inner = doc.shadow_root(b).unwrap();
        let listed = SerializeOptions {
            serializable_shadow_roots: true,
            shadow_roots: vec![inner],
            ..SerializeOptions::default()
        };
        assert_eq!(
            doc.serialize_with(a, &listed),
            "<div id=\"a\"><template shadowrootmode=\"open\" shadowrootserializable=\"\">\
             <span id=\"b\"><template shadowrootmode=\"closed\">in</template></span></template>\
             light</div>"
        );
        // A shadow root's own markup is its children's.
        let outer = doc.shadow_root(a).unwrap();
        assert_eq!(doc.inner_html(outer), "<span id=\"b\"></span>");
        // What was written parses back to the same tree.
        let again = parse(&doc.serialize_with(0, &listed), false);
        let b_again = by_id(&again, "b");
        let listed_again = SerializeOptions {
            shadow_roots: vec![again.shadow_root(b_again).unwrap()],
            ..listed.clone()
        };
        assert_eq!(
            again.serialize_with(0, &listed_again),
            doc.serialize_with(0, &listed)
        );
        // Pretty-printing keeps the template whole.
        let pretty = SerializeOptions {
            pretty: true,
            ..listed
        };
        let printed = doc.serialize_with(a, &pretty);
        assert!(printed.contains("<template shadowrootmode=\"closed\">in</template>"));
    }

    #[test]
    fn host_selectors_match_inside_the_shadow_tree_only() {
        let doc = parse(
            "<section class=ctx><div id=host class=h>\
               <template shadowrootmode=open><p id=top></p><div id=d><p id=deep></p></div>\
               <slot id=s></slot></template>\
               <span id=l class=l></span><i id=other></i></div></section>",
            false,
        );
        let host = by_id(&doc, "host");
        let root = doc.shadow_root(host).unwrap();
        let select = |css: &str, scope: NodeId| {
            ids(&doc, &SelectorList::parse(css).unwrap().select(&doc, scope))
        };
        let matches = |css: &str, el: &str, scope: Option<NodeId>| {
            SelectorList::parse(css)
                .unwrap()
                .matches(&doc, by_id(&doc, el), scope)
        };
        // The host stands in for the root.
        assert_eq!(select(":host > p", root), ["top"]);
        assert_eq!(select(":host p", root), ["top", "deep"]);
        assert_eq!(select(":host(.h) > *", root), ["top", "d", "s"]);
        assert!(select(":host(.nope) p", root).is_empty());
        assert_eq!(select(":host-context(.ctx) p", root), ["top", "deep"]);
        assert!(select(":host-context(.nope) p", root).is_empty());
        // Featureless: nothing but :host matches the host, and it has no
        // parent or siblings here.
        assert!(matches(":host", "host", Some(root)));
        assert!(matches(":is(:host(div))", "host", Some(root)));
        assert!(!matches("div", "host", Some(root)));
        assert!(!matches("*", "host", Some(root)));
        assert!(select("section p", root).is_empty());
        assert!(select("section :host p", root).is_empty());
        // From an element inside, the tree is its own.
        assert!(matches(":host > p", "top", None));
        // Outside the shadow tree :host is nothing, and select from the
        // document doesn't go into shadow roots.
        assert!(!matches(":host", "host", None));
        assert!(select("p", 0).is_empty());
        // ::slotted(): elements assigned to a slot in this tree.
        assert!(matches("::slotted(.l)", "l", Some(root)));
        assert!(matches("slot::slotted(span)", "l", Some(root)));
        assert!(matches(":host > slot::slotted(*)", "l", Some(root)));
        assert!(!matches("::slotted(i)", "l", Some(root)));
        assert!(!matches("p::slotted(span)", "l", Some(root)));
        assert!(!matches("::slotted(span)", "l", None));
        // Pseudo-elements never come out of select.
        assert!(select("::slotted(*)", root).is_empty());
        // :host()'s argument sees the host as it is outside, light
        // children and all.
        assert!(matches(":host(:has(> span.l))", "host", Some(root)));
        assert!(!matches(":host(:has(> p))", "host", Some(root)));
    }

    #[test]
    fn host_and_slotted_selectors_parse_with_compound_arguments() {
        for ok in [
            ":host",
            ":host(div.x)",
            ":host(:is(.a, .b + .c))",
            ":host-context(body)",
            "::slotted(span)",
            "slot::slotted(*)::before",
            ":not(:host)",
        ] {
            assert!(SelectorList::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            ":host(div p)",
            ":host(:not(.a .b))",
            ":host-context(a > b)",
            "::slotted(a b)",
            "::slotted(span).x",
            "::slotted(span) p",
            ":host-context",
        ] {
            assert!(SelectorList::parse(bad).is_err(), "{bad}");
        }
        let spec = |s: &str| SelectorList::parse(s).unwrap().specificity();
        assert_eq!(spec(":host"), (0, 1, 0));
        assert_eq!(spec(":host(div.x)"), (0, 2, 1));
        assert_eq!(spec("::slotted(.x)"), (0, 1, 1));
    }
}
