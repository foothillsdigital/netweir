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
    pub(crate) src: String,
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
    /// Where each node and attribute was written, when the parse was
    /// asked to keep that, and how to map offsets back to the input, for
    /// spans or errors.
    pub(crate) positions: Option<Box<Positions>>,
    /// Each template's contents, mapped to the template: no link leads
    /// back from the contents, and an edit's cycle check has to climb out
    /// of them. Edits build it the first time they need it.
    pub(crate) hosts: Option<std::collections::HashMap<NodeId, NodeId>>,
    /// The parse errors, when the parse was asked to collect them.
    pub(crate) errors: Vec<crate::SyntaxError>,
    /// A bit per node taken out with [`Document::remove`]. A slot is never
    /// reused, so a removed id stays removed. Empty until a remove.
    pub(crate) dead: Vec<u64>,
}

/// A node or attribute with no recorded position.
const UNKNOWN: [u32; 2] = [u32::MAX; 2];

/// Where nodes and attributes were written, as `[start, end)` offsets
/// into the preprocessed source; the original input differs only by the
/// CRs that preprocessing dropped and a dropped byte order mark, so these
/// two are kept to map back.
#[derive(Debug, Default)]
pub(crate) struct Positions {
    /// By node id: an element's start tag, or the whole of anything else.
    pub nodes: Vec<[u32; 2]>,
    /// By node id: an element's end tag.
    pub ends: Vec<[u32; 2]>,
    /// By index into [`Document::attrs`].
    pub attrs: Vec<[u32; 2]>,
    /// Where the preprocessed source has an LF that was a CRLF, ascending.
    pub crlf: Vec<u32>,
    /// Bytes before the source in the original: a byte order mark.
    pub skipped: u32,
}

impl Positions {
    pub(crate) fn set(list: &mut Vec<[u32; 2]>, i: usize, at: [u32; 2]) {
        if list.len() <= i {
            list.resize(i + 1, UNKNOWN);
        }
        list[i] = at;
    }

    fn get(list: &[[u32; 2]], i: usize) -> Option<[u32; 2]> {
        list.get(i).copied().filter(|&a| a != UNKNOWN)
    }

    /// An offset in the preprocessed source, as one in the original.
    pub(crate) fn original(&self, at: u32) -> usize {
        let dropped = self.crlf.partition_point(|&lf| lf < at);
        self.skipped as usize + at as usize + dropped
    }

