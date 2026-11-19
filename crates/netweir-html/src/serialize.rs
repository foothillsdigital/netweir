//! The HTML fragment serialization algorithm, section 13.3 of the HTML
//! Standard: `innerHTML` and `outerHTML`. With [`SerializeOptions`], also
//! html5lib's serializer options, an indented pretty-printer, and the
//! html5lib-tests tree dump.

use crate::dom::{AttrNs, Data, Document, Element, Kind, NodeId, Ns};
use crate::tags::Tag;

/// How [`Document::serialize_with`] writes a node. The default is the
/// HTML Standard's serialization, byte for byte what
/// [`Document::outer_html`] gives. Most fields are html5lib's serializer
/// options, under html5lib's names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerializeOptions {
    /// Indents the markup, two spaces a level, without changing what it
    /// means as the browser's default styles render it. An element's
    /// children go one to a line only when every child is a block
    /// (`div`, `p`, `li`, `table`, `tr`, `td` and the like), something
    /// that's never rendered (`head`, `meta`, `script`, `template`...), a
    /// comment, a doctype, or whitespace-only text, which is dropped for
    /// the new line; and the element is itself a block, never rendered,
    /// the document, or has at least one block child. Whitespace next to
    /// a block collapses away, so adding it there changes nothing.
    /// Anything else (text, an inline element such as `span`, `a`, `img`
    /// or a custom element, foreign content) is written as it is, and so
    /// is everything inside it, and inside `pre`, `textarea`, `listing`,
    /// `plaintext`, `xmp`, `title`, `script`, `style`, `iframe`,
    /// `noembed`, `noframes` and `noscript`. Nothing after `</body>` or
    /// `</html>` starts a new line: the parser moves whitespace there to
    /// the end of the body, where it could join text. `omit_optional_tags` is
    /// ignored: every tag is kept, so no new whitespace ends up inside an
    /// element whose end tag was left out. Author CSS that sets
    /// `white-space` or `display` can make any whitespace matter, which
    /// no pretty-printer can know.
    pub pretty: bool,
    /// The tree dump of html5lib-tests' tree-construction `.dat` files:
    /// `| <html>`, `|   <head>`, each child two spaces in, attributes
    /// sorted, foreign elements and namespaced attributes prefixed,
    /// template contents under `content`. Every other option is ignored.
    pub html5test: bool,
    /// `"` or `'` around attribute values; anything else is taken as
    /// `"`. The one chosen is escaped inside: `&quot;` or `&#39;`.
    pub quote_char: char,
    /// Quote every attribute value. Off, a value goes unquoted when the
    /// HTML syntax allows it: not empty, and no whitespace, quotes, `=`,
    /// `>` or `` ` ``.
    pub quote_attr_values: bool,
    /// Quote a value with whichever quote it doesn't contain, so neither
    /// needs escaping.
    pub use_best_quote_char: bool,
    /// Write an attribute without a value when its value is empty, or
    /// when it's a boolean attribute (`disabled`, `checked`...) whose value
    /// is its own name.
    pub minimize_boolean_attributes: bool,
    /// `<br />` rather than `<br>` for void elements.
    pub use_trailing_solidus: bool,
    /// Escape the text of `script`, `style` and the other raw text
    /// elements as other text is; this changes what a script says.
    pub escape_rcdata: bool,
    /// Leave out the start and end tags the HTML Standard lets a document
    /// leave out (section 13.1.2.4), where the parser puts back the same
    /// tree: `</p>` before another `p`, `</li>`, `</td>`, `<tbody>`.
    pub omit_optional_tags: bool,
    /// Collapse each run of whitespace in text to one space, except inside
    /// the elements `pretty` writes as they are.
    pub strip_whitespace: bool,
    /// Rewrite every `<meta charset>`, and the `content` of every
    /// `<meta http-equiv=content-type>`, to name this encoding, and add a
    /// `<meta charset>` first in a `head` that has neither.
    pub inject_meta_charset: Option<String>,
    /// A doctype's public and system identifiers too, as
    /// `<!DOCTYPE html PUBLIC "..." "...">`; the HTML Standard writes the
    /// name alone.
    pub full_doctype: bool,
}

const DEFAULT: SerializeOptions = SerializeOptions {
    pretty: false,
    html5test: false,
    quote_char: '"',
    quote_attr_values: true,
    use_best_quote_char: false,
    minimize_boolean_attributes: false,
    use_trailing_solidus: false,
    escape_rcdata: false,
    omit_optional_tags: false,
    strip_whitespace: false,
    inject_meta_charset: None,
    full_doctype: false,
};

impl Default for SerializeOptions {
    fn default() -> SerializeOptions {
        DEFAULT
    }
}

