//! Tree construction, as section 13.2.6 of the HTML Standard describes it,
//! with scripting off or on and no sanitizer.

use std::collections::HashMap;

use crate::dom::{Attr, AttrNs, Data, Document, Element, NONE, Name, NodeId, Ns, Quirks, Span};
use crate::tags::Tag;
use crate::tokenizer::{self, Doctype, Sink, TextMode, Tokenizer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Initial,
    BeforeHtml,
    BeforeHead,
    InHead,
    InHeadNoscript,
    AfterHead,
    InBody,
    Text,
    InTable,
    InTableText,
    InCaption,
    InColumnGroup,
    InTableBody,
    InRow,
    InCell,
    InTemplate,
    AfterBody,
    InFrameset,
    AfterFrameset,
    AfterAfterBody,
    AfterAfterFrameset,
}

#[derive(Clone, Copy)]
struct Start<'t> {
    tag: Tag,
    name: &'t str,
    attrs: Option<&'t tokenizer::Tag<'t>>,
    self_closing: bool,
}

impl<'t> Start<'t> {
    fn bare(tag: Tag) -> Start<'static> {
        Start {
            tag,
            name: tag.name(),
            attrs: None,
            self_closing: false,
        }
    }

    fn attrs(&self) -> impl Iterator<Item = (&'t str, &'t str)> + use<'t> {
        self.attrs.into_iter().flat_map(|t| t.attrs())
    }

    fn attr(&self, name: &str) -> Option<&'t str> {
        self.attrs().find(|(n, _)| *n == name).map(|(_, v)| v)
    }
}

#[derive(Clone, Copy)]
struct End<'t> {
    tag: Tag,
    name: &'t str,
}

