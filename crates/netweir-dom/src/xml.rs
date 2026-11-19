//! XML documents. quick-xml reads the markup and this module builds the
//! tree in the same arena as an HTML page's, so CSS, XPath, `get()` and
//! the rest of the API work on a feed or a sitemap as they do on a page.
//!
//! XML is a trust boundary, and parsing treats it as one:
//!
//! - Nothing is ever fetched. A DOCTYPE's external DTD is ignored, and a
//!   reference to an external entity is an error, never a request or a
//!   file read.
//! - Entities the document declares in its own DTD are expanded, but no
//!   more than [`MAX_ENTITY_DEPTH`] deep, and to no more than a few times
//!   the document's own size in all: a "billion laughs" document is
//!   refused before it can fill memory.
//! - Elements nest at most [`MAX_DEPTH`] deep, and a parse has a time
//!   budget the clock is checked against as the work is done, attributes
//!   and expanded entities included.
//! - Anything that isn't well-formed is an [`XmlError`] saying what is
//!   wrong and where, by line and column. Recover mode
//!   ([`XmlOptions::recover`]) repairs the faults feeds commonly have
//!   instead, and says what it repaired ([`Repairs`]); it's never the
//!   default, so a broken document is never quietly read as if it weren't.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::rc::Rc;
use std::time::{Duration, Instant};

use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};

use crate::document::{Document, Node, NodeKind, ParseTimeout};
use netweir_html as html;

/// How deep elements may nest. lxml's limit with `huge_tree`, which
/// Scrapy's selectors turn on.
pub const MAX_DEPTH: usize = 2048;
/// How deep entity references may nest inside the entities they refer to.
pub const MAX_ENTITY_DEPTH: usize = 16;
/// Entity expansion may add this many bytes to any document, plus
/// [`ENTITY_FACTOR`] bytes for each byte of the document itself.
pub const ENTITY_ALLOWANCE: usize = 1 << 20;
pub const ENTITY_FACTOR: usize = 8;

/// The namespace the `xml` prefix is always bound to.
pub use netweir_xpath::XML_NS;
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";

/// Why a document isn't well-formed XML, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XmlError {
    /// From 1.
    pub line: usize,
    /// From 1, in characters.
    pub column: usize,
    pub reason: String,
}

impl fmt::Display for XmlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "not well-formed XML at line {}, column {}: {}",
            self.line, self.column, self.reason
        )
    }
}

impl std::error::Error for XmlError {}

/// Returned by [`Document::parse_xml_within`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseXmlError {
    Malformed(XmlError),
    Timeout(ParseTimeout),
}

impl fmt::Display for ParseXmlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseXmlError::Malformed(e) => e.fmt(f),
            ParseXmlError::Timeout(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ParseXmlError {}

/// What an XML document knows that the HTML tree has no place for:
/// element namespaces, compared as written, keyed by node.
#[derive(Default)]
pub(crate) struct XmlNames {
    /// Namespace URIs; id `i` is `uris[i - 1]`, and 0 is no namespace.
    uris: Vec<Box<str>>,
    ids: HashMap<Box<str>, u32>,
    /// The namespace of each element that has one. (An attribute carries
    /// its own, as `AttrNs::Host`.)
    ns: HashMap<usize, u32>,
    /// The `xmlns` declarations each element makes, in order: a prefix
    /// ("" for the default namespace) and a URI id (0 for `xmlns=""`).
    decls: HashMap<usize, Vec<(Box<str>, u32)>>,
    /// For an element read alone ([`XmlNodes`]): the declarations of the
    /// elements it was inside, nearest first.
    outer: Vec<(Box<str>, u32)>,
}

impl XmlNames {
    fn intern(&mut self, uri: &str) -> u32 {
        if uri.is_empty() {
            return 0;
        }
        if let Some(&id) = self.ids.get(uri) {
            return id;
        }
        self.uris.push(uri.into());
        let id = self.uris.len() as u32;
        self.ids.insert(uri.into(), id);
        id
    }

    /// The URI with id `id`; "" for 0.
    pub(crate) fn uri(&self, id: u32) -> &str {
        match id {
            0 => "",
            n => self.uris.get(n as usize - 1).map_or("", |u| u),
        }
    }

    /// The id of `uri` in this document, None if nothing in it has that
    /// namespace. "" is 0.
    pub(crate) fn id_of(&self, uri: &str) -> Option<u32> {
        if uri.is_empty() {
            Some(0)
        } else {
            self.ids.get(uri).copied()
        }
    }

    /// The namespace id of the element at `raw`.
    pub(crate) fn ns_of(&self, raw: usize) -> u32 {
        self.ns.get(&raw).copied().unwrap_or(0)
    }

    fn decls_of(&self, raw: usize) -> &[(Box<str>, u32)] {
        self.decls.get(&raw).map_or(&[], |d| d)
    }

    /// Forgets every node's namespace and declarations. The URIs stay:
    /// strings borrowed from them before must still be valid after.
    pub(crate) fn clear(&mut self) {
        self.ns.clear();
        self.decls.clear();
        self.outer.clear();
    }
}

impl netweir_xpath::Namespaces for XmlNames {
    fn ns_of(&self, element: html::NodeId) -> u32 {
        XmlNames::ns_of(self, element as usize)
    }

    fn uri(&self, id: u32) -> &str {
        XmlNames::uri(self, id)
    }

    fn id_of(&self, uri: &str) -> Option<u32> {
        XmlNames::id_of(self, uri)
    }
}

// --- names (XML 1.0, fifth edition, section 2.3) --------------------------

fn name_start(c: char) -> bool {
    matches!(c,
        'A'..='Z' | '_' | 'a'..='z' | '\u{C0}'..='\u{D6}' | '\u{D8}'..='\u{F6}'
        | '\u{F8}'..='\u{2FF}' | '\u{370}'..='\u{37D}' | '\u{37F}'..='\u{1FFF}'
        | '\u{200C}'..='\u{200D}' | '\u{2070}'..='\u{218F}' | '\u{2C00}'..='\u{2FEF}'
        | '\u{3001}'..='\u{D7FF}' | '\u{F900}'..='\u{FDCF}' | '\u{FDF0}'..='\u{FFFD}'
        | '\u{10000}'..='\u{EFFFF}')
}

fn name_char(c: char) -> bool {
    name_start(c)
        || matches!(c, '-' | '.' | '0'..='9' | '\u{B7}' | '\u{300}'..='\u{36F}' | '\u{203F}'..='\u{2040}')
}

/// A name without a colon, as namespaces allow in each part of a name.
pub fn is_ncname(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(name_start) && chars.all(name_char)
}

/// `local` or `prefix:local`.
pub(crate) fn is_qname(s: &str) -> bool {
    match s.split_once(':') {
        Some((p, l)) => is_ncname(p) && is_ncname(l),
        None => is_ncname(s),
    }
}

/// A Name as the DTD uses them (entity names, PI targets): colons allowed.
fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c == ':' || name_start(c))
        && chars.all(|c| c == ':' || name_char(c))
}

/// Characters XML allows in a document.
fn legal(c: char, v11: bool) -> bool {
    match c {
        '\t' | '\n' | '\r' => true,
        '\u{1}'..='\u{1F}' => v11,
        '\u{0}' => false,
        '\u{FFFE}' | '\u{FFFF}' => false,
        _ => true,
    }
}

/// Checks text as written in the document. XML 1.1 allows the control
/// characters only as character references, so both versions check alike.
fn check_chars(s: &str) -> Result<(), String> {
    // Most text is plain: only look closer at what could be illegal.
    if s.bytes().all(|b| b >= 0x20 && b != 0xEF) {
        return Ok(());
    }
    match s.chars().find(|&c| !legal(c, false)) {
        Some(c) => Err(format!(
            "the character U+{:04X} isn't allowed in XML",
            c as u32
        )),
        None => Ok(()),
    }
}

fn prefix_part(qname: &str) -> &str {
    qname.split_once(':').map_or("", |(prefix, _)| prefix)
}

fn is_blank(s: &str) -> bool {
    s.bytes().all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
}

