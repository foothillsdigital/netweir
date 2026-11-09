//! The HTML fragment serialization algorithm, section 13.3 of the HTML
//! Standard: `innerHTML` and `outerHTML`.

use crate::dom::{Data, Document, Kind, NodeId, Ns};
use crate::tags::Tag;

/// Elements with no end tag and no contents when serialized.
fn void(e: &crate::dom::Element) -> bool {
    use Tag::*;
    e.ns == Ns::Html
        && matches!(
            e.tag,
            Area | Base
                | Basefont
                | Bgsound
                | Br
                | Col
                | Embed
                | Frame
                | Hr
                | Img
                | Input
                | Keygen
                | Link
                | Meta
                | Param
                | Source
                | Track
                | Wbr
        )
}

impl Document {
    /// The markup for `id`'s children, as `innerHTML` returns it.
    pub fn inner_html(&self, id: NodeId) -> String {
        let mut out = String::new();
        self.children_html(id, &mut out);
        out
    }

    /// The markup for `id` and its children, as `outerHTML` returns it.
    pub fn outer_html(&self, id: NodeId) -> String {
        let mut out = String::new();
        self.node_html(id, &mut out);
        out
    }

    /// Writes `id`'s children; with a stack of its own rather than
    /// recursion, so a tree as deep as any page makes can't overflow it.
    fn children_html(&self, id: NodeId, out: &mut String) {
        if self.element(id).is_some_and(void) {
            return;
        }
        let mut stack = Vec::new();
        self.push_children(id, &mut stack);
        self.write(stack, out);
    }

    fn node_html(&self, id: NodeId, out: &mut String) {
        self.write(vec![(id, false)], out);
    }

    /// `id`'s children onto the stack, the first on top: a template's
    /// contents first, then any children it has, which it only has once
    /// adopt_template_contents has moved its contents in.
    fn push_children(&self, id: NodeId, stack: &mut Vec<(NodeId, bool)>) {
        let at = stack.len();
        stack.extend(self.children(id).map(|c| (c, false)));
        if let Some(e) = self.element(id)
            && e.ns == Ns::Html
            && e.tag == Tag::Template
            && e.contents != crate::dom::NONE
        {
            stack.extend(self.children(e.contents).map(|c| (c, false)));
        }
        stack[at..].reverse();
    }

    /// Writes what's on the stack: a node, or with `true` an element's end
    /// tag, once its children are written.
    fn write(&self, mut stack: Vec<(NodeId, bool)>, out: &mut String) {
        while let Some((id, end)) = stack.pop() {
            let data = &self.nodes[id as usize].data;
            if end {
                if let Data::Element(e) = data {
                    out.push_str("</");
                    out.push_str(self.element_name(e));
                    out.push('>');
                }
                continue;
            }
            match data {
                Data::Element(e) => {
                    self.start_tag(id, out);
                    if void(e) {
                        continue;
                    }
                    stack.push((id, true));
                    self.push_children(id, &mut stack);
                }
                Data::Text(span) => {
                    let text = self.str(*span);
                    if self.raw_text_parent(id) {
                        out.push_str(text);
                    } else {
                        escape(text, false, out);
                    }
                }
                Data::Comment(span) => {
                    out.push_str("<!--");
                    out.push_str(self.str(*span));
                    out.push_str("-->");
                }
                Data::ProcessingInstruction { target, data } => {
                    out.push_str("<?");
                    out.push_str(self.str(*target));
                    out.push(' ');
                    out.push_str(self.str(*data));
                    out.push_str("?>");
                }
                Data::Cdata(span) => {
                    out.push_str("<![CDATA[");
                    out.push_str(self.str(*span));
                    out.push_str("]]>");
                }
                Data::Doctype { name, .. } => {
                    out.push_str("<!DOCTYPE ");
                    out.push_str(self.str(*name));
                    out.push('>');
                }
                Data::Document | Data::Fragment => self.push_children(id, &mut stack),
            }
        }
    }

    fn start_tag(&self, id: NodeId, out: &mut String) {
        let Some(e) = self.element(id) else { return };
        out.push('<');
        out.push_str(self.element_name(e));
        // Names as written are the spec's serialized names:
        // `xlink:href`, `xml:lang`, `xmlns`.
        for (_, name, value) in self.attrs_written(id) {
            out.push(' ');
            out.push_str(name);
            out.push_str("=\"");
            escape(value, true, out);
            out.push('"');
        }
        out.push('>');
    }

    /// The node alone: an element's start tag, escaped text, a comment
    /// or a doctype; as `outer_html` writes it, without the children. A
    /// document or a template's contents is nothing but its children, so
    /// it gives an empty string.
    pub fn html_alone(&self, id: NodeId) -> String {
        let mut out = String::new();
        match self.kind(id) {
            Kind::Element { .. } => self.start_tag(id, &mut out),
            Kind::Document | Kind::Fragment => {}
            _ => self.node_html(id, &mut out),
        }
        out
    }

    fn raw_text_parent(&self, id: NodeId) -> bool {
        use Tag::*;
        let Some(e) = self.parent(id).and_then(|p| self.element(p)) else {
            return false;
        };
        e.ns == Ns::Html
            && (matches!(
                e.tag,
                Style | Script | Xmp | Iframe | Noembed | Noframes | Plaintext
            ) || (e.tag == Noscript && self.scripting))
    }

