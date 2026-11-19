//! XPath 1.0 over a parsed document, from netweir-xpath, with the
//! extensions Scrapy users rely on: `has-class()` and the EXSLT
//! regular-expression functions `re:test()` and `re:replace()`.
//!
//! Names with a prefix (`atom:entry`) take their namespace from the
//! prefixes a query is run with ([`XPath::run_in`]), as lxml's do. In an
//! HTML document nothing has a namespace, so they match nothing there.

use std::sync::Arc;

use netweir_xpath::{self as xpath, Args, Doc, Extensions, Item, Value};

use crate::cache::{self, Cache};
use crate::document::Node;
use crate::query::Hit;

pub use netweir_xpath::{XPathError, XValue};

/// A compiled XPath expression. It holds no state of any document, so one
/// can be shared by many threads.
#[derive(Debug, Clone)]
pub struct XPath(xpath::XPath);

impl XPath {
    pub fn new(src: &str) -> Result<XPath, XPathError> {
        xpath::XPath::with_extensions(src, &Parsel).map(XPath)
    }

    /// [`XPath::new`], from this thread's cache of recently used
    /// expressions (see [`crate::QUERY_CACHE_SIZE`]).
    pub fn cached(src: &str) -> Result<Arc<XPath>, XPathError> {
        thread_local!(static CACHE: Cache<XPath> = Default::default());
        cache::lookup(&CACHE, src, XPath::new)
    }

    /// Evaluates with `scope` as the context node. Node-set results come
    /// back in document order: elements, text and comment nodes as
    /// [`Hit::Node`], attributes as their value. A number, string or
    /// boolean result is one [`Hit::Value`], formatted as Scrapy's parsel
    /// formats it (`3.0`, `1`/`0` for true/false).
    pub fn run<'a>(&self, scope: Node<'a>) -> Result<Vec<Hit<'a>>, XPathError> {
        self.run_with(scope, &[])
    }

    /// `run`, with values for `$variables` in the expression.
    pub fn run_with<'a>(
        &self,
        scope: Node<'a>,
        vars: &[(String, XValue)],
    ) -> Result<Vec<Hit<'a>>, XPathError> {
        self.run_in(scope, vars, &[])
    }

    /// `run_with`, with `(prefix, uri)` pairs binding the namespace
    /// prefixes the expression uses (a later pair for a prefix wins).
    /// `re` and `set` are bound to the EXSLT namespaces unless given.
    pub fn run_in<'a>(
        &self,
        scope: Node<'a>,
        vars: &[(String, XValue)],
        namespaces: &[(String, String)],
    ) -> Result<Vec<Hit<'a>>, XPathError> {
        let doc = scope.doc;
        let (tree, index) = (doc.tree(), doc.index());
        let view = match doc.xml_names() {
            Some(names) => Doc::xml(tree, index, names),
            None => Doc::html(tree, index),
        };
        // Given ones come later, so they win.
        let bound: Vec<(String, String)> = DEFAULT_NAMESPACES
            .iter()
            .map(|(p, uri)| (p.to_string(), uri.to_string()))
            .chain(namespaces.iter().cloned())
            .collect();
        match self.0.evaluate(view, scope.id, vars, &bound, &Parsel) {
            Ok(value) => Ok(into_hits(value, scope)),
            Err(mut e) => {
                if e.reason.starts_with("undefined namespace prefix") {
                    e.reason +=
                        ": register it with register_namespace(), or pass it in namespaces=";
                }
                Err(e)
            }
        }
    }
}

/// The EXSLT namespaces parsel binds by default.
const DEFAULT_NAMESPACES: [(&str, &str); 2] = [
    ("re", "http://exslt.org/regular-expressions"),
    ("set", "http://exslt.org/sets"),
];

fn into_hits<'a>(value: Value<'a>, scope: Node<'a>) -> Vec<Hit<'a>> {
    match value {
        Value::Nodes(items) => items
            .into_iter()
            .map(|i| match i {
                Item::Node(n) => Hit::Node(Node::from_raw(scope.doc, n)),
                Item::Attr { value, .. } => Hit::Value(value.to_string()),
            })
            .collect(),
        Value::Str(s) => vec![Hit::Value(s.into_owned())],
        Value::Num(n) => vec![Hit::Value(python_float(n))],
        // parsel gives "1" and "0" for boolean results.
        Value::Bool(b) => vec![Hit::Value(if b { "1" } else { "0" }.to_string())],
    }
}