/// HTML's named character `name` (`nbsp` gives U+00A0), if there is one.
pub fn html_entity(name: &str) -> Option<String> {
    html::named_reference(&format!("{name};")).map(str::to_string)
}

/// Whether `name` is an element HTML has (`div`, `title`; not `rss`).
pub fn is_html_tag(name: &str) -> bool {
    html::is_html_element(name)
}

fn predefined(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => return None,
    })
}

/// `&#...;` (given without `&` and `;`) as a character.
fn char_ref(body: &str, v11: bool) -> Result<char, String> {
    let digits = body.strip_prefix('#').unwrap_or(body);
    let n = match digits.strip_prefix('x') {
        Some(hex) if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) => {
            u32::from_str_radix(hex, 16).ok()
        }
        None if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
            digits.parse().ok()
        }
        _ => return Err(format!("&{body}; isn't a valid character reference")),
    };
    match n.and_then(char::from_u32) {
        Some(c) if legal(c, v11) => Ok(c),
        _ => Err(format!("&{body}; refers to a character XML doesn't allow")),
    }
}

// --- the DTD's internal subset ---------------------------------------------

#[derive(Clone)]
enum Entity {
    /// Declared with its replacement text.
    Internal(Rc<str>),
    /// Declared with SYSTEM or PUBLIC: it would have to be fetched.
    External,
}

/// The general entities declared in a DOCTYPE's internal subset. `doctype`
/// is what follows `<!DOCTYPE`. Parameter entities are not expanded; as
/// XML asks of a processor that doesn't read them, declarations after a
/// parameter entity reference are not read either.
fn declared_entities(doctype: &str, v11: bool) -> Result<HashMap<String, Entity>, String> {
    let mut out = HashMap::new();
    let bytes = doctype.as_bytes();
    // Find the internal subset's `[`, skipping quoted literals.
    let mut i = 0;
    let mut quote = None;
    while i < bytes.len() {
        match (quote, bytes[i]) {
            (None, b'"' | b'\'') => quote = Some(bytes[i]),
            (Some(q), b) if b == q => quote = None,
            (None, b'[') => break,
            _ => {}
        }
        i += 1;
    }
    let mut s = &doctype[(i + 1).min(doctype.len())..];
    loop {
        s = s.trim_start_matches([' ', '\t', '\r', '\n']);
        if s.is_empty() || s.starts_with(']') {
            return Ok(out);
        }
        if let Some(rest) = s.strip_prefix("<!--") {
            let end = rest.find("-->").ok_or("a comment in the DTD never ends")?;
            s = &rest[end + 3..];
        } else if let Some(rest) = s.strip_prefix("<?") {
            let end = rest
                .find("?>")
                .ok_or("a processing instruction in the DTD never ends")?;
            s = &rest[end + 2..];
        } else if s.starts_with('%') {
            // A parameter entity reference: what follows can't be trusted
            // to mean what it says without reading it.
            return Ok(out);
        } else if let Some(rest) = s.strip_prefix("<!ENTITY") {
            let (decl, after) = split_decl(rest)?;
            s = after;
            if let Some((name, entity)) = entity_decl(decl, v11)? {
                // The first declaration of a name is the one that counts.
                out.entry(name).or_insert(entity);
            }
        } else if s.starts_with("<!") {
            let (_, after) = split_decl(&s[2..])?;
            s = after;
        } else {
            let shown: String = s.chars().take(20).collect();
            return Err(format!(
                "the DTD has {shown:?} where a declaration was expected"
            ));
        }
    }
}

/// A markup declaration's body up to its closing `>`, and what follows.
fn split_decl(s: &str) -> Result<(&str, &str), String> {
    let mut quote = None;
    for (i, b) in s.bytes().enumerate() {
        match (quote, b) {
            (None, b'"' | b'\'') => quote = Some(b),
            (Some(q), b) if b == q => quote = None,
            (None, b'>') => return Ok((&s[..i], &s[i + 1..])),
            _ => {}
        }
    }
    Err("a declaration in the DTD never ends".into())
}

/// `name "value"`, `name SYSTEM "uri"`, `name PUBLIC "id" "uri"`, or a
/// parameter entity (`% name ...`), which gives None.
fn entity_decl(decl: &str, v11: bool) -> Result<Option<(String, Entity)>, String> {
    let ws = |c: char| matches!(c, ' ' | '\t' | '\r' | '\n');
    let decl = decl.trim_matches(ws);
    if decl.starts_with('%') {
        return Ok(None);
    }
    let end = decl.find(ws).ok_or("an ENTITY declaration has no value")?;
    let (name, rest) = decl.split_at(end);
    if !is_name(name) || name.contains(':') {
        return Err(format!("{name:?} isn't a valid entity name"));
    }
    let rest = rest.trim_start_matches(ws);
    let entity = match rest.chars().next() {
        Some(q @ ('"' | '\'')) => {
            let body = &rest[1..];
            let close = body
                .find(q)
                .ok_or("an entity's value has no closing quote")?;
            Entity::Internal(entity_value(&body[..close], v11)?.into())
        }
        _ if rest.starts_with("SYSTEM") || rest.starts_with("PUBLIC") => Entity::External,
        _ => return Err(format!("the ENTITY declaration of {name} has no value")),
    };
    Ok(Some((name.to_string(), entity)))
}

/// An entity's literal value with its character references replaced, as
/// XML does when the entity is declared. General entity references stay,
/// to be expanded where the entity is used.
fn entity_value(raw: &str, v11: bool) -> Result<String, String> {
    if raw.contains('%') {
        return Err("parameter entity references can't be used inside an entity's value".into());
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(at) = rest.find("&#") {
        out.push_str(&rest[..at]);
        let end = rest[at..]
            .find(';')
            .ok_or("a character reference has no `;`")?;
        out.push(char_ref(&rest[at + 1..at + end], v11)?);
        rest = &rest[at + end + 1..];
    }
    out.push_str(rest);
    check_chars(&out)?;
    Ok(out)
}

// --- building ---------------------------------------------------------------

/// How to read a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XmlOptions {
    /// Repair what can be repaired instead of failing, as lxml's recover
    /// mode does, and say what was repaired (see [`Repairs`]). Undeclared
    /// entities are read as HTML's named characters if they are one, and
    /// kept as text if not; a bare `&` is kept as text; an undeclared
    /// prefix puts its element or attribute in no namespace; anything
    /// after the root element is ignored; characters XML doesn't allow are
    /// dropped; of two attributes with one name the first is kept; an end
    /// tag for an element further out closes what's inside it, and one
    /// that closes nothing is ignored; a `<` that starts no markup is
    /// text; and elements still open at the end are closed. Anything else
    /// that isn't well-formed is still an error.
    pub recover: bool,
    /// How long parsing may take. In [`XmlNodes`], how long each node may
    /// take to read.
    pub budget: Duration,
}

impl Default for XmlOptions {
    fn default() -> XmlOptions {
        XmlOptions {
            recover: false,
            budget: Duration::MAX,
        }
    }
}

/// What recover mode repaired: how many problems, and the first few of
/// them, each with where it was.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Repairs {
    pub count: usize,
    pub first: Vec<XmlError>,
}

/// How many repairs [`Repairs`] keeps the details of.
pub const REPAIRS_KEPT: usize = 10;

impl Repairs {
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// One line for a log: how many, and the first few with their place.
    pub fn summary(&self) -> String {
        let shown: Vec<String> = self
            .first
            .iter()
            .map(|e| format!("line {}, column {}: {}", e.line, e.column, e.reason))
            .collect();
        let more = self.count - self.first.len();
        let tail = if more > 0 {
            format!("; and {more} more")
        } else {
            String::new()
        };
        let what = if self.count == 1 {
            "problem"
        } else {
            "problems"
        };
        format!(
            "repaired {} {what} in the XML: {}{tail}",
            self.count,
            shown.join("; ")
        )
    }
}

/// What [`walk_xml`] tells as it reads: each element inside the root, its
/// text, and its end. Names are as written, prefix included.
pub trait XmlVisitor {
    fn start(&mut self, qname: &str, attrs: &[(String, String)]);
    fn text(&mut self, text: &str);
    fn end(&mut self, qname: &str);
    /// Asked as often as the clock is: true ends the reading there, as
    /// running out of time does, with [`ParseXmlError::Timeout`]. For a
    /// reader whose caller has gone. Never, by default.
    fn stopped(&self) -> bool {
        false
    }
}