    /// The text of `id` and everything under it, in tree order, as
    /// `textContent` gives it.
    pub fn text(&self, id: NodeId) -> String {
        let mut out = String::new();
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            if let Kind::Text(t) = self.kind(n) {
                out.push_str(t);
            }
            let at = stack.len();
            stack.extend(self.children(n));
            stack[at..].reverse();
        }
        out
    }
}

/// The spec's escaping: `&`, NO-BREAK SPACE, `<` and `>` always, and `"`
/// in attribute values.
fn escape(s: &str, attribute: bool, out: &mut String) {
    let bytes = s.as_bytes();
    let mut from = 0;
    let mut i = 0;
    while i < bytes.len() {
        let replacement = match bytes[i] {
            b'&' => "&amp;",
            b'<' => "&lt;",
            b'>' => "&gt;",
            b'"' if attribute => "&quot;",
            0xC2 if bytes.get(i + 1) == Some(&0xA0) => "&nbsp;",
            _ => {
                i += 1;
                continue;
            }
        };
        out.push_str(&s[from..i]);
        out.push_str(replacement);
        i += if bytes[i] == 0xC2 { 2 } else { 1 };
        from = i;
    }
    out.push_str(&s[from..]);
}

#[cfg(test)]
mod tests {
    use crate::{Ns, parse, parse_fragment};

    fn body(html: &str) -> String {
        let doc = parse(html, false);
        let html_el = doc
            .children(0)
            .find(|&c| {
                doc.template_contents(c).is_none()
                    && matches!(doc.kind(c), crate::Kind::Element { .. })
            })
            .unwrap();
        let body = doc.children(html_el).nth(1).unwrap();
        doc.inner_html(body)
    }

    #[test]
    fn a_document_or_template_contents_alone_is_empty() {
        let doc = parse("<!DOCTYPE html><template><b>x</b></template><p>y", false);
        assert_eq!(doc.html_alone(doc.root()), "");
        let template = (0..doc.len() as crate::NodeId)
            .find_map(|n| doc.template_contents(n))
            .unwrap();
        assert_eq!(doc.html_alone(template), "");
        let fragment = parse_fragment("<b>x</b>", Ns::Html, "div", false);
        assert_eq!(fragment.html_alone(fragment.root()), "");
        // The rest are as before.
        let doctype = doc.children(doc.root()).next().unwrap();
        assert_eq!(doc.html_alone(doctype), "<!DOCTYPE html>");
    }

    #[test]
    fn a_document_round_trips() {
        let doc = parse(
            "<!DOCTYPE html><html><head><title>T</title></head><body><p class=\"a\">x</p></body></html>",
            false,
        );
        assert_eq!(
            doc.inner_html(0),
            "<!DOCTYPE html><html><head><title>T</title></head><body><p class=\"a\">x</p></body></html>"
        );
    }

    #[test]
    fn text_and_attributes_are_escaped() {
        assert_eq!(
            body("<p title='a\"b<c>&d'>1 &lt; 2 &amp; 3 &gt; 0&nbsp;!</p>"),
            "<p title=\"a&quot;b&lt;c&gt;&amp;d\">1 &lt; 2 &amp; 3 &gt; 0&nbsp;!</p>"
        );
    }

    #[test]
    fn raw_text_is_written_as_it_is() {
        assert_eq!(
            body("<body><script>if (a < b && c) {}</script>"),
            "<script>if (a < b && c) {}</script>"
        );
        assert_eq!(
            body("<body><style>a > b {}</style>"),
            "<style>a > b {}</style>"
        );
        assert_eq!(
            body("<body><noscript><b>x</b></noscript>"),
            "<noscript><b>x</b></noscript>"
        );
    }

    #[test]
    fn void_elements_have_no_end_tag() {
        assert_eq!(body("<br><img src=x><input>"), "<br><img src=\"x\"><input>");
        let doc = parse_fragment("<br>", Ns::Html, "div", false);
        let br = doc.children(0).next().unwrap();
        assert_eq!(doc.inner_html(br), "");
        assert_eq!(doc.outer_html(br), "<br>");
    }

    #[test]
    fn templates_comments_pis_and_foreign_elements() {
        assert_eq!(
            body(
                "<body><template><b>x</b></template><!--c--><?t d?><svg viewBox='0 0 1 1'><clipPath/></svg>"
            ),
            "<template><b>x</b></template><!--c--><?t d?><svg viewBox=\"0 0 1 1\"><clipPath></clipPath></svg>"
        );
        assert_eq!(
            body("<svg xlink:href=a xml:lang=en xmlns:xlink=b></svg>"),
            "<svg xlink:href=\"a\" xml:lang=\"en\" xmlns:xlink=\"b\"></svg>"
        );
    }

    #[test]
    fn text_content_joins_every_text_node() {
        let doc = parse("<p>a<b>b</b>c</p>", false);
        assert!(doc.text(0).ends_with("abc"));
    }
}

#[cfg(test)]
mod deep {
    /// Serializing recursed once per level and overflowed the stack on deep
    /// pages; Netweir parses on threads with 2 MB stacks.
    #[test]
    fn deep_trees_serialize_on_a_small_stack() {
        let html = "<div>".repeat(20_000);
        let out = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                let doc = crate::parse(&html, false);
                doc.inner_html(0).len()
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(
            out,
            "<html><head></head><body></body></html>".len() + 20_000 * 11
        );
    }
}