/// A number as Python prints a float, which is what Scrapy users see from
/// lxml: `3.0`, `0.5`, `nan`, `inf`, `1e+16`.
fn python_float(n: f64) -> String {
    if n.is_nan() {
        return "nan".into();
    }
    if n.is_infinite() {
        return if n > 0.0 { "inf" } else { "-inf" }.into();
    }
    if n != 0.0 && (n.abs() >= 1e16 || n.abs() < 1e-4) {
        // Python writes the exponent signed and at least two digits long.
        let s = format!("{n:e}");
        let (mantissa, exponent) = s.split_once('e').unwrap_or((&s, "0"));
        let (sign, digits) = exponent
            .strip_prefix('-')
            .map_or(("+", exponent), |d| ("-", d));
        return format!("{mantissa}e{sign}{digits:0>2}");
    }
    if n.fract() == 0.0 {
        format!("{n:.1}")
    } else {
        format!("{n}")
    }
}

/// The functions parsel adds to XPath 1.0.
struct Parsel;

impl Extensions for Parsel {
    fn scalar(&self, name: &str) -> bool {
        name == "re:test"
    }

    fn call<'a>(&self, name: &str, args: &Args<'_, 'a>) -> Option<Result<Value<'a>, String>> {
        let result = match name {
            "has-class" => has_class(args),
            "re:test" => re_test(args),
            "re:replace" => re_replace(args),
            _ => return None,
        };
        Some(result)
    }
}

/// parsel's extension: the context element has every class given.
fn has_class<'a>(args: &Args<'_, 'a>) -> Result<Value<'a>, String> {
    args.arity(1, usize::MAX)?;
    let classes = match args.context() {
        Item::Node(n) => args.doc().attr(n, "class").unwrap_or(""),
        Item::Attr { .. } => "",
    };
    let have: Vec<&str> = classes.split_ascii_whitespace().collect();
    let mut all = true;
    for i in 0..args.len() {
        all &= have.contains(&args.string(i)?.as_str());
    }
    Ok(Value::Bool(all))
}

fn re_test<'a>(args: &Args<'_, 'a>) -> Result<Value<'a>, String> {
    args.arity(2, 3)?;
    let flags = if args.len() == 3 {
        args.string(2)?
    } else {
        String::new()
    };
    Ok(Value::Bool(
        regex(&args.string(1)?, &flags)?.is_match(&args.string(0)?),
    ))
}

fn re_replace<'a>(args: &Args<'_, 'a>) -> Result<Value<'a>, String> {
    args.arity(4, 4)?;
    let (input, flags, with) = (args.string(0)?, args.string(2)?, args.string(3)?);
    let re = regex(&args.string(1)?, &flags)?;
    let expand = |caps: &regex::Captures<'_>| substitute(caps, &with);
    let out = if flags.contains('g') {
        re.replace_all(&input, expand).into_owned()
    } else {
        re.replace(&input, expand).into_owned()
    };
    Ok(Value::Str(out.into()))
}

/// A replacement written as lxml (and Python's `re`) reads it: `\1` to
/// `\99` and `\g<name>` are groups, `\\` is a backslash, `\n` and `\t`
/// are escapes, and `$` is just a dollar sign.
fn substitute(caps: &regex::Captures<'_>, with: &str) -> String {
    let group = |out: &mut String, m: Option<regex::Match<'_>>| {
        if let Some(m) = m {
            out.push_str(m.as_str());
        }
    };
    let mut out = String::new();
    let mut chars = with.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some(d @ '0'..='9') => {
                let mut n = d.to_digit(10).unwrap() as usize;
                if let Some(e) = chars.peek().and_then(|e| e.to_digit(10)) {
                    n = n * 10 + e as usize;
                    chars.next();
                }
                group(&mut out, caps.get(n));
            }
            Some('g') if chars.peek() == Some(&'<') => {
                chars.next();
                let name: String = chars.by_ref().take_while(|&c| c != '>').collect();
                let m = match name.parse::<usize>() {
                    Ok(n) => caps.get(n),
                    Err(_) => caps.name(&name),
                };
                group(&mut out, m);
            }
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// EXSLT flags: `i` ignores case, `g` (replace only) replaces every match.
fn regex(pattern: &str, flags: &str) -> Result<regex::Regex, String> {
    regex::RegexBuilder::new(pattern)
        .case_insensitive(flags.contains('i'))
        .build()
        // The regex crate's message spans several lines with a caret; its
        // last line says what is wrong.
        .map_err(|e| {
            let text = e.to_string();
            let why = text
                .lines()
                .last()
                .unwrap_or("")
                .trim_start_matches("error: ")
                .to_string();
            format!("bad regular expression {pattern:?}: {why}")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_float_formatting() {
        for (n, s) in [
            (3.0, "3.0"),
            (-2.0, "-2.0"),
            (0.5, "0.5"),
            (12.5, "12.5"),
            (-0.0, "-0.0"),
            (1e16, "1e+16"),
            (1.5e-5, "1.5e-05"),
            (1e100, "1e+100"),
            (20000.0, "20000.0"),
        ] {
            assert_eq!(python_float(n), s, "{n}");
        }
    }
}
