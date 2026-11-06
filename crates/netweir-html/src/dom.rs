//! The parsed document: every node in one `Vec`, linked by index, and every
//! string in one buffer, so building it allocates a handful of times rather
//! than once per node, and dropping it frees a handful of blocks.

use crate::tags::Tag;

/// A node's index in its document; the document itself is node 0.
pub type NodeId = u32;

/// No node, in the links.
pub(crate) const NONE: NodeId = NodeId::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ns {
    Html,
    Svg,
    MathMl,
    /// An element of an XML document: its name is the qualified name as
    /// written, and its namespace is the host's to keep.
    Xml,
}

/// A namespaced attribute's namespace, from the spec's "adjust foreign
/// attributes" table; `None` for every ordinary attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrNs {
    None,
    XLink,
    Xml,
    Xmlns,
    /// An XML attribute in a namespace the host numbered; its name is the
    /// qualified name as written.
    Host(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quirks {
    No,
    Limited,
    Quirks,
}

/// A span's high bit says it's in the document's copy of its source, not
/// in the buffer of strings the parser had to make.
const IN_SOURCE: u32 = 1 << 31;

/// A span with this bit (and not IN_SOURCE) is a string an edit added:
/// `start` is its index in `extras`.
const IN_EXTRA: u32 = 1 << 30;

/// The most input a document can hold. Spans keep offsets in 30 bits, and
/// the parse buffer, which only holds copies of what the input says, stays
/// well under that.
pub const MAX_INPUT: usize = 1 << 29;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Span {
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Name {
    /// The tag's own lowercase name.
    Tag,
    /// An SVG name the parser fixed the case of, such as `clipPath`: its
    /// index in the table of them.
    Svg(u8),
    /// Any other name, in the document's buffer.
    Span(Span),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Attr {
    pub ns: AttrNs,
    pub name: Span,
    pub value: Span,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Element {
    pub ns: Ns,
    pub tag: Tag,
    pub name: Name,
    pub attrs: Span,
    /// A template's contents, a fragment node of its own.
    pub contents: NodeId,
    /// A MathML annotation-xml whose encoding makes it an HTML integration
    /// point.
    pub html_integration: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Data {
    Document,
    Fragment,
    Doctype {
        name: Span,
        public_id: Span,
        system_id: Span,
    },
    Element(Element),
    Text(Span),
    Comment(Span),
    ProcessingInstruction {
        target: Span,
        data: Span,
    },
    /// An XML CDATA section.
    Cdata(Span),
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Node {
    pub parent: NodeId,
    pub first_child: NodeId,
    pub last_child: NodeId,
    pub prev: NodeId,
    pub next: NodeId,
    pub data: Data,
}

/// A parsed HTML document, or a fragment.
pub struct Document {
    pub(crate) nodes: Vec<Node>,
    /// The input, after preprocessing. Text and attribute values that
    /// didn't need changing point into it rather than being copied.
    pub(crate) src: Box<str>,
    pub(crate) buf: String,
    /// Strings with an allocation each: what edits add, and text the
    /// parser had to keep growing away from the end of `buf`. Only the
    /// parse grows one, so once it's done none moves, and an edit never
    /// moves a string already handed out, as growing `buf` would.
    pub(crate) extras: Vec<String>,
    pub(crate) attrs: Vec<Attr>,
    pub(crate) quirks: Quirks,
    /// Whether it was parsed with scripting on, which decides how a
    /// noscript element's text serializes.
    pub(crate) scripting: bool,
}

/// What a node is, as [`Document::kind`] reports it.
#[derive(Debug, PartialEq, Eq)]
pub enum Kind<'a> {
    Document,
    Fragment,
    Doctype {
        name: &'a str,
        public_id: &'a str,
        system_id: &'a str,
    },
    Element {
        ns: Ns,
        name: &'a str,
    },
    Text(&'a str),
    Comment(&'a str),
    ProcessingInstruction {
        target: &'a str,
        data: &'a str,
    },
    Cdata(&'a str),
}

impl Document {
    pub(crate) fn new(root: Data, capacity: usize) -> Document {
        let mut nodes = Vec::with_capacity(capacity / 24 + 16);
        nodes.push(Node {
            parent: NONE,
            first_child: NONE,
            last_child: NONE,
            prev: NONE,
            next: NONE,
            data: root,
        });
        Document {
            nodes,
            src: Box::default(),
            buf: String::with_capacity(capacity / 8),
            extras: Vec::new(),
            attrs: Vec::with_capacity(capacity / 32 + 16),
            quirks: Quirks::No,
            scripting: false,
        }
    }

    /// An empty document to build into, as XML is: no source, no nodes
    /// but the document itself.
    pub fn empty() -> Document {
        Document::new(Data::Document, 0)
    }

    pub fn root(&self) -> NodeId {
        0
    }

    pub fn quirks(&self) -> Quirks {
        self.quirks
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.len() <= 1
    }

    pub(crate) fn str(&self, s: Span) -> &str {
        if s.start & IN_SOURCE != 0 {
            &self.src[(s.start & !IN_SOURCE) as usize..(s.end & !IN_SOURCE) as usize]
        } else if s.start & IN_EXTRA != 0 {
            &self.extras[(s.start & !IN_EXTRA) as usize]
        } else {
            &self.buf[s.start as usize..s.end as usize]
        }
    }

    /// A span for a string an edit adds. Unlike the parser's strings, it
    /// never moves, so a `&str` read before the edit stays good.
    pub(crate) fn push_stable(&mut self, s: &str) -> Span {
        let at = self.extras.len() as u32 | IN_EXTRA;
        self.extras.push(s.to_owned());
        Span { start: at, end: at }
    }

    /// Where `s` is in the source, if it's a slice of it.
    fn source_offset(&self, s: &str) -> Option<usize> {
        let base = self.src.as_ptr() as usize;
        let at = s.as_ptr() as usize;
        (at >= base && at + s.len() <= base + self.src.len()).then(|| at - base)
    }

    /// A span for `s`: into the source when it's a slice of it, which
    /// costs nothing, else a copy in the buffer.
    pub(crate) fn intern(&mut self, s: &str) -> Span {
        match self.source_offset(s) {
            Some(at) => Span {
                start: at as u32 | IN_SOURCE,
                end: (at + s.len()) as u32 | IN_SOURCE,
            },
            None => self.push_str(s),
        }
    }

    pub(crate) fn push_str(&mut self, s: &str) -> Span {
        let start = self.buf.len() as u32;
        self.buf.push_str(s);
        Span {
            start,
            end: self.buf.len() as u32,
        }
    }

    pub(crate) fn add(&mut self, data: Data) -> NodeId {
        let id = self.nodes.len() as NodeId;
        self.nodes.push(Node {
            parent: NONE,
            first_child: NONE,
            last_child: NONE,
            prev: NONE,
            next: NONE,
            data,
        });
        id
    }

    pub(crate) fn element(&self, id: NodeId) -> Option<&Element> {
        match &self.nodes[id as usize].data {
            Data::Element(e) => Some(e),
            _ => None,
        }
    }

    pub(crate) fn element_name(&self, e: &Element) -> &str {
        match e.name {
            Name::Tag => e.tag.name(),
            Name::Svg(i) => crate::tree::SVG_TAG_NAMES[i as usize].1,
            Name::Span(s) => self.str(s),
        }
    }

    /// Inserts `child`, which has no parent, into `parent` before `before`,
    /// or last when `before` is [`NONE`].
    pub(crate) fn insert(&mut self, parent: NodeId, child: NodeId, before: NodeId) {
        let prev = if before == NONE {
            self.nodes[parent as usize].last_child
        } else {
            self.nodes[before as usize].prev
        };
        {
            let c = &mut self.nodes[child as usize];
            c.parent = parent;
            c.prev = prev;
            c.next = before;
        }
        if prev == NONE {
            self.nodes[parent as usize].first_child = child;
        } else {
            self.nodes[prev as usize].next = child;
        }
        if before == NONE {
            self.nodes[parent as usize].last_child = child;
        } else {
            self.nodes[before as usize].prev = child;
        }
    }

    pub(crate) fn detach(&mut self, child: NodeId) {
        let Node {
            parent, prev, next, ..
        } = self.nodes[child as usize];
        if parent == NONE {
            return;
        }
        if prev == NONE {
            self.nodes[parent as usize].first_child = next;
        } else {
            self.nodes[prev as usize].next = next;
        }
        if next == NONE {
            self.nodes[parent as usize].last_child = prev;
        } else {
            self.nodes[next as usize].prev = prev;
        }
        let c = &mut self.nodes[child as usize];
        c.parent = NONE;
        c.prev = NONE;
        c.next = NONE;
    }

    /// Appends text to a text node, copying its old text to the end of the
    /// buffer first if something else has been written after it.
    /// Only the parser calls this: it may move the buffer.
    pub(crate) fn append_text(&mut self, id: NodeId, text: &str) {
        let Data::Text(span) = self.nodes[id as usize].data else {
            unreachable!("not a text node")
        };
        let in_source = span.start & IN_SOURCE != 0;
        let in_buf = span.start & (IN_SOURCE | IN_EXTRA) == 0;
        let span =
            if in_source && self.source_offset(text) == Some((span.end & !IN_SOURCE) as usize) {
                // The next slice of the source: the span just grows.
                Span {
                    start: span.start,
                    end: span.end + text.len() as u32,
                }
            } else if in_buf && span.end as usize == self.buf.len() {
                self.buf.push_str(text);
                Span {
                    start: span.start,
                    end: self.buf.len() as u32,
                }
            } else if span.start & IN_EXTRA != 0 {
                // Already a string of its own: grows in place.
                self.extras[(span.start & !IN_EXTRA) as usize].push_str(text);
                span
            } else {
                // Something was written after it: move it to a string of its
                // own, once, rather than copying it to the end of the buffer on
                // every append (repeated <body x> made that quadratic).
                let mut own = self.str(span).to_owned();
                own.push_str(text);
                let at = self.extras.len() as u32 | IN_EXTRA;
                self.extras.push(own);
                Span { start: at, end: at }
            };
        self.nodes[id as usize].data = Data::Text(span);
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        Some(self.nodes[id as usize].parent).filter(|&p| p != NONE)
    }

    pub fn first_child(&self, id: NodeId) -> Option<NodeId> {
        Some(self.nodes[id as usize].first_child).filter(|&c| c != NONE)
    }

    pub fn prev_sibling(&self, id: NodeId) -> Option<NodeId> {
        Some(self.nodes[id as usize].prev).filter(|&c| c != NONE)
    }

    pub fn last_child(&self, id: NodeId) -> Option<NodeId> {
        Some(self.nodes[id as usize].last_child).filter(|&c| c != NONE)
    }

    pub fn next_sibling(&self, id: NodeId) -> Option<NodeId> {
        Some(self.nodes[id as usize].next).filter(|&c| c != NONE)
    }

    pub fn children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        std::iter::successors(self.first_child(id), move |&c| self.next_sibling(c))
    }

    /// A template element's contents.
    pub fn template_contents(&self, id: NodeId) -> Option<NodeId> {
        self.element(id).map(|e| e.contents).filter(|&c| c != NONE)
    }

    pub fn kind(&self, id: NodeId) -> Kind<'_> {
        match &self.nodes[id as usize].data {
            Data::Document => Kind::Document,
            Data::Fragment => Kind::Fragment,
            Data::Doctype {
                name,
                public_id,
                system_id,
            } => Kind::Doctype {
                name: self.str(*name),
                public_id: self.str(*public_id),
                system_id: self.str(*system_id),
            },
            Data::Element(e) => Kind::Element {
                ns: e.ns,
                name: self.element_name(e),
            },
            Data::Text(s) => Kind::Text(self.str(*s)),
            Data::Comment(s) => Kind::Comment(self.str(*s)),
            Data::Cdata(s) => Kind::Cdata(self.str(*s)),
            Data::ProcessingInstruction { target, data } => Kind::ProcessingInstruction {
                target: self.str(*target),
                data: self.str(*data),
            },
        }
    }

    /// An element's attributes, in source order, as (namespace, local name,
    /// value).
    pub fn attrs(&self, id: NodeId) -> impl Iterator<Item = (AttrNs, &str, &str)> + '_ {
        self.attrs_of(id).iter().map(|a| {
            let name = self.str(a.name);
            let name = match a.ns {
                AttrNs::XLink | AttrNs::Xml | AttrNs::Xmlns => {
                    name.split_once(':').map_or(name, |(_, local)| local)
                }
                _ => name,
            };
            (a.ns, name, self.str(a.value))
        })
    }

    /// An element's attributes with their names as written: `xlink:href`
    /// rather than `href`, and an XML attribute's prefix kept.
    pub fn attrs_written(&self, id: NodeId) -> impl Iterator<Item = (AttrNs, &str, &str)> + '_ {
        let ns = self.element(id).map_or(Ns::Html, |e| e.ns);
        self.attrs_of(id)
            .iter()
            .map(move |a| (a.ns, self.written_name(ns, a), self.str(a.value)))
    }

    pub(crate) fn attrs_of(&self, id: NodeId) -> &[Attr] {
        let range = self
            .element(id)
            .map_or(0..0, |e| e.attrs.start as usize..e.attrs.end as usize);
        &self.attrs[range]
    }

    /// An attribute's name as the spec writes it on an element in `ns`:
    /// SVG's `viewBox` and MathML's `definitionURL` get their case back.
    pub(crate) fn written_name(&self, ns: Ns, a: &Attr) -> &str {
        let name = self.str(a.name);
        match (ns, a.ns) {
            (Ns::Svg, AttrNs::None) => crate::tree::svg_attr_name(name).unwrap_or(name),
            (Ns::MathMl, AttrNs::None) if name == "definitionurl" => "definitionURL",
            _ => name,
        }
    }

    /// An element's name lowercased, without allocating: SVG's
    /// `foreignObject` gives `foreignobject`, whether it was parsed or made
    /// with [`Document::create_element`]. An XML element's name as written,
    /// and so is a foreign element made under a name in another case
    /// (`LINEARGRADIENT`), which the DOM treats as a different element.
    pub fn lower_name(&self, id: NodeId) -> Option<&str> {
        let e = self.element(id)?;
        Some(match e.name {
            Name::Tag => e.tag.name(),
            Name::Svg(i) => crate::tree::SVG_TAG_NAMES[i as usize].0,
            Name::Span(s) => self.str(s),
        })
    }

    /// The parser's number for an element's tag, from its lowercase name:
    /// what [`crate::tag_number`] gives for that name. HTML, SVG and MathML
    /// elements alike get one, so an SVG `<title>` shares HTML `title`'s;
    /// an XML element never does.
    pub fn known_tag(&self, id: NodeId) -> Option<u8> {
        self.element(id)
            .filter(|e| e.tag != Tag::Other && e.ns != Ns::Xml)
            .map(|e| e.tag as u8)
    }
}

#[cfg(test)]
mod tests {
    use crate::{AttrNs, parse};

    /// Names as tokenized, lowercase and without a prefix, from attrs();
    /// as the spec writes them, case fixed and prefix kept, from
    /// attrs_written() and the serializer.
    #[test]
    fn foreign_attributes_have_a_lowercase_name_and_a_written_one() {
        let doc = parse(
            "<svg viewBox=0 xmlns:xlink=x><use xlink:href=#i></use></svg>\
             <math definitionURL=d></math>",
            false,
        );
        let svg = (0..doc.len() as u32)
            .find(|&n| doc.lower_name(n) == Some("svg"))
            .unwrap();
        let use_ = doc.first_child(svg).unwrap();
        let math = doc.next_sibling(svg).unwrap();
        let names = |n, written: bool| -> Vec<String> {
            if written {
                doc.attrs_written(n)
                    .map(|(_, a, _)| a.to_string())
                    .collect()
            } else {
                doc.attrs(n).map(|(_, a, _)| a.to_string()).collect()
            }
        };
        assert_eq!(names(svg, false), ["viewbox", "xlink"]);
        assert_eq!(names(svg, true), ["viewBox", "xmlns:xlink"]);
        assert_eq!(names(use_, false), ["href"]);
        assert_eq!(doc.attrs(use_).next().unwrap().0, AttrNs::XLink);
        assert_eq!(names(use_, true), ["xlink:href"]);
        assert_eq!(names(math, false), ["definitionurl"]);
        assert_eq!(names(math, true), ["definitionURL"]);
        assert!(doc.outer_html(svg).starts_with(r#"<svg viewBox="0""#));
        assert!(doc.outer_html(math).contains("definitionURL"));
    }

    #[test]
    fn an_edit_finds_a_foreign_attribute_by_its_written_name() {
        let mut doc = parse("<svg viewBox=0></svg>", false);
        let svg = (0..doc.len() as u32)
            .find(|&n| doc.lower_name(n) == Some("svg"))
            .unwrap();
        doc.set_attr(svg, "viewBox", "1");
        assert_eq!(doc.outer_html(svg), r#"<svg viewBox="1"></svg>"#);
        assert!(doc.remove_attr(svg, "viewBox"));
        assert_eq!(doc.outer_html(svg), "<svg></svg>");
    }
}
