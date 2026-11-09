//! CSS selectors, as Selectors Level 4 defines them and browsers match
//! them in a static page: parsed once, matched right to left.
//!
//! What browsers do on a page nobody is looking at decides the rest:
//! `:hover`, `:focus` and the like parse but never match, `:scope` is the
//! element a query runs from, and ids and classes compare exactly except
//! in a quirks-mode document.

use std::cell::Cell;

use crate::dom::{AttrNs, Data, Document, NodeId, Ns, Quirks};
use crate::pattern::Pattern;
use crate::tags::Tag;

/// A selector that couldn't be parsed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorError {
    pub reason: String,
}

impl std::fmt::Display for SelectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for SelectorError {}

fn error<T>(reason: impl Into<String>) -> Result<T, SelectorError> {
    Err(SelectorError {
        reason: reason.into(),
    })
}

/// A comma-separated list of selectors; an element matches if any does.
#[derive(Debug, Clone)]
pub struct SelectorList(Vec<Complex>);

/// Compound selectors joined by combinators, kept right to left: `parts[0]`
/// is the subject, and each part's combinator relates it to the next one.
#[derive(Debug, Clone)]
struct Complex {
    parts: Vec<(Compound, Combinator)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Combinator {
    /// The leftmost part has none.
    None,
    Descendant,
    Child,
    NextSibling,
    SubsequentSibling,
}

#[derive(Debug, Clone, Default)]
struct Compound(Vec<Simple>);

#[derive(Debug, Clone)]
enum Simple {
    /// A type selector, lowercased for HTML elements, as written for the
    /// rest.
    Type {
        lower: String,
        exact: String,
        /// The lowercased name as a known tag, so HTML elements compare
        /// by number.
        tag: Tag,
    },
    Id(String),
    Class(String),
    Attr(AttrSelector),
    Pseudo(Pseudo),
    /// The element a `:has()` argument is relative to.
    Anchor,
}

#[derive(Debug, Clone)]
struct AttrSelector {
    lower: String,
    exact: String,
    /// `[*|name]`: in any namespace, or none.
    any_ns: bool,
    op: AttrOp,
    value: String,
    case: Case,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttrOp {
    Exists,
    Equals,
    Includes,
    DashMatch,
    Prefix,
    Suffix,
    Substring,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Case {
    /// As the document language says: HTML lists some attributes whose
    /// values compare without case.
    Default,
    Insensitive,
    Sensitive,
}

#[derive(Debug, Clone)]
enum Pseudo {
    Not(SelectorList),
    Is(SelectorList),
    Has(SelectorList),
    Nth {
        a: i32,
        b: i32,
        last: bool,
        of_type: bool,
        of: Option<SelectorList>,
    },
    Only {
        of_type: bool,
    },
    Empty,
    Root,
    Scope,
    AnyLink,
    Checked,
    Disabled,
    Enabled,
    Required,
    Optional,
    ReadWrite,
    ReadOnly,
    PlaceholderShown,
    /// `:lang()`'s language ranges, lowercased.
    Lang(Vec<String>),
    /// `:dir(rtl)` when true, `:dir(ltr)` when false.
    Dir(bool),
    Default,
    Indeterminate,
    InRange,
    OutOfRange,
    Valid,
    Invalid,
    /// A state no static page is in: `:hover`, `:focus`, `:visited`...
    Never,
}

// ---- parsing ---------------------------------------------------------------

struct Parser<'a> {
    s: &'a str,
    pos: usize,
    /// How deep in `:not()`, `:is()` and the like it is; parsing and
    /// matching both recurse, so a limit keeps a pathological selector
    /// from overflowing the stack.
    depth: u32,
    /// Inside `:has()`, where Selectors 4 doesn't allow another `:has()`.
    in_has: bool,
}

/// Nesting deeper than any real selector, and shallow enough for a
/// thread's stack.
const MAX_DEPTH: u32 = 32;

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || !c.is_ascii()
}

fn is_name(c: char) -> bool {
    is_name_start(c) || c.is_ascii_digit() || c == '-'
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<char> {
        self.s[self.pos..].chars().next()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.s[self.pos..].chars().nth(n)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += c.len_utf8();
            true
        } else {
            false
        }
    }