#[derive(Clone, Copy)]
enum Tok<'t> {
    Doctype(&'t Doctype<'t>),
    Start(Start<'t>),
    End(End<'t>),
    Text(&'t str),
    Comment(&'t str),
    Pi(&'t str, &'t str),
    Eof,
}

/// An entry in the stack of open elements, with its namespace and tag
/// beside it so scope checks don't touch the nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Open {
    id: NodeId,
    ns: Ns,
    tag: Tag,
}

impl Open {
    fn is(self, tag: Tag) -> bool {
        self.ns == Ns::Html && self.tag == tag
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Default,
    ListItem,
    Button,
    Table,
}

const MARKER: NodeId = NONE;

pub(crate) struct TreeBuilder {
    pub(crate) doc: Document,
    open: Vec<Open>,
    active: Vec<NodeId>,
    mode: Mode,
    original: Mode,
    template_modes: Vec<Mode>,
    head: NodeId,
    form: NodeId,
    frameset_ok: bool,
    foster: bool,
    scripting: bool,
    context: Option<Open>,
    pending: String,
    pending_text: bool,
    skip_newline: bool,
    switch: Option<TextMode>,
    done: bool,
    /// Each select's selected option, and each option's select, for
    /// `<selectedcontent>`.
    selected: HashMap<NodeId, NodeId>,
    option_select: HashMap<NodeId, NodeId>,
    /// How many HTML elements of each tag are open, so "is there a p in
    /// button scope" is answered without walking the stack when none is.
    counts: [u32; 256],
    /// How many open elements have each name, for those `counts` doesn't
    /// cover: HTML elements of no known tag, and foreign elements. Keyed
    /// by [`name_key`]: two names can share a slot, which only makes a
    /// count too high, so the walk it saves is made after all.
    names: Box<[u32; NAME_SLOTS]>,
    /// Whether each node is on the stack of open elements, so asking is
    /// one lookup however deep the stack is.
    on_stack: Vec<bool>,
}

/// How many counters [`TreeBuilder::names`] has.
const NAME_SLOTS: usize = 1024;

/// FNV-1a over `name` in ASCII lower case, and whether it's an HTML
/// element's: a slot in [`TreeBuilder::names`], found without allocating.
fn name_key(html: bool, name: &str) -> usize {
    let mut h: u64 = if html {
        0xcbf2_9ce4_8422_2325
    } else {
        0x8422_2325_cbf2_9ce4
    };
    for b in name.bytes() {
        h ^= u64::from(b.to_ascii_lowercase());
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    (h ^ (h >> 32)) as usize % NAME_SLOTS
}

fn is_space(c: u8) -> bool {
    matches!(c, b'\t' | b'\n' | b'\x0C' | b'\r' | b' ')
}

/// The leading whitespace of `s`, and the rest.
fn split_space(s: &str) -> (&str, &str) {
    let n = s.bytes().take_while(|&b| is_space(b)).count();
    s.split_at(n)
}

fn all_space(s: &str) -> bool {
    s.bytes().all(is_space)
}

fn special(o: Open) -> bool {
    use Tag::*;
    match o.ns {
        Ns::Html => matches!(
            o.tag,
            Address
                | Applet
                | Area
                | Article
                | Aside
                | Base
                | Basefont
                | Bgsound
                | Blockquote
                | Body
                | Br
                | Button
                | Caption
                | Center
                | Col
                | Colgroup
                | Dd
                | Details
                | Dir
                | Div
                | Dl
                | Dt
                | Embed
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
                | Head
                | Header
                | Hgroup
                | Hr
                | Html
                | Iframe
                | Img
                | Input
                | Keygen
                | Li
                | Link
                | Listing
                | Main
                | Marquee
                | Menu
                | Meta
                | Nav
                | Noembed
                | Noframes
                | Noscript
                | Object
                | Ol
                | P
                | Param
                | Plaintext
                | Pre
                | Script
                | Search
                | Section
                | Select
                | Source
                | Style
                | Summary
                | Table
                | Tbody
                | Td
                | Template
                | Textarea
                | Tfoot
                | Th
                | Thead
                | Title
                | Tr
                | Track
                | Ul
                | Wbr
                | Xmp
        ),
        Ns::MathMl => matches!(o.tag, Mi | Mo | Mn | Ms | Mtext | AnnotationXml),
        Ns::Svg => matches!(o.tag, ForeignObject | Desc | Title),
        // The HTML parser never makes XML elements.
        Ns::Xml => false,
    }
}

fn scope_boundary(o: Open, scope: Scope) -> bool {
    use Tag::*;
    let base = || match o.ns {
        Ns::Html => matches!(
            o.tag,
            Applet | Caption | Html | Table | Td | Th | Marquee | Object | Select | Template
        ),
        Ns::MathMl => matches!(o.tag, Mi | Mo | Mn | Ms | Mtext | AnnotationXml),
        Ns::Svg => matches!(o.tag, ForeignObject | Desc | Title),
        // The HTML parser never makes XML elements.
        Ns::Xml => false,
    };
    match scope {
        Scope::Default => base(),
        Scope::ListItem => base() || o.is(Ol) || o.is(Ul),
        Scope::Button => base() || o.is(Button),
        Scope::Table => o.is(Html) || o.is(Table) || o.is(Template),
    }
}

fn heading(tag: Tag) -> bool {
    matches!(
        tag,
        Tag::H1 | Tag::H2 | Tag::H3 | Tag::H4 | Tag::H5 | Tag::H6
    )
}

fn implied(tag: Tag) -> bool {
    use Tag::*;
    matches!(
        tag,
        Dd | Dt | Li | Optgroup | Option | P | Rb | Rp | Rt | Rtc
    )
}

fn mathml_text_integration(o: Open) -> bool {
    o.ns == Ns::MathMl && matches!(o.tag, Tag::Mi | Tag::Mo | Tag::Mn | Tag::Ms | Tag::Mtext)
}

/// SVG element names whose case the parser fixes: as tokenized, and as
/// the element has it.
pub(crate) static SVG_TAG_NAMES: [(&str, &str); 37] = [
    ("altglyph", "altGlyph"),
    ("altglyphdef", "altGlyphDef"),
    ("altglyphitem", "altGlyphItem"),
    ("animatecolor", "animateColor"),
    ("animatemotion", "animateMotion"),
    ("animatetransform", "animateTransform"),
    ("clippath", "clipPath"),
    ("feblend", "feBlend"),
    ("fecolormatrix", "feColorMatrix"),
    ("fecomponenttransfer", "feComponentTransfer"),
    ("fecomposite", "feComposite"),
    ("feconvolvematrix", "feConvolveMatrix"),
    ("fediffuselighting", "feDiffuseLighting"),
    ("fedisplacementmap", "feDisplacementMap"),
    ("fedistantlight", "feDistantLight"),
    ("fedropshadow", "feDropShadow"),
    ("feflood", "feFlood"),
    ("fefunca", "feFuncA"),
    ("fefuncb", "feFuncB"),
    ("fefuncg", "feFuncG"),
    ("fefuncr", "feFuncR"),
    ("fegaussianblur", "feGaussianBlur"),
    ("feimage", "feImage"),
    ("femerge", "feMerge"),
    ("femergenode", "feMergeNode"),
    ("femorphology", "feMorphology"),
    ("feoffset", "feOffset"),
    ("fepointlight", "fePointLight"),
    ("fespecularlighting", "feSpecularLighting"),
    ("fespotlight", "feSpotLight"),
    ("fetile", "feTile"),
    ("feturbulence", "feTurbulence"),
    ("foreignobject", "foreignObject"),
    ("glyphref", "glyphRef"),
    ("lineargradient", "linearGradient"),
    ("radialgradient", "radialGradient"),
    ("textpath", "textPath"),
];

fn svg_tag_name(name: &str) -> Option<u8> {
    SVG_TAG_NAMES
        .iter()
        .position(|(lower, _)| *lower == name)
        .map(|i| i as u8)
}

/// SVG attribute names whose case the parser fixes.
pub(crate) fn svg_attr_name(name: &str) -> Option<&'static str> {
    Some(match name {
        "attributename" => "attributeName",
        "attributetype" => "attributeType",
        "basefrequency" => "baseFrequency",
        "baseprofile" => "baseProfile",
        "calcmode" => "calcMode",
        "clippathunits" => "clipPathUnits",
        "diffuseconstant" => "diffuseConstant",
        "edgemode" => "edgeMode",
        "filterunits" => "filterUnits",
        "glyphref" => "glyphRef",
        "gradienttransform" => "gradientTransform",
        "gradientunits" => "gradientUnits",
        "kernelmatrix" => "kernelMatrix",
        "kernelunitlength" => "kernelUnitLength",
        "keypoints" => "keyPoints",
        "keysplines" => "keySplines",
        "keytimes" => "keyTimes",
        "lengthadjust" => "lengthAdjust",
        "limitingconeangle" => "limitingConeAngle",
        "markerheight" => "markerHeight",
        "markerunits" => "markerUnits",
        "markerwidth" => "markerWidth",
        "maskcontentunits" => "maskContentUnits",
        "maskunits" => "maskUnits",
        "numoctaves" => "numOctaves",
        "pathlength" => "pathLength",
        "patterncontentunits" => "patternContentUnits",
        "patterntransform" => "patternTransform",
        "patternunits" => "patternUnits",
        "pointsatx" => "pointsAtX",
        "pointsaty" => "pointsAtY",
        "pointsatz" => "pointsAtZ",
        "preservealpha" => "preserveAlpha",
        "preserveaspectratio" => "preserveAspectRatio",
        "primitiveunits" => "primitiveUnits",
        "refx" => "refX",
        "refy" => "refY",
        "repeatcount" => "repeatCount",
        "repeatdur" => "repeatDur",
        "requiredextensions" => "requiredExtensions",
        "requiredfeatures" => "requiredFeatures",
        "specularconstant" => "specularConstant",
        "specularexponent" => "specularExponent",
        "spreadmethod" => "spreadMethod",
        "startoffset" => "startOffset",
        "stddeviation" => "stdDeviation",
        "stitchtiles" => "stitchTiles",
        "surfacescale" => "surfaceScale",
        "systemlanguage" => "systemLanguage",
        "tablevalues" => "tableValues",
        "targetx" => "targetX",
        "targety" => "targetY",
        "textlength" => "textLength",
        "viewbox" => "viewBox",
        "viewtarget" => "viewTarget",
        "xchannelselector" => "xChannelSelector",
        "ychannelselector" => "yChannelSelector",
        "zoomandpan" => "zoomAndPan",
        _ => return None,
    })
}

/// The spec's "adjust foreign attributes": a namespace and local name.
fn foreign_attr(name: &str) -> Option<(AttrNs, &'static str)> {
    Some(match name {
        "xlink:actuate" => (AttrNs::XLink, "actuate"),
        "xlink:arcrole" => (AttrNs::XLink, "arcrole"),
        "xlink:href" => (AttrNs::XLink, "href"),
        "xlink:role" => (AttrNs::XLink, "role"),
        "xlink:show" => (AttrNs::XLink, "show"),
        "xlink:title" => (AttrNs::XLink, "title"),
        "xlink:type" => (AttrNs::XLink, "type"),
        "xml:lang" => (AttrNs::Xml, "lang"),
        "xml:space" => (AttrNs::Xml, "space"),
        "xmlns" => (AttrNs::Xmlns, "xmlns"),
        "xmlns:xlink" => (AttrNs::Xmlns, "xlink"),
        _ => return None,
    })
}

fn quirky_public_id(public: &str) -> bool {
    const STARTS: &[&str] = &[
        "+//silmaril//dtd html pro v0r11 19970101//",
        "-//as//dtd html 3.0 aswedit + extensions//",
        "-//advasoft ltd//dtd html 3.0 aswedit + extensions//",
        "-//ietf//dtd html 2.0 level 1//",
        "-//ietf//dtd html 2.0 level 2//",
        "-//ietf//dtd html 2.0 strict level 1//",
        "-//ietf//dtd html 2.0 strict level 2//",
        "-//ietf//dtd html 2.0 strict//",
        "-//ietf//dtd html 2.0//",
        "-//ietf//dtd html 2.1e//",
        "-//ietf//dtd html 3.0//",
        "-//ietf//dtd html 3.2 final//",
        "-//ietf//dtd html 3.2//",
        "-//ietf//dtd html 3//",
        "-//ietf//dtd html level 0//",
        "-//ietf//dtd html level 1//",
        "-//ietf//dtd html level 2//",
        "-//ietf//dtd html level 3//",
        "-//ietf//dtd html strict level 0//",
        "-//ietf//dtd html strict level 1//",
        "-//ietf//dtd html strict level 2//",
        "-//ietf//dtd html strict level 3//",
        "-//ietf//dtd html strict//",
        "-//ietf//dtd html//",
        "-//metrius//dtd metrius presentational//",
        "-//microsoft//dtd internet explorer 2.0 html strict//",
        "-//microsoft//dtd internet explorer 2.0 html//",
        "-//microsoft//dtd internet explorer 2.0 tables//",
        "-//microsoft//dtd internet explorer 3.0 html strict//",
        "-//microsoft//dtd internet explorer 3.0 html//",
        "-//microsoft//dtd internet explorer 3.0 tables//",
        "-//netscape comm. corp.//dtd html//",
        "-//netscape comm. corp.//dtd strict html//",
        "-//o'reilly and associates//dtd html 2.0//",
        "-//o'reilly and associates//dtd html extended 1.0//",
        "-//o'reilly and associates//dtd html extended relaxed 1.0//",
        "-//sq//dtd html 2.0 hotmetal + extensions//",
        "-//softquad software//dtd hotmetal pro 6.0::19990601::extensions to html 4.0//",
        "-//softquad//dtd hotmetal pro 4.0::19971010::extensions to html 4.0//",
        "-//spyglass//dtd html 2.0 extended//",
        "-//sun microsystems corp.//dtd hotjava html//",
        "-//sun microsystems corp.//dtd hotjava strict html//",
        "-//w3c//dtd html 3 1995-03-24//",
        "-//w3c//dtd html 3.2 draft//",
        "-//w3c//dtd html 3.2 final//",
        "-//w3c//dtd html 3.2//",
        "-//w3c//dtd html 3.2s draft//",
        "-//w3c//dtd html 4.0 frameset//",
        "-//w3c//dtd html 4.0 transitional//",
        "-//w3c//dtd html experimental 19960712//",
        "-//w3c//dtd html experimental 970421//",
        "-//w3c//dtd w3 html//",
        "-//w3o//dtd w3 html 3.0//",
        "-//webtechs//dtd mozilla html 2.0//",
        "-//webtechs//dtd mozilla html//",
    ];
    STARTS.iter().any(|s| starts_with_ci(public, s))
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

fn doctype_quirks(d: &Doctype<'_>) -> Quirks {
    let public = d.public_id.unwrap_or("");
    let system = d.system_id;
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    if d.force_quirks
        || d.name != Some("html")
        || d.public_id.is_some_and(|p| {
            eq(p, "-//W3O//DTD W3 HTML Strict 3.0//EN//")
                || eq(p, "-/W3C/DTD HTML 4.0 Transitional/EN")
                || eq(p, "HTML")
        })
        || system.is_some_and(|s| {
            eq(
                s,
                "http://www.ibm.com/data/dtd/v11/ibmxhtml1-transitional.dtd",
            )
        })
        || d.public_id.is_some_and(quirky_public_id)
        || (system.is_none()
            && (starts_with_ci(public, "-//W3C//DTD HTML 4.01 Frameset//")
                || starts_with_ci(public, "-//W3C//DTD HTML 4.01 Transitional//")))
    {
        Quirks::Quirks
    } else if starts_with_ci(public, "-//W3C//DTD XHTML 1.0 Frameset//")
        || starts_with_ci(public, "-//W3C//DTD XHTML 1.0 Transitional//")
        || (system.is_some()
            && (starts_with_ci(public, "-//W3C//DTD HTML 4.01 Frameset//")
                || starts_with_ci(public, "-//W3C//DTD HTML 4.01 Transitional//")))
    {
        Quirks::Limited
    } else {
        Quirks::No
    }
}

impl TreeBuilder {
    pub(crate) fn new(capacity: usize, scripting: bool) -> TreeBuilder {
        TreeBuilder {
            doc: Document::new(Data::Document, capacity),
            open: Vec::with_capacity(64),
            active: Vec::new(),
            mode: Mode::Initial,
            original: Mode::Initial,
            template_modes: Vec::new(),
            head: NONE,
            form: NONE,
            frameset_ok: true,
            foster: false,
            scripting,
            context: None,
            pending: String::new(),
            pending_text: false,
            skip_newline: false,
            switch: None,
            done: false,
            selected: HashMap::new(),
            option_select: HashMap::new(),
            counts: [0; 256],
            names: Box::new([0; NAME_SLOTS]),
            on_stack: vec![false; capacity / 24 + 1024],
        }
    }

    /// Sets up the fragment case with a context element, and returns the
    /// mode the tokenizer starts in.
    pub(crate) fn fragment(&mut self, ns: Ns, name: &str) -> Option<TextMode> {
        let tag = Tag::from_name(&name.to_ascii_lowercase());
        let context = self.create(
            &Start {
                tag,
                name,
                attrs: None,
                self_closing: false,
            },
            ns,
        );
        self.context = Some(Open {
            id: context,
            ns,
            tag,
        });
        let root = self.create(&Start::bare(Tag::Html), Ns::Html);
        self.doc.insert(0, root, NONE);
        self.push_open(Open {
            id: root,
            ns: Ns::Html,
            tag: Tag::Html,
        });
        if ns == Ns::Html && tag == Tag::Template {
            self.template_modes.push(Mode::InTemplate);
        }
        if ns == Ns::Html && tag == Tag::Form {
            self.form = context;
        }
        self.reset_mode();
        if ns != Ns::Html {
            return None;
        }
        match tag {
            Tag::Title | Tag::Textarea => Some(TextMode::Rcdata),
            Tag::Style | Tag::Xmp | Tag::Iframe | Tag::Noembed | Tag::Noframes => {
                Some(TextMode::Rawtext)
            }
            Tag::Script => Some(TextMode::ScriptData),
            Tag::Noscript if self.scripting => Some(TextMode::Rawtext),
            Tag::Plaintext => Some(TextMode::Plaintext),
            _ => None,
        }
    }

    /// After a fragment parse: the html element's children become the
    /// document's, and the document becomes a fragment.
    pub(crate) fn finish_fragment(&mut self) {
        let root = self
            .doc
            .first_child(0)
            .expect("the fragment's html element");
        self.doc.detach(root);
        while let Some(child) = self.doc.first_child(root) {
            self.doc.detach(child);
            self.doc.insert(0, child, NONE);
        }
        self.doc.nodes[0].data = Data::Fragment;
    }

    // ---- the stack and the list -------------------------------------------

    fn current(&self) -> Open {
        *self.open.last().expect("an open element")
    }

    fn adjusted_current(&self) -> Option<Open> {
        match self.context {
            Some(c) if self.open.len() == 1 => Some(c),
            _ => self.open.last().copied(),
        }
    }

    fn mark(&mut self, id: NodeId, on: bool) {
        let i = id as usize;
        if let Some(flag) = self.on_stack.get_mut(i) {
            *flag = on;
        } else {
            self.on_stack.resize(i * 2 + 1024, false);
            self.on_stack[i] = on;
        }
    }

    /// Counts `o` as opened (`by` 1) or closed (`by` -1).
    fn count(&mut self, o: Open, by: i32) {
        self.mark(o.id, by > 0);
        if o.ns == Ns::Html && o.tag != Tag::Other {
            self.counts[o.tag as usize] = self.counts[o.tag as usize].wrapping_add_signed(by);
            return;
        }
        let Some(name) = self.doc.element(o.id).map(|e| self.doc.element_name(e)) else {
            return;
        };
        let n = &mut self.names[name_key(o.ns == Ns::Html, name)];
        *n = n.wrapping_add_signed(by);
    }

    /// How many open elements are named `name`, HTML ones of no known tag
    /// (`html`) or foreign ones; or more, when names share a key.
    fn open_named(&self, html: bool, name: &str) -> u32 {
        self.names[name_key(html, name)]
    }

    fn push_open(&mut self, o: Open) {
        self.count(o, 1);
        self.open.push(o);
    }

    fn remove_open_at(&mut self, i: usize) -> Open {
        let o = self.open.remove(i);
        self.count(o, -1);
        o
    }

    fn insert_open_at(&mut self, i: usize, o: Open) {
        self.count(o, 1);
        self.open.insert(i, o);
    }

    fn open_count(&self, tag: Tag) -> u32 {
        self.counts[tag as usize]
    }

    fn pop(&mut self) -> Option<Open> {
        let o = self.open.pop()?;
        self.count(o, -1);
        if o.is(Tag::Option) {
            self.option_popped(o.id);
        }
        Some(o)
    }

    fn truncate_open(&mut self, len: usize) {
        while self.open.len() > len {
            self.pop();
        }
    }

    fn template_open(&self) -> bool {
        self.open_count(Tag::Template) > 0
    }

    /// The spec's "parsing template contents", which the form element
    /// pointer ignores.
    fn in_template_contents(&self) -> bool {
        self.template_open() || self.context.is_some_and(|c| c.is(Tag::Template))
    }

    fn in_scope(&self, tag: Tag, scope: Scope) -> bool {
        if tag != Tag::Other && self.open_count(tag) == 0 {
            return false;
        }
        for &o in self.open.iter().rev() {
            if o.is(tag) {
                return true;
            }
            if scope_boundary(o, scope) {
                return false;
            }
        }
        false
    }

    fn in_scope_where(&self, f: impl Fn(Tag) -> bool, scope: Scope) -> bool {
        for &o in self.open.iter().rev() {
            if o.ns == Ns::Html && f(o.tag) {
                return true;
            }
            if scope_boundary(o, scope) {
                return false;
            }
        }
        false
    }

    fn node_in_scope(&self, id: NodeId) -> bool {
        for &o in self.open.iter().rev() {
            if o.id == id {
                return true;
            }
            if scope_boundary(o, Scope::Default) {
                return false;
            }
        }
        false
    }

    fn pop_until(&mut self, tag: Tag) {
        while let Some(o) = self.pop() {
            if o.is(tag) {
                break;
            }
        }
    }

    fn pop_until_where(&mut self, f: impl Fn(Tag) -> bool) {
        while let Some(o) = self.pop() {
            if o.ns == Ns::Html && f(o.tag) {
                break;
            }
        }
    }

    fn remove_open(&mut self, id: NodeId) {
        if let Some(i) = self.open.iter().rposition(|o| o.id == id) {
            self.remove_open_at(i);
        }
    }

    fn generate_implied(&mut self, except: Option<Tag>) {
        while let Some(&o) = self.open.last() {
            if o.ns == Ns::Html && implied(o.tag) && Some(o.tag) != except {
                self.pop();
            } else {
                break;
            }
        }
    }

    fn generate_implied_thoroughly(&mut self) {
        use Tag::*;
        while let Some(&o) = self.open.last() {
            if o.ns == Ns::Html
                && (implied(o.tag)
                    || matches!(
                        o.tag,
                        Caption | Colgroup | Tbody | Td | Tfoot | Th | Thead | Tr
                    ))
            {
                self.pop();
            } else {
                break;
            }
        }
    }

    fn close_p(&mut self) {
        self.generate_implied(Some(Tag::P));
        self.pop_until(Tag::P);
    }

    fn close_p_in_button_scope(&mut self) {
        if self.in_scope(Tag::P, Scope::Button) {
            self.close_p();
        }
    }

    fn clear_to_table_context(&mut self) {
        while !matches!(self.current().tag, Tag::Table | Tag::Template | Tag::Html)
            || self.current().ns != Ns::Html
        {
            self.pop();
        }
    }

    fn clear_to_table_body_context(&mut self) {
        use Tag::*;
        while !matches!(self.current().tag, Tbody | Tfoot | Thead | Template | Html)
            || self.current().ns != Ns::Html
        {
            self.pop();
        }
    }

    fn clear_to_table_row_context(&mut self) {
        while !matches!(self.current().tag, Tag::Tr | Tag::Template | Tag::Html)
            || self.current().ns != Ns::Html
        {
            self.pop();
        }
    }

    fn reset_mode(&mut self) {
        use Tag::*;
        for i in (0..self.open.len()).rev() {
            let last = i == 0;
            let mut node = self.open[i];
            if last && let Some(c) = self.context {
                node = c;
            }
            if node.ns != Ns::Html {
                if last {
                    self.mode = Mode::InBody;
                    return;
                }
                continue;
            }
            self.mode = match node.tag {
                Td | Th if !last => Mode::InCell,
                Tr => Mode::InRow,
                Tbody | Thead | Tfoot => Mode::InTableBody,
                Caption => Mode::InCaption,
                Colgroup => Mode::InColumnGroup,
                Table => Mode::InTable,
                Template => *self.template_modes.last().unwrap_or(&Mode::InBody),
                Head if !last => Mode::InHead,
                Body => Mode::InBody,
                Frameset => Mode::InFrameset,
                Html => {
                    if self.head == NONE {
                        Mode::BeforeHead
                    } else {
                        Mode::AfterHead
                    }
                }
                _ if last => Mode::InBody,
                _ => continue,
            };
            return;
        }
    }

    fn open_contains(&self, id: NodeId) -> bool {
        self.on_stack.get(id as usize).copied().unwrap_or(false)
    }

    fn same_element(&self, a: NodeId, b: NodeId) -> bool {
        let (Some(x), Some(y)) = (self.doc.element(a), self.doc.element(b)) else {
            return false;
        };
        if x.ns != y.ns
            || x.tag != y.tag
            || self.doc.element_name(x) != self.doc.element_name(y)
            || x.attrs.end - x.attrs.start != y.attrs.end - y.attrs.start
        {
            return false;
        }
        let ys = &self.doc.attrs[y.attrs.start as usize..y.attrs.end as usize];
        self.doc.attrs[x.attrs.start as usize..x.attrs.end as usize]
            .iter()
            .all(|a| {
                ys.iter().any(|b| {
                    a.ns == b.ns
                        && self.doc.str(a.name) == self.doc.str(b.name)
                        && self.doc.str(a.value) == self.doc.str(b.value)
                })
            })
    }

    fn push_active(&mut self, id: NodeId) {
        let start = self
            .active
            .iter()
            .rposition(|&e| e == MARKER)
            .map_or(0, |i| i + 1);
        let same: Vec<usize> = (start..self.active.len())
            .filter(|&i| self.same_element(self.active[i], id))
            .collect();
        if same.len() >= 3 {
            self.active.remove(same[0]);
        }
        self.active.push(id);
    }

    fn clear_active_to_marker(&mut self) {
        while let Some(e) = self.active.pop() {
            if e == MARKER {
                break;
            }
        }
    }

    fn reconstruct(&mut self) {
        let Some(&last) = self.active.last() else {
            return;
        };
        if last == MARKER || self.open_contains(last) {
            return;
        }
        let mut i = self.active.len() - 1;
        while i > 0 {
            let e = self.active[i - 1];
            if e == MARKER || self.open_contains(e) {
                break;
            }
            i -= 1;
        }
        for j in i..self.active.len() {
            let new = self.clone_element(self.active[j]);
            let place = self.place(None);
            self.insert_at(place, new);
            let e = self.doc.element(new).unwrap();
            let (ns, tag) = (e.ns, e.tag);
            self.push_open(Open { id: new, ns, tag });
            self.active[j] = new;
        }
    }

    // ---- creating and inserting -------------------------------------------

    fn clone_element(&mut self, id: NodeId) -> NodeId {
        let e = *self.doc.element(id).expect("an element");
        self.doc.add(Data::Element(Element {
            contents: NONE,
            ..e
        }))
    }

    fn create(&mut self, s: &Start<'_>, ns: Ns) -> NodeId {
        let name = match ns {
            Ns::Svg => match svg_tag_name(s.name) {
                Some(fixed) => Name::Svg(fixed),
                None if s.tag != Tag::Other => Name::Tag,
                None => Name::Span(self.doc.intern(s.name)),
            },
            _ if s.tag != Tag::Other => Name::Tag,
            _ => Name::Span(self.doc.intern(s.name)),
        };
        let first = self.doc.attrs.len() as u32;
        let mut html_integration = false;
        for (n, v) in s.attrs() {
            let (attr_ns, local) = if ns == Ns::Html {
                (AttrNs::None, n)
            } else if let Some((attr_ns, _)) = foreign_attr(n) {
                // Kept as written (`xlink:href`); attrs() gives the local
                // name, attrs_written() this.
                (attr_ns, n)
            } else {
                // Lowercase as tokenized: attrs_written() fixes SVG's and
                // MathML's case.
                (AttrNs::None, n)
            };
            if ns == Ns::MathMl
                && s.tag == Tag::AnnotationXml
                && n == "encoding"
                && (v.eq_ignore_ascii_case("text/html")
                    || v.eq_ignore_ascii_case("application/xhtml+xml"))
            {
                html_integration = true;
            }
            let name = self.doc.intern(local);
            let value = self.doc.intern(v);
            self.doc.attrs.push(Attr {
                ns: attr_ns,
                name,
                value,
            });
        }
        let contents = if ns == Ns::Html && s.tag == Tag::Template {
            self.doc.add(Data::Fragment)
        } else {
            NONE
        };
        self.doc.add(Data::Element(Element {
            ns,
            tag: s.tag,
            name,
            attrs: Span {
                start: first,
                end: self.doc.attrs.len() as u32,
            },
            contents,
            html_integration,
        }))
    }

    fn element_at(&self, id: NodeId) -> Option<Open> {
        self.doc.element(id).map(|e| Open {
            id,
            ns: e.ns,
            tag: e.tag,
        })
    }

    /// The appropriate place for inserting a node: a parent, and the child
    /// to insert before ([`NONE`] for last).
    fn place(&self, target: Option<NodeId>) -> (NodeId, NodeId) {
        use Tag::*;
        let mut target = target.unwrap_or_else(|| self.current().id);
        let mut before = NONE;
        let t = self.element_at(target);
        if self.foster
            && t.is_some_and(|t| {
                t.ns == Ns::Html && matches!(t.tag, Table | Tbody | Tfoot | Thead | Tr)
            })
        {
            let last = self
                .open
                .iter()
                .rposition(|o| o.is(Template) || o.is(Table));
            match last {
                None => return (self.open[0].id, NONE),
                Some(i) if self.open[i].is(Template) => target = self.open[i].id,
                Some(i) => {
                    let table = self.open[i].id;
                    match self.doc.parent(table) {
                        Some(parent) => {
                            target = parent;
                            before = table;
                        }
                        None => target = self.open[i - 1].id,
                    }
                }
            }
        }
        if let Some(e) = self.doc.element(target)
            && e.ns == Ns::Html
            && e.tag == Template
        {
            return (e.contents, NONE);
        }
        (target, before)
    }

    fn insert_at(&mut self, (parent, before): (NodeId, NodeId), node: NodeId) {
        if parent == 0
            && matches!(self.doc.nodes[0].data, Data::Document)
            && self.doc.children(0).any(|c| self.doc.element(c).is_some())
        {
            return;
        }
        self.doc.insert(parent, node, before);
    }

    fn insert_element(&mut self, s: &Start<'_>, ns: Ns) -> NodeId {
        let id = self.create(s, ns);
        let place = self.place(None);
        self.insert_at(place, id);
        self.push_open(Open { id, ns, tag: s.tag });
        if ns == Ns::Html {
            match s.tag {
                Tag::Option => self.option_inserted(id, s),
                Tag::Selectedcontent => self.selectedcontent_inserted(id),
                _ => {}
            }
        }
        id
    }

    /// Stop parsing: every open element is popped.
    fn stop(&mut self) {
        self.done = true;
        self.truncate_open(0);
    }

    // ---- <select> and <selectedcontent> -----------------------------------

    fn html_element(&self, id: NodeId, tag: Tag) -> bool {
        self.doc
            .element(id)
            .is_some_and(|e| e.ns == Ns::Html && e.tag == tag)
    }

    fn has_attr(&self, id: NodeId, name: &str) -> bool {
        self.doc
            .attrs(id)
            .any(|(ns, n, _)| ns == AttrNs::None && n == name)
    }

    /// The spec's "nearest ancestor select" of an option.
    fn nearest_select(&self, id: NodeId) -> NodeId {
        // An option is inserted where the parser is, under open elements,
        // so with no select open there is none above it; and the walk
        // below would go all the way up to say so.
        if self.open_count(Tag::Select) == 0 {
            return NONE;
        }
        let mut optgroup = false;
        let mut at = self.doc.parent(id);
        while let Some(a) = at {
            if let Some(e) = self.doc.element(a)
                && e.ns == Ns::Html
            {
                match e.tag {
                    Tag::Datalist | Tag::Hr | Tag::Option => return NONE,
                    Tag::Optgroup if optgroup => return NONE,
                    Tag::Optgroup => optgroup = true,
                    Tag::Select => return a,
                    _ => {}
                }
            }
            at = self.doc.parent(a);
        }
        NONE
    }

    /// The selectedness setting algorithm, as options arrive in tree
    /// order: the last with `selected` wins, else the first enabled one.
    // ponytail: assumes options are inserted in tree order, as parsing
    // does; one moved by the adoption agency or foster parenting keeps
    // its place in this order.
    fn option_inserted(&mut self, option: NodeId, s: &Start<'_>) {
        let select = self.nearest_select(option);
        if select == NONE {
            return;
        }
        self.option_select.insert(option, select);
        if self.has_attr(select, "multiple") {
            return;
        }
        let size_one = self
            .doc
            .attrs(select)
            .find(|(ns, n, _)| *ns == AttrNs::None && *n == "size")
            .and_then(|(_, _, v)| v.trim().parse::<u32>().ok())
            .is_none_or(|n| n <= 1);
        let disabled = s.attr("disabled").is_some()
            || self.doc.parent(option).is_some_and(|p| {
                self.html_element(p, Tag::Optgroup) && self.has_attr(p, "disabled")
            });
        if s.attr("selected").is_some()
            || (!self.selected.contains_key(&select) && size_one && !disabled)
        {
            self.selected.insert(select, option);
        }
    }

    fn option_popped(&mut self, option: NodeId) {
        let Some(&select) = self.option_select.get(&option) else {
            return;
        };
        if self.selected.get(&select) != Some(&option) || self.has_attr(select, "multiple") {
            return;
        }
        for target in self.selectedcontents(select) {
            self.clone_children_into(option, target);
        }
    }

    fn selectedcontent_inserted(&mut self, id: NodeId) {
        let mut select = NONE;
        let mut at = self.doc.parent(id);
        while let Some(a) = at {
            if self.html_element(a, Tag::Select) {
                if select != NONE {
                    return;
                }
                select = a;
            } else if self.html_element(a, Tag::Option)
                || self.html_element(a, Tag::Selectedcontent)
            {
                return;
            }
            at = self.doc.parent(a);
        }
        if select == NONE || self.has_attr(select, "multiple") {
            return;
        }
        if let Some(&option) = self.selected.get(&select) {
            self.clone_children_into(option, id);
        }
    }

    /// The `<selectedcontent>` elements a select updates, in tree order.
    fn selectedcontents(&self, select: NodeId) -> Vec<NodeId> {
        let mut found = Vec::new();
        let mut stack: Vec<NodeId> = self.doc.children(select).collect();
        stack.reverse();
        while let Some(n) = stack.pop() {
            if let Some(e) = self.doc.element(n)
                && e.ns == Ns::Html
            {
                match e.tag {
                    Tag::Selectedcontent => {
                        found.push(n);
                        continue;
                    }
                    Tag::Option | Tag::Select => continue,
                    _ => {}
                }
            }
            let at = stack.len();
            stack.extend(self.doc.children(n));
            stack[at..].reverse();
        }
        found
    }

    fn clone_children_into(&mut self, from: NodeId, to: NodeId) {
        while let Some(child) = self.doc.first_child(to) {
            self.doc.detach(child);
        }
        let children: Vec<NodeId> = self.doc.children(from).collect();
        for child in children {
            let copy = self.deep_clone(child);
            self.doc.insert(to, copy, NONE);
        }
    }

    /// Copies `id` and everything under it, in tree order, with a stack
    /// of its own: the page decides how deep it goes, and a call per
    /// level overflowed the thread's stack.
    fn deep_clone(&mut self, id: NodeId) -> NodeId {
        let mut root = NONE;
        let mut stack = vec![(id, NONE)];
        while let Some((from, parent)) = stack.pop() {
            let mut data = self.doc.nodes[from as usize].data;
            if let Data::Element(e) = &mut data {
                e.contents = NONE;
            }
            let copy = self.doc.add(data);
            if parent == NONE {
                root = copy;
            } else {
                self.doc.insert(parent, copy, NONE);
            }
            let at = stack.len();
            stack.extend(self.doc.children(from).map(|c| (c, copy)));
            stack[at..].reverse();
        }
        root
    }

    fn insert_html(&mut self, s: &Start<'_>) -> NodeId {
        self.insert_element(s, Ns::Html)
    }

    fn insert_void(&mut self, s: &Start<'_>) {
        self.insert_html(s);
        self.pop();
    }

    fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let (parent, before) = self.place(None);
        if parent == 0 && matches!(self.doc.nodes[0].data, Data::Document) {
            return;
        }
        let prev = if before == NONE {
            self.doc.nodes[parent as usize].last_child
        } else {
            self.doc.nodes[before as usize].prev
        };
        if prev != NONE && matches!(self.doc.nodes[prev as usize].data, Data::Text(_)) {
            self.doc.append_text(prev, text);
        } else {
            let span = self.doc.intern(text);
            let id = self.doc.add(Data::Text(span));
            self.doc.insert(parent, id, before);
        }
    }

    fn insert_comment(&mut self, text: &str, at: Option<(NodeId, NodeId)>) {
        let place = at.unwrap_or_else(|| self.place(None));
        let span = self.doc.intern(text);
        let id = self.doc.add(Data::Comment(span));
        self.doc.insert(place.0, id, place.1);
    }

    fn insert_pi(&mut self, target: &str, data: &str, at: Option<(NodeId, NodeId)>) {
        let place = at.unwrap_or_else(|| self.place(None));
        let target = self.doc.intern(target);
        let data = self.doc.intern(data);
        let id = self.doc.add(Data::ProcessingInstruction { target, data });
        self.doc.insert(place.0, id, place.1);
    }

    fn rawtext(&mut self, s: &Start<'_>, mode: TextMode) {
        self.insert_html(s);
        self.switch = Some(mode);
        self.original = self.mode;
        self.mode = Mode::Text;
    }

    /// Adds the token's attributes that `id` doesn't already have, as a
    /// second `<html>` or `<body>` does.
    fn merge_attrs(&mut self, id: NodeId, s: &Start<'_>) {
        for (n, v) in s.attrs() {
            if self.doc.attrs(id).any(|(_, name, _)| name == n) {
                continue;
            }
            let name = self.doc.intern(n);
            let value = self.doc.intern(v);
            let attr = Attr {
                ns: AttrNs::None,
                name,
                value,
            };
            let len = self.doc.attrs.len() as u32;
            let Data::Element(e) = &mut self.doc.nodes[id as usize].data else {
                return;
            };
            if e.attrs.end == len {
                e.attrs.end += 1;
                self.doc.attrs.push(attr);
            } else {
                let old = e.attrs;
                e.attrs = Span {
                    start: len,
                    end: len + (old.end - old.start) + 1,
                };
                self.doc
                    .attrs
                    .extend_from_within(old.start as usize..old.end as usize);
                self.doc.attrs.push(attr);
            }
        }
    }

    // ---- the dispatcher ---------------------------------------------------

    fn process(&mut self, t: Tok<'_>) {
        if self.done {
            return;
        }
        if self.html_rules(&t) {
            self.html(t);
        } else {
            self.foreign(t);
        }
    }

    fn html_rules(&self, t: &Tok<'_>) -> bool {
        let Some(a) = self.adjusted_current() else {
            return true;
        };
        if a.ns == Ns::Html {
            return true;
        }
        let html_integration = match a.ns {
            Ns::Svg => matches!(a.tag, Tag::ForeignObject | Tag::Desc | Tag::Title),
            Ns::MathMl => self.doc.element(a.id).is_some_and(|e| e.html_integration),
            Ns::Html | Ns::Xml => false,
        };
        match t {
            Tok::Start(s) => {
                (mathml_text_integration(a) && !matches!(s.tag, Tag::Mglyph | Tag::Malignmark))
                    || (a.ns == Ns::MathMl && a.tag == Tag::AnnotationXml && s.tag == Tag::Svg)
                    || html_integration
            }
            Tok::Text(_) => mathml_text_integration(a) || html_integration,
            Tok::Eof => true,
            _ => false,
        }
    }

    fn html(&mut self, t: Tok<'_>) {
        match self.mode {
            Mode::Initial => self.initial(t),
            Mode::BeforeHtml => self.before_html(t),
            Mode::BeforeHead => self.before_head(t),
            Mode::InHead => self.in_head(t),
            Mode::InHeadNoscript => self.in_head_noscript(t),
            Mode::AfterHead => self.after_head(t),
            Mode::InBody => self.in_body(t),
            Mode::Text => self.text(t),
            Mode::InTable => self.in_table(t),
            Mode::InTableText => self.in_table_text(t),
            Mode::InCaption => self.in_caption(t),
            Mode::InColumnGroup => self.in_column_group(t),
            Mode::InTableBody => self.in_table_body(t),
            Mode::InRow => self.in_row(t),
            Mode::InCell => self.in_cell(t),
            Mode::InTemplate => self.in_template(t),
            Mode::AfterBody => self.after_body(t),
            Mode::InFrameset => self.in_frameset(t),
            Mode::AfterFrameset => self.after_frameset(t),
            Mode::AfterAfterBody => self.after_after_body(t),
            Mode::AfterAfterFrameset => self.after_after_frameset(t),
        }
    }

    fn reprocess(&mut self, mode: Mode, t: Tok<'_>) {
        self.mode = mode;
        self.process(t);
    }

    // ---- the insertion modes ----------------------------------------------

    fn initial(&mut self, t: Tok<'_>) {
        match t {
            Tok::Text(s) => {
                let (_, rest) = split_space(s);
                if !rest.is_empty() {
                    self.doc.quirks = Quirks::Quirks;
                    self.reprocess(Mode::BeforeHtml, Tok::Text(rest));
                }
            }
            Tok::Comment(c) => self.insert_comment(c, Some((0, NONE))),
            Tok::Pi(target, data) => self.insert_pi(target, data, Some((0, NONE))),
            Tok::Doctype(d) => {
                let name = self.doc.intern(d.name.unwrap_or(""));
                let public_id = self.doc.intern(d.public_id.unwrap_or(""));
                let system_id = self.doc.intern(d.system_id.unwrap_or(""));
                let id = self.doc.add(Data::Doctype {
                    name,
                    public_id,
                    system_id,
                });
                self.doc.insert(0, id, NONE);
                self.doc.quirks = doctype_quirks(d);
                self.mode = Mode::BeforeHtml;
            }
            _ => {
                self.doc.quirks = Quirks::Quirks;
                self.reprocess(Mode::BeforeHtml, t);
            }
        }
    }

    fn before_html(&mut self, t: Tok<'_>) {
        match t {
            Tok::Doctype(_) => {}
            Tok::Comment(c) => self.insert_comment(c, Some((0, NONE))),
            Tok::Pi(target, data) => self.insert_pi(target, data, Some((0, NONE))),
            Tok::Text(s) => {
                let (_, rest) = split_space(s);
                if !rest.is_empty() {
                    self.before_html_else(Tok::Text(rest));
                }
            }
            Tok::Start(s) if s.tag == Tag::Html => {
                let id = self.create(&s, Ns::Html);
                self.insert_at((0, NONE), id);
                self.push_open(Open {
                    id,
                    ns: Ns::Html,
                    tag: Tag::Html,
                });
                self.mode = Mode::BeforeHead;
            }
            Tok::End(e) if !matches!(e.tag, Tag::Head | Tag::Body | Tag::Html | Tag::Br) => {}
            _ => self.before_html_else(t),
        }
    }

    fn before_html_else(&mut self, t: Tok<'_>) {
        let id = self.create(&Start::bare(Tag::Html), Ns::Html);
        self.insert_at((0, NONE), id);
        self.push_open(Open {
            id,
            ns: Ns::Html,
            tag: Tag::Html,
        });
        self.reprocess(Mode::BeforeHead, t);
    }

    fn before_head(&mut self, t: Tok<'_>) {
        match t {
            Tok::Text(s) => {
                let (_, rest) = split_space(s);
                if !rest.is_empty() {
                    self.before_head_else(Tok::Text(rest));
                }
            }
            Tok::Comment(c) => self.insert_comment(c, None),
            Tok::Pi(target, data) => self.insert_pi(target, data, None),
            Tok::Doctype(_) => {}
            Tok::Start(s) if s.tag == Tag::Html => self.in_body(t),
            Tok::Start(s) if s.tag == Tag::Head => {
                self.head = self.insert_html(&s);
                self.mode = Mode::InHead;
            }
            Tok::End(e) if !matches!(e.tag, Tag::Head | Tag::Body | Tag::Html | Tag::Br) => {}
            _ => self.before_head_else(t),
        }
    }

    fn before_head_else(&mut self, t: Tok<'_>) {
        self.head = self.insert_html(&Start::bare(Tag::Head));
        self.reprocess(Mode::InHead, t);
    }

    fn in_head(&mut self, t: Tok<'_>) {
        use Tag::*;
        match t {
            Tok::Text(s) => {
                let (space, rest) = split_space(s);
                self.insert_text(space);
                if !rest.is_empty() {
                    self.in_head_else(Tok::Text(rest));
                }
            }
            Tok::Comment(c) => self.insert_comment(c, None),
            Tok::Pi(target, data) => self.insert_pi(target, data, None),
            Tok::Doctype(_) => {}
            Tok::Start(s) => match s.tag {
                Html => self.in_body(t),
                Base | Basefont | Bgsound | Link | Meta => self.insert_void(&s),
                Title => self.rawtext(&s, TextMode::Rcdata),
                Noscript if self.scripting => self.rawtext(&s, TextMode::Rawtext),
                Noframes | Style => self.rawtext(&s, TextMode::Rawtext),
                Noscript => {
                    self.insert_html(&s);
                    self.mode = Mode::InHeadNoscript;
                }
                Script => self.rawtext(&s, TextMode::ScriptData),
                Template => {
                    self.active.push(MARKER);
                    self.frameset_ok = false;
                    self.mode = Mode::InTemplate;
                    self.template_modes.push(Mode::InTemplate);
                    self.insert_html(&s);
                }
                Head => {}
                _ => self.in_head_else(t),
            },
            Tok::End(e) => match e.tag {
                Head => {
                    self.pop();
                    self.mode = Mode::AfterHead;
                }
                Body | Html | Br => self.in_head_else(t),
                Template => {
                    if !self.template_open() {
                        return;
                    }
                    self.generate_implied_thoroughly();
                    self.pop_until(Template);
                    self.clear_active_to_marker();
                    self.template_modes.pop();
                    self.reset_mode();
                }
                _ => {}
            },
            Tok::Eof => self.in_head_else(t),
        }
    }

    fn in_head_else(&mut self, t: Tok<'_>) {
        self.pop();
        self.reprocess(Mode::AfterHead, t);
    }

    fn in_head_noscript(&mut self, t: Tok<'_>) {
        use Tag::*;
        match t {
            Tok::Doctype(_) => {}
            Tok::Start(s) if s.tag == Html => self.in_body(t),
            Tok::End(e) if e.tag == Noscript => {
                self.pop();
                self.mode = Mode::InHead;
            }
            Tok::Text(s) => {
                let (space, rest) = split_space(s);
                if !space.is_empty() {
                    self.in_head(Tok::Text(space));
                }
                if !rest.is_empty() {
                    self.in_head_noscript_else(Tok::Text(rest));
                }
            }
            Tok::Comment(_) | Tok::Pi(..) => self.in_head(t),
            Tok::Start(s)
                if matches!(s.tag, Basefont | Bgsound | Link | Meta | Noframes | Style) =>
            {
                self.in_head(t)
            }
            Tok::End(e) if e.tag == Br => self.in_head_noscript_else(t),
            Tok::Start(s) if matches!(s.tag, Head | Noscript) => {}
            Tok::End(_) => {}
            _ => self.in_head_noscript_else(t),
        }
    }

    fn in_head_noscript_else(&mut self, t: Tok<'_>) {
        self.pop();
        self.reprocess(Mode::InHead, t);
    }

    fn after_head(&mut self, t: Tok<'_>) {
        use Tag::*;
        match t {
            Tok::Text(s) => {
                let (space, rest) = split_space(s);
                self.insert_text(space);
                if !rest.is_empty() {
                    self.after_head_else(Tok::Text(rest));
                }
            }
            Tok::Comment(c) => self.insert_comment(c, None),
            Tok::Pi(target, data) => self.insert_pi(target, data, None),
            Tok::Doctype(_) => {}
            Tok::Start(s) => match s.tag {
                Html => self.in_body(t),
                Body => {
                    self.insert_html(&s);
                    self.frameset_ok = false;
                    self.mode = Mode::InBody;
                }
                Frameset => {
                    self.insert_html(&s);
                    self.mode = Mode::InFrameset;
                }
                Base | Basefont | Bgsound | Link | Meta | Noframes | Script | Style | Template
                | Title => {
                    let head = self.head;
                    self.push_open(Open {
                        id: head,
                        ns: Ns::Html,
                        tag: Head,
                    });
                    self.in_head(t);
                    self.remove_open(head);
                }
                Head => {}
                _ => self.after_head_else(t),
            },
            Tok::End(e) => match e.tag {
                Template => self.in_head(t),
                Body | Html | Br => self.after_head_else(t),
                _ => {}
            },
            Tok::Eof => self.after_head_else(t),
        }
    }

    fn after_head_else(&mut self, t: Tok<'_>) {
        self.insert_html(&Start::bare(Tag::Body));
        self.frameset_ok = true;
        self.reprocess(Mode::InBody, t);
    }

    fn body_text(&mut self, s: &str) {
        if memchr::memchr(0, s.as_bytes()).is_none() {
            self.reconstruct();
            self.insert_text(s);
            if self.frameset_ok && !all_space(s) {
                self.frameset_ok = false;
            }
            return;
        }
        for piece in s.split('\0').filter(|p| !p.is_empty()) {
            self.body_text(piece);
        }
    }

    fn in_body(&mut self, t: Tok<'_>) {
        match t {
            Tok::Text(s) => self.body_text(s),
            Tok::Comment(c) => self.insert_comment(c, None),
            Tok::Pi(target, data) => self.insert_pi(target, data, None),
            Tok::Doctype(_) => {}
            Tok::Start(s) => self.in_body_start(s),
            Tok::End(e) => self.in_body_end(e),
            Tok::Eof => {
                if !self.template_modes.is_empty() {
                    self.in_template(t);
                } else {
                    self.stop();
                }
            }
        }
    }

    fn in_body_start(&mut self, s: Start<'_>) {
        use Tag::*;
        match s.tag {
            Html => {
                if !self.template_open() {
                    let root = self.open[0].id;
                    self.merge_attrs(root, &s);
                }
            }
            Base | Basefont | Bgsound | Link | Meta | Noframes | Script | Style | Template
            | Title => self.in_head(Tok::Start(s)),
            Body => {
                if self.open.len() == 1 || !self.open[1].is(Body) || self.template_open() {
                    return;
                }
                self.frameset_ok = false;
                let body = self.open[1].id;
                self.merge_attrs(body, &s);
            }
            Frameset => {
                if self.open.len() == 1 || !self.open[1].is(Body) || !self.frameset_ok {
                    return;
                }
                let body = self.open[1].id;
                self.doc.detach(body);
                self.truncate_open(1);
                self.insert_html(&s);
                self.mode = Mode::InFrameset;
            }
            Address | Article | Aside | Blockquote | Center | Details | Dialog | Dir | Div | Dl
            | Fieldset | Figcaption | Figure | Footer | Header | Hgroup | Main | Menu | Nav
            | Ol | P | Search | Section | Summary | Ul => {
                self.close_p_in_button_scope();
                self.insert_html(&s);
            }
            H1 | H2 | H3 | H4 | H5 | H6 => {
                self.close_p_in_button_scope();
                let c = self.current();
                if c.ns == Ns::Html && heading(c.tag) {
                    self.pop();
                }
                self.insert_html(&s);
            }
            Pre | Listing => {
                self.close_p_in_button_scope();
                self.insert_html(&s);
                self.skip_newline = true;
                self.frameset_ok = false;
            }
            Form => {
                let template = self.in_template_contents();
                if self.form != NONE && !template {
                    return;
                }
                self.close_p_in_button_scope();
                let id = self.insert_html(&s);
                if !template {
                    self.form = id;
                }
            }
            Li | Dd | Dt => {
                self.frameset_ok = false;
                for i in (0..self.open.len()).rev() {
                    let node = self.open[i];
                    let closes = match s.tag {
                        Li => node.is(Li),
                        _ => node.is(Dd) || node.is(Dt),
                    };
                    if closes {
                        self.generate_implied(Some(node.tag));
                        self.pop_until(node.tag);
                        break;
                    }
                    if special(node) && !node.is(Address) && !node.is(Div) && !node.is(P) {
                        break;
                    }
                }
                self.close_p_in_button_scope();
                self.insert_html(&s);
            }
            Plaintext => {
                self.close_p_in_button_scope();
                self.insert_html(&s);
                self.switch = Some(TextMode::Plaintext);
            }
            Button => {
                if self.in_scope(Button, Scope::Default) {
                    self.generate_implied(None);
                    self.pop_until(Button);
                }
                self.reconstruct();
                self.insert_html(&s);
                self.frameset_ok = false;
            }
            A => {
                let start = self
                    .active
                    .iter()
                    .rposition(|&e| e == MARKER)
                    .map_or(0, |i| i + 1);
                let existing = self.active[start..].iter().rev().copied().find(|&e| {
                    self.doc
                        .element(e)
                        .is_some_and(|el| el.ns == Ns::Html && el.tag == A)
                });
                if let Some(a) = existing {
                    if self.adoption_agency(A, "a") {
                        self.any_other_end(End { tag: A, name: "a" });
                    }
                    self.active.retain(|&e| e != a);
                    self.remove_open(a);
                }
                self.reconstruct();
                let id = self.insert_html(&s);
                self.push_active(id);
            }
            B | Big | Code | Em | Font | I | S | Small | Strike | Strong | Tt | U => {
                self.reconstruct();
                let id = self.insert_html(&s);
                self.push_active(id);
            }
            Nobr => {
                self.reconstruct();
                if self.in_scope(Nobr, Scope::Default) {
                    if self.adoption_agency(Nobr, "nobr") {
                        self.any_other_end(End {
                            tag: Nobr,
                            name: "nobr",
                        });
                    }
                    self.reconstruct();
                }
                let id = self.insert_html(&s);
                self.push_active(id);
            }
            Applet | Marquee | Object => {
                self.reconstruct();
                self.insert_html(&s);
                self.active.push(MARKER);
                self.frameset_ok = false;
            }
            Table => {
                if self.doc.quirks != Quirks::Quirks {
                    self.close_p_in_button_scope();
                }
                self.insert_html(&s);
                self.frameset_ok = false;
                self.mode = Mode::InTable;
            }
            Area | Br | Embed | Img | Keygen | Wbr => {
                self.reconstruct();
                self.insert_void(&s);
                self.frameset_ok = false;
            }
            Input => {
                if self.context.is_some_and(|c| c.is(Select)) {
                    return;
                }
                if self.in_scope(Select, Scope::Default) {
                    self.pop_until(Select);
                }
                self.reconstruct();
                self.insert_void(&s);
                if !s
                    .attr("type")
                    .is_some_and(|t| t.eq_ignore_ascii_case("hidden"))
                {
                    self.frameset_ok = false;
                }
            }
            Param | Source | Track => self.insert_void(&s),
            Hr => {
                self.close_p_in_button_scope();
                if self.in_scope(Select, Scope::Default) {
                    self.generate_implied(None);
                }
                self.insert_void(&s);
                self.frameset_ok = false;
            }
            Image => self.process(Tok::Start(Start {
                tag: Img,
                name: "img",
                ..s
            })),
            Textarea => {
                self.insert_html(&s);
                self.skip_newline = true;
                self.switch = Some(TextMode::Rcdata);
                self.original = self.mode;
                self.frameset_ok = false;
                self.mode = Mode::Text;
            }
            Xmp => {
                self.close_p_in_button_scope();
                self.reconstruct();
                self.frameset_ok = false;
                self.rawtext(&s, TextMode::Rawtext);
            }
            Iframe => {
                self.frameset_ok = false;
                self.rawtext(&s, TextMode::Rawtext);
            }
            Noembed => self.rawtext(&s, TextMode::Rawtext),
            Noscript if self.scripting => self.rawtext(&s, TextMode::Rawtext),
            Select => {
                if self.context.is_some_and(|c| c.is(Select)) {
                    return;
                }
                if self.in_scope(Select, Scope::Default) {
                    self.pop_until(Select);
                    return;
                }
                self.reconstruct();
                self.insert_html(&s);
                self.frameset_ok = false;
            }
            Option => {
                if self.in_scope(Select, Scope::Default) {
                    self.generate_implied(Some(Optgroup));
                } else if self.current().is(Option) {
                    self.pop();
                }
                self.reconstruct();
                self.insert_html(&s);
            }
            Optgroup => {
                if self.in_scope(Select, Scope::Default) {
                    self.generate_implied(None);
                } else if self.current().is(Option) {
                    self.pop();
                }
                self.reconstruct();
                self.insert_html(&s);
            }
            Rb | Rtc => {
                if self.in_scope(Ruby, Scope::Default) {
                    self.generate_implied(None);
                }
                self.insert_html(&s);
            }
            Rp | Rt => {
                if self.in_scope(Ruby, Scope::Default) {
                    self.generate_implied(Some(Rtc));
                }
                self.insert_html(&s);
            }
            Math | Svg => {
                self.reconstruct();
                let ns = if s.tag == Math { Ns::MathMl } else { Ns::Svg };
                self.insert_element(&s, ns);
                if s.self_closing {
                    self.pop();
                }
            }
            Caption | Col | Colgroup | Frame | Head | Tbody | Td | Tfoot | Th | Thead | Tr => {}
            _ => {
                self.reconstruct();
                self.insert_html(&s);
            }
        }
    }

    fn in_body_end(&mut self, e: End<'_>) {
        use Tag::*;
        match e.tag {
            Template => self.in_head(Tok::End(e)),
            Body => {
                if self.in_scope(Body, Scope::Default) {
                    self.mode = Mode::AfterBody;
                }
            }
            Html => {
                if self.in_scope(Body, Scope::Default) {
                    self.reprocess(Mode::AfterBody, Tok::End(e));
                }
            }
            Address | Article | Aside | Blockquote | Button | Center | Details | Dialog | Dir
            | Div | Dl | Fieldset | Figcaption | Figure | Footer | Header | Hgroup | Listing
            | Main | Menu | Nav | Ol | Pre | Search | Section | Select | Summary | Ul => {
                if self.in_scope(e.tag, Scope::Default) {
                    self.generate_implied(None);
                    self.pop_until(e.tag);
                }
            }
            Form => {
                if !self.in_template_contents() {
                    let node = self.form;
                    self.form = NONE;
                    if node == NONE || !self.node_in_scope(node) {
                        return;
                    }
                    self.generate_implied(None);
                    self.remove_open(node);
                } else if self.in_scope(Form, Scope::Default) {
                    self.generate_implied(None);
                    self.pop_until(Form);
                }
            }
            P => {
                if !self.in_scope(P, Scope::Button) {
                    self.insert_html(&Start::bare(P));
                }
                self.close_p();
            }
            Li => {
                if self.in_scope(Li, Scope::ListItem) {
                    self.generate_implied(Some(Li));
                    self.pop_until(Li);
                }
            }
            Dd | Dt => {
                if self.in_scope(e.tag, Scope::Default) {
                    self.generate_implied(Some(e.tag));
                    self.pop_until(e.tag);
                }
            }
            H1 | H2 | H3 | H4 | H5 | H6 => {
                if self.in_scope_where(heading, Scope::Default) {
                    self.generate_implied(None);
                    self.pop_until_where(heading);
                }
            }
            A | B | Big | Code | Em | Font | I | Nobr | S | Small | Strike | Strong | Tt | U => {
                if self.adoption_agency(e.tag, e.name) {
                    self.any_other_end(e);
                }
            }
            Applet | Marquee | Object => {
                if self.in_scope(e.tag, Scope::Default) {
                    self.generate_implied(None);
                    self.pop_until(e.tag);
                    self.clear_active_to_marker();
                }
            }
            Br => self.in_body_start(Start::bare(Br)),
            _ => self.any_other_end(e),
        }
    }

    fn names_match(&self, o: Open, e: &End<'_>) -> bool {
        if o.ns != Ns::Html {
            return false;
        }
        if e.tag != Tag::Other || o.tag != Tag::Other {
            return o.tag == e.tag;
        }
        self.doc
            .element(o.id)
            .is_some_and(|el| self.doc.element_name(el) == e.name)
    }

    fn any_other_end(&mut self, e: End<'_>) {
        // With no open element of that name the walk below can only end
        // without doing anything, after going through the whole stack.
        let open = match e.tag {
            Tag::Other => self.open_named(true, e.name),
            tag => self.open_count(tag),
        };
        if open == 0 {
            return;
        }
        for i in (0..self.open.len()).rev() {
            let node = self.open[i];
            if self.names_match(node, &e) {
                self.generate_implied(Some(node.tag).filter(|&t| t != Tag::Other));
                self.truncate_open(i);
                return;
            }
            if special(node) {
                return;
            }
        }
    }

    /// The adoption agency algorithm. True when the end tag is to be
    /// handled as any other end tag instead.
    fn adoption_agency(&mut self, subject: Tag, _name: &str) -> bool {
        let c = self.current();
        if c.is(subject) && !self.active.contains(&c.id) {
            self.pop();
            return false;
        }
        for _ in 0..8 {
            let start = self
                .active
                .iter()
                .rposition(|&e| e == MARKER)
                .map_or(0, |i| i + 1);
            let Some(fe_active) = (start..self.active.len()).rev().find(|&i| {
                self.doc
                    .element(self.active[i])
                    .is_some_and(|el| el.ns == Ns::Html && el.tag == subject)
            }) else {
                return true;
            };
            let fe = self.active[fe_active];
            let Some(fe_open) = self.open.iter().rposition(|o| o.id == fe) else {
                self.active.remove(fe_active);
                return false;
            };
            if !self.node_in_scope(fe) {
                return false;
            }
            let Some(fb_open) = (fe_open + 1..self.open.len()).find(|&i| special(self.open[i]))
            else {
                self.truncate_open(fe_open);
                self.active.remove(fe_active);
                return false;
            };
            let furthest = self.open[fb_open].id;
            let common = self.open[fe_open - 1].id;
            let mut bookmark = fe_active;
            let mut node_index = fb_open;
            let mut last = furthest;
            let mut inner = 0;
            let mut bookmark_moved = false;
            loop {
                inner += 1;
                node_index -= 1;
                let node = self.open[node_index];
                if node.id == fe {
                    break;
                }
                let mut in_active = self.active.iter().position(|&e| e == node.id);
                if inner > 3
                    && let Some(i) = in_active
                {
                    self.active.remove(i);
                    if i < bookmark {
                        bookmark -= 1;
                    }
                    in_active = None;
                }
                let Some(ai) = in_active else {
                    self.remove_open_at(node_index);
                    continue;
                };
                let new = self.clone_element(node.id);
                self.active[ai] = new;
                self.open[node_index] = Open { id: new, ..node };
                self.mark(node.id, false);
                self.mark(new, true);
                if !bookmark_moved {
                    bookmark = ai + 1;
                    bookmark_moved = true;
                }
                self.doc.detach(last);
                self.doc.insert(new, last, NONE);
                last = new;
            }
            let place = self.place(Some(common));
            self.doc.detach(last);
            self.insert_at(place, last);
            let new = self.clone_element(fe);
            while let Some(child) = self.doc.first_child(furthest) {
                self.doc.detach(child);
                self.doc.insert(new, child, NONE);
            }
            self.doc.insert(furthest, new, NONE);
            let fe_active = self
                .active
                .iter()
                .position(|&e| e == fe)
                .expect("still active");
            self.active.remove(fe_active);
            if fe_active < bookmark {
                bookmark -= 1;
            }
            self.active.insert(bookmark.min(self.active.len()), new);
            let fe_open = self
                .open
                .iter()
                .position(|o| o.id == fe)
                .expect("still open");
            let fe_entry = self.remove_open_at(fe_open);
            let fb = self
                .open
                .iter()
                .position(|o| o.id == furthest)
                .expect("still open");
            self.insert_open_at(
                fb + 1,
                Open {
                    id: new,
                    ..fe_entry
                },
            );
        }
        false
    }

    fn text(&mut self, t: Tok<'_>) {
        match t {
            Tok::Text(s) => self.insert_text(s),
            Tok::Eof => {
                self.pop();
                let original = self.original;
                self.reprocess(original, t);
            }
            Tok::End(_) => {
                self.pop();
                self.mode = self.original;
            }
            _ => {}
        }
    }

    fn in_table(&mut self, t: Tok<'_>) {
        use Tag::*;
        match t {
            Tok::Text(_)
                if matches!(
                    self.current().tag,
                    Table | Tbody | Template | Tfoot | Thead | Tr
                ) && self.current().ns == Ns::Html =>
            {
                self.pending.clear();
                self.pending_text = false;
                self.original = self.mode;
                self.reprocess(Mode::InTableText, t);
            }
            Tok::Comment(c) => self.insert_comment(c, None),
            Tok::Pi(target, data) => self.insert_pi(target, data, None),
            Tok::Doctype(_) => {}
            Tok::Start(s) => match s.tag {
                Caption => {
                    self.clear_to_table_context();
                    self.active.push(MARKER);
                    self.insert_html(&s);
                    self.mode = Mode::InCaption;
                }
                Colgroup => {
                    self.clear_to_table_context();
                    self.insert_html(&s);
                    self.mode = Mode::InColumnGroup;
                }
                Col => {
                    self.clear_to_table_context();
                    self.insert_html(&Start::bare(Colgroup));
                    self.reprocess(Mode::InColumnGroup, t);
                }
                Tbody | Tfoot | Thead => {
                    self.clear_to_table_context();
                    self.insert_html(&s);
                    self.mode = Mode::InTableBody;
                }
                Td | Th | Tr => {
                    self.clear_to_table_context();
                    self.insert_html(&Start::bare(Tbody));
                    self.reprocess(Mode::InTableBody, t);
                }
                Table => {
                    if self.in_scope(Table, Scope::Table) {
                        self.pop_until(Table);
                        self.reset_mode();
                        self.process(t);
                    }
                }
                Style | Script | Template => self.in_head(t),
                Input
                    if s.attr("type")
                        .is_some_and(|v| v.eq_ignore_ascii_case("hidden")) =>
                {
                    self.insert_void(&s);
                }
                Form => {
                    let template = self.in_template_contents();
                    if self.form != NONE && !template {
                        return;
                    }
                    let id = self.insert_html(&s);
                    if !template {
                        self.form = id;
                    }
                    self.pop();
                }
                _ => self.in_table_else(t),
            },
            Tok::End(e) => match e.tag {
                Table => {
                    if self.in_scope(Table, Scope::Table) {
                        self.pop_until(Table);
                        self.reset_mode();
                    }
                }
                Body | Caption | Col | Colgroup | Html | Tbody | Td | Tfoot | Th | Thead | Tr => {}
                Template => self.in_head(t),
                _ => self.in_table_else(t),
            },
            Tok::Eof => self.in_body(t),
            Tok::Text(_) => self.in_table_else(t),
        }
    }

    fn in_table_else(&mut self, t: Tok<'_>) {
        let foster = self.foster;
        self.foster = true;
        self.in_body(t);
        self.foster = foster;
    }

    fn in_table_text(&mut self, t: Tok<'_>) {
        if let Tok::Text(s) = t {
            for piece in s.split('\0') {
                if !all_space(piece) {
                    self.pending_text = true;
                }
                self.pending.push_str(piece);
            }
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        if self.pending_text {
            let foster = self.foster;
            self.foster = true;
            self.body_text(&pending);
            self.foster = foster;
        } else {
            self.insert_text(&pending);
        }
        self.pending = pending;
        self.pending.clear();
        let original = self.original;
        self.reprocess(original, t);
    }

    fn close_caption(&mut self) -> bool {
        if !self.in_scope(Tag::Caption, Scope::Table) {
            return false;
        }
        self.generate_implied(None);
        self.pop_until(Tag::Caption);
        self.clear_active_to_marker();
        self.mode = Mode::InTable;
        true
    }

    fn in_caption(&mut self, t: Tok<'_>) {
        use Tag::*;
        match t {
            Tok::End(e) if e.tag == Caption => {
                self.close_caption();
            }
            Tok::Start(s)
                if matches!(
                    s.tag,
                    Caption | Col | Colgroup | Tbody | Td | Tfoot | Th | Thead | Tr
                ) =>
            {
                if self.close_caption() {
                    self.process(t);
                }
            }
            Tok::End(e) if e.tag == Table => {
                if self.close_caption() {
                    self.process(t);
                }
            }
            Tok::End(e)
                if matches!(
                    e.tag,
                    Body | Col | Colgroup | Html | Tbody | Td | Tfoot | Th | Thead | Tr
                ) => {}
            _ => self.in_body(t),
        }
    }

    fn in_column_group(&mut self, t: Tok<'_>) {
        use Tag::*;
        match t {
            Tok::Text(s) => {
                let (space, rest) = split_space(s);
                self.insert_text(space);
                if !rest.is_empty() {
                    self.in_column_group_else(Tok::Text(rest));
                }
            }
            Tok::Comment(c) => self.insert_comment(c, None),
            Tok::Pi(target, data) => self.insert_pi(target, data, None),
            Tok::Doctype(_) => {}
            Tok::Start(s) if s.tag == Html => self.in_body(t),
            Tok::Start(s) if s.tag == Col => self.insert_void(&s),
            Tok::End(e) if e.tag == Colgroup => {
                if self.current().is(Colgroup) {
                    self.pop();
                    self.mode = Mode::InTable;
                }
            }
            Tok::End(e) if e.tag == Col => {}
            Tok::Start(s) if s.tag == Template => self.in_head(t),
            Tok::End(e) if e.tag == Template => self.in_head(t),
            Tok::Eof => self.in_body(t),
            _ => self.in_column_group_else(t),
        }
    }

    fn in_column_group_else(&mut self, t: Tok<'_>) {
        if self.current().is(Tag::Colgroup) {
            self.pop();
            self.reprocess(Mode::InTable, t);
        }
    }

    fn in_table_body(&mut self, t: Tok<'_>) {
        use Tag::*;
        match t {
            Tok::Start(s) if s.tag == Tr => {
                self.clear_to_table_body_context();
                self.insert_html(&s);
                self.mode = Mode::InRow;
            }
            Tok::Start(s) if matches!(s.tag, Th | Td) => {
                self.clear_to_table_body_context();
                self.insert_html(&Start::bare(Tr));
                self.reprocess(Mode::InRow, t);
            }
            Tok::End(e) if matches!(e.tag, Tbody | Tfoot | Thead) => {
                if self.in_scope(e.tag, Scope::Table) {
                    self.clear_to_table_body_context();
                    self.pop();
                    self.mode = Mode::InTable;
                }
            }
            Tok::Start(s) if matches!(s.tag, Caption | Col | Colgroup | Tbody | Tfoot | Thead) => {
                self.table_body_exit(t);
            }
            Tok::End(e) if e.tag == Table => self.table_body_exit(t),
            Tok::End(e)
                if matches!(e.tag, Body | Caption | Col | Colgroup | Html | Td | Th | Tr) => {}
            _ => self.in_table(t),
        }
    }

    fn table_body_exit(&mut self, t: Tok<'_>) {
        use Tag::*;
        if self.in_scope_where(|t| matches!(t, Tbody | Thead | Tfoot), Scope::Table) {
            self.clear_to_table_body_context();
            self.pop();
            self.reprocess(Mode::InTable, t);
        }
    }

    fn in_row(&mut self, t: Tok<'_>) {
        use Tag::*;
        match t {
            Tok::Start(s) if matches!(s.tag, Th | Td) => {
                self.clear_to_table_row_context();
                self.insert_html(&s);
                self.mode = Mode::InCell;
                self.active.push(MARKER);
            }
            Tok::End(e) if e.tag == Tr => {
                if self.in_scope(Tr, Scope::Table) {
                    self.clear_to_table_row_context();
                    self.pop();
                    self.mode = Mode::InTableBody;
                }
            }
            Tok::Start(s)
                if matches!(s.tag, Caption | Col | Colgroup | Tbody | Tfoot | Thead | Tr) =>
            {
                self.row_exit(t);
            }
            Tok::End(e) if e.tag == Table => self.row_exit(t),
            Tok::End(e) if matches!(e.tag, Tbody | Tfoot | Thead) => {
                if self.in_scope(e.tag, Scope::Table) {
                    self.row_exit(t);
                }
            }
            Tok::End(e) if matches!(e.tag, Body | Caption | Col | Colgroup | Html | Td | Th) => {}
            _ => self.in_table(t),
        }
    }

    fn row_exit(&mut self, t: Tok<'_>) {
        if self.in_scope(Tag::Tr, Scope::Table) {
            self.clear_to_table_row_context();
            self.pop();
            self.reprocess(Mode::InTableBody, t);
        }
    }

    fn close_cell(&mut self) {
        self.generate_implied(None);
        self.pop_until_where(|t| matches!(t, Tag::Td | Tag::Th));
        self.clear_active_to_marker();
        self.mode = Mode::InRow;
    }

    fn in_cell(&mut self, t: Tok<'_>) {
        use Tag::*;
        match t {
            Tok::End(e) if matches!(e.tag, Td | Th) => {
                if self.in_scope(e.tag, Scope::Table) {
                    self.generate_implied(None);
                    self.pop_until(e.tag);
                    self.clear_active_to_marker();
                    self.mode = Mode::InRow;
                }
            }
            Tok::Start(s)
                if matches!(
                    s.tag,
                    Caption | Col | Colgroup | Tbody | Td | Tfoot | Th | Thead | Tr
                ) =>
            {
                if self.in_scope_where(|t| matches!(t, Td | Th), Scope::Table) {
                    self.close_cell();
                    self.process(t);
                }
            }
            Tok::End(e) if matches!(e.tag, Body | Caption | Col | Colgroup | Html) => {}
            Tok::End(e) if matches!(e.tag, Table | Tbody | Tfoot | Thead | Tr) => {
                if self.in_scope(e.tag, Scope::Table) {
                    self.close_cell();
                    self.process(t);
                }
            }
            _ => self.in_body(t),
        }
    }

    fn in_template(&mut self, t: Tok<'_>) {
        use Tag::*;
        let switch = |me: &mut Self, mode: Mode| {
            me.template_modes.pop();
            me.template_modes.push(mode);
            me.reprocess(mode, t);
        };
        match t {
            Tok::Text(_) | Tok::Comment(_) | Tok::Pi(..) | Tok::Doctype(_) => self.in_body(t),
            Tok::Start(s) => match s.tag {
                Base | Basefont | Bgsound | Link | Meta | Noframes | Script | Style | Template
                | Title => self.in_head(t),
                Caption | Colgroup | Tbody | Tfoot | Thead => switch(self, Mode::InTable),
                Col => switch(self, Mode::InColumnGroup),
                Tr => switch(self, Mode::InTableBody),
                Td | Th => switch(self, Mode::InRow),
                _ => switch(self, Mode::InBody),
            },
            Tok::End(e) if e.tag == Template => self.in_head(t),
            Tok::End(_) => {}
            Tok::Eof => {
                if !self.template_open() {
                    self.stop();
                    return;
                }
                self.pop_until(Template);
                self.clear_active_to_marker();
                self.template_modes.pop();
                self.reset_mode();
                self.process(t);
            }
        }
    }

    fn after_body(&mut self, t: Tok<'_>) {
        match t {
            Tok::Text(s) => {
                let (space, rest) = split_space(s);
                if !space.is_empty() {
                    self.in_body(Tok::Text(space));
                }
                if !rest.is_empty() {
                    self.reprocess(Mode::InBody, Tok::Text(rest));
                }
            }
            Tok::Comment(c) => {
                let root = self.open[0].id;
                self.insert_comment(c, Some((root, NONE)));
            }
            Tok::Pi(target, data) => {
                let root = self.open[0].id;
                self.insert_pi(target, data, Some((root, NONE)));
            }
            Tok::Doctype(_) => {}
            Tok::Start(s) if s.tag == Tag::Html => self.in_body(t),
            Tok::End(e) if e.tag == Tag::Html => {
                if self.context.is_none() {
                    self.mode = Mode::AfterAfterBody;
                }
            }
            Tok::Eof => self.stop(),
            _ => self.reprocess(Mode::InBody, t),
        }
    }

    fn frameset_space(&mut self, s: &str) {
        let space: String = s.bytes().filter(|&b| is_space(b)).map(char::from).collect();
        self.insert_text(&space);
    }

    fn in_frameset(&mut self, t: Tok<'_>) {
        use Tag::*;
        match t {
            Tok::Text(s) => self.frameset_space(s),
            Tok::Comment(c) => self.insert_comment(c, None),
            Tok::Pi(target, data) => self.insert_pi(target, data, None),
            Tok::Doctype(_) => {}
            Tok::Start(s) => match s.tag {
                Html => self.in_body(t),
                Frameset => {
                    self.insert_html(&s);
                }
                Frame => self.insert_void(&s),
                Noframes => self.in_head(t),
                _ => {}
            },
            Tok::End(e) if e.tag == Frameset => {
                if self.open.len() == 1 {
                    return;
                }
                self.pop();
                if self.context.is_none() && !self.current().is(Frameset) {
                    self.mode = Mode::AfterFrameset;
                }
            }
            Tok::End(_) => {}
            Tok::Eof => self.stop(),
        }
    }

    fn after_frameset(&mut self, t: Tok<'_>) {
        match t {
            Tok::Text(s) => self.frameset_space(s),
            Tok::Comment(c) => self.insert_comment(c, None),
            Tok::Pi(target, data) => self.insert_pi(target, data, None),
            Tok::Doctype(_) => {}
            Tok::Start(s) if s.tag == Tag::Html => self.in_body(t),
            Tok::End(e) if e.tag == Tag::Html => self.mode = Mode::AfterAfterFrameset,
            Tok::Start(s) if s.tag == Tag::Noframes => self.in_head(t),
            Tok::Eof => self.stop(),
            _ => {}
        }
    }

    fn after_after_body(&mut self, t: Tok<'_>) {
        match t {
            Tok::Comment(c) => self.insert_comment(c, Some((0, NONE))),
            Tok::Pi(target, data) => self.insert_pi(target, data, Some((0, NONE))),
            Tok::Doctype(_) => self.in_body(t),
            Tok::Text(s) => {
                let (space, rest) = split_space(s);
                if !space.is_empty() {
                    self.in_body(Tok::Text(space));
                }
                if !rest.is_empty() {
                    self.reprocess(Mode::InBody, Tok::Text(rest));
                }
            }
            Tok::Start(s) if s.tag == Tag::Html => self.in_body(t),
            Tok::Eof => self.stop(),
            _ => self.reprocess(Mode::InBody, t),
        }
    }

    fn after_after_frameset(&mut self, t: Tok<'_>) {
        match t {
            Tok::Comment(c) => self.insert_comment(c, Some((0, NONE))),
            Tok::Pi(target, data) => self.insert_pi(target, data, Some((0, NONE))),
            Tok::Doctype(_) => self.in_body(t),
            Tok::Text(s) => {
                let space: String = s.bytes().filter(|&b| is_space(b)).map(char::from).collect();
                if !space.is_empty() {
                    self.in_body(Tok::Text(&space));
                }
            }
            Tok::Start(s) if s.tag == Tag::Html => self.in_body(t),
            Tok::Start(s) if s.tag == Tag::Noframes => self.in_head(t),
            Tok::Eof => self.stop(),
            _ => {}
        }
    }

    // ---- foreign content --------------------------------------------------

    fn foreign(&mut self, t: Tok<'_>) {
        use Tag::*;
        match t {
            Tok::Text(s) => {
                if s.contains('\0') {
                    self.insert_text(&s.replace('\0', "\u{FFFD}"));
                } else {
                    self.insert_text(s);
                }
                if s.bytes().any(|b| !is_space(b) && b != 0) {
                    self.frameset_ok = false;
                }
            }
            Tok::Comment(c) => self.insert_comment(c, None),
            Tok::Pi(target, data) => self.insert_pi(target, data, None),
            Tok::Doctype(_) => {}
            Tok::Start(s)
                if matches!(
                    s.tag,
                    B | Big
                        | Blockquote
                        | Body
                        | Br
                        | Center
                        | Code
                        | Dd
                        | Div
                        | Dl
                        | Dt
                        | Em
                        | Embed
                        | H1
                        | H2
                        | H3
                        | H4
                        | H5
                        | H6
                        | Head
                        | Hr
                        | I
                        | Img
                        | Li
                        | Listing
                        | Menu
                        | Meta
                        | Nobr
                        | Ol
                        | P
                        | Pre
                        | Ruby
                        | S
                        | Small
                        | Span
                        | Strong
                        | Strike
                        | Sub
                        | Sup
                        | Table
                        | Tt
                        | U
                        | Ul
                        | Var
                ) || (s.tag == Font
                    && s.attrs()
                        .any(|(n, _)| matches!(n, "color" | "face" | "size"))) =>
            {
                self.break_out(t);
            }
            Tok::End(e) if matches!(e.tag, Br | P) => self.break_out(t),
            Tok::Start(s) => {
                let ns = self.adjusted_current().map_or(Ns::Html, |a| a.ns);
                self.insert_element(&s, ns);
                if s.self_closing {
                    self.pop();
                }
            }
            Tok::End(e) => {
                let c = self.current();
                if e.tag == Script && c.ns == Ns::Svg && c.tag == Script {
                    self.pop();
                    return;
                }
                let mut i = self.open.len() - 1;
                // Unless the current node or an open foreign element has
                // that name, the walk below ends at the nearest HTML
                // element, which then takes the token; without walking
                // there one element at a time.
                let current = self
                    .doc
                    .element(c.id)
                    .is_some_and(|el| self.doc.element_name(el).eq_ignore_ascii_case(e.name));
                if !current && self.open_named(false, e.name) == 0 {
                    if i > 0 {
                        self.html(t);
                    }
                    return;
                }
                loop {
                    if i == 0 {
                        return;
                    }
                    let node = self.open[i];
                    let name = self
                        .doc
                        .element(node.id)
                        .map(|el| self.doc.element_name(el))
                        .unwrap_or("");
                    if name.eq_ignore_ascii_case(e.name) {
                        self.truncate_open(i);
                        return;
                    }
                    i -= 1;
                    if self.open[i].ns == Ns::Html {
                        self.html(t);
                        return;
                    }
                }
            }
            Tok::Eof => self.html(t),
        }
    }

    fn break_out(&mut self, t: Tok<'_>) {
        loop {
            let c = self.current();
            let html_integration = match c.ns {
                Ns::Svg => matches!(c.tag, Tag::ForeignObject | Tag::Desc | Tag::Title),
                Ns::MathMl => self.doc.element(c.id).is_some_and(|e| e.html_integration),
                Ns::Html | Ns::Xml => true,
            };
            if mathml_text_integration(c) || html_integration {
                break;
            }
            self.pop();
        }
        self.html(t);
    }
}

impl Sink for TreeBuilder {
    fn doctype(&mut self, d: &Doctype<'_>) {
        self.skip_newline = false;
        self.process(Tok::Doctype(d));
    }

    fn start_tag(&mut self, tag: &tokenizer::Tag<'_>) -> Option<TextMode> {
        self.skip_newline = false;
        self.switch = None;
        self.process(Tok::Start(Start {
            tag: Tag::from_name(tag.name),
            name: tag.name,
            attrs: Some(tag),
            self_closing: tag.self_closing,
        }));
        self.switch.take()
    }

    fn end_tag(&mut self, tag: &tokenizer::Tag<'_>) {
        self.skip_newline = false;
        self.process(Tok::End(End {
            tag: Tag::from_name(tag.name),
            name: tag.name,
        }));
    }

    fn comment(&mut self, text: &str) {
        self.skip_newline = false;
        self.process(Tok::Comment(text));
    }

    fn processing_instruction(&mut self, target: &str, data: &str) {
        self.skip_newline = false;
        self.process(Tok::Pi(target, data));
    }

    fn text(&mut self, text: &str) {
        let mut text = text;
        if std::mem::take(&mut self.skip_newline) {
            text = text.strip_prefix('\n').unwrap_or(text);
        }
        if !text.is_empty() {
            self.process(Tok::Text(text));
        }
    }

    fn eof(&mut self) {
        self.process(Tok::Eof);
    }

    fn cdata_allowed(&self) -> bool {
        self.adjusted_current().is_some_and(|a| a.ns != Ns::Html)
    }
}

/// How to parse.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// As a browser with JavaScript on: `<noscript>` holds raw text.
    pub scripting: bool,
    /// Give up after this long. Some inputs are slow by design (the spec
    /// makes deeply misnested markup cost more), so a crawler parsing
    /// pages it doesn't control should set one. The clock is read every
    /// 4,096 tokenizer steps, so a small input finishes even with
    /// `Duration::ZERO`: the budget bounds slow parses, not fast ones.
    pub budget: Option<std::time::Duration>,
}

/// Why a parse returned no document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The input is over [`crate::MAX_INPUT`] bytes.
    TooLarge { len: usize },
    /// The budget ran out after `parsed` bytes.
    Timeout {
        budget: std::time::Duration,
        parsed: usize,
    },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::TooLarge { len } => write!(
                f,
                "{len} bytes of HTML is over the {} byte limit",
                crate::MAX_INPUT
            ),
            ParseError::Timeout { budget, parsed } => write!(
                f,
                "parsing took longer than {budget:?} (gave up after {parsed} bytes)"
            ),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parses a whole document. A leading byte order mark (U+FEFF) is dropped,
/// as decoding the page would have.
///
/// # Panics
/// On input over [`crate::MAX_INPUT`] bytes; [`parse_with`] returns an
/// error instead.
pub fn parse(html: &str, scripting: bool) -> Document {
    parse_with(
        html,
        Options {
            scripting,
            budget: None,
        },
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

/// Parses a whole document, within a size limit and an optional budget.
pub fn parse_with(html: &str, options: Options) -> Result<Document, ParseError> {
    run(html, options, None)
}

/// Parses `html` as the contents of an element named `context` in
/// namespace `ns`, as `innerHTML` does.
///
/// # Panics
/// As [`parse`].
pub fn parse_fragment(html: &str, ns: Ns, context: &str, scripting: bool) -> Document {
    parse_fragment_with(
        html,
        ns,
        context,
        Options {
            scripting,
            budget: None,
        },
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

/// Parses `html` as [`parse_fragment`] does, within a size limit and an
/// optional budget, as [`parse_with`] does for a whole document.
pub fn parse_fragment_with(
    html: &str,
    ns: Ns,
    context: &str,
    options: Options,
) -> Result<Document, ParseError> {
    run(html, options, Some((ns, context)))
}

fn run(html: &str, options: Options, fragment: Option<(Ns, &str)>) -> Result<Document, ParseError> {
    // Decoding a document drops a byte order mark, so a string that still
    // starts with one gets the same tree. A fragment, as innerHTML, keeps it.
    let html = match fragment {
        None => html.strip_prefix('\u{FEFF}').unwrap_or(html),
        Some(_) => html,
    };
    if html.len() > crate::MAX_INPUT {
        return Err(ParseError::TooLarge { len: html.len() });
    }
    let start = std::time::Instant::now();
    let mut builder = TreeBuilder::new(html.len(), options.scripting);
    builder.doc.scripting = options.scripting;
    let input = source(&mut builder, html);
    let mode = match fragment {
        Some((ns, context)) => builder.fragment(ns, context),
        None => None,
    };
    let mut tokenizer = Tokenizer::new(input);
    if let Some(mode) = mode {
        tokenizer.set_text_mode(mode);
    }
    // A budget past what the clock can count is no limit at all.
    if let Some(deadline) = options.budget.and_then(|b| start.checked_add(b)) {
        tokenizer.set_deadline(deadline);
    }
    tokenizer.run(&mut builder);
    if tokenizer.timed_out() {
        return Err(ParseError::Timeout {
            budget: options.budget.unwrap_or_default(),
            parsed: tokenizer.position(),
        });
    }
    if fragment.is_some() {
        builder.finish_fragment();
    }
    Ok(builder.doc)
}

/// Gives the document its preprocessed source, and returns it for the
/// tokenizer to read.
fn source<'a>(builder: &mut TreeBuilder, html: &str) -> &'a str {
    builder.doc.src = match tokenizer::preprocess(html) {
        std::borrow::Cow::Borrowed(s) => s.into(),
        std::borrow::Cow::Owned(s) => s.into_boxed_str(),
    };
    // SAFETY: the source is a boxed str the document never changes or
    // drops while it's parsed, so its bytes stay where they are while the
    // tokenizer reads them, even though the builder holding them moves.
    // The returned reference doesn't outlive the parse functions above.
    unsafe { &*(builder.doc.src.as_ref() as *const str) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_budget_stops_a_long_parse_and_says_how_far_it_got() {
        let html = "<div><b>x</b> y</div>".repeat(200_000);
        let tight = Options {
            budget: Some(Duration::from_micros(50)),
            ..Options::default()
        };
        match parse_with(&html, tight) {
            Err(ParseError::Timeout { parsed, .. }) => assert!(parsed < html.len()),
            other => panic!("expected a timeout, got {:?}", other.map(|d| d.len())),
        }
        let roomy = Options {
            budget: Some(Duration::from_secs(60)),
            ..Options::default()
        };
        assert!(parse_with(&html, roomy).is_ok());
    }
    #[test]
    fn a_fragment_has_a_budget_and_a_size_limit_too() {
        let html = "<div><b>x</b> y</div>".repeat(200_000);
        let tight = Options {
            budget: Some(Duration::from_micros(50)),
            ..Options::default()
        };
        match parse_fragment_with(&html, Ns::Html, "div", tight) {
            Err(ParseError::Timeout { parsed, .. }) => assert!(parsed < html.len()),
            other => panic!("expected a timeout, got {:?}", other.map(|d| d.len())),
        }
        let doc = parse_fragment_with("<b>x</b>", Ns::Html, "div", Options::default()).unwrap();
        assert_eq!(doc.inner_html(doc.root()), "<b>x</b>");
        let huge = "x".repeat(crate::MAX_INPUT + 1);
        assert_eq!(
            parse_fragment_with(&huge, Ns::Html, "div", Options::default()).map(|d| d.len()),
            Err(ParseError::TooLarge { len: huge.len() })
        );
    }

    #[test]
    fn a_leading_byte_order_mark_is_dropped_from_a_document() {
        let page = "<!DOCTYPE html><p>x</p>";
        let with = parse(&format!("\u{FEFF}{page}"), false);
        let without = parse(page, false);
        assert_eq!(with.quirks(), crate::Quirks::No);
        assert_eq!(
            with.inner_html(with.root()),
            without.inner_html(without.root())
        );
        // Only the first one goes; one later on is text like any other.
        let twice = parse("\u{FEFF}\u{FEFF}<p>x</p>", false);
        assert!(twice.inner_html(twice.root()).contains('\u{FEFF}'));
        // innerHTML doesn't decode, so a fragment keeps it.
        let fragment = parse_fragment("\u{FEFF}x", Ns::Html, "div", false);
        assert!(fragment.inner_html(fragment.root()).contains('\u{FEFF}'));
    }

    #[test]
    fn a_budget_too_big_for_the_clock_is_no_limit() {
        let endless = Options {
            budget: Some(Duration::MAX),
            ..Options::default()
        };
        assert!(parse_with("<p>x", endless).is_ok());
    }
}

#[cfg(test)]
mod hostile {
    use super::*;
    use std::time::{Duration, Instant};

    fn time(html: &str) -> Duration {
        let start = Instant::now();
        std::hint::black_box(parse(html, false));
        start.elapsed()
    }

    /// Fails if four times the input takes more than eight times as long:
    /// linear work takes four, quadratic sixteen. The two sizes are timed
    /// back to back, five times, and one round inside the limit passes: a
    /// slow spell on a loaded machine that lands on the large parses only
    /// can't fail it, and quadratic work fails every round.
    fn grows_linearly(make: impl Fn(usize) -> String) {
        let (small, large) = (make(5_000), make(20_000));
        let rounds: Vec<_> = (0..5).map(|_| (time(&small), time(&large))).collect();
        assert!(
            rounds
                .iter()
                .any(|&(a, b)| b < a * 8 + Duration::from_millis(20)),
            "{} bytes, then {} bytes, took {rounds:?}",
            small.len(),
            large.len()
        );
    }

    /// A selected option's contents are copied into `<selectedcontent>`
    /// as it closes, and the copy recursed once per level: a hundred
    /// kilobytes of nesting overflowed a thread's stack.
    #[test]
    fn copying_deep_option_contents_keeps_to_a_small_stack() {
        let html = format!(
            "<select><button><selectedcontent></selectedcontent></button><option>{}",
            "<div>".repeat(20_000)
        );
        let parsed = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || parse(&html, false).len())
            .unwrap()
            .join()
            .unwrap();
        assert!(parsed > 40_000, "both copies are there: {parsed} nodes");
    }

    /// An end tag nothing open matches walked the whole stack of open
    /// elements, for each such tag.
    #[test]
    fn unmatched_end_tags_under_deep_nesting_cost_linear_time() {
        grows_linearly(|n| format!("{}{}", "<span>".repeat(n), "</x>".repeat(n)));
        grows_linearly(|n| format!("{}{}", "<span>".repeat(n), "</p>".repeat(n)));
        grows_linearly(|n| format!("{}{}", "<x-a>".repeat(n), "</x-b>".repeat(n)));
    }

    /// The same, inside SVG, where an end tag is first looked for among
    /// the open foreign elements.
    #[test]
    fn unmatched_end_tags_in_deep_svg_cost_linear_time() {
        grows_linearly(|n| format!("<svg>{}{}", "<g>".repeat(n), "</x>".repeat(n)));
    }

    /// Each option looked up the tree for a select, all the way up when
    /// there was none.
    #[test]
    fn options_under_deep_nesting_cost_linear_time() {
        grows_linearly(|n| format!("{}{}", "<div>".repeat(n), "<option>".repeat(n)));
    }

    /// Each `<body X>` merges an attribute (copied into the parse buffer)
    /// and then text goes back into the same text node, so a text node
    /// copied whole on every append grew the buffer quadratically: 310 KB
    /// reached 1 GB and panicked.
    #[test]
    fn text_split_by_merged_attributes_costs_linear_memory() {
        let tail: String = (0..11_000).map(|i| format!("<body X{i}>b")).collect();
        let html = format!("<body>a&amp;{}{tail}", "x".repeat(100_000));
        let doc = parse(&html, false);
        assert!(doc.buf.len() + doc.extras.iter().map(|e| e.len()).sum::<usize>() < 4 * html.len());
        assert!(doc.text(0).ends_with('b'));
    }
}
