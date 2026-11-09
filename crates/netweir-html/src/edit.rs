//! Changing a parsed document: new nodes, moves, removals and attributes.
//! A removed node stays in the document's arena, detached, and can be
//! inserted again.

use crate::dom::{Attr, AttrNs, Data, Document, Element, NONE, Name, NodeId, Ns, Span};
use crate::tags::Tag;

/// Why an edit was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// The node would end up inside itself.
    Cycle,
    /// The parent can't have children: text, a comment, a doctype or a
    /// processing instruction.
    NotAParent,
    /// `before` isn't a child of the parent.
    NotAChild,
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            EditError::Cycle => "a node can't be inserted into itself or its descendants",
            EditError::NotAParent => "only an element, a document or a fragment can have children",
            EditError::NotAChild => "the node to insert before isn't a child of the parent",
        })
    }
}

impl std::error::Error for EditError {}

impl Document {
    /// A new element, not yet in the tree. HTML names are lowercased, as
    /// `createElement` does.
    pub fn create_element(&mut self, ns: Ns, name: &str) -> NodeId {
        let (tag, name) = self.name_element(ns, name);
        let at = self.attrs.len() as u32;
        let contents = if ns == Ns::Html && tag == Tag::Template {
            self.add(Data::Fragment)
        } else {
            NONE
        };
        self.add(Data::Element(Element {
            ns,
            tag,
            name,
            attrs: Span { start: at, end: at },
            contents,
            html_integration: false,
        }))
    }

    pub fn create_text(&mut self, text: &str) -> NodeId {
        let span = self.push_stable(text);
        self.add(Data::Text(span))
    }

    pub fn create_cdata(&mut self, text: &str) -> NodeId {
        let span = self.push_stable(text);
        self.add(Data::Cdata(span))
    }

    pub fn create_comment(&mut self, text: &str) -> NodeId {
        let span = self.push_stable(text);
        self.add(Data::Comment(span))
    }

    pub fn create_processing_instruction(&mut self, target: &str, data: &str) -> NodeId {
        let target = self.push_stable(target);
        let data = self.push_stable(data);
        self.add(Data::ProcessingInstruction { target, data })
    }

    /// Moves `child` to be the last child of `parent`.
    pub fn append(&mut self, parent: NodeId, child: NodeId) -> Result<(), EditError> {
        self.insert_before(parent, child, None)
    }

    /// Moves `child` into `parent`, before `before`, or last for `None`.
    pub fn insert_before(
        &mut self,
        parent: NodeId,
        child: NodeId,
        before: Option<NodeId>,
    ) -> Result<(), EditError> {
        if !matches!(
            self.nodes[parent as usize].data,
            Data::Element(_) | Data::Document | Data::Fragment
        ) {
            return Err(EditError::NotAParent);
        }
        if let Some(b) = before
            && self.parent(b) != Some(parent)
        {
            return Err(EditError::NotAChild);
        }
        let mut at = Some(parent);
        while let Some(a) = at {
            if a == child {
                return Err(EditError::Cycle);
            }
            at = self.parent(a);
        }
        if before == Some(child) {
            return Ok(());
        }
        self.detach(child);
        self.insert(parent, child, before.unwrap_or(NONE));
        Ok(())
    }

    /// Moves every template's contents in under the template element
    /// itself, as lxml and Beautiful Soup show them and scrapers expect:
    /// selectors and walks then see them. The contents are left empty.
    pub fn adopt_template_contents(&mut self) {
        for id in 0..self.nodes.len() as NodeId {
            let Data::Element(e) = self.nodes[id as usize].data else {
                continue;
            };
            if e.ns != Ns::Html || e.tag != Tag::Template || e.contents == NONE {
                continue;
            }
            while let Some(child) = self.first_child(e.contents) {
                self.detach(child);
                self.insert(id, child, NONE);
            }
        }
    }

    /// Takes `id` out of the tree. It stays usable, and can be inserted
    /// again.
    pub fn remove(&mut self, id: NodeId) {
        self.detach(id);
    }

    /// Sets an attribute, replacing one of the same name. HTML elements'
    /// attribute names are lowercased, as `setAttribute` does; SVG's and
    /// MathML's keep the case they're given.
    pub fn set_attr(&mut self, id: NodeId, name: &str, value: &str) {
        let Some(e) = self.element(id) else { return };
        let ns = e.ns;
        let given = name;
        let name = stored_attr_name(ns, given);
        let value = self.push_stable(value);
        let name_span = self.push_stable(&name);
        self.rewrite_attrs(id, |doc, attrs| {
            // By the name as written, so viewBox finds a parsed viewBox, or
            // as stored, so no two attributes are ever written alike.
            match attrs
                .iter_mut()
                .find(|a| a.ns == AttrNs::None && doc.is_attr(ns, a, &name, given))
            {
                Some(a) => a.value = value,
                None => attrs.push(Attr {
                    ns: AttrNs::None,
                    name: name_span,
                    value,
                }),
            }
            true
        });
    }