    /// Whitespace and comments; true if there was whitespace. A comment
    /// alone separates nothing, so `a/**/.b` is the compound `a.b`.
    fn skip_space(&mut self) -> bool {
        let mut space = false;
        loop {
            if matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r' | '\x0C')) {
                self.pos += 1;
                space = true;
            } else if !self.skip_comment() {
                return space;
            }
        }
    }

    /// One comment, if one starts here. An unclosed one runs to the end.
    fn skip_comment(&mut self) -> bool {
        if !self.s[self.pos..].starts_with("/*") {
            return false;
        }
        match self.s[self.pos + 2..].find("*/") {
            Some(end) => self.pos += end + 4,
            None => self.pos = self.s.len(),
        }
        true
    }

    fn at_end(&self) -> bool {
        self.pos >= self.s.len()
    }

    /// A backslash escape, the backslash already read.
    fn escape(&mut self) -> Result<char, SelectorError> {
        let Some(c) = self.peek() else {
            return Ok('\u{FFFD}');
        };
        if c == '\n' {
            return error("a backslash before a newline");
        }
        if c.is_ascii_hexdigit() {
            let start = self.pos;
            while self.pos - start < 6 && self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                self.pos += 1;
            }
            let code = u32::from_str_radix(&self.s[start..self.pos], 16).unwrap();
            if matches!(self.peek(), Some(' ' | '\t' | '\n' | '\x0C')) {
                self.pos += 1;
            }
            return Ok(match code {
                0 | 0xD800..=0xDFFF | 0x11_0000.. => '\u{FFFD}',
                _ => char::from_u32(code).unwrap_or('\u{FFFD}'),
            });
        }
        self.pos += c.len_utf8();
        Ok(c)
    }

    fn starts_name(&self) -> bool {
        match self.peek() {
            Some('\\') => self.peek_at(1).is_some_and(|c| c != '\n'),
            Some('-') => match self.peek_at(1) {
                Some(c) if is_name_start(c) || c == '-' => true,
                Some('\\') => self.peek_at(2).is_some_and(|c| c != '\n'),
                _ => false,
            },
            Some(c) => is_name_start(c),
            None => false,
        }
    }

    fn name(&mut self) -> Result<String, SelectorError> {
        let mut out = String::new();
        loop {
            match self.peek() {
                Some('\\') if self.peek_at(1).is_some_and(|c| c != '\n') => {
                    self.pos += 1;
                    out.push(self.escape()?);
                }
                Some(c) if is_name(c) => {
                    self.pos += c.len_utf8();
                    out.push(c);
                }
                _ => return Ok(out),
            }
        }
    }

    fn ident(&mut self) -> Result<String, SelectorError> {
        if !self.starts_name() {
            return error(format!("expected a name at {:?}", &self.s[self.pos..]));
        }
        self.name()
    }

    fn string(&mut self, quote: char) -> Result<String, SelectorError> {
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Ok(out),
                Some(c) if c == quote => return Ok(out),
                Some('\n') => return error("a newline in a string"),
                Some('\\') => match self.peek() {
                    Some('\n') => self.pos += 1,
                    None => {}
                    Some(_) => out.push(self.escape()?),
                },
                Some(c) => out.push(c),
            }
        }
    }

    fn selector_list(&mut self, forgiving: bool) -> Result<SelectorList, SelectorError> {
        let mut out = Vec::new();
        loop {
            self.skip_space();
            let start = self.pos;
            match self.complex(false) {
                Ok(c) => out.push(c),
                Err(e) if !forgiving => return Err(e),
                Err(_) => {
                    // Forgiving: skip to the next comma at this level.
                    self.pos = start;
                    self.skip_to_comma();
                }
            }
            self.skip_space();
            if !self.eat(',') {
                break;
            }
        }
        if out.is_empty() && !forgiving {
            return error("an empty selector");
        }
        Ok(SelectorList(out))
    }

    fn skip_to_comma(&mut self) {
        let mut depth = 0;
        while let Some(c) = self.peek() {
            match c {
                '(' | '[' => depth += 1,
                ')' | ']' if depth == 0 => return,
                ')' | ']' => depth -= 1,
                ',' if depth == 0 => return,
                '"' | '\'' => {
                    self.pos += 1;
                    let _ = self.string(c);
                    continue;
                }
                '\\' => self.pos += 1,
                _ => {}
            }
            self.bump();
        }
    }

    /// A complex selector; `relative` for `:has()`, where it may start with
    /// a combinator and is relative to the anchor.
    fn complex(&mut self, relative: bool) -> Result<Complex, SelectorError> {
        let mut parts: Vec<(Compound, Combinator)> = Vec::new();
        let mut pending = Combinator::None;
        if relative {
            self.skip_space();
            pending = self.combinator().unwrap_or(Combinator::Descendant);
            parts.push((Compound(vec![Simple::Anchor]), Combinator::None));
            self.skip_space();
        }
        loop {
            let compound = self.compound()?;
            if let Some(last) = parts.last_mut() {
                last.1 = pending;
            }
            parts.push((compound, Combinator::None));
            let space = self.skip_space();
            if self.at_end() || matches!(self.peek(), Some(',' | ')')) {
                break;
            }
            pending = match self.combinator() {
                Some(c) => {
                    self.skip_space();
                    c
                }
                None if space => Combinator::Descendant,
                None => return error(format!("unexpected {:?}", &self.s[self.pos..])),
            };
        }
        // Stored left to right while reading; matched right to left.
        let mut combinators: Vec<Combinator> = parts.iter().map(|p| p.1).collect();
        combinators.pop();
        parts.reverse();
        combinators.reverse();
        for (i, c) in combinators.into_iter().enumerate() {
            parts[i].1 = c;
        }
        let last = parts.len() - 1;
        parts[last].1 = Combinator::None;
        Ok(Complex { parts })
    }

    fn combinator(&mut self) -> Option<Combinator> {
        match self.peek()? {
            '>' => {
                self.pos += 1;
                Some(Combinator::Child)
            }
            '+' => {
                self.pos += 1;
                Some(Combinator::NextSibling)
            }
            '~' => {
                self.pos += 1;
                Some(Combinator::SubsequentSibling)
            }
            _ => None,
        }
    }

    fn compound(&mut self) -> Result<Compound, SelectorError> {
        let mut simple = Vec::new();
        let mut universal = false;
        // A type selector or universal, with an optional namespace prefix.
        if self.peek() == Some('*') || self.starts_name() || self.peek() == Some('|') {
            let star = self.eat('*');
            universal = star;
            let name = if star || self.peek() == Some('|') {
                String::new()
            } else {
                self.name()?
            };
            if self.peek() == Some('|') && self.peek_at(1) != Some('=') {
                self.pos += 1;
                // Only `*|` (any namespace) and `|` (none) can be used
                // without @namespace, which a query can't declare.
                if !(star || name.is_empty()) {
                    return error(format!("namespace prefix {name:?} isn't declared"));
                }
                // `|x` asks for no namespace, which no element in an HTML
                // document has.
                let none = !star;
                if !self.eat('*') {
                    let local = self.ident()?;
                    simple.push(type_selector(local));
                }
                if none {
                    simple.push(Simple::Pseudo(Pseudo::Never));
                }
            } else if !star {
                simple.push(type_selector(name));
            }
        }
        loop {
            while self.skip_comment() {}
            match self.peek() {
                Some('#') => {
                    self.pos += 1;
                    // An ID is an identifier, so `#1a` isn't one.
                    if !self.starts_name() {
                        return error(match self.peek() {
                            Some(c) if c.is_ascii_digit() => "an ID can't start with a digit",
                            Some('-') if self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) => {
                                "an ID can't start with a hyphen and a digit"
                            }
                            _ => "a # with no ID after it",
                        });
                    }
                    simple.push(Simple::Id(self.name()?));
                }
                Some('.') => {
                    self.pos += 1;
                    simple.push(Simple::Class(self.ident()?));
                }
                Some('[') => {
                    self.pos += 1;
                    simple.push(Simple::Attr(self.attr()?));
                }
                Some(':') if self.peek_at(1) == Some(':') => {
                    return error("pseudo-elements can't be selected");
                }
                Some(':') => {
                    self.pos += 1;
                    simple.push(Simple::Pseudo(self.pseudo()?));
                }
                _ => break,
            }
        }
        if simple.is_empty() && !universal {
            return error(match self.peek() {
                Some(c) => format!("unexpected {c:?}"),
                None => "a selector ends early".to_string(),
            });
        }
        Ok(Compound(simple))
    }

    fn attr(&mut self) -> Result<AttrSelector, SelectorError> {
        self.skip_space();
        let mut any_ns = false;
        if self.peek() == Some('*') && self.peek_at(1) == Some('|') {
            self.pos += 2;
            any_ns = true;
        } else if self.peek() == Some('|') {
            self.pos += 1;
        }
        let name = self.ident()?;
        if self.peek() == Some('|') && self.peek_at(1) != Some('=') {
            return error(format!("namespace prefix {name:?} isn't declared"));
        }
        self.skip_space();
        let op = match self.peek() {
            Some(']') => {
                self.pos += 1;
                return Ok(AttrSelector {
                    lower: name.to_ascii_lowercase(),
                    exact: name,
                    any_ns,
                    op: AttrOp::Exists,
                    value: String::new(),
                    case: Case::Default,
                });
            }
            Some('=') => {
                self.pos += 1;
                AttrOp::Equals
            }
            Some(c @ ('~' | '|' | '^' | '$' | '*')) if self.peek_at(1) == Some('=') => {
                self.pos += 2;
                match c {
                    '~' => AttrOp::Includes,
                    '|' => AttrOp::DashMatch,
                    '^' => AttrOp::Prefix,
                    '$' => AttrOp::Suffix,
                    _ => AttrOp::Substring,
                }
            }
            _ => return error("a bad attribute selector"),
        };
        self.skip_space();
        let value = match self.peek() {
            Some(q @ ('"' | '\'')) => {
                self.pos += 1;
                self.string(q)?
            }
            _ => self.ident()?,
        };
        self.skip_space();
        let mut case = Case::Default;
        if let Some(c) = self.peek()
            && c.is_ascii_alphabetic()
        {
            let flag = self.ident()?;
            case = match flag.to_ascii_lowercase().as_str() {
                "i" => Case::Insensitive,
                "s" => Case::Sensitive,
                _ => return error(format!("unknown attribute flag {flag:?}")),
            };
            self.skip_space();
        }
        if !self.eat(']') {
            return error("an attribute selector without its ]");
        }
        Ok(AttrSelector {
            lower: name.to_ascii_lowercase(),
            exact: name,
            any_ns,
            op,
            value,
            case,
        })
    }

    fn pseudo(&mut self) -> Result<Pseudo, SelectorError> {
        let name = self.ident()?.to_ascii_lowercase();
        if self.eat('(') {
            let p = self.pseudo_function(&name)?;
            self.skip_space();
            if !self.eat(')') {
                return error(format!(":{name}() without its )"));
            }
            return Ok(p);
        }
        Ok(match name.as_str() {
            "first-child" => nth(0, 1, false, false),
            "last-child" => nth(0, 1, true, false),
            "only-child" => Pseudo::Only { of_type: false },
            "first-of-type" => nth(0, 1, false, true),
            "last-of-type" => nth(0, 1, true, true),
            "only-of-type" => Pseudo::Only { of_type: true },
            "empty" => Pseudo::Empty,
            "root" => Pseudo::Root,
            "scope" => Pseudo::Scope,
            "link" | "any-link" => Pseudo::AnyLink,
            "checked" => Pseudo::Checked,
            "disabled" => Pseudo::Disabled,
            "enabled" => Pseudo::Enabled,
            "required" => Pseudo::Required,
            "optional" => Pseudo::Optional,
            "read-write" => Pseudo::ReadWrite,
            "read-only" => Pseudo::ReadOnly,
            "placeholder-shown" => Pseudo::PlaceholderShown,
            "default" => Pseudo::Default,
            "indeterminate" => Pseudo::Indeterminate,
            "in-range" => Pseudo::InRange,
            "out-of-range" => Pseudo::OutOfRange,
            "valid" => Pseudo::Valid,
            "invalid" => Pseudo::Invalid,
            "hover" | "active" | "focus" | "focus-visible" | "focus-within" | "visited"
            | "target" | "target-within" | "local-link" | "user-invalid" | "user-valid" => {
                Pseudo::Never
            }
            _ => return error(format!(":{name} isn't supported")),
        })
    }

    fn pseudo_function(&mut self, name: &str) -> Result<Pseudo, SelectorError> {
        if self.depth >= MAX_DEPTH {
            return error(format!("selectors nest more than {MAX_DEPTH} deep"));
        }
        self.depth += 1;
        let result = self.pseudo_function_body(name);
        self.depth -= 1;
        result
    }

    fn pseudo_function_body(&mut self, name: &str) -> Result<Pseudo, SelectorError> {
        Ok(match name {
            "not" => Pseudo::Not(self.selector_list(false)?),
            "is" | "where" | "matches" => Pseudo::Is(self.selector_list(true)?),
            "has" => {
                if self.in_has {
                    return error(":has() can't go inside :has()");
                }
                self.in_has = true;
                let list = self.has_list();
                self.in_has = false;
                Pseudo::Has(list?)
            }
            "nth-child" | "nth-last-child" | "nth-of-type" | "nth-last-of-type" => {
                let of_type = name.ends_with("of-type");
                let last = name.starts_with("nth-last");
                let close = self.close_paren();
                let arg = &self.s[self.pos..close];
                let (anb, rest) = match split_of(arg) {
                    Some((anb, rest)) if !of_type => (anb, Some(rest)),
                    Some(_) => return error("`of` only goes in :nth-child and :nth-last-child"),
                    None => (arg, None),
                };
                let (a, b) = an_plus_b(anb)?;
                let of = rest
                    .map(|rest| SelectorList::parse_at(rest, self.depth, self.in_has))
                    .transpose()?;
                self.pos = close;
                Pseudo::Nth {
                    a,
                    b,
                    last,
                    of_type,
                    of,
                }
            }
            "lang" => {
                let mut ranges = Vec::new();
                loop {
                    self.skip_space();
                    // A wildcard such as *-CH can't be a name, so it's quoted.
                    let range = match self.peek() {
                        Some(q @ ('"' | '\'')) => {
                            self.pos += 1;
                            self.string(q)?
                        }
                        _ => self.ident()?,
                    };
                    ranges.push(range.to_ascii_lowercase());
                    self.skip_space();
                    if !self.eat(',') {
                        break;
                    }
                }
                Pseudo::Lang(ranges)
            }
            "dir" => {
                self.skip_space();
                let dir = self.ident()?.to_ascii_lowercase();
                self.skip_space();
                match dir.as_str() {
                    "ltr" => Pseudo::Dir(false),
                    "rtl" => Pseudo::Dir(true),
                    _ => return error(format!(":dir() takes ltr or rtl, not {dir}")),
                }
            }
            _ => return error(format!(":{name}() isn't supported")),
        })
    }

    fn has_list(&mut self) -> Result<SelectorList, SelectorError> {
        let mut out = Vec::new();
        loop {
            out.push(self.complex(true)?);
            self.skip_space();
            if !self.eat(',') {
                return Ok(SelectorList(out));
            }
        }
    }

    /// Where the `)` closing the current parenthesis is, past nested
    /// ones and strings.
    fn close_paren(&self) -> usize {
        let b = self.s.as_bytes();
        let mut depth = 0;
        let mut i = self.pos;
        while i < b.len() {
            match b[i] {
                b'\\' => i += 1,
                q @ (b'"' | b'\'') => {
                    i += 1;
                    while i < b.len() && b[i] != q {
                        if b[i] == b'\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                }
                b'(' => depth += 1,
                b')' if depth == 0 => return i,
                b')' => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        b.len()
    }
}

/// Whether the first strong character of `text` is right to left (Some(true))
/// or left to right (Some(false)); None when it has none.
// shortcut: letters in the right-to-left scripts' blocks count as strong
// right to left and any other letter as strong left to right, rather than
// reading Unicode's Bidi_Class; use the real table if a script needs it.
fn first_strong_rtl(text: &str) -> Option<bool> {
    text.chars()
        .filter(|c| c.is_alphabetic())
        .map(|c| {
            matches!(c as u32,
            0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF
            | 0x10800..=0x10FFF | 0x1E800..=0x1EFFF)
        })
        .next()
}

/// Whether a language `tag` matches a `range`, both lowercase, by RFC 4647's
/// extended filtering as Selectors 4 uses it: `de` matches `de-CH`, `de-CH`
/// matches `de-Latn-CH`, and `*` stands for any subtag. Subtags the range
/// leaves out match anything, but never past a one-letter one such as `x`.
fn lang_matches(range: &str, tag: &str) -> bool {
    if range.is_empty() || tag.is_empty() {
        return range == tag;
    }
    let r: Vec<&str> = range.split('-').collect();
    let t: Vec<&str> = tag.split('-').collect();
    if r[0] != "*" && r[0] != t[0] {
        return false;
    }
    let (mut i, mut j) = (1, 1);
    while i < r.len() {
        if r[i] == "*" {
            i += 1;
        } else if j >= t.len() {
            return false;
        } else if r[i] == t[j] {
            i += 1;
            j += 1;
        } else if t[j].len() == 1 {
            return false;
        } else {
            j += 1;
        }
    }
    true
}

/// An `<input>`'s type as the HTML Standard reads it: a known type in any
/// case, anything else (or none) text.
fn input_type_state(typ: Option<&str>) -> &'static str {
    const TYPES: [&str; 22] = [
        "hidden",
        "text",
        "search",
        "tel",
        "url",
        "email",
        "password",
        "date",
        "month",
        "week",
        "time",
        "datetime-local",
        "number",
        "range",
        "color",
        "checkbox",
        "radio",
        "file",
        "submit",
        "image",
        "reset",
        "button",
    ];
    typ.and_then(|t| TYPES.iter().find(|k| k.eq_ignore_ascii_case(t)))
        .copied()
        .unwrap_or("text")
}

fn readonly_applies(typ: &str) -> bool {
    matches!(
        typ,
        "text"
            | "search"
            | "url"
            | "tel"
            | "email"
            | "password"
            | "date"
            | "month"
            | "week"
            | "time"
            | "datetime-local"
            | "number"
    )
}

/// The value sanitization algorithm for the types validation reads: line
/// breaks out, url and email trimmed, an email list trimmed item by item, and
/// a date, time or number that isn't valid gone.
fn sanitized(typ: &str, value: &str, multiple: bool) -> String {
    let no_breaks: String = value.chars().filter(|&c| c != '\n' && c != '\r').collect();
    let trim = |s: &str| {
        s.trim_matches(|c: char| c.is_ascii_whitespace())
            .to_string()
    };
    match typ {
        "url" => trim(&no_breaks),
        "email" if multiple => value.split(',').map(trim).collect::<Vec<_>>().join(","),
        "email" => trim(&no_breaks),
        "number" | "date" | "month" | "week" | "time" | "datetime-local" => {
            if input_number(typ, value).is_some() {
                value.to_string()
            } else {
                String::new()
            }
        }
        _ => no_breaks,
    }
}

/// The rules for parsing floating-point number values: leading whitespace
/// skipped, an optional sign, then as much of a number as there is.
fn parse_float_value(s: &str) -> Option<f64> {
    let s = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let b = s.as_bytes();
    let mut i = usize::from(matches!(b.first(), Some(b'-' | b'+')));
    let digits = |i: &mut usize| {
        let at = *i;
        while b.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        *i > at
    };
    let mut end;
    let whole = digits(&mut i);
    end = i;
    if b.get(i) == Some(&b'.') {
        let mut j = i + 1;
        if digits(&mut j) {
            i = j;
            end = i;
        } else if !whole {
            return None;
        }
    }
    if !whole && end == usize::from(matches!(b.first(), Some(b'-' | b'+'))) {
        return None;
    }
    if matches!(b.get(end), Some(b'e' | b'E')) {
        let mut j = end + 1;
        if matches!(b.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        if digits(&mut j) {
            end = j;
        }
    }
    s[..end]
        .trim_start_matches('+')
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
}

/// The rules for parsing non-negative integers: leading whitespace and a
/// `+` skipped, then the digits up to the first that isn't one.
fn parse_non_negative(s: &str) -> Option<u32> {
    let s = s.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let s = s.strip_prefix('+').unwrap_or(s);
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    s[..end].parse().ok()
}

/// The HTML Standard's valid email address: a local part of letters,
/// digits and ``.!#$%&'*+/=?^_`{|}~-``, then `@` and dot-separated labels
/// of up to 63 letters, digits and hyphens that don't start or end with one.
fn valid_email(address: &str) -> bool {
    let Some((local, domain)) = address.split_once('@') else {
        return false;
    };
    let local_ok = !local.is_empty()
        && local
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".!#$%&'*+/=?^_`{|}~-".contains(c));
    let label_ok = |l: &str| {
        !l.is_empty()
            && l.len() <= 63
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    local_ok && domain.split('.').all(label_ok)
}

/// The day the Monday of the year's first ISO week falls on.
fn monday_of_week_1(y: i64) -> i64 {
    let jan4 = days(y, 1, 4);
    let weekday = (jan4 - 1) % 7; // 1 January of year 1 was a Monday: 0
    jan4 - weekday
}

/// An `<input>` value of `typ` as a number that orders as the values do:
/// the number, or for dates and times a count of days, months, weeks or
/// milliseconds. None when it isn't a valid value of that type.
fn input_number(typ: &str, v: &str) -> Option<f64> {
    match typ {
        "number" | "range" => float_number(v),
        "date" => date(v).map(|(y, m, d)| days(y, m, d) as f64),
        "month" => {
            let (y, m) = v.split_once('-')?;
            let (y, m) = (year(y)?, two_digits(m)?);
            (1..=12).contains(&m).then(|| (y * 12 + m - 1) as f64)
        }
        "week" => {
            let (y, w) = v.split_once("-W")?;
            let (y, w) = (year(y)?, two_digits(w)?);
            (1..=weeks_in(y))
                .contains(&w)
                .then(|| (monday_of_week_1(y) + (w - 1) * 7) as f64)
        }
        "time" => time_ms(v),
        "datetime-local" => {
            let at = v.find(['T', ' '])?;
            let (y, m, d) = date(&v[..at])?;
            Some(days(y, m, d) as f64 * 86_400_000.0 + time_ms(&v[at + 1..])?)
        }
        _ => None,
    }
}

/// HTML's valid floating-point number: -?digits(.digits)?(e[+-]?digits)?,
/// or with no digits before a point.
fn float_number(v: &str) -> Option<f64> {
    let b = v.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    let digits = |i: &mut usize| {
        let at = *i;
        while b.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        *i > at
    };
    let whole = digits(&mut i);
    // A point needs digits after it: 1. isn't a number.
    let point = b.get(i) == Some(&b'.');
    if point {
        i += 1;
    }
    let fraction = point && digits(&mut i);
    if point && !fraction {
        return None;
    }
    if !whole && !fraction {
        return None;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !digits(&mut i) {
            return None;
        }
    }
    if i != b.len() {
        return None;
    }
    v.parse().ok().filter(|n: &f64| n.is_finite())
}

/// Four or more ASCII digits, above zero.
fn year(s: &str) -> Option<i64> {
    (s.len() >= 4 && s.bytes().all(|c| c.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
        .filter(|&y| y > 0)
}

fn two_digits(s: &str) -> Option<i64> {
    (s.len() == 2 && s.bytes().all(|c| c.is_ascii_digit())).then(|| s.parse().ok())?
}

fn leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// YYYY-MM-DD, a real day.
fn date(v: &str) -> Option<(i64, i64, i64)> {
    let mut parts = v.splitn(3, '-');
    let (y, m, d) = (
        year(parts.next()?)?,
        two_digits(parts.next()?)?,
        two_digits(parts.next()?)?,
    );
    let last = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap(y) => 29,
        2 => 28,
        _ => return None,
    };
    (1..=last).contains(&d).then_some((y, m, d))
}

/// Days from 1 January of year 1 to the date, for comparing.
fn days(y: i64, m: i64, d: i64) -> i64 {
    let before = y - 1;
    let mut n = before * 365 + before / 4 - before / 100 + before / 400;
    for month in 1..m {
        n += match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            _ if leap(y) => 29,
            _ => 28,
        };
    }
    n + d
}

/// 53 for a year whose 1 January is a Thursday, or a Wednesday in a leap
/// year; 52 for the rest.
fn weeks_in(y: i64) -> i64 {
    let weekday = days(y, 1, 1) % 7; // 1 January of year 1 was a Monday: 1
    if weekday == 4 || (weekday == 3 && leap(y)) {
        53
    } else {
        52
    }
}

/// HH:MM, HH:MM:SS or HH:MM:SS.fff in milliseconds.
fn time_ms(v: &str) -> Option<f64> {
    let mut parts = v.splitn(3, ':');
    let (h, m) = (two_digits(parts.next()?)?, two_digits(parts.next()?)?);
    if h > 23 || m > 59 {
        return None;
    }
    let seconds = match parts.next() {
        None => 0.0,
        Some(s) => {
            let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
            let sec = two_digits(whole)?;
            let frac_ok = frac.len() <= 3
                && frac.bytes().all(|c| c.is_ascii_digit())
                && (!s.contains('.') || !frac.is_empty());
            if sec > 59 || !frac_ok {
                return None;
            }
            s.parse::<f64>().ok()?
        }
    };
    Some(((h * 60 + m) as f64 * 60.0 + seconds) * 1000.0)
}

/// `2n+1 of .x` into `2n+1` and `.x`.
fn split_of(arg: &str) -> Option<(&str, &str)> {
    let lower = arg.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find("of") {
        let at = from + i;
        let before = lower[..at].chars().next_back();
        let after = lower[at + 2..].chars().next();
        if before.is_some_and(is_space_char) && after.is_some_and(is_space_char) {
            return Some((&arg[..at], &arg[at + 2..]));
        }
        from = at + 2;
    }
    None
}

/// The `An+B` microsyntax: `odd`, `even`, `3`, `-n+2`, `2n + 1`...
fn an_plus_b(text: &str) -> Result<(i32, i32), SelectorError> {
    let text = text.trim().to_ascii_lowercase();
    let bad = || SelectorError {
        reason: format!("{text:?} isn't An+B"),
    };
    match text.as_str() {
        "odd" => return Ok((2, 1)),
        "even" => return Ok((2, 0)),
        "" => return Err(bad()),
        _ => {}
    }
    // Whitespace can only go around the sign before B, as in `2n + 1`:
    // `2 n` and `+ 5` are two tokens where An+B wants one.
    let Some(i) = text.find('n') else {
        return text.parse::<i32>().map(|b| (0, b)).map_err(|_| bad());
    };
    let compact: String = text[i + 1..].split_whitespace().collect();
    let a = match &text[..i] {
        "" | "+" => 1,
        "-" => -1,
        s => s.parse::<i32>().map_err(|_| bad())?,
    };
    let rest = compact.as_str();
    let b = if rest.is_empty() {
        0
    } else if rest.len() > 1
        && rest.starts_with(['+', '-'])
        && rest[1..].bytes().all(|c| c.is_ascii_digit())
    {
        rest.parse::<i32>().map_err(|_| bad())?
    } else {
        return Err(bad());
    };
    Ok((a, b))
}

fn type_selector(name: String) -> Simple {
    let lower = name.to_ascii_lowercase();
    Simple::Type {
        tag: Tag::from_name(&lower),
        lower,
        exact: name,
    }
}

fn nth(a: i32, b: i32, last: bool, of_type: bool) -> Pseudo {
    Pseudo::Nth {
        a,
        b,
        last,
        of_type,
        of: None,
    }
}

impl SelectorList {
    pub fn parse(selector: &str) -> Result<SelectorList, SelectorError> {
        SelectorList::parse_at(selector, 0, false)
    }

    fn parse_at(selector: &str, depth: u32, in_has: bool) -> Result<SelectorList, SelectorError> {
        let mut p = Parser {
            s: selector,
            pos: 0,
            depth,
            in_has,
        };
        let list = p.selector_list(false)?;
        p.skip_space();
        if !p.at_end() {
            return error(format!("unexpected {:?}", &selector[p.pos..]));
        }
        Ok(list)
    }

    /// Whether `el` matches, with `scope` as `:scope` (the root element
    /// when `None`).
    pub fn matches(&self, doc: &Document, el: NodeId, scope: Option<NodeId>) -> bool {
        let cx = Cx::new(doc, scope);
        self.0.iter().any(|c| cx.complex(c, 0, el, el))
    }

    /// The elements under `scope` that match, in document order. As in a
    /// browser, combinators look past `scope`: `article .price` run on an
    /// article matches the prices inside it.
    pub fn select(&self, doc: &Document, scope: NodeId) -> Vec<NodeId> {
        let cx = Cx::new(doc, Some(scope));
        let mut out = Vec::new();
        // A walk by the tree's own links, in document order: down to the
        // first child, else along to the next sibling, else back up.
        let mut at = doc.first_child(scope);
        while let Some(n) = at {
            let element = doc.element(n).is_some();
            if element && self.0.iter().any(|c| cx.complex(c, 0, n, n)) {
                out.push(n);
            }
            at = match doc.first_child(n).filter(|_| element) {
                Some(child) => Some(child),
                None => {
                    let mut up = Some(n);
                    let mut next = None;
                    while let Some(u) = up {
                        if u == scope {
                            break;
                        }
                        if let Some(s) = doc.next_sibling(u) {
                            next = Some(s);
                            break;
                        }
                        up = doc.parent(u);
                    }
                    next
                }
            };
        }
        out
    }
}

fn push_children_reversed(doc: &Document, n: NodeId, stack: &mut Vec<NodeId>) {
    let at = stack.len();
    stack.extend(doc.children(n));
    stack[at..].reverse();
}

// ---- matching --------------------------------------------------------------

struct Cx<'a> {
    doc: &'a Document,
    scope: Option<NodeId>,
    /// Steps left for `pattern` matches in this query, shared by all of them.
    pattern_budget: Cell<u32>,
}

/// HTML attributes whose values selectors compare without case, unless the
/// selector says `s` (the HTML Standard, "case-sensitivity of selectors").
fn case_insensitive_attr(name: &str) -> bool {
    matches!(
        name,
        "accept"
            | "accept-charset"
            | "align"
            | "alink"
            | "axis"
            | "bgcolor"
            | "charset"
            | "checked"
            | "clear"
            | "codetype"
            | "color"
            | "compact"
            | "declare"
            | "defer"
            | "dir"
            | "direction"
            | "disabled"
            | "enctype"
            | "face"
            | "frame"
            | "hreflang"
            | "http-equiv"
            | "lang"
            | "language"
            | "link"
            | "media"
            | "method"
            | "multiple"
            | "nohref"
            | "noresize"
            | "noshade"
            | "nowrap"
            | "readonly"
            | "rel"
            | "rev"
            | "rules"
            | "scope"
            | "scrolling"
            | "selected"
            | "shape"
            | "target"
            | "text"
            | "type"
            | "valign"
            | "valuetype"
            | "vlink"
    )
}

fn is_space_char(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0C')
}

impl<'a> Cx<'a> {
    fn new(doc: &'a Document, scope: Option<NodeId>) -> Self {
        Cx {
            doc,
            scope,
            pattern_budget: Cell::new(crate::pattern::SELECT_STEP_LIMIT),
        }
    }

    fn parent_element(&self, n: NodeId) -> Option<NodeId> {
        self.doc
            .parent(n)
            .filter(|&p| self.doc.element(p).is_some())
    }

    fn prev_element(&self, n: NodeId) -> Option<NodeId> {
        let mut at = self.doc.prev_sibling(n);
        while let Some(p) = at {
            if self.doc.element(p).is_some() {
                return Some(p);
            }
            at = self.doc.prev_sibling(p);
        }
        None
    }

    fn next_element(&self, n: NodeId) -> Option<NodeId> {
        let mut at = self.doc.next_sibling(n);
        while let Some(p) = at {
            if self.doc.element(p).is_some() {
                return Some(p);
            }
            at = self.doc.next_sibling(p);
        }
        None
    }

    /// Does `el` match `c.parts[i..]`? `anchor` is the element a relative
    /// selector in `:has()` hangs from.
    fn complex(&self, c: &Complex, i: usize, el: NodeId, anchor: NodeId) -> bool {
        let (compound, combinator) = &c.parts[i];
        if !self.compound(compound, el, anchor) {
            return false;
        }
        if i + 1 == c.parts.len() {
            return true;
        }
        match combinator {
            Combinator::None => true,
            Combinator::Child => self
                .doc
                .parent(el)
                .is_some_and(|p| self.complex(c, i + 1, p, anchor)),
            Combinator::Descendant => {
                let mut at = self.doc.parent(el);
                while let Some(p) = at {
                    if self.complex(c, i + 1, p, anchor) {
                        return true;
                    }
                    at = self.doc.parent(p);
                }
                false
            }
            Combinator::NextSibling => self
                .prev_element(el)
                .is_some_and(|p| self.complex(c, i + 1, p, anchor)),
            Combinator::SubsequentSibling => {
                let mut at = self.prev_element(el);
                while let Some(p) = at {
                    if self.complex(c, i + 1, p, anchor) {
                        return true;
                    }
                    at = self.prev_element(p);
                }
                false
            }
        }
    }

    fn compound(&self, compound: &Compound, el: NodeId, anchor: NodeId) -> bool {
        if matches!(compound.0.as_slice(), [Simple::Anchor]) {
            return el == anchor;
        }
        let Some(e) = self.doc.element(el) else {
            return false;
        };
        compound.0.iter().all(|s| match s {
            Simple::Type { lower, exact, tag } => {
                if e.ns == Ns::Html {
                    if *tag != Tag::Other || e.tag != Tag::Other {
                        e.tag == *tag
                    } else {
                        self.doc.element_name(e) == lower
                    }
                } else if e.ns == Ns::Xml {
                    // In XML a type selector names the local part, in any
                    // case: `creator` finds `dc:creator` and `Creator`.
                    xml_local(self.doc.element_name(e)).eq_ignore_ascii_case(lower)
                } else {
                    self.doc.element_name(e) == exact
                }
            }
            Simple::Id(id) => self.attr(el, "id").is_some_and(|v| self.quirks_eq(v, id)),
            Simple::Class(class) => self.attr(el, "class").is_some_and(|v| {
                v.split(is_space_char)
                    .any(|c| !c.is_empty() && self.quirks_eq(c, class))
            }),
            Simple::Attr(a) => self.attr_matches(el, e.ns, a),
            Simple::Pseudo(p) => self.pseudo(p, el, anchor),
            Simple::Anchor => el == anchor,
        })
    }

    fn quirks_eq(&self, a: &str, b: &str) -> bool {
        if self.doc.quirks() == Quirks::Quirks {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    }

    /// The element's language: the nearest `xml:lang` or `lang` on it or
    /// an ancestor, `xml:lang` first, as HTML has it. None when nothing says.
    fn lang(&self, el: NodeId) -> Option<&str> {
        let mut at = Some(el);
        while let Some(n) = at {
            let mut plain = None;
            for (ns, name, value) in self.doc.attrs(n) {
                match (ns, name) {
                    (AttrNs::Xml, "lang") => return Some(value),
                    (AttrNs::None, "lang") => plain = Some(value),
                    _ => {}
                }
            }
            if plain.is_some() {
                return plain;
            }
            at = self.doc.parent(n);
        }
        None
    }

    /// Whether the element's directionality is right to left, by HTML's
    /// rules as far as the document decides them: `dir="ltr"` or `"rtl"`,
    /// `dir="auto"` and `<bdi>` from their text, and otherwise the parent's,
    /// left to right at the top. Only an HTML element's `dir` counts.
    fn is_rtl(&self, el: NodeId) -> bool {
        let mut at = Some(el);
        while let Some(n) = at {
            let Some(e) = self.doc.element(n) else {
                return false;
            };
            if e.ns == Ns::Html {
                match self.dir(n).as_deref() {
                    Some("ltr") => return false,
                    Some("rtl") => return true,
                    Some("auto") => return self.auto_rtl(n),
                    _ if self.doc.lower_name(n) == Some("bdi") => return self.auto_rtl(n),
                    _ => {}
                }
            }
            at = self.doc.parent(n);
        }
        false
    }

    /// A valid `dir` value, lowercased.
    fn dir(&self, el: NodeId) -> Option<String> {
        self.attr(el, "dir")
            .map(|d| d.to_ascii_lowercase())
            .filter(|d| matches!(d.as_str(), "ltr" | "rtl" | "auto"))
    }

    /// `dir="auto"`: right to left when the first strong character is. A
    /// text field reads its value; anything else its text, leaving out
    /// `<bdi>`, `<script>`, `<style>`, `<textarea>` and anything with its own
    /// `dir`. No strong character at all is left to right.
    fn auto_rtl(&self, el: NodeId) -> bool {
        let tag = self.doc.element(el).map(|e| e.tag);
        if tag == Some(Tag::Textarea) {
            return first_strong_rtl(&self.doc.text(el)).unwrap_or(false);
        }
        if tag == Some(Tag::Input) {
            let text_field = self.attr(el, "type").is_none_or(|t| {
                matches!(
                    t.to_ascii_lowercase().as_str(),
                    "text" | "search" | "tel" | "url" | "email"
                )
            });
            return text_field
                && first_strong_rtl(self.attr(el, "value").unwrap_or("")).unwrap_or(false);
        }
        let mut stack: Vec<NodeId> = self.doc.children(el).collect();
        stack.reverse();
        while let Some(n) = stack.pop() {
            match self.doc.kind(n) {
                crate::Kind::Text(t) => {
                    if let Some(rtl) = first_strong_rtl(t) {
                        return rtl;
                    }
                }
                crate::Kind::Element { ns, .. } => {
                    let skip = ns == Ns::Html
                        && (self.dir(n).is_some()
                            || matches!(
                                self.doc.element(n).map(|e| e.tag),
                                Some(Tag::Script | Tag::Style | Tag::Textarea)
                            )
                            || self.doc.lower_name(n) == Some("bdi"));
                    if !skip {
                        let at = stack.len();
                        stack.extend(self.doc.children(n));
                        stack[at..].reverse();
                    }
                }
                _ => {}
            }
        }
        false
    }

    /// Whether `el` is an `<input>` whose type, in any case, is one of `types`.
    fn input_type(&self, el: NodeId, types: &[&str]) -> bool {
        self.attr(el, "type")
            .is_some_and(|t| types.iter().any(|w| t.eq_ignore_ascii_case(w)))
    }

    fn html_tag(&self, n: NodeId, tag: Tag) -> bool {
        self.doc
            .element(n)
            .is_some_and(|e| e.ns == Ns::Html && e.tag == tag)
    }

    fn html_named(&self, n: NodeId, name: &str) -> bool {
        self.doc.element(n).is_some_and(|e| e.ns == Ns::Html)
            && self.doc.lower_name(n) == Some(name)
    }

    /// `el`'s tree in tree order, from its root.
    // shortcut: walked afresh for each control matched, so :default or
    // :indeterminate over a page of n radio buttons is O(n^2); keep a
    // per-select index of forms and radio groups if pages ever have
    // thousands.
    fn tree_of(&self, el: NodeId) -> Vec<NodeId> {
        let mut root = el;
        while let Some(p) = self.doc.parent(root) {
            root = p;
        }
        let (mut out, mut stack) = (Vec::new(), vec![root]);
        while let Some(n) = stack.pop() {
            out.push(n);
            let at = stack.len();
            stack.extend(self.doc.children(n));
            stack[at..].reverse();
        }
        out
    }

    /// The form a control belongs to: the one its `form` attribute names,
    /// or with no such attribute the nearest form around it.
    fn form_owner(&self, el: NodeId) -> Option<NodeId> {
        if let Some(id) = self.attr(el, "form") {
            return self
                .tree_of(el)
                .into_iter()
                .find(|&n| self.doc.element(n).is_some() && self.attr(n, "id") == Some(id))
                .filter(|&f| self.html_tag(f, Tag::Form));
        }
        let mut at = self.parent_element(el);
        while let Some(p) = at {
            if self.html_tag(p, Tag::Form) {
                return Some(p);
            }
            at = self.parent_element(p);
        }
        None
    }

    fn submit_button(&self, el: NodeId, tag: Tag) -> bool {
        match tag {
            // A button's type is submit unless it says reset or button.
            Tag::Button => self.attr(el, "type").is_none_or(|t| {
                !t.eq_ignore_ascii_case("reset") && !t.eq_ignore_ascii_case("button")
            }),
            Tag::Input => self.input_type(el, &["submit", "image"]),
            _ => false,
        }
    }

    /// Whether `el` is its form's default button: the first submit button
    /// in tree order that belongs to the same form.
    fn default_button(&self, el: NodeId, tag: Tag) -> bool {
        if !self.submit_button(el, tag) {
            return false;
        }
        let Some(form) = self.form_owner(el) else {
            return false;
        };
        self.tree_of(el).into_iter().find(|&n| {
            self.doc.element(n).is_some_and(|e| {
                e.ns == Ns::Html && self.submit_button(n, e.tag) && self.form_owner(n) == Some(form)
            })
        }) == Some(el)
    }

    /// Whether no radio button in `el`'s group is checked: the radio
    /// buttons with its name and form, in its tree. With no name it's a
    /// group of one.
    fn radio_group_unchecked(&self, el: NodeId) -> bool {
        if self.attr(el, "checked").is_some() {
            return false;
        }
        let Some(name) = self.attr(el, "name").filter(|n| !n.is_empty()) else {
            return true;
        };
        let form = self.form_owner(el);
        !self.tree_of(el).into_iter().any(|n| {
            self.html_tag(n, Tag::Input)
                && self.input_type(n, &["radio"])
                && self.attr(n, "name") == Some(name)
                && self.attr(n, "checked").is_some()
                && self.form_owner(n) == form
        })
    }

    /// For an `<input>` with range limitations that takes part in
    /// validation, whether its value is in range; None for any other.
    /// An empty or unreadable value isn't out of range, and a range
    /// input's value is always brought into range.
    fn in_range(&self, el: NodeId) -> Option<bool> {
        let typ = self.attr(el, "type")?.to_ascii_lowercase();
        let read = |name: &str| self.attr(el, name).and_then(|v| input_number(&typ, v));
        let (min, max) = match typ.as_str() {
            "range" => (read("min").or(Some(0.0)), read("max").or(Some(100.0))),
            "number" | "date" | "month" | "week" | "time" | "datetime-local" => {
                (read("min"), read("max"))
            }
            _ => return None,
        };
        if min.is_none() && max.is_none() {
            return None;
        }
        // Barred from validation: disabled, read-only where that applies,
        // or inside a datalist.
        let in_datalist = {
            let mut at = self.parent_element(el);
            let mut found = false;
            while let Some(p) = at {
                if self.html_named(p, "datalist") {
                    found = true;
                    break;
                }
                at = self.parent_element(p);
            }
            found
        };
        if self.disabled(el, Tag::Input)
            || (typ != "range" && self.attr(el, "readonly").is_some())
            || in_datalist
        {
            return None;
        }
        if typ == "range" {
            return Some(true);
        }
        let Some(value) = read("value") else {
            return Some(true);
        };
        Some(match (min, max) {
            // A time range may wrap past midnight: 22:00 to 06:00.
            (Some(lo), Some(hi)) if typ == "time" && lo > hi => value >= lo || value <= hi,
            _ => min.is_none_or(|lo| value >= lo) && max.is_none_or(|hi| value <= hi),
        })
    }

    /// For `:valid` and `:invalid`: whether `el` satisfies its constraints,
    /// or for a form or fieldset whether everything in it does; None for an
    /// element that doesn't take part in validation.
    fn valid(&self, el: NodeId, tag: Tag) -> Option<bool> {
        match tag {
            Tag::Form => Some(!self.tree_of(el).into_iter().any(|n| {
                self.doc.element(n).is_some_and(|e| e.ns == Ns::Html)
                    && self.form_owner(n) == Some(el)
                    && self.control_valid(n) == Some(false)
            })),
            Tag::Fieldset => {
                let mut stack: Vec<NodeId> = self.doc.children(el).collect();
                while let Some(n) = stack.pop() {
                    if self.control_valid(n) == Some(false) {
                        return Some(false);
                    }
                    stack.extend(self.doc.children(n));
                }
                Some(true)
            }
            _ => self.control_valid(el),
        }
    }

    /// Whether a form control satisfies its constraints, None when it's
    /// barred from validation. The static document's value is the `value`
    /// attribute, or a textarea's text. Only what the markup decides is
    /// checked: not too long, too short or bad input, which only someone
    /// typing can cause.
    fn control_valid(&self, el: NodeId) -> Option<bool> {
        let e = self.doc.element(el)?;
        if e.ns != Ns::Html || self.disabled(el, e.tag) || self.in_datalist(el) {
            return None;
        }
        let required = self.attr(el, "required").is_some();
        match e.tag {
            Tag::Input => {
                let typ = input_type_state(self.attr(el, "type"));
                if matches!(typ, "hidden" | "reset" | "button")
                    || (readonly_applies(typ) && self.attr(el, "readonly").is_some())
                {
                    return None;
                }
                Some(self.input_valid(el, typ, required))
            }
            Tag::Textarea => {
                if self.attr(el, "readonly").is_some() {
                    return None;
                }
                Some(!required || !self.doc.text(el).is_empty())
            }
            Tag::Select => Some(!required || self.select_has_value(el)),
            Tag::Button => self.submit_button(el, Tag::Button).then_some(true),
            _ => None,
        }
    }

    fn input_valid(&self, el: NodeId, typ: &str, required: bool) -> bool {
        match typ {
            "checkbox" => !required || self.attr(el, "checked").is_some(),
            "radio" => !self.radio_value_missing(el),
            // A static page has no files chosen.
            "file" => !required,
            "submit" | "image" | "color" | "range" => true,
            _ => {
                let value = sanitized(
                    typ,
                    self.attr(el, "value").unwrap_or(""),
                    self.attr(el, "multiple").is_some(),
                );
                if value.is_empty() {
                    return !required;
                }
                let multiple = typ == "email" && self.attr(el, "multiple").is_some();
                if !self.pattern_fits(el, typ, &value, multiple) {
                    return false;
                }
                match typ {
                    "email" if self.attr(el, "multiple").is_some() => {
                        value.split(',').all(valid_email)
                    }
                    "email" => valid_email(&value),
                    "url" => url::Url::parse(&value).is_ok(),
                    "number" | "date" | "month" | "week" | "time" | "datetime-local" => {
                        self.in_range(el) != Some(false) && self.step_fits(el, typ, &value)
                    }
                    _ => true,
                }
            }
        }
    }

    /// Whether the value matches `pattern`, for the types it applies to:
    /// each value of a multiple email. A pattern that doesn't compile counts
    /// as none, as the standard says.
    fn pattern_fits(&self, el: NodeId, typ: &str, value: &str, multiple: bool) -> bool {
        if !matches!(
            typ,
            "text" | "search" | "tel" | "url" | "email" | "password"
        ) {
            return true;
        }
        let Some(pattern) = self.attr(el, "pattern").and_then(Pattern::compile) else {
            return true;
        };
        if multiple {
            value
                .split(',')
                .all(|v| pattern.matches(v, &self.pattern_budget))
        } else {
            pattern.matches(value, &self.pattern_budget)
        }
    }

    /// Whether the value lands on a step from `min`. With no valid `min` the
    /// step base is the value itself, so any value fits.
    fn step_fits(&self, el: NodeId, typ: &str, value: &str) -> bool {
        let (default, scale) = match typ {
            "week" => (1.0, 7.0),
            "time" | "datetime-local" => (60.0, 1000.0),
            _ => (1.0, 1.0),
        };
        let step = match self.attr(el, "step") {
            Some(s) if s.eq_ignore_ascii_case("any") => return true,
            Some(s) => parse_float_value(s).filter(|&s| s > 0.0).unwrap_or(default),
            None => default,
        };
        let (Some(value), Some(base)) = (
            input_number(typ, value),
            self.attr(el, "min").and_then(|m| input_number(typ, m)),
        ) else {
            return true;
        };
        let steps = (value - base) / (step * scale);
        (steps - steps.round()).abs() < 1e-9
    }

    /// Whether `el`'s radio group is required and has nothing checked.
    fn radio_value_missing(&self, el: NodeId) -> bool {
        let Some(name) = self.attr(el, "name").filter(|n| !n.is_empty()) else {
            return self.attr(el, "required").is_some() && self.attr(el, "checked").is_none();
        };
        let form = self.form_owner(el);
        let group: Vec<NodeId> = self
            .tree_of(el)
            .into_iter()
            .filter(|&n| {
                self.html_tag(n, Tag::Input)
                    && self.input_type(n, &["radio"])
                    && self.attr(n, "name") == Some(name)
                    && self.form_owner(n) == form
            })
            .collect();
        group.iter().any(|&n| self.attr(n, "required").is_some())
            && group.iter().all(|&n| self.attr(n, "checked").is_none())
    }

    /// Whether a select has a value to submit: an option selected, and in a
    /// one-line single select not the placeholder (a first child option
    /// whose value is empty).
    fn select_has_value(&self, el: NodeId) -> bool {
        let mut options = Vec::new();
        let mut stack: Vec<NodeId> = self.doc.children(el).collect();
        stack.reverse();
        while let Some(n) = stack.pop() {
            if self.html_tag(n, Tag::Option) {
                options.push(n);
            } else if self.html_tag(n, Tag::Optgroup) {
                let at = stack.len();
                stack.extend(self.doc.children(n));
                stack[at..].reverse();
            }
        }
        let marked = |n: &&NodeId| self.attr(**n, "selected").is_some();
        let multiple = self.attr(el, "multiple").is_some();
        let size = self
            .attr(el, "size")
            .and_then(parse_non_negative)
            .filter(|&s| s > 0)
            .unwrap_or(if multiple { 4 } else { 1 });
        if multiple {
            return options.iter().any(|n| marked(&n));
        }
        // The last one marked selected wins, else the first that isn't disabled.
        // The last one marked selected wins. With none, a one-line select
        // shows its first option that isn't disabled; a list box shows none.
        let selected = options.iter().rev().find(marked).or_else(|| {
            options
                .iter()
                .filter(|_| size == 1)
                .find(|&&n| self.attr(n, "disabled").is_none() && !self.disabled(n, Tag::Option))
        });
        let Some(&selected) = selected else {
            return false;
        };
        let placeholder = size == 1
            && options.first() == Some(&selected)
            && self.doc.parent(selected) == Some(el)
            && self.option_value(selected).is_empty();
        !placeholder
    }

    fn option_value(&self, el: NodeId) -> String {
        match self.attr(el, "value") {
            Some(v) => v.to_string(),
            None => self
                .doc
                .text(el)
                .split_ascii_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
        }
    }

    fn in_datalist(&self, el: NodeId) -> bool {
        let mut at = self.parent_element(el);
        while let Some(p) = at {
            if self.html_named(p, "datalist") {
                return true;
            }
            at = self.parent_element(p);
        }
        false
    }

    fn attr(&self, el: NodeId, name: &str) -> Option<&str> {
        self.doc
            .attrs(el)
            .find(|(ns, n, _)| *ns == AttrNs::None && *n == name)
            .map(|(_, _, v)| v)
    }

    fn attr_matches(&self, el: NodeId, ns: Ns, a: &AttrSelector) -> bool {
        let doc = self.doc;
        if ns == Ns::Xml {
            return self.xml_attr_matches(el, a);
        }
        let html = ns == Ns::Html;
        let insensitive = match a.case {
            Case::Insensitive => true,
            Case::Sensitive => false,
            Case::Default => html && case_insensitive_attr(&a.lower),
        };
        let want = if html { &a.lower } else { &a.exact };
        // The raw attributes, so a value is only looked at once the name
        // matches.
        doc.attrs_of(el).iter().any(|attr| {
            // A browser matches SVG's `viewBox` with its case.
            let name = doc.written_name(ns, attr);
            // A namespaced attribute is stored as written (`xlink:href`);
            // `[*|href]` names its local part.
            let name = match attr.ns {
                AttrNs::None => name,
                _ => name.split_once(':').map_or(name, |(_, local)| local),
            };
            (attr.ns == AttrNs::None || a.any_ns)
                && name == want.as_str()
                && (a.op == AttrOp::Exists
                    || value_matches(a.op, doc.str(attr.value), &a.value, insensitive))
        })
    }

    /// An attribute selector in XML: the attribute is found by its local
    /// name in any case, one written exactly as asked winning over others
    /// (`id` over `ID`), and only that one is compared.
    fn xml_attr_matches(&self, el: NodeId, a: &AttrSelector) -> bool {
        let doc = self.doc;
        let range = doc
            .element(el)
            .map_or(0..0, |e| e.attrs.start as usize..e.attrs.end as usize);
        let attrs = &doc.attrs[range];
        let local = |attr: &crate::dom::Attr| xml_local(doc.str(attr.name));
        let found = attrs
            .iter()
            .find(|attr| local(attr) == a.exact)
            .or_else(|| {
                attrs
                    .iter()
                    .find(|attr| local(attr).eq_ignore_ascii_case(&a.lower))
            });
        let insensitive = a.case == Case::Insensitive;
        found.is_some_and(|attr| {
            a.op == AttrOp::Exists
                || value_matches(a.op, doc.str(attr.value), &a.value, insensitive)
        })
    }

    fn pseudo(&self, p: &Pseudo, el: NodeId, anchor: NodeId) -> bool {
        let doc = self.doc;
        let e = doc.element(el).expect("an element");
        let html = e.ns == Ns::Html;
        let tag = if html { e.tag } else { Tag::Other };
        match p {
            Pseudo::Not(list) => !list.0.iter().any(|c| self.complex(c, 0, el, anchor)),
            Pseudo::Is(list) => list.0.iter().any(|c| self.complex(c, 0, el, anchor)),
            Pseudo::Has(list) => list.0.iter().any(|c| self.has(c, el)),
            Pseudo::Nth {
                a,
                b,
                last,
                of_type,
                of,
            } => {
                let same = |other: NodeId| {
                    if *of_type {
                        same_type(doc, doc.element(other).unwrap(), e)
                    } else if let Some(of) = of {
                        of.0.iter().any(|c| self.complex(c, 0, other, other))
                    } else {
                        true
                    }
                };
                if !same(el) {
                    return false;
                }
                let mut index = 1i64;
                let mut at = if *last {
                    self.next_element(el)
                } else {
                    self.prev_element(el)
                };
                while let Some(s) = at {
                    if same(s) {
                        index += 1;
                        // With a <= 0 the positions that match end at b, so
                        // counting further can't help: `:last-child` looks
                        // at one sibling, not all of them.
                        if *a <= 0 && index > *b as i64 {
                            return false;
                        }
                    }
                    at = if *last {
                        self.next_element(s)
                    } else {
                        self.prev_element(s)
                    };
                }
                nth_matches(*a as i64, *b as i64, index)
            }
            Pseudo::Only { of_type } => {
                let same =
                    |other: NodeId| !*of_type || same_type(doc, doc.element(other).unwrap(), e);
                let mut at = self.prev_element(el);
                while let Some(s) = at {
                    if same(s) {
                        return false;
                    }
                    at = self.prev_element(s);
                }
                let mut at = self.next_element(el);
                while let Some(s) = at {
                    if same(s) {
                        return false;
                    }
                    at = self.next_element(s);
                }
                true
            }
            Pseudo::Empty => doc.children(el).all(|c| {
                matches!(
                    doc.nodes[c as usize].data,
                    Data::Comment(_) | Data::ProcessingInstruction { .. }
                ) || matches!(doc.kind(c), crate::Kind::Text(t) if t.is_empty())
            }),
            Pseudo::Root => doc
                .parent(el)
                .is_some_and(|p| matches!(doc.nodes[p as usize].data, Data::Document)),
            Pseudo::Scope => match self.scope {
                Some(s) if doc.element(s).is_some() => el == s,
                _ => doc
                    .parent(el)
                    .is_some_and(|p| matches!(doc.nodes[p as usize].data, Data::Document)),
            },
            Pseudo::AnyLink => matches!(tag, Tag::A | Tag::Area) && self.attr(el, "href").is_some(),
            Pseudo::Checked => match tag {
                Tag::Input => {
                    self.attr(el, "checked").is_some()
                        && self.attr(el, "type").is_some_and(|t| {
                            t.eq_ignore_ascii_case("checkbox") || t.eq_ignore_ascii_case("radio")
                        })
                }
                Tag::Option => self.attr(el, "selected").is_some(),
                _ => false,
            },
            Pseudo::Default => match tag {
                Tag::Input if self.input_type(el, &["checkbox", "radio"]) => {
                    self.attr(el, "checked").is_some()
                }
                Tag::Option => self.attr(el, "selected").is_some(),
                Tag::Button | Tag::Input => self.default_button(el, tag),
                _ => false,
            },
            Pseudo::Indeterminate => match tag {
                Tag::Input if self.input_type(el, &["radio"]) => self.radio_group_unchecked(el),
                // A checkbox is only indeterminate when a script says so.
                _ => self.html_named(el, "progress") && self.attr(el, "value").is_none(),
            },
            Pseudo::InRange => tag == Tag::Input && self.in_range(el) == Some(true),
            Pseudo::OutOfRange => tag == Tag::Input && self.in_range(el) == Some(false),
            Pseudo::Valid => self.valid(el, tag) == Some(true),
            Pseudo::Invalid => self.valid(el, tag) == Some(false),
            Pseudo::Disabled => self.disabled(el, tag),
            Pseudo::Enabled => self.form_control(tag) && !self.disabled(el, tag),
            Pseudo::Required => {
                matches!(tag, Tag::Input | Tag::Select | Tag::Textarea)
                    && self.attr(el, "required").is_some()
            }
            Pseudo::Optional => {
                matches!(tag, Tag::Input | Tag::Select | Tag::Textarea)
                    && self.attr(el, "required").is_none()
            }
            Pseudo::ReadWrite => self.read_write(el, tag),
            Pseudo::ReadOnly => !self.read_write(el, tag),
            Pseudo::PlaceholderShown => match tag {
                Tag::Input => {
                    self.attr(el, "placeholder").is_some()
                        && self.attr(el, "value").is_none_or(str::is_empty)
                }
                Tag::Textarea => self.attr(el, "placeholder").is_some() && doc.text(el).is_empty(),
                _ => false,
            },
            Pseudo::Lang(ranges) => self.lang(el).is_some_and(|tag| {
                let tag = tag.to_ascii_lowercase();
                ranges.iter().any(|range| lang_matches(range, &tag))
            }),
            Pseudo::Dir(rtl) => self.is_rtl(el) == *rtl,
            Pseudo::Never => false,
        }
    }

    /// `:has()`: some element related to `el` as the relative selector
    /// says matches it, with `el` as its anchor.
    fn has(&self, c: &Complex, el: NodeId) -> bool {
        let doc = self.doc;
        // The combinator next to the anchor decides where to look: below
        // it, or among its later siblings and below them.
        let first = c.parts[c.parts.len() - 2].1;
        let mut stack: Vec<NodeId> = Vec::new();
        match first {
            Combinator::NextSibling | Combinator::SubsequentSibling => {
                let mut at = self.next_element(el);
                while let Some(s) = at {
                    stack.push(s);
                    at = self.next_element(s);
                }
            }
            _ => push_children_reversed(doc, el, &mut stack),
        }
        while let Some(n) = stack.pop() {
            if doc.element(n).is_none() {
                continue;
            }
            if self.complex(c, 0, n, el) {
                return true;
            }
            push_children_reversed(doc, n, &mut stack);
        }
        false
    }

    fn form_control(&self, tag: Tag) -> bool {
        matches!(
            tag,
            Tag::Button
                | Tag::Input
                | Tag::Select
                | Tag::Textarea
                | Tag::Optgroup
                | Tag::Option
                | Tag::Fieldset
        )
    }

    /// The HTML Standard's "actually disabled".
    fn disabled(&self, el: NodeId, tag: Tag) -> bool {
        let doc = self.doc;
        match tag {
            Tag::Optgroup => self.attr(el, "disabled").is_some(),
            Tag::Option => {
                self.attr(el, "disabled").is_some()
                    || self.parent_element(el).is_some_and(|p| {
                        doc.element(p)
                            .is_some_and(|e| e.ns == Ns::Html && e.tag == Tag::Optgroup)
                            && self.attr(p, "disabled").is_some()
                    })
            }
            Tag::Button | Tag::Input | Tag::Select | Tag::Textarea | Tag::Fieldset => {
                if self.attr(el, "disabled").is_some() {
                    return true;
                }
                // A descendant of a disabled fieldset, outside its first
                // legend.
                let mut child = el;
                let mut at = self.parent_element(el);
                while let Some(p) = at {
                    let pe = doc.element(p).unwrap();
                    if pe.ns == Ns::Html
                        && pe.tag == Tag::Fieldset
                        && self.attr(p, "disabled").is_some()
                    {
                        let first_legend = doc.children(p).find(|&c| {
                            doc.element(c).is_some_and(|e| {
                                e.ns == Ns::Html && doc.element_name(e) == "legend"
                            })
                        });
                        if first_legend != Some(child) {
                            return true;
                        }
                    }
                    child = p;
                    at = self.parent_element(p);
                }
                false
            }
            _ => false,
        }
    }

    fn read_write(&self, el: NodeId, tag: Tag) -> bool {
        match tag {
            Tag::Input => {
                let typ = self.attr(el, "type").unwrap_or("text").to_ascii_lowercase();
                let mutable = matches!(
                    typ.as_str(),
                    "text"
                        | "search"
                        | "url"
                        | "tel"
                        | "email"
                        | "password"
                        | "date"
                        | "month"
                        | "week"
                        | "time"
                        | "datetime-local"
                        | "number"
                        | ""
                );
                mutable && self.attr(el, "readonly").is_none() && !self.disabled(el, tag)
            }
            Tag::Textarea => self.attr(el, "readonly").is_none() && !self.disabled(el, tag),
            _ => self.editable(el),
        }
    }

    /// Inside an element whose contenteditable says it's editable.
    fn editable(&self, el: NodeId) -> bool {
        let mut at = Some(el);
        while let Some(n) = at {
            if let Some(v) = self.attr(n, "contenteditable") {
                return !v.eq_ignore_ascii_case("false");
            }
            at = self.parent_element(n);
        }
        false
    }
}

/// Same namespace and name: by tag number when the parser knows the tag.
fn same_type(doc: &Document, a: &crate::dom::Element, b: &crate::dom::Element) -> bool {
    a.ns == b.ns
        && if a.tag != Tag::Other || b.tag != Tag::Other {
            a.tag == b.tag && (a.ns == Ns::Html || doc.element_name(a) == doc.element_name(b))
        } else {
            doc.element_name(a) == doc.element_name(b)
        }
}

/// An XML name without its prefix.
fn xml_local(name: &str) -> &str {
    name.split_once(':').map_or(name, |(_, local)| local)
}

fn nth_matches(a: i64, b: i64, index: i64) -> bool {
    if a == 0 {
        return index == b;
    }
    let diff = index - b;
    diff % a == 0 && diff / a >= 0
}

fn value_matches(op: AttrOp, value: &str, want: &str, insensitive: bool) -> bool {
    let eq = |x: &str, y: &str| {
        if insensitive {
            x.eq_ignore_ascii_case(y)
        } else {
            x == y
        }
    };
    match op {
        AttrOp::Exists => true,
        AttrOp::Equals => eq(value, want),
        AttrOp::Includes => {
            !want.is_empty()
                && !want.contains(is_space_char)
                && value.split(is_space_char).any(|w| eq(w, want))
        }
        AttrOp::DashMatch => {
            eq(value, want)
                || (value.len() > want.len()
                    && value.as_bytes()[want.len()] == b'-'
                    && value.is_char_boundary(want.len())
                    && eq(&value[..want.len()], want))
        }
        AttrOp::Prefix => {
            !want.is_empty()
                && value.len() >= want.len()
                && value.is_char_boundary(want.len())
                && eq(&value[..want.len()], want)
        }
        AttrOp::Suffix => {
            !want.is_empty()
                && value.len() >= want.len()
                && value.is_char_boundary(value.len() - want.len())
                && eq(&value[value.len() - want.len()..], want)
        }
        AttrOp::Substring => {
            !want.is_empty()
                && if insensitive {
                    value
                        .to_ascii_lowercase()
                        .contains(&want.to_ascii_lowercase())
                } else {
                    value.contains(want)
                }
        }
    }
}