enum Stop {
    Bad(String),
    Late,
    /// In recover mode: the `<` the event started with starts no markup,
    /// and is text; reading goes on after it.
    LessThan,
}

impl From<String> for Stop {
    fn from(s: String) -> Stop {
        Stop::Bad(s)
    }
}

impl From<&str> for Stop {
    fn from(s: &str) -> Stop {
        Stop::Bad(s.to_string())
    }
}

type Step<T = ()> = Result<T, Stop>;

const LESS_THAN: &str = "a `<` that starts no markup";

/// An element that has started and not ended.
struct Open {
    qname: String,
    /// How many namespace bindings were in scope before it.
    mark: usize,
    /// Its node, when it's part of what's being built.
    node: Option<u32>,
}

/// What reading gives: the whole document, each element with this
/// namespace and local name alone, or only events for a visitor.
enum Want {
    Whole,
    Nodes {
        /// The namespace asked for; None to take the prefix's meaning from
        /// the document, where it's declared.
        ns: Option<String>,
        prefix: String,
        local: String,
    },
    Walk,
}

/// The document being built, and how deep in the source it starts.
struct Out {
    doc: Document,
    depth: usize,
}

/// A node read alone, and the prefix of its name with the namespace the
/// document bound it to, when that came from the document.
pub type Ready = (Document, Option<(String, String)>);

/// Work between looks at the clock: events, attributes, and 64-byte
/// pieces of expanded entities.
const WORK_BETWEEN_CHECKS: u64 = 1024;

struct Builder<'v> {
    want: Want,
    recover: bool,
    out: Option<Out>,
    ready: VecDeque<Ready>,
    visitor: Option<&'v mut dyn XmlVisitor>,
    /// Namespace bindings in scope: prefix ("" for the default) and URI,
    /// with where each prefix's bindings are, innermost last.
    bindings: Vec<(String, String)>,
    by_prefix: HashMap<String, Vec<usize>>,
    open: Vec<Open>,
    /// How many of `open` have each name, so recover mode can tell at once
    /// whether an end tag closes anything open.
    open_names: HashMap<String, usize>,
    entities: HashMap<String, Entity>,
    /// Entities being expanded, outermost first.
    expanding: Vec<String>,
    /// How many elements were open when the innermost expansion began:
    /// an entity's content can't close more than it opened.
    floor: usize,
    expanded: usize,
    allowance: usize,
    /// Text not yet put in a node, so text, CDATA and references next to
    /// each other become one text node, as in lxml.
    pending: String,
    /// Whether `pending` has a CDATA section in it, so even an empty one
    /// makes a text node, as in lxml.
    pending_cdata: bool,
    root_seen: bool,
    root_done: bool,
    doctype_seen: bool,
    /// Whether an XML declaration may come next: before anything but
    /// whitespace.
    decl_allowed: bool,
    /// In recover mode, set once something comes after the root: the rest
    /// is ignored.
    stopped: bool,
    v11: bool,
    work: u64,
    next_check: u64,
    deadline: Option<Instant>,
    /// Where in the source the event being read starts.
    at: usize,
    /// For the node being read (Nodes mode): its prefix and namespace,
    /// when the document bound the prefix.
    binding: Option<(String, String)>,
    /// Repairs made: where, and what.
    repairs: Vec<(usize, String)>,
    repair_count: usize,
}