    /// Adds an attribute as given, after the others, without replacing one
    /// of the same name: for building XML, where the caller has already
    /// checked names.
    pub fn add_attr(&mut self, id: NodeId, ns: AttrNs, name: &str, value: &str) {
        let name = self.push_stable(name);
        let value = self.push_stable(value);
        let attr = Attr { ns, name, value };
        let len = self.attrs.len() as u32;
        // An element whose attributes are the last in the list, as while
        // it's being built, grows in place: a clone sharing the range keeps
        // its own end, so it's unchanged. Anything else goes through a copy.
        if let Data::Element(e) = &mut self.nodes[id as usize].data
            && e.attrs.end == len
        {
            e.attrs.end += 1;
            self.attrs.push(attr);
            return;
        }
        self.rewrite_attrs(id, |_, attrs| {
            attrs.push(attr);
            true
        });
    }

    /// Replaces all of the element's attributes at once.
    pub fn set_attrs(&mut self, id: NodeId, attrs: &[(AttrNs, &str, &str)]) {
        if self.element(id).is_none() {
            return;
        }
        let start = self.attrs.len() as u32;
        for &(ns, name, value) in attrs {
            let name = self.push_stable(name);
            let value = self.push_stable(value);
            self.attrs.push(Attr { ns, name, value });
        }
        let end = self.attrs.len() as u32;
        if let Data::Element(e) = &mut self.nodes[id as usize].data {
            e.attrs = Span { start, end };
        }
    }

    /// Renames the element: lowercased for HTML, as given for the rest.
    /// An element renamed to `template` gets empty contents, as a new one
    /// does. A template renamed to anything else gives its contents up as
    /// ordinary children, ahead of any it has, so its markup stays the same
    /// and nothing in them is lost.
    pub fn rename(&mut self, id: NodeId, name: &str) {
        let Some(e) = self.element(id).copied() else {
            return;
        };
        let (tag, new) = self.name_element(e.ns, name);
        let template = e.ns == Ns::Html && tag == Tag::Template;
        let contents = if template && e.contents == NONE {
            self.add(Data::Fragment)
        } else if !template && e.contents != NONE {
            let first = self.first_child(id).unwrap_or(NONE);
            while let Some(child) = self.first_child(e.contents) {
                self.detach(child);
                self.insert(id, child, first);
            }
            NONE
        } else {
            e.contents
        };
        if let Data::Element(e) = &mut self.nodes[id as usize].data {
            e.tag = tag;
            e.name = new;
            e.contents = contents;
        }
    }

    /// The tag and stored name of an element called `name` in `ns`. HTML
    /// names are lowercased; foreign ones keep their case.
    fn name_element(&mut self, ns: Ns, name: &str) -> (Tag, Name) {
        let lower = name.to_ascii_lowercase();
        match ns {
            Ns::Html => {
                let tag = Tag::from_name(&lower);
                let name = if tag == Tag::Other {
                    Name::Span(self.push_stable(&lower))
                } else {
                    Name::Tag
                };
                (tag, name)
            }
            // An XML element is never an HTML one, whatever its name.
            Ns::Xml => (Tag::Other, Name::Span(self.push_stable(name))),
            _ => {
                let tag = Tag::from_name(&lower);
                let name = if tag != Tag::Other && tag.name() == name {
                    Name::Tag
                } else {
                    Name::Span(self.push_stable(name))
                };
                (tag, name)
            }
        }
    }

    /// Renames the element's `index`th attribute, and sets its namespace.
    pub fn rename_attr(&mut self, id: NodeId, index: usize, ns: AttrNs, name: &str) {
        let name = self.push_stable(name);
        self.rewrite_attrs(id, |_, attrs| match attrs.get_mut(index) {
            Some(a) => {
                a.ns = ns;
                a.name = name;
                true
            }
            None => false,
        });
    }

    /// Removes the element's `index`th attribute; false if there's none.
    pub fn remove_attr_at(&mut self, id: NodeId, index: usize) -> bool {
        self.rewrite_attrs(id, |_, attrs| {
            if index < attrs.len() {
                attrs.remove(index);
                true
            } else {
                false
            }
        })
    }