    fn range(&self, at: [u32; 2]) -> std::ops::Range<usize> {
        self.original(at[0])..self.original(at[1])
    }
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
            src: String::new(),
            buf: String::with_capacity(capacity / 8),
            extras: Vec::new(),
            attrs: Vec::with_capacity(capacity / 32 + 16),
            quirks: Quirks::No,
            scripting: false,
            positions: None,
            hosts: None,
            errors: Vec::new(),
            dead: Vec::new(),
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
    pub(crate) fn source_offset(&self, s: &str) -> Option<usize> {
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
        // Past this, an offset would read as one of the flags.
        debug_assert!(self.buf.len() < IN_EXTRA as usize, "buffer over 2^30 bytes");
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

    /// Where a node was written, as a byte range of the input the parse
    /// was given: an element's start tag from `<` to `>`, or the whole of
    /// a text node, comment, doctype or processing instruction. A text
    /// node made of several runs (text, then a character reference, then
    /// text again, or text foster-parented out of a table) spans the first
    /// run's start to the last one's end.
    ///
    /// `None` for a node the parser made up rather than read (an implied
    /// `<html>`, `<head>`, `<body>` or `<tbody>`, a formatting element it
    /// reopened, a copy the adoption agency made), for one an edit made,
    /// and for every node unless the document was parsed with
    /// [`Options::spans`](crate::Options::spans) on.
    pub fn span(&self, id: NodeId) -> Option<std::ops::Range<usize>> {
        if self.is_removed(id) {
            return None;
        }
        let p = self.positions.as_deref()?;
        Positions::get(&p.nodes, id as usize).map(|at| p.range(at))
    }

    /// Where an element's end tag was written, when it had one: the end
    /// tag whose handling closed it, `</p>` for a `<p>`, or a `</body>`.
    pub fn end_span(&self, id: NodeId) -> Option<std::ops::Range<usize>> {
        if self.is_removed(id) {
            return None;
        }
        let p = self.positions.as_deref()?;
        Positions::get(&p.ends, id as usize).map(|at| p.range(at))
    }

    /// Where an element's `i`th attribute, as [`Document::attrs`] lists
    /// them, was written: from its name's first byte to the end of its
    /// value, past a closing quote, or to the end of its name when it has
    /// no value. An attribute an edit set has none; a reopened formatting
    /// element's are where the original's were written.
    pub fn attr_span(&self, id: NodeId, i: usize) -> Option<std::ops::Range<usize>> {
        if self.is_removed(id) {
            return None;
        }
        let p = self.positions.as_deref()?;
        let e = self.element(id)?;
        let at = e.attrs.start as usize + i;
        if at >= e.attrs.end as usize {
            return None;
        }
        Positions::get(&p.attrs, at).map(|at| p.range(at))
    }

    /// The line and column of a byte offset into the input the parse was
    /// given, as an editor shows them: both start at 1, a line ends at an
    /// LF, a CRLF or a lone CR, and the column counts characters (Unicode
    /// scalar values), not bytes. Worked out from the source each time,
    /// so it costs a pass over the text before `offset`.
    pub fn line_col(&self, offset: usize) -> (usize, usize) {
        // Back into the preprocessed source, where every line ends at an
        // LF: past the skipped bytes and the CRs that were dropped.
        let (skipped, crlf) = self
            .positions
            .as_deref()
            .map_or((0, &[][..]), |p| (p.skipped as usize, &p.crlf[..]));
        let o = offset.saturating_sub(skipped);
        // The CRLFs wholly before `o`: the `i`th one's CR is at `lf + i`
        // in the original, where `lf` is its LF's place in the source.
        let (mut lo, mut hi) = (0, crlf.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if crlf[mid] as usize + mid + 1 < o {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let at = (o - lo.min(o)).min(self.src.len());
        let bytes = &self.src.as_bytes()[..at];
        let line = 1 + memchr::memchr_iter(b'\n', bytes).count();
        let start = memchr::memrchr(b'\n', bytes).map_or(0, |i| i + 1);
        // Every byte but a UTF-8 continuation byte starts a character.
        let column = 1 + bytes[start..]
            .iter()
            .filter(|&&b| (b as i8) >= -0x40)
            .count();
        (line, column)
    }

    /// The parse errors in the input, in source order: empty unless the
    /// document was parsed with [`Options::errors`](crate::Options::errors)
    /// on.
    pub fn errors(&self) -> &[crate::SyntaxError] {
        &self.errors
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
        debug_assert!(
            matches!(
                self.nodes[parent as usize].data,
                Data::Element(_) | Data::Document | Data::Fragment
            ),
            "node {parent} can't have children"
        );
        debug_assert!(
            !matches!(
                self.nodes[child as usize].data,
                Data::Document | Data::Fragment
            ),
            "node {child} is a document or a fragment, never a child"
        );
        debug_assert!(
            self.nodes[child as usize].parent == NONE
                && self.nodes[child as usize].prev == NONE
                && self.nodes[child as usize].next == NONE,
            "node {child} is inserted while still linked"
        );
        debug_assert!(parent != child, "node {child} inserted into itself");
        debug_assert!(
            before == NONE || self.nodes[before as usize].parent == parent,
            "node {before} isn't a child of {parent}"
        );
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
        debug_assert!(self.links_agree(child), "links around node {child}");
    }

    /// Takes `child` out of the tree, still usable: insert it again to move
    /// it, subtree and all. To be done with a node for good, use
    /// [`Document::remove`], after which its id reads as nothing.
    pub fn detach(&mut self, child: NodeId) {
        let Node {
            parent, prev, next, ..
        } = self.nodes[child as usize];
        if parent == NONE {
            debug_assert!(
                prev == NONE && next == NONE,
                "node {child} has siblings, no parent"
            );
            return;
        }
        debug_assert!(self.links_agree(child), "links around node {child}");
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
        debug_assert!(
            (prev == NONE || self.links_agree(prev)) && (next == NONE || self.links_agree(next)),
            "links where node {child} was"
        );
    }

    /// Whether `id`'s links and its neighbours' agree: its siblings point
    /// back at it, and its parent's first and last child are it exactly
    /// when it has no sibling on that side. Constant time, for the debug
    /// assertions on every insert and removal; [`Document::check_tree`]
    /// checks every node.
    pub(crate) fn links_agree(&self, id: NodeId) -> bool {
        let n = &self.nodes[id as usize];
        if n.parent == NONE {
            return n.prev == NONE && n.next == NONE;
        }
        let p = &self.nodes[n.parent as usize];
        let prev_ok = if n.prev == NONE {
            p.first_child == id
        } else {
            let s = &self.nodes[n.prev as usize];
            s.next == id && s.parent == n.parent && p.first_child != id
        };
        let next_ok = if n.next == NONE {
            p.last_child == id
        } else {
            let s = &self.nodes[n.next as usize];
            s.prev == id && s.parent == n.parent && p.last_child != id
        };
        prev_ok && next_ok
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
                debug_assert!((span.end & !IN_SOURCE) as usize + text.len() <= self.src.len());
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

    /// Checks every invariant of the tree, panicking at the first that
    /// doesn't hold: every node's links agree with its parent's and its
    /// siblings', only elements, documents and fragments have children,
    /// each node is reached from the root at most once (template contents
    /// included), each template's contents are a fragment of its own
    /// that's nobody's child, the map of hosts agrees with the templates,
    /// and every string and recorded position is inside its buffer.
    ///
    /// Linear in the document's size, so it's for tests and fuzzing, not
    /// for every edit; the parser and the edits assert the parts of this
    /// they touch as they go. Only in builds with debug assertions.
    #[cfg(debug_assertions)]
    pub fn check_tree(&self) {
        let len = self.nodes.len();
        let span_ok = |s: Span| {
            if s.start & IN_SOURCE != 0 {
                let (a, b) = (s.start & !IN_SOURCE, s.end & !IN_SOURCE);
                s.end & IN_SOURCE != 0
                    && a <= b
                    && self.src.is_char_boundary(a as usize)
                    && self.src.is_char_boundary(b as usize)
            } else if s.start & IN_EXTRA != 0 {
                s.start == s.end && ((s.start & !IN_EXTRA) as usize) < self.extras.len()
            } else {
                s.start <= s.end
                    && self.buf.is_char_boundary(s.start as usize)
                    && self.buf.is_char_boundary(s.end as usize)
            }
        };
        let mut contents_of = vec![NONE; len];
        for (id, n) in self.nodes.iter().enumerate() {
            let id = id as NodeId;
            for link in [n.parent, n.first_child, n.last_child, n.prev, n.next] {
                assert!(
                    link == NONE || (link as usize) < len,
                    "node {id} links past the end"
                );
            }
            assert!(self.links_agree(id), "links around node {id}");
            assert!(
                !self.is_removed(id) || n.parent == NONE || self.is_removed(n.parent),
                "removed node {id} is under {}, which isn't",
                n.parent
            );
            assert_eq!(
                n.first_child == NONE,
                n.last_child == NONE,
                "node {id} has a first child or a last, not both"
            );
            if n.first_child != NONE {
                let first = &self.nodes[n.first_child as usize];
                let last = &self.nodes[n.last_child as usize];
                assert!(
                    first.parent == id && first.prev == NONE,
                    "node {id}'s first child"
                );
                assert!(
                    last.parent == id && last.next == NONE,
                    "node {id}'s last child"
                );
                assert!(
                    matches!(n.data, Data::Element(_) | Data::Document | Data::Fragment),
                    "node {id} can't have children"
                );
            }
            match n.data {
                Data::Document | Data::Fragment => {
                    assert!(
                        n.parent == NONE,
                        "node {id}, a document or fragment, has a parent"
                    )
                }
                Data::Element(e) => {
                    let name_ok = match e.name {
                        Name::Tag => true,
                        Name::Svg(i) => (i as usize) < crate::tree::SVG_TAG_NAMES.len(),
                        Name::Span(s) => span_ok(s),
                    };
                    assert!(name_ok, "node {id}'s name");
                    assert!(
                        e.attrs.start <= e.attrs.end && e.attrs.end as usize <= self.attrs.len(),
                        "node {id}'s attributes"
                    );
                    if e.contents != NONE {
                        let c = e.contents as usize;
                        assert!(
                            e.ns == Ns::Html && e.tag == Tag::Template,
                            "node {id} has contents but isn't a template"
                        );
                        assert!(
                            c < len && matches!(self.nodes[c].data, Data::Fragment),
                            "node {id}'s contents aren't a fragment"
                        );
                        assert!(contents_of[c] == NONE, "two templates have contents {c}");
                        contents_of[c] = id;
                    }
                }
                Data::Doctype {
                    name,
                    public_id,
                    system_id,
                } => assert!(
                    span_ok(name) && span_ok(public_id) && span_ok(system_id),
                    "node {id}'s strings"
                ),
                Data::Text(s) | Data::Comment(s) | Data::Cdata(s) => {
                    assert!(span_ok(s), "node {id}'s text")
                }
                Data::ProcessingInstruction { target, data } => {
                    assert!(span_ok(target) && span_ok(data), "node {id}'s strings")
                }
            }
        }
        for (i, a) in self.attrs.iter().enumerate() {
            assert!(
                span_ok(a.name) && span_ok(a.value),
                "attribute {i}'s strings"
            );
        }
        if let Some(hosts) = &self.hosts {
            for (&contents, &host) in hosts {
                assert_eq!(
                    contents_of.get(contents as usize),
                    Some(&host),
                    "the hosts map at {contents}"
                );
            }
            let templates = contents_of.iter().filter(|&&h| h != NONE).count();
            assert_eq!(hosts.len(), templates, "the hosts map misses a template");
        }
        // Down from the root, each node once: no cycle, no node in two
        // places. Siblings are walked by their links, so a loop among
        // them shows as a node seen twice.
        let mut seen = vec![false; len];
        let mut stack = vec![self.root()];
        while let Some(id) = stack.pop() {
            assert!(!seen[id as usize], "node {id} reached twice");
            seen[id as usize] = true;
            let n = &self.nodes[id as usize];
            let mut child = n.first_child;
            while child != NONE {
                assert!(!seen[child as usize], "a loop among node {id}'s children");
                assert_eq!(
                    self.nodes[child as usize].parent, id,
                    "node {child}'s parent"
                );
                stack.push(child);
                child = self.nodes[child as usize].next;
                if stack.len() > len {
                    panic!("a loop among node {id}'s children");
                }
            }
            if let Data::Element(e) = n.data
                && e.contents != NONE
            {
                stack.push(e.contents);
            }
        }
        if let Some(p) = self.positions.as_deref() {
            let src = self.src.len() as u32;
            let in_src = |at: &[u32; 2]| *at == UNKNOWN || (at[0] <= at[1] && at[1] <= src);
            assert!(
                p.nodes.len() <= len && p.ends.len() <= len,
                "positions past the nodes"
            );
            assert!(
                p.attrs.len() <= self.attrs.len(),
                "positions past the attributes"
            );
            for (i, at) in p.nodes.iter().chain(&p.ends).chain(&p.attrs).enumerate() {
                assert!(
                    in_src(at),
                    "position {i}, {at:?}, outside {src} bytes of source"
                );
            }
            assert!(p.crlf.windows(2).all(|w| w[0] < w[1]), "CRLFs out of order");
            assert!(
                p.crlf.last().is_none_or(|&lf| lf < src),
                "a CRLF past the source"
            );
            let original = p.original(src);
            for e in &self.errors {
                assert!(
                    e.offset <= original,
                    "{e:?} past the input's {original} bytes"
                );
            }
        }
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        if self.is_removed(id) {
            return None;
        }
        Some(self.nodes[id as usize].parent).filter(|&p| p != NONE)
    }

    pub fn first_child(&self, id: NodeId) -> Option<NodeId> {
        if self.is_removed(id) {
            return None;
        }
        Some(self.nodes[id as usize].first_child).filter(|&c| c != NONE)
    }

    pub fn prev_sibling(&self, id: NodeId) -> Option<NodeId> {
        if self.is_removed(id) {
            return None;
        }
        Some(self.nodes[id as usize].prev).filter(|&c| c != NONE)
    }

    pub fn last_child(&self, id: NodeId) -> Option<NodeId> {
        if self.is_removed(id) {
            return None;
        }
        Some(self.nodes[id as usize].last_child).filter(|&c| c != NONE)
    }

    pub fn next_sibling(&self, id: NodeId) -> Option<NodeId> {
        if self.is_removed(id) {
            return None;
        }
        Some(self.nodes[id as usize].next).filter(|&c| c != NONE)
    }

    pub fn children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + '_ {
        std::iter::successors(self.first_child(id), move |&c| self.next_sibling(c))
    }

    /// A template element's contents.
    pub fn template_contents(&self, id: NodeId) -> Option<NodeId> {
        if self.is_removed(id) {
            return None;
        }
        self.element(id).map(|e| e.contents).filter(|&c| c != NONE)
    }

    /// Whether `id` was taken out with [`Document::remove`], or was inside
    /// a node that was. A removed node reads as nothing: the readers that
    /// return an `Option` give `None`, it has no children, attributes or
    /// text, it serializes to `""`, it matches no selector, and inserting
    /// it, or anything into it, is an error.
    pub fn is_removed(&self, id: NodeId) -> bool {
        self.dead
            .get(id as usize / 64)
            .is_some_and(|w| w >> (id % 64) & 1 == 1)
    }

    pub(crate) fn mark_removed(&mut self, id: NodeId) {
        let word = id as usize / 64;
        if self.dead.len() <= word {
            self.dead.resize(word + 1, 0);
        }
        self.dead[word] |= 1 << (id % 64);
    }

    /// What `id` is. A removed node keeps its kind and an element its
    /// name, but its text, a comment's, and a processing instruction's
    /// data read as empty, as it has no children or attributes.
    pub fn kind(&self, id: NodeId) -> Kind<'_> {
        let kind = self.kind_of(id);
        if !self.is_removed(id) {
            return kind;
        }
        match kind {
            Kind::Text(_) => Kind::Text(""),
            Kind::Comment(_) => Kind::Comment(""),
            Kind::Cdata(_) => Kind::Cdata(""),
            Kind::ProcessingInstruction { target, .. } => {
                Kind::ProcessingInstruction { target, data: "" }
            }
            other => other,
        }
    }

    fn kind_of(&self, id: NodeId) -> Kind<'_> {
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
        let attrs = if self.is_removed(id) {
            &[][..]
        } else {
            self.attrs_of(id)
        };
        attrs.iter().map(|a| {
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
        let attrs = if self.is_removed(id) {
            &[][..]
        } else {
            self.attrs_of(id)
        };
        attrs
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
        if self.is_removed(id) {
            return None;
        }
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
        if self.is_removed(id) {
            return None;
        }
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