impl<'v> Builder<'v> {
    fn new(want: Want, size: usize, options: XmlOptions) -> Builder<'v> {
        let out = matches!(want, Want::Whole).then(|| Out {
            doc: Document::new_xml(),
            depth: 0,
        });
        Builder {
            want,
            recover: options.recover,
            out,
            ready: VecDeque::new(),
            visitor: None,
            bindings: Vec::new(),
            by_prefix: HashMap::new(),
            open: Vec::new(),
            open_names: HashMap::new(),
            entities: HashMap::new(),
            expanding: Vec::new(),
            floor: 0,
            expanded: 0,
            allowance: ENTITY_ALLOWANCE.saturating_add(size.saturating_mul(ENTITY_FACTOR)),
            pending: String::new(),
            pending_cdata: false,
            root_seen: false,
            root_done: false,
            doctype_seen: false,
            decl_allowed: true,
            stopped: false,
            v11: false,
            work: 0,
            next_check: WORK_BETWEEN_CHECKS,
            deadline: Instant::now().checked_add(options.budget),
            at: 0,
            binding: None,
            repairs: Vec::new(),
            repair_count: 0,
        }
    }

    fn version(&self) -> XmlVersion {
        if self.v11 {
            XmlVersion::Explicit1_1
        } else {
            XmlVersion::Explicit1_0
        }
    }

    /// Counts work, and looks at the clock now and then.
    fn tick(&mut self, units: u64) -> Step {
        self.work += units;
        if self.work >= self.next_check {
            self.next_check = self.work + WORK_BETWEEN_CHECKS;
            if self.deadline.is_some_and(|d| Instant::now() > d)
                || self.visitor.as_ref().is_some_and(|v| v.stopped())
            {
                return Err(Stop::Late);
            }
        }
        Ok(())
    }

    /// In recover mode, notes that `problem` was repaired by `fix` and
    /// carries on; otherwise `problem` is the error.
    fn repair(&mut self, problem: impl Into<String>, fix: &str) -> Step {
        let problem = problem.into();
        if !self.recover {
            return Err(Stop::Bad(problem));
        }
        self.repair_count += 1;
        if self.repairs.len() < REPAIRS_KEPT {
            self.repairs.push((self.at, format!("{problem}; {fix}")));
        }
        Ok(())
    }

    fn repairs(&self, src: &str) -> Repairs {
        Repairs {
            count: self.repair_count,
            first: self
                .repairs
                .iter()
                .map(|(at, why)| error_at(src, *at, why.clone()))
                .collect(),
        }
    }

    /// Text as written, with the characters XML doesn't allow dropped in
    /// recover mode.
    fn clean<'t>(&mut self, text: &'t str) -> Result<Cow<'t, str>, Stop> {
        match check_chars(text) {
            Ok(()) => Ok(Cow::Borrowed(text)),
            Err(why) => {
                self.repair(why, "dropped")?;
                Ok(Cow::Owned(
                    text.chars().filter(|&c| legal(c, false)).collect(),
                ))
            }
        }
    }

    /// The node new content goes into, if what's being read is being built.
    fn parent(&self) -> Option<u32> {
        let out = self.out.as_ref()?;
        if self.open.len() > out.depth {
            self.open.last().and_then(|o| o.node)
        } else if matches!(self.want, Want::Whole) {
            Some(0)
        } else {
            None
        }
    }

    fn append(&mut self, node: u32) {
        if let (Some(parent), Some(out)) = (self.parent(), self.out.as_mut()) {
            out.doc
                .tree_to_build()
                .append(parent, node)
                .expect("an XML element takes children");
        }
    }

    fn flush(&mut self) {
        if self.pending.is_empty() && !self.pending_cdata {
            return;
        }
        if let Some(visitor) = self.visitor.as_deref_mut() {
            visitor.text(&self.pending);
        } else if let (Some(_), Some(out)) = (self.parent(), self.out.as_mut()) {
            let n = out.doc.tree_to_build().create_text(&self.pending);
            self.append(n);
        }
        self.pending.clear();
        self.pending_cdata = false;
    }

    /// Something other than whitespace outside the root element. Before
    /// the root it's an error (dropped, in recover mode); after it, recover
    /// mode stops reading there.
    fn outside(&mut self, what: String) -> Step {
        if self.root_done && self.recover {
            self.repair(
                format!("{what} after the root element"),
                "the rest is ignored",
            )?;
            self.stopped = true;
            return Ok(());
        }
        if self.root_done {
            return Err(format!("{what} after the root element; a document has one root").into());
        }
        self.repair(format!("{what} outside the root element"), "dropped")
    }

    fn event(&mut self, ev: Event<'_>) -> Step {
        self.tick(1)?;
        let decl_allowed = self.decl_allowed;
        if !matches!(ev, Event::Text(_)) {
            self.decl_allowed = false;
        }
        match ev {
            Event::Decl(d) => {
                if !decl_allowed {
                    return Err("an XML declaration can only come at the very start".into());
                }
                match d.version().map_err(|e| e.to_string())?.as_ref() {
                    "1.0" => {}
                    "1.1" => self.v11 = true,
                    v => return Err(format!("XML version {v:?} isn't 1.0 or 1.1").into()),
                }
            }
            Event::DocType(t) => {
                if self.root_seen {
                    return self.outside("a DOCTYPE".into());
                }
                if self.doctype_seen {
                    return Err("a document can only have one DOCTYPE".into());
                }
                self.doctype_seen = true;
                self.entities = declared_entities(&t.into_inner(), self.v11)?;
            }
            Event::PI(p) => {
                let target = p.target();
                if target.eq_ignore_ascii_case("xml") {
                    return Err("an XML declaration can only come at the very start".into());
                }
                if !is_name(target) {
                    return Err(
                        format!("{target:?} isn't a valid processing instruction target").into(),
                    );
                }
                let data = p.content().trim_start_matches([' ', '\t', '\r', '\n']);
                let data = self.clean(data)?.into_owned();
                self.flush();
                if self.parent().is_some()
                    && let Some(out) = self.out.as_mut()
                {
                    let n = out
                        .doc
                        .tree_to_build()
                        .create_processing_instruction(target, &data);
                    self.append(n);
                }
            }
            Event::Comment(c) => {
                let text = c.xml_content(self.version());
                let text = self.clean(&text)?.into_owned();
                self.flush();
                if self.parent().is_some()
                    && let Some(out) = self.out.as_mut()
                {
                    let n = out.doc.tree_to_build().create_comment(&text);
                    self.append(n);
                }
            }
            Event::Start(e) => self.start(&e, false)?,
            Event::Empty(e) => self.start(&e, true)?,
            Event::End(e) => self.end(e.name().as_ref())?,
            Event::Text(t) => {
                let text = t.xml_content(self.version());
                if !is_blank(&text) {
                    self.decl_allowed = false;
                }
                if text.contains("]]>") {
                    return Err("`]]>` can't appear in text (write ]]&gt;)".into());
                }
                // References come as events of their own, so an `&` here
                // is one that starts none (allowed only in recover mode).
                if text.contains('&') {
                    self.repair("a `&` starts no reference", "kept as text")?;
                }
                self.text(&text)?;
            }
            Event::CData(c) => {
                if self.open.is_empty() {
                    return self.outside("CDATA".into());
                }
                let text = c.xml_content(self.version());
                self.text(&text)?;
                // As lxml has it: an empty section is an empty text node in
                // a document parsed whole, and nothing in one read node by
                // node (Scrapy's iternodes gives <x/>).
                if self.capturing() && !matches!(self.want, Want::Nodes { .. }) {
                    self.pending_cdata = true;
                }
            }
            Event::GeneralRef(r) => {
                if self.open.is_empty() {
                    return self.outside("an entity reference".into());
                }
                let body = r.into_inner();
                if body.starts_with('#') {
                    let c = match char_ref(&body, self.v11) {
                        Ok(c) => Some(c),
                        Err(why) => {
                            self.repair(why, "dropped")?;
                            None
                        }
                    };
                    if let (Some(c), true) = (c, self.capturing()) {
                        self.pending.push(c);
                    }
                } else if let Some(c) = predefined(&body) {
                    if self.capturing() {
                        self.pending.push(c);
                    }
                } else {
                    self.expand(&body)?;
                }
            }
            Event::Eof => {}
        }
        Ok(())
    }

    /// Whether text read now is wanted.
    fn capturing(&self) -> bool {
        if matches!(self.want, Want::Walk) {
            return !self.open.is_empty();
        }
        self.out
            .as_ref()
            .is_some_and(|out| self.open.len() > out.depth)
    }

    fn text(&mut self, text: &str) -> Step {
        if self.open.is_empty() {
            if !is_blank(text) {
                let shown: String = text.trim().chars().take(30).collect();
                return self.outside(format!("text ({shown:?})"));
            }
            return Ok(());
        }
        let text = self.clean(text)?;
        if self.capturing() {
            self.pending.push_str(&text);
        }
        Ok(())
    }

    /// Counts an expansion of `name`, refusing one too deep, one that
    /// refers to itself, and one past the document's allowance.
    fn enter(&mut self, name: &str, size: usize) -> Step {
        if self.expanding.iter().any(|n| n == name) {
            return Err(format!("the entity &{name}; refers to itself").into());
        }
        if self.expanding.len() >= MAX_ENTITY_DEPTH {
            return Err(
                format!("entities nest more than {MAX_ENTITY_DEPTH} deep (at &{name};)").into(),
            );
        }
        self.expanded = self.expanded.saturating_add(size);
        if self.expanded > self.allowance {
            return Err(format!(
                "expanding entities would take more than {} bytes, which this document's size allows (a \"billion laughs\" document?)",
                self.allowance
            )
            .into());
        }
        self.tick(size as u64 / 64 + 1)?;
        self.expanding.push(name.to_string());
        Ok(())
    }

    /// An undeclared entity in recover mode: HTML's named character of
    /// that name if there is one, else the reference as written.
    fn undeclared(&mut self, name: &str) -> Result<String, Stop> {
        let html = html_entity(name);
        let how = if html.is_some() {
            "read as HTML's"
        } else {
            "kept as text"
        };
        self.repair(format!("the entity &{name}; isn't declared"), how)?;
        Ok(html.unwrap_or_else(|| format!("&{name};")))
    }

    /// The replacement text of a declared entity; None (after a repair)
    /// for one that isn't declared.
    fn entity(&mut self, name: &str) -> Result<Option<Rc<str>>, Stop> {
        match self.entities.get(name) {
            Some(Entity::Internal(v)) => Ok(Some(v.clone())),
            Some(Entity::External) => Err(format!(
                "&{name}; is an external entity, and external entities are never fetched"
            )
            .into()),
            None if self.recover => Ok(None),
            None => Err(format!("the entity &{name}; isn't declared").into()),
        }
    }

    /// Reads a declared entity's replacement text where it's referred to.
    fn expand(&mut self, name: &str) -> Step {
        let Some(value) = self.entity(name)? else {
            let text = self.undeclared(name)?;
            if self.capturing() {
                self.pending.push_str(&text);
            }
            return Ok(());
        };
        self.enter(name, value.len())?;
        let floor = std::mem::replace(&mut self.floor, self.open.len());
        let mut result = self.read_entity(&value);
        if result.is_ok() && self.open.len() != self.floor {
            let left = &self.open[self.open.len() - 1].qname;
            result = Err(format!("the entity &{name}; leaves <{left}> open").into());
        }
        self.floor = floor;
        self.expanding.pop();
        result.map_err(|e| match e {
            Stop::Bad(why) if !why.starts_with("in &") => Stop::Bad(format!("in &{name};: {why}")),
            other => other,
        })
    }

    fn read_entity(&mut self, value: &str) -> Step {
        let value = &value[self.lead(value)?..];
        let mut reader = reader(value, self.recover);
        loop {
            match reader.read_event().map_err(|e| e.to_string())? {
                Event::Eof => return Ok(()),
                Event::Decl(_) | Event::DocType(_) => {
                    return Err("an entity can't contain a declaration".into());
                }
                ev => match self.event(ev) {
                    Err(Stop::LessThan) => {
                        return Err(LESS_THAN.into());
                    }
                    other => other?,
                },
            }
        }
    }

    /// An attribute's value as XML normalises it: references replaced,
    /// and each line break or tab a space.
    fn attr_value(&mut self, raw: &str, out: &mut String) -> Step {
        let mut rest = raw;
        while let Some(at) = rest.find(['&', '<', '\r', '\n', '\t']) {
            let head = self.clean(&rest[..at])?;
            out.push_str(&head);
            let c = rest.as_bytes()[at];
            rest = &rest[at + 1..];
            match c {
                b'<' => return Err("`<` isn't allowed in an attribute value".into()),
                b'\r' => {
                    rest = rest.strip_prefix('\n').unwrap_or(rest);
                    out.push(' ');
                }
                b'\n' | b'\t' => out.push(' '),
                _ => {
                    // A reference is a name (or #digits) and a `;`. Read as
                    // far as one could go, not to the next `;`, which for a
                    // value of many `&`s would mean rereading the rest.
                    let len = rest
                        .char_indices()
                        .find(|&(i, c)| !(c == ':' || name_char(c) || (i == 0 && c == '#')))
                        .map_or(rest.len(), |(i, _)| i);
                    let end = (rest[len..].starts_with(';') && len > 0)
                        .then_some(len)
                        .filter(|&e| rest[..e].starts_with('#') || is_name(&rest[..e]));
                    let Some(end) = end else {
                        self.repair(
                            "`&` in an attribute value starts no reference (write &amp;)",
                            "kept as text",
                        )?;
                        out.push('&');
                        continue;
                    };
                    let body = &rest[..end];
                    rest = &rest[end + 1..];
                    if body.starts_with('#') {
                        match char_ref(body, self.v11) {
                            Ok(c) => out.push(c),
                            Err(why) => self.repair(why, "dropped")?,
                        }
                    } else if let Some(c) = predefined(body) {
                        out.push(c);
                    } else {
                        let Some(value) = self.entity(body)? else {
                            let text = self.undeclared(body)?;
                            out.push_str(&text);
                            continue;
                        };
                        if value.contains('<') {
                            return Err(format!(
                                "the entity &{body}; contains `<`, so it can't be used in an attribute value"
                            )
                            .into());
                        }
                        self.enter(body, value.len())?;
                        let result = self.attr_value(&value, out);
                        self.expanding.pop();
                        result?;
                    }
                }
            }
        }
        let tail = self.clean(rest)?;
        out.push_str(&tail);
        Ok(())
    }

    /// The URI `prefix` is bound to; None for no namespace.
    fn resolve(&self, prefix: &str) -> Option<&str> {
        if prefix == "xml" {
            return Some(XML_NS);
        }
        let at = *self.by_prefix.get(prefix)?.last()?;
        Some(self.bindings[at].1.as_str()).filter(|u| !u.is_empty())
    }

    fn bind(&mut self, prefix: &str, uri: String) -> Step {
        if prefix == "xmlns" {
            return Err("the xmlns prefix can't be declared".into());
        }
        if (prefix == "xml") != (uri == XML_NS) {
            return Err(format!(
                "the xml prefix is only ever bound to {XML_NS}, and nothing else can be"
            )
            .into());
        }
        if uri == XMLNS_NS {
            return Err(format!("{XMLNS_NS} can't be declared as a namespace").into());
        }
        if !prefix.is_empty() && uri.is_empty() && !self.v11 {
            return Err(format!("xmlns:{prefix} can't be empty in XML 1.0").into());
        }
        if !prefix.is_empty() && !is_ncname(prefix) {
            return Err(format!("{prefix:?} isn't a valid namespace prefix").into());
        }
        self.by_prefix
            .entry(prefix.to_string())
            .or_default()
            .push(self.bindings.len());
        self.bindings.push((prefix.to_string(), uri));
        Ok(())
    }

    /// Drops the bindings from `mark` on.
    fn unbind_to(&mut self, mark: usize) {
        while self.bindings.len() > mark {
            let (prefix, _) = self.bindings.pop().expect("longer than mark");
            if let Some(at) = self.by_prefix.get_mut(&prefix) {
                at.pop();
            }
        }
    }

    /// The URI of `qname`'s prefix ("" for none). An undeclared prefix is
    /// an error, or in recover mode no namespace.
    fn namespace_of(&mut self, qname: &str, what: &str) -> Result<String, Stop> {
        match qname.split_once(':') {
            None if what == "element" => Ok(self.resolve("").unwrap_or("").to_string()),
            None => Ok(String::new()),
            Some((p, _)) => match self.resolve(p) {
                Some(u) => Ok(u.to_string()),
                None => {
                    let named = if what == "element" {
                        format!("<{qname}>")
                    } else {
                        format!("the attribute {qname}")
                    };
                    self.repair(
                        format!("the namespace prefix {p} of {named} isn't declared"),
                        "it's in no namespace",
                    )?;
                    Ok(String::new())
                }
            },
        }
    }

    fn start(&mut self, e: &BytesStart<'_>, empty: bool) -> Step {
        let qname = e.name().as_ref().to_string();
        if !is_qname(&qname) {
            // `1 < 2`, `a<=b`: no name follows the `<`, so it's no tag.
            if self.recover && !qname.chars().next().is_some_and(name_start) {
                return Err(Stop::LessThan);
            }
            return Err(format!("<{qname}> isn't a valid element name").into());
        }
        self.flush();
        if self.open.is_empty() {
            if self.root_done {
                return self.outside(format!("<{qname}>"));
            }
            self.root_seen = true;
        }
        if self.open.len() >= MAX_DEPTH {
            return Err(format!("elements nest more than {MAX_DEPTH} deep").into());
        }
        let mark = self.bindings.len();
        let mut attrs: Vec<(String, String)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut declared: HashSet<String> = HashSet::new();
        // Checked here, in linear time: quick-xml's own check compares
        // every attribute with every other.
        let mut all = e.attributes();
        all.with_checks(false);
        for a in all {
            self.tick(1)?;
            let a = a.map_err(|e| format!("in <{qname}>: {e}"))?;
            let key = a.key.as_ref();
            let first = if key == "xmlns" || key.starts_with("xmlns:") {
                declared.insert(key.to_string())
            } else {
                seen.insert(key.to_string())
            };
            if !first {
                self.repair(
                    format!("<{qname}> has the attribute {key} twice"),
                    "the first is kept",
                )?;
                continue;
            }
            let mut value = String::new();
            self.attr_value(&a.value, &mut value)?;
            if key == "xmlns" {
                self.bind("", value)?;
            } else if let Some(prefix) = key.strip_prefix("xmlns:") {
                if !is_ncname(prefix) {
                    return Err(format!("{prefix:?} isn't a valid namespace prefix").into());
                }
                self.bind(prefix, value)?;
            } else if !is_qname(key) {
                return Err(format!("{key:?} isn't a valid attribute name").into());
            } else {
                attrs.push((key.to_string(), value));
            }
        }
        let local = qname.split_once(':').map_or(qname.as_str(), |(_, l)| l);
        let ns = self.namespace_of(&qname, "element")?;
        let mut attr_ns: Vec<String> = Vec::with_capacity(attrs.len());
        // Two prefixes for one namespace still name one attribute.
        let mut expanded: HashSet<(String, String)> = HashSet::new();
        let mut keep = vec![true; attrs.len()];
        for (i, (key, _)) in attrs.iter().enumerate() {
            let uri = self.namespace_of(key, "attribute")?;
            if !uri.is_empty() {
                let local = key.split_once(':').map_or(key.as_str(), |(_, l)| l);
                if !expanded.insert((uri.clone(), local.to_string())) {
                    self.repair(
                        format!(
                            "<{qname}> has {key} and another name for it, the same attribute twice"
                        ),
                        "the first is kept",
                    )?;
                    keep[i] = false;
                }
            }
            attr_ns.push(uri);
        }
        let mut doc_binding = None;
        let wanted = match &self.want {
            Want::Whole | Want::Walk => false,
            Want::Nodes {
                ns: want_ns,
                prefix,
                local: want_local,
            } => {
                want_local == local
                    && match want_ns {
                        Some(u) => *u == ns,
                        None if prefix.is_empty() => ns.is_empty(),
                        None => {
                            let found = self.resolve(prefix).is_some_and(|u| u == ns);
                            if found {
                                doc_binding = Some((prefix.clone(), ns.clone()));
                            }
                            found
                        }
                    }
            }
        };
        if let Some(visitor) = self.visitor.as_deref_mut() {
            let kept: Vec<(String, String)> = attrs
                .iter()
                .zip(&keep)
                .filter(|(_, k)| **k)
                .map(|(a, _)| a.clone())
                .collect();
            visitor.start(&qname, &kept);
        }
        if self.out.is_none() && wanted {
            self.out = Some(Out {
                doc: Document::new_xml(),
                depth: self.open.len(),
            });
        }
        let mut node = None;
        let top = self
            .out
            .as_ref()
            .is_some_and(|o| o.depth == self.open.len());
        if let Some(out) = self.out.as_mut().filter(|o| self.open.len() >= o.depth) {
            let _ = &local;
            let n = out
                .doc
                .tree_to_build()
                .create_element(html::Ns::Xml, &qname);
            node = Some(n);
            if !ns.is_empty() {
                let names = out.doc.xml_names_mut();
                let id = names.intern(&ns);
                names.ns.insert(n as usize, id);
            }
            for (((key, value), uri), _) in
                attrs.iter().zip(&attr_ns).zip(&keep).filter(|(_, k)| **k)
            {
                let attr_ns = if uri.is_empty() {
                    html::AttrNs::None
                } else {
                    html::AttrNs::Host(out.doc.xml_names_mut().intern(uri))
                };
                out.doc.tree_to_build().add_attr(n, attr_ns, key, value);
            }
            let names = out.doc.xml_names_mut();
            // A node read alone keeps what the elements it was cut from
            // declared, nearest first, each in the order written, for
            // writing it out as it would be written from the whole.
            if top && !matches!(self.want, Want::Whole) {
                let mut end = mark;
                for open in self.open.iter().rev() {
                    for (p, u) in &self.bindings[open.mark..end] {
                        let id = names.intern(u);
                        names.outer.push((Box::from(p.as_str()), id));
                    }
                    end = open.mark;
                }
            }
            let declared = &self.bindings[mark..];
            if !declared.is_empty() {
                let decls = declared
                    .iter()
                    .map(|(p, u)| (Box::from(p.as_str()), names.intern(u)))
                    .collect();
                names.decls.insert(n as usize, decls);
            }
            let tree = out.doc.tree_to_build();
            if top {
                // The node of its own document, or the root of the whole.
                tree.append(0, n).expect("a document takes its root");
            } else if let Some(parent) = self.open.last().and_then(|o| o.node) {
                tree.append(parent, n)
                    .expect("an XML element takes children");
            }
        }
        if top {
            self.binding = doc_binding;
        }
        *self.open_names.entry(qname.clone()).or_default() += 1;
        self.open.push(Open { qname, mark, node });
        if empty {
            self.close()?;
        }
        Ok(())
    }

    fn end(&mut self, name: &str) -> Step {
        self.flush();
        let Some(top) = self.open.last() else {
            return self.repair(
                format!("</{name}> closes an element that never started"),
                "ignored",
            );
        };
        if self.open.len() <= self.floor {
            return Err(format!("</{name}> would close an element the entity didn't start").into());
        }
        if top.qname == name {
            return self.close();
        }
        let problem = format!("</{name}> doesn't close <{}>", top.qname);
        // As lxml's recover mode does: an end tag for an element further
        // out closes everything inside it; one for no open element is
        // ignored.
        // Looked up first: a run of end tags that close nothing would
        // otherwise each search every open element.
        let ancestor = self
            .open_names
            .contains_key(name)
            .then(|| {
                self.open[self.floor..]
                    .iter()
                    .rposition(|o| o.qname == name)
            })
            .flatten()
            .map(|i| i + self.floor);
        match ancestor {
            Some(at) => {
                let inside: Vec<&str> = self.open[at + 1..]
                    .iter()
                    .map(|o| o.qname.as_str())
                    .collect();
                let fix = format!("<{}> closed with it", inside.join("><"));
                self.repair(problem, &fix)?;
                while self.open.len() > at {
                    self.close()?;
                }
                Ok(())
            }
            None => self.repair(problem, "ignored, as it closes nothing open"),
        }
    }

    /// Ends the innermost open element.
    fn close(&mut self) -> Step {
        self.flush();
        let top = self.open.pop().expect("an element is open");
        if let Some(count) = self.open_names.get_mut(&top.qname) {
            *count -= 1;
            if *count == 0 {
                self.open_names.remove(&top.qname);
            }
        }
        self.unbind_to(top.mark);
        if let Some(visitor) = self.visitor.as_deref_mut() {
            visitor.end(&top.qname);
        }
        if self.open.is_empty() {
            self.root_done = true;
        }
        if matches!(self.want, Want::Nodes { .. })
            && self
                .out
                .as_ref()
                .is_some_and(|o| o.depth == self.open.len())
        {
            let doc = self.out.take().map(|o| o.doc).expect("checked above");
            self.ready.push_back((doc, self.binding.take()));
        }
        Ok(())
    }

    /// Takes the U+FEFFs that start `src` as text, and returns their
    /// length, or 0 when there are none. quick-xml drops one there as a
    /// byte order mark, and leaves it out of its positions, which then
    /// point three bytes short: the document's own mark is already gone
    /// by here, so these are characters. All of them, as a reader started
    /// after the first would drop the second.
    fn lead(&mut self, src: &str) -> Step<usize> {
        let rest = src.trim_start_matches('\u{feff}');
        let lead = &src[..src.len() - rest.len()];
        if !lead.is_empty() {
            self.text(lead)?;
        }
        Ok(lead.len())
    }

    /// [`Self::lead`] for `src` from byte `start`: where a reader starts.
    fn lead_at(&mut self, src: &str, start: usize) -> Result<usize, (Stop, usize)> {
        self.at = start;
        match self.lead(&src[start..]) {
            Ok(n) => Ok(start + n),
            Err(stop) => Err((stop, start)),
        }
    }

    /// Reads `src` from byte `start` until its end, or in Nodes mode until
    /// a node is ready. Returns where it stopped, or why and where it
    /// failed.
    fn pump(&mut self, src: &str, start: usize) -> Result<usize, (Stop, usize)> {
        let mut start = start;
        start = self.lead_at(src, start)?;
        let mut reader = reader(&src[start..], self.recover);
        loop {
            let before = start + reader.buffer_position() as usize;
            self.at = before;
            // A `<` that can't start markup, in recover mode: it's text.
            // Looked at here, not left to quick-xml, which reads on to
            // the next `>` before giving up on it, so a run of them would
            // cost the square of its length.
            if self.recover
                && let Some(after) = src[before..].strip_prefix('<')
                && !after
                    .chars()
                    .next()
                    .is_some_and(|c| name_start(c) || matches!(c, '/' | '!' | '?'))
            {
                self.tick(1)
                    .and_then(|()| self.repair(LESS_THAN, "kept as text"))
                    .and_then(|()| self.text("<"))
                    .map_err(|s| (s, before))?;
                if self.stopped {
                    return Ok(src.len());
                }
                start = before + 1;
                start = self.lead_at(src, start)?;
                reader = self::reader(&src[start..], self.recover);
                continue;
            }
            let ev = match reader.read_event() {
                Ok(ev) => ev,
                Err(e) => {
                    let at = start + reader.error_position() as usize;
                    return Err((Stop::Bad(e.to_string()), at));
                }
            };
            if matches!(ev, Event::Eof) {
                return Ok(src.len());
            }
            match self.event(ev) {
                Ok(()) => {}
                Err(Stop::LessThan) => {
                    // The `<` is text, and reading starts again after it.
                    self.repair(LESS_THAN, "kept as text")
                        .and_then(|()| self.text("<"))
                        .map_err(|s| (s, before))?;
                    start = before + 1;
                    start = self.lead_at(src, start)?;
                    reader = self::reader(&src[start..], self.recover);
                    continue;
                }
                Err(stop) => return Err((stop, before)),
            }
            if self.stopped {
                return Ok(src.len());
            }
            if !self.ready.is_empty() {
                return Ok(start + reader.buffer_position() as usize);
            }
        }
    }

    /// The checks at the end of the document. In recover mode, elements
    /// still open are closed.
    fn finish(&mut self, src: &str) -> Step {
        self.at = src.len();
        self.flush();
        if !self.open.is_empty() {
            let names: Vec<&str> = self.open.iter().map(|o| o.qname.as_str()).collect();
            let why = format!("the document ends inside <{}>", names.join("><"));
            self.repair(why, "closed")?;
            self.floor = 0;
            while !self.open.is_empty() {
                self.close()?;
            }
        }
        if is_blank(src) {
            if self.recover {
                return Ok(());
            }
            return Err("Document is empty".into());
        }
        if !self.root_seen {
            self.repair("the document has no root element", "it's read as empty")?;
        }
        Ok(())
    }
}