    /// Removes an attribute by name; false if there wasn't one.
    pub fn remove_attr(&mut self, id: NodeId, name: &str) -> bool {
        let Some(e) = self.element(id) else {
            return false;
        };
        let (ns, given) = (e.ns, name);
        let name = stored_attr_name(ns, given);
        self.rewrite_attrs(id, |doc, attrs| {
            let before = attrs.len();
            attrs.retain(|a| !doc.is_attr(ns, a, &name, given));
            attrs.len() != before
        })
    }

    /// Whether `a`, on an element in `ns`, is the attribute code calls
    /// `given`, stored as `stored`: by the name as stored, or on a foreign
    /// element by the name as written, so viewBox finds a parsed viewBox
    /// and no two attributes are ever written alike.
    fn is_attr(&self, ns: Ns, a: &Attr, stored: &str, given: &str) -> bool {
        self.str(a.name) == stored || (ns != Ns::Html && self.written_name(ns, a) == given)
    }

    /// Changes an element's attributes through a copy at the end of the
    /// list, never in place: the parser's clones of a formatting element
    /// share their original's attributes.
    fn rewrite_attrs(
        &mut self,
        id: NodeId,
        change: impl FnOnce(&Document, &mut Vec<Attr>) -> bool,
    ) -> bool {
        let Some(e) = self.element(id) else {
            return false;
        };
        let mut attrs = self.attrs[e.attrs.start as usize..e.attrs.end as usize].to_vec();
        if !change(self, &mut attrs) {
            return false;
        }
        let start = self.attrs.len() as u32;
        self.attrs.extend(attrs);
        let end = self.attrs.len() as u32;
        if let Data::Element(e) = &mut self.nodes[id as usize].data {
            e.attrs = Span { start, end };
        }
        true
    }
}

/// An attribute name set from code, as the parser would store it: on an
/// HTML element lowercased, as `setAttribute` does. On SVG and MathML a
/// name whose case the writer puts back (viewBox, definitionURL) is
/// lowercased too, and any other keeps the case it's given, as does an XML
/// one.
fn stored_attr_name(ns: Ns, given: &str) -> String {
    let lower = given.to_ascii_lowercase();
    let restored = match ns {
        Ns::Html => true,
        Ns::Svg => crate::tree::svg_attr_name(&lower) == Some(given),
        Ns::MathMl => lower == "definitionurl" && given == "definitionURL",
        Ns::Xml => false,
    };
    if restored { lower } else { given.to_string() }
}

#[cfg(test)]
mod tests {
    use super::EditError;
    use crate::{Ns, parse_fragment};

    #[test]
    fn nodes_can_be_made_moved_and_removed() {
        let mut doc = parse_fragment("<p>a</p><p>b</p>", Ns::Html, "div", false);
        let (first, second) = {
            let mut c = doc.children(0);
            (c.next().unwrap(), c.next().unwrap())
        };
        let em = doc.create_element(Ns::Html, "EM");
        let t = doc.create_text("x & y");
        doc.append(em, t).unwrap();
        doc.insert_before(first, em, doc.first_child(first))
            .unwrap();
        assert_eq!(doc.inner_html(0), "<p><em>x &amp; y</em>a</p><p>b</p>");
        doc.append(second, em).unwrap();
        assert_eq!(doc.inner_html(0), "<p>a</p><p>b<em>x &amp; y</em></p>");
        doc.remove(second);
        assert_eq!(doc.inner_html(0), "<p>a</p>");
        doc.append(0, second).unwrap();
        assert_eq!(doc.inner_html(0), "<p>a</p><p>b<em>x &amp; y</em></p>");
    }