/// A node to write, with how: these flags, and its indent for `pretty`.
#[derive(Clone, Copy)]
struct Item {
    id: NodeId,
    /// Past 65,535 levels, `pretty` indents no further.
    depth: u16,
    flags: u8,
}

/// The element's end tag, once its children are written.
const END: u8 = 1;
/// Starts on a line of its own, for `pretty`.
const LINE: u8 = 2;
/// Under an element `pretty` writes as it is: no new lines.
const INLINE: u8 = 4;
/// Under `pre` or the like, where `strip_whitespace` leaves text alone.
const KEEP_SPACE: u8 = 8;
/// The node's children only, as `innerHTML`.
const INNER: u8 = 16;

/// Elements with no end tag and no contents when serialized.
fn void(e: &Element) -> bool {
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

/// Elements whose whitespace is kept, by the parser or by their default
/// `white-space`, or whose text is raw.
fn keeps_space(e: &Element) -> bool {
    use Tag::*;
    e.ns == Ns::Html
        && matches!(
            e.tag,
            Pre | Listing
                | Plaintext
                | Xmp
                | Textarea
                | Title
                | Script
                | Style
                | Iframe
                | Noembed
                | Noframes
                | Noscript
        )
}

/// Elements the default styles make blocks: list items, table parts and
/// the rest of `display: block`.
fn block(tag: Tag) -> bool {
    use Tag::*;
    matches!(
        tag,
        Html | Body
            | Address
            | Article
            | Aside
            | Blockquote
            | Caption
            | Center
            | Col
            | Colgroup
            | Dd
            | Details
            | Dialog
            | Dir
            | Div
            | Dl
            | Dt
            | Fieldset
            | Figcaption
            | Figure
            | Footer
            | Form
            | Frame
            | Frameset
            | H1
            | H2
            | H3
            | H4
            | H5
            | H6
            | Header
            | Hgroup
            | Hr
            | Li
            | Listing
            | Main
            | Menu
            | Nav
            | Ol
            | Optgroup
            | Option
            | P
            | Plaintext
            | Pre
            | Search
            | Section
            | Summary
            | Table
            | Tbody
            | Td
            | Tfoot
            | Th
            | Thead
            | Tr
            | Ul
            | Xmp
    )
}

/// Elements the default styles never render.
fn unrendered(tag: Tag) -> bool {
    use Tag::*;
    matches!(
        tag,
        Head | Title
            | Meta
            | Link
            | Style
            | Script
            | Base
            | Template
            | Datalist
            | Param
            | Source
            | Track
            | Area
            | Noembed
            | Noframes
    )
}

/// HTML's boolean attributes, current and obsolete, which
/// `minimize_boolean_attributes` writes bare when the value is the name.
/// `irrelevant` was an early name for `hidden`.
fn boolean(name: &str) -> bool {
    matches!(
        name,
        "allowfullscreen"
            | "async"
            | "autofocus"
            | "autoplay"
            | "checked"
            | "compact"
            | "controls"
            | "declare"
            | "default"
            | "defer"
            | "disabled"
            | "formnovalidate"
            | "hidden"
            | "inert"
            | "irrelevant"
            | "ismap"
            | "itemscope"
            | "loop"
            | "multiple"
            | "muted"
            | "nohref"
            | "nomodule"
            | "noresize"
            | "noshade"
            | "novalidate"
            | "nowrap"
            | "open"
            | "playsinline"
            | "readonly"
            | "required"
            | "reversed"
            | "scoped"
            | "seamless"
            | "selected"
            | "shadowrootclonable"
            | "shadowrootdelegatesfocus"
            | "shadowrootserializable"
            | "truespeed"
    )
}

fn space(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

fn blank(s: &str) -> bool {
    s.bytes().all(space)
}

fn newline(depth: u16, out: &mut String) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

impl Document {
    /// The markup for `id`'s children, as `innerHTML` returns it.
    pub fn inner_html(&self, id: NodeId) -> String {
        self.inner_html_with(id, &DEFAULT)
    }

    /// The markup for `id` and its children, as `outerHTML` returns it.
    pub fn outer_html(&self, id: NodeId) -> String {
        self.serialize_with(id, &DEFAULT)
    }

    /// The markup for `id` and its children, written as `options` say;
    /// with the defaults, what [`Document::outer_html`] gives.
    pub fn serialize_with(&self, id: NodeId, options: &SerializeOptions) -> String {
        self.serialize(id, 0, options)
    }

    /// The markup for `id`'s children, written as `options` say; with the
    /// defaults, what [`Document::inner_html`] gives.
    pub fn inner_html_with(&self, id: NodeId, options: &SerializeOptions) -> String {
        self.serialize(id, INNER, options)
    }

    fn serialize(&self, id: NodeId, flags: u8, o: &SerializeOptions) -> String {
        let mut out = String::new();
        if flags & INNER != 0
            && !matches!(
                self.nodes[id as usize].data,
                Data::Element(_) | Data::Document | Data::Fragment
            )
        {
            return out;
        }
        if o.html5test {
            self.dump(id, flags & INNER != 0, &mut out);
            return out;
        }
        let flags = if o.pretty || o.strip_whitespace {
            flags | self.context(id)
        } else {
            flags
        };
        let mut stack = vec![Item {
            id,
            depth: 0,
            flags,
        }];
        if *o == DEFAULT {
            if flags & INNER != 0 {
                stack.clear();
                if !self.element(id).is_some_and(void) {
                    self.push_children(id, 0, 0, &mut stack);
                }
            }
            self.write::<true>(stack, o, &mut out);
        } else {
            self.write::<false>(stack, o, &mut out);
        }
        out
    }

    /// What `id`'s ancestors make of it: under `pre` or the like, it's
    /// written as it is.
    fn context(&self, id: NodeId) -> u8 {
        let mut at = self.parent(id);
        while let Some(p) = at {
            if self.element(p).is_some_and(keeps_space) {
                return INLINE | KEEP_SPACE;
            }
            at = self.parent(p);
        }
        0
    }

    /// The flags `id`'s children are written with, given its own.
    fn child_flags(&self, id: NodeId, flags: u8, o: &SerializeOptions) -> u8 {
        let mut f = flags & (INLINE | KEEP_SPACE);
        if self.element(id).is_some_and(keeps_space) {
            f |= INLINE | KEEP_SPACE;
        }
        if o.pretty && f & INLINE == 0 {
            f |= if self.breaks(id) { LINE } else { INLINE };
        }
        f
    }

    /// Whether `pretty` puts `id`'s children one to a line: see
    /// [`SerializeOptions::pretty`].
    fn breaks(&self, id: NodeId) -> bool {
        let mut has_block = match &self.nodes[id as usize].data {
            Data::Document => true,
            Data::Element(e) => {
                e.ns == Ns::Html && (block(e.tag) || (unrendered(e.tag) && e.tag != Tag::Template))
            }
            _ => false,
        };
        let contents = self.template_contents(id);
        for c in self
            .children(id)
            .chain(contents.into_iter().flat_map(|t| self.children(t)))
        {
            match &self.nodes[c as usize].data {
                Data::Text(s) if !blank(self.str(*s)) => return false,
                Data::Element(e) if e.ns == Ns::Html && block(e.tag) => has_block = true,
                Data::Element(e) if e.ns == Ns::Html && unrendered(e.tag) => {}
                Data::Element(_) | Data::Cdata(_) => return false,
                _ => {}
            }
        }
        has_block
    }

    /// `id`'s children onto the stack, the first on top: any children it
    /// has, then a template's contents, which it only has both of once
    /// adopt_template_contents has moved its contents in. With `LINE`,
    /// whitespace-only text isn't pushed. Whether the end tag goes on a
    /// line of its own: there were children, and `LINE` still held after
    /// them.
    fn push_children(&self, id: NodeId, mut flags: u8, depth: u16, stack: &mut Vec<Item>) -> bool {
        let at = stack.len();
        if flags & LINE == 0 {
            let item = |c| Item {
                id: c,
                depth,
                flags,
            };
            stack.extend(self.children(id).map(item));
            if let Some(contents) = self.template_contents(id) {
                stack.extend(self.children(contents).map(item));
            }
            stack[at..].reverse();
            return false;
        }
        let contents = self.template_contents(id);
        for c in self
            .children(id)
            .chain(contents.into_iter().flat_map(|t| self.children(t)))
        {
            let data = &self.nodes[c as usize].data;
            if flags & LINE != 0 && matches!(data, Data::Text(s) if blank(self.str(*s))) {
                continue;
            }
            stack.push(Item {
                id: c,
                depth,
                flags,
            });
            // The parser puts whitespace after `</body>` or `</html>` at
            // the end of the body, where it could join the body's text.
            if let Data::Element(e) = data
                && e.ns == Ns::Html
                && matches!(e.tag, Tag::Body | Tag::Frameset | Tag::Html)
            {
                flags &= !LINE;
            }
        }
        stack[at..].reverse();
        stack.len() > at && flags & LINE != 0
    }

    /// Writes what's on the stack, with a stack of its own rather than
    /// recursion, so a tree as deep as any page makes can't overflow it.
    /// `PLAIN` is for the default options, which then cost nothing.
    fn write<const PLAIN: bool>(
        &self,
        mut stack: Vec<Item>,
        o: &SerializeOptions,
        out: &mut String,
    ) {
        let start = out.len();
        let omit = !PLAIN && o.omit_optional_tags && !o.pretty;
        while let Some(Item { id, depth, flags }) = stack.pop() {
            if !PLAIN && flags & LINE != 0 && out.len() > start {
                newline(depth, out);
            }
            let data = &self.nodes[id as usize].data;
            if flags & END != 0 {
                if let Data::Element(e) = data
                    && !(omit && self.omit_end(id, e))
                {
                    out.push_str("</");
                    out.push_str(self.element_name(e));
                    out.push('>');
                }
                continue;
            }
            match data {
                Data::Element(e) => {
                    let inner = !PLAIN && flags & INNER != 0;
                    if !inner && !(omit && self.omit_start(id, e, o)) {
                        self.start_tag::<PLAIN>(id, e, o, out);
                    }
                    if void(e) {
                        continue;
                    }
                    let children = if PLAIN {
                        0
                    } else {
                        self.child_flags(id, flags, o)
                    };
                    let inside = if inner {
                        depth
                    } else {
                        depth.saturating_add(1)
                    };
                    let meta = if PLAIN {
                        None
                    } else {
                        self.injects_meta(id, e, o)
                    };
                    if let Some(charset) = meta {
                        if children & LINE != 0 && out.len() > start {
                            newline(inside, out);
                        }
                        out.push_str("<meta");
                        self.attr("charset", charset, o, out);
                        out.push_str(if o.use_trailing_solidus { " />" } else { ">" });
                    }
                    let end = stack.len();
                    if !inner {
                        stack.push(Item {
                            id,
                            depth,
                            flags: END,
                        });
                    }
                    let line = self.push_children(id, children, inside, &mut stack);
                    let empty = stack.len() == end + 1;
                    if !inner && children & LINE != 0 && (line || (empty && meta.is_some())) {
                        stack[end].flags |= LINE;
                    }
                }
                Data::Text(span) => {
                    let text = self.str(*span);
                    if self.raw_text_parent(id) && (PLAIN || !o.escape_rcdata) {
                        out.push_str(text);
                    } else if !PLAIN && o.strip_whitespace && flags & KEEP_SPACE == 0 {
                        escape::<0>(&collapse(text), out);
                    } else {
                        escape::<0>(text, out);
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
                Data::Doctype {
                    name,
                    public_id,
                    system_id,
                } => {
                    out.push_str("<!DOCTYPE ");
                    out.push_str(self.str(*name));
                    if !PLAIN && o.full_doctype {
                        let (public_id, system_id) = (self.str(*public_id), self.str(*system_id));
                        if !public_id.is_empty() {
                            out.push_str(" PUBLIC ");
                            quoted(public_id, out);
                        } else if !system_id.is_empty() {
                            out.push_str(" SYSTEM");
                        }
                        if !system_id.is_empty() {
                            out.push(' ');
                            quoted(system_id, out);
                        }
                    }
                    out.push('>');
                }
                Data::Document | Data::Fragment => {
                    let children = if PLAIN {
                        0
                    } else {
                        self.child_flags(id, flags, o)
                    };
                    self.push_children(id, children, depth, &mut stack);
                }
            }
        }
    }

    fn start_tag<const PLAIN: bool>(
        &self,
        id: NodeId,
        e: &Element,
        o: &SerializeOptions,
        out: &mut String,
    ) {
        out.push('<');
        out.push_str(self.element_name(e));
        if PLAIN {
            // Names as written are the spec's serialized names:
            // `xlink:href`, `xml:lang`, `xmlns`.
            for (_, name, value) in self.attrs_written(id) {
                out.push(' ');
                out.push_str(name);
                out.push_str("=\"");
                escape::<b'"'>(value, out);
                out.push('"');
            }
            out.push('>');
            return;
        }
        let charset = o
            .inject_meta_charset
            .as_deref()
            .filter(|_| e.ns == Ns::Html && e.tag == Tag::Meta)
            .and_then(|c| Some((c, self.meta_charset(id)?)));
        for (i, (_, name, value)) in self.attrs_written(id).enumerate() {
            match charset {
                Some((c, (at, false))) if at == i => self.attr(name, c, o, out),
                Some((c, (at, true))) if at == i => {
                    self.attr(name, &format!("text/html; charset={c}"), o, out)
                }
                _ => self.attr(name, value, o, out),
            }
        }
        if o.use_trailing_solidus && void(e) {
            out.push_str(" /");
        }
        out.push('>');
    }

    fn attr(&self, name: &str, value: &str, o: &SerializeOptions, out: &mut String) {
        out.push(' ');
        out.push_str(name);
        if o.minimize_boolean_attributes
            && (value.is_empty() || (boolean(name) && value.eq_ignore_ascii_case(name)))
        {
            return;
        }
        out.push('=');
        if !o.quote_attr_values
            && !value.is_empty()
            && !value
                .bytes()
                .any(|b| space(b) || matches!(b, b'"' | b'\'' | b'=' | b'>' | b'`'))
        {
            escape::<0>(value, out);
            return;
        }
        let mut quote = if o.quote_char == '\'' { b'\'' } else { b'"' };
        if o.use_best_quote_char {
            match (value.contains('"'), value.contains('\'')) {
                (true, false) => quote = b'\'',
                (false, true) => quote = b'"',
                _ => {}
            }
        }
        out.push(quote as char);
        if quote == b'\'' {
            escape::<b'\''>(value, out);
        } else {
            escape::<b'"'>(value, out);
        }
        out.push(quote as char);
    }

    /// A meta element's attribute that names the encoding, for
    /// `inject_meta_charset`: its index, and whether it's the `content` of
    /// an `http-equiv=content-type`, not a `charset`.
    fn meta_charset(&self, id: NodeId) -> Option<(usize, bool)> {
        let attrs: Vec<_> = self.attrs(id).collect();
        let named = |n: &str| {
            attrs
                .iter()
                .position(|&(ns, name, _)| ns == AttrNs::None && name.eq_ignore_ascii_case(n))
        };
        if let Some(at) = named("charset") {
            return Some((at, false));
        }
        named("http-equiv")
            .filter(|&at| attrs[at].2.eq_ignore_ascii_case("content-type"))
            .and(named("content"))
            .map(|at| (at, true))
    }

    /// The encoding to write a `<meta charset>` for first in `id`: a head
    /// with no meta that names one, under `inject_meta_charset`.
    fn injects_meta<'o>(
        &self,
        id: NodeId,
        e: &Element,
        o: &'o SerializeOptions,
    ) -> Option<&'o str> {
        let charset = o.inject_meta_charset.as_deref()?;
        (e.ns == Ns::Html
            && e.tag == Tag::Head
            && !self.children(id).any(|c| {
                self.element(c)
                    .is_some_and(|m| m.ns == Ns::Html && m.tag == Tag::Meta)
                    && self.meta_charset(c).is_some()
            }))
        .then_some(charset)
    }

    /// The HTML tag of `id` if it's an HTML element.
    fn html_tag(&self, id: Option<NodeId>) -> Option<Tag> {
        let e = self.element(id?)?;
        (e.ns == Ns::Html).then_some(e.tag)
    }

    /// Whether `id` is there and is a comment, or text that starts with
    /// whitespace.
    fn space_or_comment(&self, id: Option<NodeId>) -> bool {
        id.is_some_and(|n| match &self.nodes[n as usize].data {
            Data::Comment(_) => true,
            Data::Text(s) => self.str(*s).bytes().next().is_some_and(space),
            _ => false,
        })
    }

    /// Whether `omit_optional_tags` leaves out `id`'s start tag, by
    /// section 13.1.2.4. Never with attributes. `<html>` is kept before
    /// whitespace too, which the parser would otherwise drop.
    fn omit_start(&self, id: NodeId, e: &Element, o: &SerializeOptions) -> bool {
        if e.ns != Ns::Html || e.attrs.start != e.attrs.end {
            return false;
        }
        let first = self.first_child(id);
        let prev_omitted = |tags: &[Tag]| {
            self.prev_sibling(id).is_some_and(|p| {
                self.element(p).is_some_and(|pe| {
                    pe.ns == Ns::Html && tags.contains(&pe.tag) && self.omit_end(p, pe)
                })
            })
        };
        match e.tag {
            Tag::Html => !self.space_or_comment(first),
            Tag::Head => {
                self.injects_meta(id, e, o).is_some()
                    || first.is_none_or(|f| self.element(f).is_some())
            }
            Tag::Body => {
                !self.space_or_comment(first)
                    && !matches!(
                        self.html_tag(first),
                        Some(
                            Tag::Meta
                                | Tag::Noscript
                                | Tag::Link
                                | Tag::Script
                                | Tag::Style
                                | Tag::Template
                        )
                    )
            }
            Tag::Colgroup => {
                self.html_tag(first) == Some(Tag::Col) && !prev_omitted(&[Tag::Colgroup])
            }
            Tag::Tbody => {
                self.html_tag(first) == Some(Tag::Tr)
                    && !prev_omitted(&[Tag::Tbody, Tag::Thead, Tag::Tfoot])
            }
            _ => false,
        }
    }

    /// Whether `omit_optional_tags` leaves out `id`'s end tag, by section
    /// 13.1.2.4. `</html>` and `</body>` are kept before whitespace too,
    /// as `</head>` is. `</p>` goes before `dir` as well, and `</tfoot>`
    /// before `tbody`, as earlier versions of the rules had it: the parser
    /// closes a `p` at `<dir>` and a `tfoot` at `<tbody>`.
    fn omit_end(&self, id: NodeId, e: &Element) -> bool {
        use Tag::*;
        if e.ns != Ns::Html {
            return false;
        }
        let next = self.next_sibling(id);
        let last = next.is_none();
        let tag = self.html_tag(next);
        let is = |tags: &[Tag]| tag.is_some_and(|t| tags.contains(&t));
        match e.tag {
            Html | Head | Body | Colgroup | Caption => !self.space_or_comment(next),
            Li => last || is(&[Li]),
            Dt => is(&[Dt, Dd]),
            Dd => last || is(&[Dd, Dt]),
            Rt | Rp => last || is(&[Rt, Rp]),
            Optgroup => last || is(&[Optgroup, Hr]),
            Option => last || is(&[Option, Optgroup, Hr]),
            Thead => is(&[Tbody, Tfoot]),
            Tbody => last || is(&[Tbody, Tfoot]),
            Tfoot => last || is(&[Tbody]),
            Tr => last || is(&[Tr]),
            Td | Th => last || is(&[Td, Th]),
            P if last => !self.parent(id).is_some_and(|p| {
                self.element(p).is_some_and(|pe| pe.ns == Ns::Html)
                    && self.lower_name(p).is_some_and(|n| {
                        matches!(
                            n,
                            "a" | "audio" | "del" | "ins" | "map" | "noscript" | "video"
                        ) || n.contains('-')
                    })
            }),
            P => is(&[
                Address, Article, Aside, Blockquote, Details, Dialog, Dir, Div, Dl, Fieldset,
                Figcaption, Figure, Footer, Form, H1, H2, H3, H4, H5, H6, Header, Hgroup, Hr, Main,
                Menu, Nav, Ol, P, Pre, Search, Section, Table, Ul,
            ]),
            _ => false,
        }
    }

    /// The node alone: an element's start tag, escaped text, a comment
    /// or a doctype; as `outer_html` writes it, without the children. A
    /// document or a template's contents is nothing but its children, so
    /// it gives an empty string.
    pub fn html_alone(&self, id: NodeId) -> String {
        let mut out = String::new();
        match &self.nodes[id as usize].data {
            Data::Element(e) => self.start_tag::<true>(id, e, &DEFAULT, &mut out),
            Data::Document | Data::Fragment => {}
            _ => out = self.outer_html(id),
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

    /// Writes the html5lib-tests tree dump of `id`, or of its children
    /// with `inner`; see [`SerializeOptions::html5test`].
    fn dump(&self, id: NodeId, inner: bool, out: &mut String) {
        // (node, depth, whether it's a template's contents, under `content`)
        let mut stack = Vec::new();
        if inner {
            self.push_dump(id, 0, &mut stack);
        } else {
            stack.push((id, 0, false));
        }
        while let Some((id, depth, contents)) = stack.pop() {
            let line = |out: &mut String| {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str("| ");
                for _ in 0..depth {
                    out.push_str("  ");
                }
            };
            if contents {
                line(out);
                out.push_str("content");
                let at = stack.len();
                stack.extend(self.children(id).map(|c| (c, depth + 1, false)));
                stack[at..].reverse();
                continue;
            }
            match self.kind(id) {
                Kind::Document | Kind::Fragment => self.push_dump(id, depth, &mut stack),
                Kind::Doctype {
                    name,
                    public_id,
                    system_id,
                } => {
                    line(out);
                    if public_id.is_empty() && system_id.is_empty() {
                        out.push_str(&format!("<!DOCTYPE {name}>"));
                    } else {
                        out.push_str(&format!(
                            "<!DOCTYPE {name} \"{public_id}\" \"{system_id}\">"
                        ));
                    }
                }
                Kind::Element { ns, name } => {
                    let prefix = match ns {
                        Ns::Html => "",
                        Ns::Svg => "svg ",
                        Ns::MathMl => "math ",
                        Ns::Xml => "xml ",
                    };
                    line(out);
                    out.push_str(&format!("<{prefix}{name}>"));
                    let mut attrs: Vec<String> = self
                        .attrs(id)
                        .zip(self.attrs_written(id))
                        .map(|((ns, local, value), (_, written, _))| {
                            let name = match ns {
                                AttrNs::None => written.to_string(),
                                AttrNs::XLink => format!("xlink {local}"),
                                AttrNs::Xml => format!("xml {local}"),
                                AttrNs::Xmlns if local == "xmlns" => "xmlns".to_string(),
                                AttrNs::Xmlns => format!("xmlns {local}"),
                                AttrNs::Host(_) => local.to_string(),
                            };
                            format!("{name}=\"{value}\"")
                        })
                        .collect();
                    attrs.sort();
                    for a in attrs {
                        line(out);
                        out.push_str("  ");
                        out.push_str(&a);
                    }
                    self.push_dump(id, depth + 1, &mut stack);
                }
                Kind::Text(t) => {
                    line(out);
                    out.push_str(&format!("\"{t}\""));
                }
                Kind::Comment(c) => {
                    line(out);
                    out.push_str(&format!("<!-- {c} -->"));
                }
                Kind::ProcessingInstruction { target, data } => {
                    line(out);
                    out.push_str(&format!("<?{target} {data}?>"));
                }
                Kind::Cdata(c) => {
                    line(out);
                    out.push_str(&format!("<![CDATA[{c}]]>"));
                }
            }
        }
    }

    /// `id`'s children onto the dump's stack, the first on top: a
    /// template's `content` first, then any children it has.
    fn push_dump(&self, id: NodeId, depth: usize, stack: &mut Vec<(NodeId, usize, bool)>) {
        let at = stack.len();
        if let Some(contents) = self.template_contents(id) {
            stack.push((contents, depth, true));
        }
        stack.extend(self.children(id).map(|c| (c, depth, false)));
        stack[at..].reverse();
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

/// Each run of whitespace in `s` as one space, for `strip_whitespace`.
fn collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_space = false;
    for c in s.chars() {
        if c.is_ascii() && space(c as u8) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// A doctype identifier in `"`, or `'` when it has a `"` in it, which
/// the tokenizer only allows in an identifier the other quote ends.
fn quoted(s: &str, out: &mut String) {
    let quote = if s.contains('"') { '\'' } else { '"' };
    out.push(quote);
    out.push_str(s);
    out.push(quote);
}

/// The spec's escaping: `&`, NO-BREAK SPACE, `<` and `>` always, and in an
/// attribute value the `QUOTE` around it, `"` or `'` (0 for text).
fn escape<const QUOTE: u8>(s: &str, out: &mut String) {
    let bytes = s.as_bytes();
    let mut from = 0;
    let mut i = 0;
    while i < bytes.len() {
        let replacement = match bytes[i] {
            b'&' => "&amp;",
            b'<' => "&lt;",
            b'>' => "&gt;",
            b'"' if QUOTE == b'"' => "&quot;",
            b'\'' if QUOTE == b'\'' => "&#39;",
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
    use crate::{Ns, SerializeOptions, parse, parse_fragment};

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

    fn with(html: &str, o: &SerializeOptions) -> String {
        let doc = parse(html, false);
        doc.serialize_with(doc.root(), o)
    }

    #[test]
    fn the_defaults_are_outer_html() {
        let html = "<!DOCTYPE html PUBLIC \"p\" \"s\"><p title='a\"b' hidden>x&nbsp;<br><script>a<b</script>";
        let doc = parse(html, false);
        assert_eq!(
            doc.serialize_with(0, &SerializeOptions::default()),
            doc.outer_html(0)
        );
        let body = (0..doc.len() as crate::NodeId)
            .find(|&n| doc.lower_name(n) == Some("body"))
            .unwrap();
        assert_eq!(
            doc.inner_html_with(body, &SerializeOptions::default()),
            doc.inner_html(body)
        );
    }

    #[test]
    fn pretty_indents_blocks_and_leaves_inline_content_alone() {
        let o = SerializeOptions {
            pretty: true,
            ..SerializeOptions::default()
        };
        assert_eq!(
            with(
                "<!DOCTYPE html><title>T</title><div>\n  <p>a <b>b</b></p><ul><li>x<li>y</ul>\
                 <!--c--></div><pre>\n\n a</pre><p><span><i>s</i></span></p>",
                &o
            ),
            "<!DOCTYPE html>\n<html>\n  <head>\n    <title>T</title>\n  </head>\n  <body>\
             \n    <div>\n      <p>a <b>b</b></p>\n      <ul>\n        <li>x</li>\
             \n        <li>y</li>\n      </ul>\n      <!--c-->\n    </div>\
             \n    <pre>\n a</pre>\n    <p><span><i>s</i></span></p>\n  </body></html>"
        );
        // A span among the blocks keeps them all as they are.
        assert_eq!(
            with("<body><div><p>a</p></div><span>b</span>", &o),
            "<html>\n  <head></head>\n  <body><div><p>a</p></div><span>b</span></body></html>"
        );
        // Nothing after `</body>` gets a new line: the parser would put it
        // at the end of the body.
        assert_eq!(
            with("<p>x</p></body><!--after-->", &o),
            "<html>\n  <head></head>\n  <body>\n    <p>x</p>\n  </body><!--after--></html>"
        );
        // Inside a pre, even blocks are left alone.
        let doc = parse("<pre><div><p>a</p><p>b</p></div></pre>", false);
        let div = (0..doc.len() as crate::NodeId)
            .find(|&n| doc.lower_name(n) == Some("div"))
            .unwrap();
        assert_eq!(doc.serialize_with(div, &o), "<div><p>a</p><p>b</p></div>");
        assert_eq!(doc.inner_html_with(div, &o), "<p>a</p><p>b</p>");
    }

    #[test]
    fn attribute_quoting_and_minimizing() {
        let o = SerializeOptions {
            quote_attr_values: false,
            use_best_quote_char: true,
            minimize_boolean_attributes: true,
            use_trailing_solidus: true,
            ..SerializeOptions::default()
        };
        assert_eq!(
            body_with(
                "<input disabled=disabled name=name value='' a=b c='d e' f='g\"h' i=\"j'k\" l='<&>'>",
                &o
            ),
            "<input disabled name=name value a=b c=\"d e\" f='g\"h' i=\"j'k\" l=\"&lt;&amp;&gt;\" />"
        );
        let single = SerializeOptions {
            quote_char: '\'',
            ..SerializeOptions::default()
        };
        assert_eq!(
            body_with("<p title=\"a'b&quot;\">", &single),
            "<p title='a&#39;b\"'></p>"
        );
    }

    fn body_with(html: &str, o: &SerializeOptions) -> String {
        let doc = parse(html, false);
        let body = (0..doc.len() as crate::NodeId)
            .find(|&n| doc.lower_name(n) == Some("body"))
            .unwrap();
        doc.inner_html_with(body, o)
    }

    #[test]
    fn optional_tags_are_left_out_where_the_parser_puts_them_back() {
        let o = SerializeOptions {
            omit_optional_tags: true,
            ..SerializeOptions::default()
        };
        assert_eq!(
            with(
                "<!DOCTYPE html><title>T</title><p>a<p>b<ul><li>c<li>d</ul>\
                 <table><tr><td>e<td>f</table><a><p>g</p></a>",
                &o
            ),
            "<!DOCTYPE html><title>T</title><p>a<p>b<ul><li>c<li>d</ul>\
             <table><tr><td>e<td>f</table><a><p>g</p></a>"
        );
        // A start tag with attributes is kept, and so is `<body>` before
        // whitespace, which would otherwise go to the head.
        assert_eq!(
            with("<html lang=en><body> <p>x", &o),
            "<html lang=\"en\"><body> <p>x"
        );
    }

    #[test]
    fn whitespace_charset_doctype_and_raw_text_options() {
        let o = SerializeOptions {
            strip_whitespace: true,
            inject_meta_charset: Some("utf-8".into()),
            full_doctype: true,
            escape_rcdata: true,
            ..SerializeOptions::default()
        };
        assert_eq!(
            with(
                "<!DOCTYPE html PUBLIC \"-//W3C//DTD HTML 4.01//EN\" \"x\"><meta charset=latin1>\
                 <meta http-equiv=Content-Type content='text/html; charset=latin1'>\
                 <script>a<b</script><p>a \n\t b</p><pre> a  b</pre>",
                &o
            ),
            "<!DOCTYPE html PUBLIC \"-//W3C//DTD HTML 4.01//EN\" \"x\"><html><head>\
             <meta charset=\"utf-8\"><meta http-equiv=\"Content-Type\" \
             content=\"text/html; charset=utf-8\"><script>a&lt;b</script></head>\
             <body><p>a b</p><pre> a  b</pre></body></html>"
        );
        assert_eq!(
            with("<title>t</title>", &o),
            "<html><head><meta charset=\"utf-8\"><title>t</title></head><body></body></html>"
        );
    }

    #[test]
    fn the_html5lib_tree_dump() {
        let o = SerializeOptions {
            html5test: true,
            ..SerializeOptions::default()
        };
        let doc = parse(
            "<!DOCTYPE html><p b=2 a=1>x<template><i>y</i></template><svg xlink:href=z>",
            false,
        );
        let p = (0..doc.len() as crate::NodeId)
            .find(|&n| doc.lower_name(n) == Some("p"))
            .unwrap();
        assert_eq!(
            doc.serialize_with(p, &o),
            "| <p>\n|   a=\"1\"\n|   b=\"2\"\n|   \"x\"\n|   <template>\n|     content\
             \n|       <i>\n|         \"y\"\n|   <svg svg>\n|     xlink href=\"z\""
        );
        assert_eq!(doc.inner_html_with(p, &o).lines().next(), Some("| \"x\""));
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
                for o in [
                    crate::SerializeOptions {
                        pretty: true,
                        ..Default::default()
                    },
                    crate::SerializeOptions {
                        html5test: true,
                        ..Default::default()
                    },
                ] {
                    assert!(!doc.serialize_with(0, &o).is_empty());
                }
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