fn reader(src: &str, recover: bool) -> Reader<&[u8]> {
    let mut reader = Reader::from_str(src);
    let config = reader.config_mut();
    // Ends are checked here, against elements opened in other readers too
    // (an entity's, or the one before a node was handed out).
    config.check_end_names = false;
    config.allow_unmatched_ends = true;
    config.check_comments = true;
    config.allow_dangling_amp = recover;
    config.expand_empty_elements = false;
    config.trim_text(false);
    reader
}

/// The line and column (both from 1, the column in characters) of byte
/// `at` of `src`.
fn position(src: &str, at: usize) -> (usize, usize) {
    let mut at = at.min(src.len());
    while !src.is_char_boundary(at) {
        at -= 1;
    }
    let head = &src[..at];
    let line = head.bytes().filter(|&b| b == b'\n').count() + 1;
    let column = head.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}

fn error_at(src: &str, at: usize, reason: String) -> XmlError {
    let (line, column) = position(src, at);
    XmlError {
        line,
        column,
        reason,
    }
}

/// Reads all of `src`, then the checks at the end.
fn run(b: &mut Builder<'_>, src: &str, budget: Duration) -> Result<(), ParseXmlError> {
    let fail = |stop: Stop, at: usize| match stop {
        Stop::Bad(why) => ParseXmlError::Malformed(error_at(src, at, why)),
        Stop::LessThan => ParseXmlError::Malformed(error_at(src, at, LESS_THAN.into())),
        Stop::Late => ParseXmlError::Timeout(ParseTimeout { budget, parsed: at }),
    };
    b.pump(src, 0).map_err(|(stop, at)| fail(stop, at))?;
    b.finish(src).map_err(|stop| fail(stop, src.len()))
}