    #[test]
    fn set_attr_keeps_the_case_of_foreign_names() {
        let mut doc = parse_fragment(
            "<svg viewBox=\"0 0 1 1\"></svg><math definitionURL=\"a\"></math><p></p>",
            Ns::Html,
            "div",
            false,
        );
        let kids: Vec<_> = doc.children(doc.root()).collect();
        let (svg, math, p) = (kids[0], kids[1], kids[2]);
        doc.set_attr(svg, "dataFoo", "x");
        doc.set_attr(svg, "viewBox", "0 0 2 2");
        doc.set_attr(math, "mathVariant", "bold");
        doc.set_attr(math, "definitionURL", "b");
        doc.set_attr(p, "DATA-X", "y");
        assert_eq!(
            doc.outer_html(svg),
            "<svg viewBox=\"0 0 2 2\" dataFoo=\"x\"></svg>"
        );
        assert_eq!(
            doc.outer_html(math),
            "<math definitionURL=\"b\" mathVariant=\"bold\"></math>"
        );
        assert_eq!(doc.outer_html(p), "<p data-x=\"y\"></p>");
        // A lowercase viewbox finds the same attribute: never two written alike.
        doc.set_attr(svg, "viewbox", "0 0 3 3");
        assert_eq!(
            doc.outer_html(svg),
            "<svg viewBox=\"0 0 3 3\" dataFoo=\"x\"></svg>"
        );
        // And it's set again, not added, when the case matches what was given.
        doc.set_attr(svg, "dataFoo", "z");
        assert_eq!(
            doc.outer_html(svg),
            "<svg viewBox=\"0 0 3 3\" dataFoo=\"z\"></svg>"
        );
        // What set_attr sets, remove_attr finds by the same name.
        assert!(doc.remove_attr(svg, "dataFoo"));
        assert!(doc.remove_attr(svg, "viewBox"));
        assert!(doc.remove_attr(math, "mathVariant"));
        assert!(!doc.remove_attr(svg, "dataFoo"));
        assert_eq!(doc.outer_html(svg), "<svg></svg>");
        assert_eq!(doc.outer_html(math), "<math definitionURL=\"b\"></math>");
    }

    #[test]
    fn renaming_to_or_from_template_adds_or_gives_up_its_contents() {
        let mut doc = parse_fragment("<template><b>x</b>y</template>", Ns::Html, "div", false);
        let t = doc.first_child(doc.root()).unwrap();
        doc.rename(t, "div");
        assert_eq!(doc.template_contents(t), None);
        assert_eq!(doc.outer_html(t), "<div><b>x</b>y</div>");
        doc.rename(t, "template");
        let contents = doc.template_contents(t).expect("new contents");
        assert_eq!(doc.first_child(contents), None);
        assert_eq!(doc.outer_html(t), "<template><b>x</b>y</template>");
        // Contents go ahead of children a template already has, as written.
        let p = doc.create_element(Ns::Html, "p");
        doc.append(contents, p).unwrap();
        doc.rename(t, "section");
        assert_eq!(doc.outer_html(t), "<section><p></p><b>x</b>y</section>");
        // Renaming a template to template leaves its contents be.
        let u = doc.create_element(Ns::Html, "template");
        let kept = doc.template_contents(u);
        doc.rename(u, "TEMPLATE");
        assert_eq!(doc.template_contents(u), kept);
    }

    #[test]
    fn a_renamed_foreign_element_takes_its_new_names_tag() {
        let mut doc = parse_fragment("", Ns::Html, "div", false);
        let svg_title = doc.create_element(Ns::Svg, "title");
        let title_tag = doc.known_tag(svg_title);
        assert!(title_tag.is_some());
        let n = doc.create_element(Ns::Svg, "rect");
        doc.rename(n, "title");
        assert_eq!(doc.known_tag(n), title_tag);
        doc.rename(n, "rect");
        assert_eq!(doc.known_tag(n), None);
    }

    #[test]
    fn template_contents_can_become_children() {
        let mut doc = parse_fragment("<template><b>x</b></template>", Ns::Html, "div", false);
        let t = doc.first_child(0).unwrap();
        assert_eq!(doc.first_child(t), None);
        doc.adopt_template_contents();
        let b = doc.first_child(t).unwrap();
        assert_eq!(doc.outer_html(b), "<b>x</b>");
        // Written the same before and after.
        assert_eq!(doc.inner_html(0), "<template><b>x</b></template>");
    }

    /// Netweir reads strings through a shared borrow while an edit may
    /// run under its own lock, so a string an edit didn't touch must stay
    /// exactly where it was.
    #[test]
    fn edits_never_move_strings_already_read() {
        // Values with references: the parser copies those into its buffer,
        // the one place that could move.
        let mut doc = parse_fragment("<a href='x&amp;y'>t&amp;u</a>", Ns::Html, "div", false);
        let a = doc.first_child(0).unwrap();
        let text = doc.first_child(a).unwrap();
        let href = doc.attrs(a).next().unwrap().2.as_ptr();
        let data = match doc.kind(text) {
            crate::Kind::Text(t) => t.as_ptr(),
            _ => unreachable!(),
        };
        for i in 0..10_000 {
            let n = doc.create_element(Ns::Html, &format!("custom-{i}"));
            doc.set_attr(n, "data-long", &"y".repeat(100));
            let t = doc.create_text(&"z".repeat(100));
            doc.append(n, t).unwrap();
            doc.append(0, n).unwrap();
        }
        doc.set_attr(a, "title", "new");
        assert_eq!(doc.attrs(a).next().unwrap().2.as_ptr(), href);
        assert!(matches!(doc.kind(text), crate::Kind::Text(t) if t.as_ptr() == data));
    }