impl Document {
    /// Parses well-formed XML (see the module's documentation for what
    /// that means here, and what is refused).
    pub fn parse_xml(text: &str) -> Result<Document, XmlError> {
        Document::parse_xml_within(text, Duration::MAX).map_err(|e| match e {
            ParseXmlError::Malformed(e) => e,
            ParseXmlError::Timeout(_) => unreachable!("an unlimited budget cannot run out"),
        })
    }

    /// Like [`Document::parse_xml`], but gives up once `budget` has passed.
    pub fn parse_xml_within(text: &str, budget: Duration) -> Result<Document, ParseXmlError> {
        let options = XmlOptions {
            recover: false,
            budget,
        };
        Document::parse_xml_with(text, options).map(|(doc, _)| doc)
    }

    /// Parses XML as `options` say, and says what recover mode repaired.
    pub fn parse_xml_with(
        text: &str,
        options: XmlOptions,
    ) -> Result<(Document, Repairs), ParseXmlError> {
        let src = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut b = Builder::new(Want::Whole, src.len(), options);
        run(&mut b, src, options.budget)?;
        let repairs = b.repairs(src);
        Ok((
            b.out.take().expect("a whole document is always built").doc,
            repairs,
        ))
    }
}

/// Reads `text` without building a tree, telling `visitor` what's in it:
/// for reading a large document in one pass and little memory. The checks
/// and limits are a parse's.
pub fn walk_xml(
    text: &str,
    options: XmlOptions,
    visitor: &mut dyn XmlVisitor,
) -> Result<Repairs, ParseXmlError> {
    let src = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut b = Builder::new(Want::Walk, src.len(), options);
    b.visitor = Some(visitor);
    run(&mut b, src, options.budget)?;
    Ok(b.repairs(src))
}

/// The elements of an XML document with one name, each read on its own
/// as a document of its own, as the document is read: the whole document
/// is never a tree in memory, which suits a large feed. Scrapy's
/// `iternodes`.
///
/// Elements inside a matching one are part of it, not matches of their
/// own. The document is still checked from start to end: a node is only
/// given out once it's well-formed, and an error ends the iteration.
pub struct XmlNodes {
    src: String,
    at: usize,
    builder: Builder<'static>,
    budget: Duration,
    done: bool,
}

// SAFETY: the raw pointers inside are to nodes of the document being
// built, and the Rcs to entity values the builder has read; the builder
// owns both, and none of them is shared with anything outside it (no
// node is reachable, and no Rc cloned out, until a document is handed
// out, which takes no Rc with it). Moving the lot to another thread moves
// every pointer and every Rc together.
unsafe impl Send for XmlNodes {}

impl XmlNodes {
    /// The elements named `tag` in `text`: `local`, or `prefix:local`.
    /// The prefix is the one bound in `namespaces`, or failing that the
    /// one the document itself declares where the element is. A name
    /// without a prefix means one in no namespace, as in XPath.
    /// `options.budget` is for each node.
    pub fn new(
        text: String,
        tag: &str,
        namespaces: &[(String, String)],
        options: XmlOptions,
    ) -> Result<XmlNodes, String> {
        let (ns, prefix, local) = match tag.split_once(':') {
            Some((prefix, local)) => {
                let uri = namespaces
                    .iter()
                    .rev()
                    .find(|(p, _)| p == prefix)
                    .map(|(_, u)| u.clone())
                    .or_else(|| (prefix == "xml").then(|| XML_NS.to_string()));
                (uri, prefix.to_string(), local)
            }
            None => (Some(String::new()), String::new(), tag),
        };
        if !is_ncname(local) || (!prefix.is_empty() && !is_ncname(&prefix)) {
            return Err(format!("{tag:?} isn't a valid element name"));
        }
        let src = match text.strip_prefix('\u{feff}') {
            Some(rest) => rest.to_string(),
            None => text,
        };
        let want = Want::Nodes {
            ns,
            prefix,
            local: local.to_string(),
        };
        let size = src.len();
        Ok(XmlNodes {
            src,
            at: 0,
            builder: Builder::new(want, size, options),
            budget: options.budget,
            done: false,
        })
    }

    /// The next node, with the prefix of its name and the namespace the
    /// document bound it to when the document is where that came from.
    pub fn next_node(&mut self) -> Option<Result<Ready, XmlError>> {
        if let Some(ready) = self.builder.ready.pop_front() {
            return Some(Ok(ready));
        }
        if self.done {
            return None;
        }
        self.builder.deadline = Instant::now().checked_add(self.budget);
        let fail = |src: &str, stop: Stop, at: usize| {
            let why = match stop {
                Stop::Bad(why) => why,
                Stop::LessThan => LESS_THAN.into(),
                Stop::Late => format!("reading one node took longer than {:?}", self.budget),
            };
            error_at(src, at, why)
        };
        match self.builder.pump(&self.src, self.at) {
            Ok(at) => {
                self.at = at;
                if let Some(ready) = self.builder.ready.pop_front() {
                    return Some(Ok(ready));
                }
                self.done = true;
                if let Err(stop) = self.builder.finish(&self.src) {
                    return Some(Err(fail(&self.src, stop, self.src.len())));
                }
                self.builder.ready.pop_front().map(Ok)
            }
            Err((stop, at)) => {
                self.done = true;
                Some(Err(fail(&self.src, stop, at)))
            }
        }
    }

    /// What recover mode has repaired so far.
    pub fn repairs(&self) -> Repairs {
        self.builder.repairs(&self.src)
    }
}

impl Iterator for XmlNodes {
    type Item = Result<Document, XmlError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_node().map(|r| r.map(|(doc, _)| doc))
    }
}

// --- writing XML -------------------------------------------------------------

fn escape_text(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => out.push_str("&#13;"),
            c => out.push(c),
        }
    }
}

fn escape_attr(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            '\t' => out.push_str("&#9;"),
            c => out.push(c),
        }
    }
}