    #[test]
    fn xml_is_built_as_written_and_matched_as_netweir_did() {
        use crate::css::SelectorList;
        use crate::{AttrNs, Document};
        let mut doc = Document::empty();
        let feed = doc.create_element(Ns::Xml, "Feed");
        doc.append(0, feed).unwrap();
        let entry = doc.create_element(Ns::Xml, "dc:Creator");
        doc.add_attr(entry, AttrNs::None, "ID", "up");
        doc.add_attr(entry, AttrNs::None, "id", "lo");
        doc.add_attr(entry, AttrNs::Host(3), "x:href", "/p");
        doc.append(feed, entry).unwrap();
        let cdata = doc.create_cdata("<raw>");
        doc.append(entry, cdata).unwrap();
        let found = |css: &str| SelectorList::parse(css).unwrap().select(&doc, 0).len();
        assert_eq!(found("feed creator"), 1);
        assert_eq!(found("[id=lo]"), 1);
        assert_eq!(found("[id=up]"), 0);
        assert_eq!(found("#lo"), 1);
        assert_eq!(found("#up"), 0);
        assert_eq!(found("[href^='/']"), 1);
        assert_eq!(
            doc.outer_html(entry),
            "<dc:Creator ID=\"up\" id=\"lo\" x:href=\"/p\"><![CDATA[<raw>]]></dc:Creator>"
        );
        doc.rename(entry, "Creator");
        doc.rename_attr(entry, 2, AttrNs::None, "href");
        assert!(doc.remove_attr_at(entry, 0));
        assert_eq!(
            doc.outer_html(entry),
            "<Creator id=\"lo\" href=\"/p\"><![CDATA[<raw>]]></Creator>"
        );
    }

    #[test]
    fn a_node_cant_go_inside_itself() {
        let mut doc = parse_fragment("<div><span></span></div>", Ns::Html, "body", false);
        let div = doc.first_child(0).unwrap();
        let span = doc.first_child(div).unwrap();
        assert_eq!(doc.append(span, div), Err(EditError::Cycle));
        assert_eq!(doc.append(div, div), Err(EditError::Cycle));
        let text = doc.create_text("t");
        assert_eq!(doc.append(text, span), Err(EditError::NotAParent));
        assert_eq!(
            doc.insert_before(span, text, Some(div)),
            Err(EditError::NotAChild)
        );
    }

    #[test]
    fn attributes_can_be_set_replaced_and_removed() {
        let mut doc = parse_fragment("<a href=x>a</a><a href=x>b</a>", Ns::Html, "div", false);
        let a = doc.first_child(0).unwrap();
        doc.set_attr(a, "HREF", "y");
        doc.set_attr(a, "title", "t");
        assert_eq!(doc.outer_html(a), "<a href=\"y\" title=\"t\">a</a>");
        assert!(doc.remove_attr(a, "href"));
        assert!(!doc.remove_attr(a, "href"));
        assert_eq!(doc.outer_html(a), "<a title=\"t\">a</a>");
        let b = doc.next_sibling(a).unwrap();
        assert_eq!(doc.outer_html(b), "<a href=\"x\">b</a>");
    }

    /// The parser's clones of a formatting element share its attributes
    /// until one of them is changed.
    #[test]
    fn changing_a_reconstructed_clone_leaves_the_original_alone() {
        let mut doc = parse_fragment("<p><b class=x>a</p>t", Ns::Html, "body", false);
        let p = doc.first_child(0).unwrap();
        let original = doc.first_child(p).unwrap();
        let clone = doc.next_sibling(p).unwrap();
        assert_eq!(doc.outer_html(clone), "<b class=\"x\">t</b>");
        doc.set_attr(clone, "class", "y");
        assert_eq!(doc.outer_html(original), "<b class=\"x\">a</b>");
        assert_eq!(doc.outer_html(clone), "<b class=\"y\">t</b>");
    }

    #[test]
    fn foreign_names_keep_their_case() {
        let mut doc = parse_fragment("<svg></svg>", Ns::Svg, "svg", false);
        let g = doc.create_element(Ns::Svg, "linearGradient");
        doc.set_attr(g, "gradientUnits", "u");
        doc.append(0, g).unwrap();
        assert!(
            doc.inner_html(0)
                .contains("<linearGradient gradientUnits=\"u\"></linearGradient>")
        );
    }
}