/// `node`'s start tag, without its `>`. The outermost element written
/// (`top`) also declares the namespaces it inherits, so the XML written
/// stands on its own, as lxml writes an element taken from a document:
/// its own declarations, then those of the namespaces its name and
/// attributes are in, then the rest in scope, nearest element first, each
/// prefix once.
fn start_tag(node: Node<'_>, names: &XmlNames, top: bool, out: &mut String) {
    out.push('<');
    out.push_str(node.qualified_name());
    let mut decls: Vec<(&str, u32)> = names
        .decls_of(node.id as usize)
        .iter()
        .map(|(p, u)| (&**p, *u))
        .collect();
    if top {
        let ns = node.ns_id();
        if ns != 0 {
            decls.push((prefix_part(node.qualified_name()), ns));
        }
        for (name, ns, _) in node.xml_attrs() {
            let prefix = prefix_part(name);
            if ns != 0 && prefix != "xml" {
                decls.push((prefix, ns));
            }
        }
        let mut up = node.parent();
        while let Some(a) = up {
            decls.extend(
                names
                    .decls_of(a.id as usize)
                    .iter()
                    .map(|(p, u)| (&**p, *u)),
            );
            up = a.parent();
        }
        decls.extend(names.outer.iter().map(|(p, u)| (&**p, *u)));
    }
    let mut written: HashSet<&str> = HashSet::new();
    for &(prefix, uri) in &decls {
        if !written.insert(prefix) {
            continue;
        }
        if prefix.is_empty() {
            out.push_str(" xmlns=\"");
        } else {
            out.push_str(" xmlns:");
            out.push_str(prefix);
            out.push_str("=\"");
        }
        escape_attr(names.uri(uri), out);
        out.push('"');
    }
    for (name, value) in node.attrs() {
        out.push(' ');
        out.push_str(name);
        out.push_str("=\"");
        escape_attr(value, out);
        out.push('"');
    }
}

fn leaf(node: Node<'_>, out: &mut String) {
    match node.kind() {
        NodeKind::Text => escape_text(node.data().unwrap_or(""), out),
        NodeKind::Comment => {
            out.push_str("<!--");
            out.push_str(node.data().unwrap_or(""));
            out.push_str("-->");
        }
        NodeKind::ProcessingInstruction => {
            out.push_str("<?");
            out.push_str(node.pi_target().unwrap_or(""));
            let data = node.data().unwrap_or("");
            if !data.is_empty() {
                out.push(' ');
                out.push_str(data);
            }
            out.push_str("?>");
        }
        _ => {}
    }
}

/// `node` as XML. A document is its top-level nodes, one to a line.
pub(crate) fn serialize(node: Node<'_>, names: &XmlNames) -> String {
    let mut out = String::new();
    if node.kind() == NodeKind::Document {
        for (i, child) in node.children().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            write(child, names, &mut out);
        }
        return out;
    }
    write(node, names, &mut out);
    out
}

/// Without recursion, so the deepest document allowed can't overflow the
/// stack.
fn write(node: Node<'_>, names: &XmlNames, out: &mut String) {
    enum Step<'a> {
        Enter(Node<'a>),
        Leave(Node<'a>),
    }
    let mut stack = vec![Step::Enter(node)];
    while let Some(step) = stack.pop() {
        match step {
            Step::Enter(n) if n.kind() == NodeKind::Element => {
                start_tag(n, names, n == node, out);
                if n.first_child().is_none() {
                    out.push_str("/>");
                    continue;
                }
                out.push('>');
                stack.push(Step::Leave(n));
                let children: Vec<Node<'_>> = n.children().collect();
                stack.extend(children.into_iter().rev().map(Step::Enter));
            }
            Step::Enter(n) => leaf(n, out),
            Step::Leave(n) => {
                out.push_str("</");
                out.push_str(n.qualified_name());
                out.push('>');
            }
        }
    }
}

/// `node` as indented XML: one tag, piece of text, comment or processing
/// instruction to a line, one space of indent a level. Text is trimmed and
/// whitespace-only text left out.
pub(crate) fn pretty(node: Node<'_>, names: &XmlNames) -> String {
    let mut out = String::new();
    let line = |out: &mut String, depth: usize, text: &str| {
        out.extend(std::iter::repeat_n(' ', depth));
        out.push_str(text);
        out.push('\n');
    };
    enum Step<'a> {
        Enter(Node<'a>, usize),
        Leave(Node<'a>, usize),
    }
    let mut stack = vec![];
    if node.kind() == NodeKind::Document {
        let children: Vec<Node<'_>> = node.children().collect();
        stack.extend(children.into_iter().rev().map(|c| Step::Enter(c, 0)));
    } else {
        stack.push(Step::Enter(node, 0));
    }
    let top = |n: Node<'_>| n == node || n.parent().is_some_and(|p| p.kind() == NodeKind::Document);
    while let Some(step) = stack.pop() {
        match step {
            Step::Enter(n, depth) => match n.kind() {
                NodeKind::Element => {
                    let mut tag = String::new();
                    start_tag(n, names, top(n), &mut tag);
                    if n.first_child().is_none() {
                        tag.push_str("/>");
                        line(&mut out, depth, &tag);
                        continue;
                    }
                    tag.push('>');
                    line(&mut out, depth, &tag);
                    stack.push(Step::Leave(n, depth));
                    let children: Vec<Node<'_>> = n.children().collect();
                    stack.extend(
                        children
                            .into_iter()
                            .rev()
                            .map(|c| Step::Enter(c, depth + 1)),
                    );
                }
                NodeKind::Text => {
                    let text = n.data().unwrap_or("").trim();
                    if !text.is_empty() {
                        let mut escaped = String::new();
                        escape_text(text, &mut escaped);
                        line(&mut out, depth, &escaped);
                    }
                }
                _ => {
                    let mut s = String::new();
                    leaf(n, &mut s);
                    line(&mut out, depth, &s);
                }
            },
            Step::Leave(n, depth) => line(&mut out, depth, &format!("</{}>", n.qualified_name())),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_xml_grammar() {
        for good in ["a", "_x", "a-b.c1", "é", "日本"] {
            assert!(is_ncname(good), "{good}");
        }
        for bad in ["", "1a", "-a", "a b", "a:b", ".a"] {
            assert!(!is_ncname(bad), "{bad}");
        }
        assert!(is_qname("atom:entry"));
        assert!(!is_qname("a:b:c"));
        assert!(!is_qname(":a"));
        assert!(!is_qname("a:"));
    }

    #[test]
    fn character_references_are_checked() {
        assert_eq!(char_ref("#65", false), Ok('A'));
        assert_eq!(char_ref("#x263A", false), Ok('☺'));
        assert!(char_ref("#0", false).is_err());
        assert!(char_ref("#1", false).is_err());
        assert_eq!(char_ref("#1", true), Ok('\u{1}'));
        assert!(char_ref("#xD800", false).is_err());
        assert!(char_ref("#x", false).is_err());
        assert!(char_ref("#12a", false).is_err());
    }

    #[test]
    fn positions_count_lines_and_characters() {
        let src = "ab\ncdé\nfg";
        assert_eq!(position(src, 0), (1, 1));
        assert_eq!(position(src, 3), (2, 1));
        assert_eq!(position(src, 7), (2, 4));
        assert_eq!(position(src, src.len()), (3, 3));
    }

    #[test]
    fn the_internal_subset_declares_entities() {
        let e = declared_entities(
            r#"x SYSTEM "x.dtd" [
                <!-- a comment with an <!ENTITY inside -->
                <!ELEMENT x (#PCDATA)>
                <!ATTLIST x a CDATA "with > inside">
                <!ENTITY a "one &#38;#38; two">
                <!ENTITY a "ignored: the first counts">
                <!ENTITY b SYSTEM "file:///etc/passwd">
                <!ENTITY % p "parameter">
                <?pi with ]> inside?>
            ]"#,
            false,
        )
        .unwrap();
        assert!(matches!(e.get("a"), Some(Entity::Internal(v)) if &**v == "one &#38; two"));
        assert!(matches!(e.get("b"), Some(Entity::External)));
        assert!(!e.contains_key("p"));
    }

    #[test]
    fn declarations_after_a_parameter_entity_are_not_read() {
        let e = declared_entities(r#"x [ %ext; <!ENTITY a "x"> ]"#, false).unwrap();
        assert!(e.is_empty());
    }
}
