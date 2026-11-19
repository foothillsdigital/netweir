//! XPath 1.0 over a netweir-html [`Document`], written from the W3C
//! recommendation: every axis, node test and function of the core
//! library, with the number, string and boolean rules of sections 3 and 4.
//!
//! The tree is read as lxml reads an HTML page, which is what scrapers
//! expect: element and attribute names are the parser's lowercase ones
//! (SVG's `clippath`, `viewbox`) in no namespace, so `//div` and `@href`
//! match, `//DIV` and `@HREF` don't, and a prefixed name (`svg:rect`)
//! matches nothing. An
//! XML document built into the same tree brings its namespaces through
//! [`Namespaces`], and names with a prefix take theirs from the prefixes
//! a query is run with.
//!
//! A query reads the document through an [`Index`], which its owner
//! builds once per version of the tree. Functions beyond the core library
//! come from the caller through [`Extensions`].

mod eval;
mod index;
mod lexer;
mod parser;

use std::borrow::Cow;
use std::fmt;

use netweir_html::{Document, NodeId};

pub use eval::{Args, WORK_LIMIT};
pub use index::{CDATA, COMMENT, DOCTYPE, DOCUMENT, ELEMENT, Index, PI, TEXT};

/// The namespace the `xml` prefix is always bound to.
pub const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XPathError {
    pub query: String,
    pub reason: String,
}

impl fmt::Display for XPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid XPath {:?}: {}", self.query, self.reason)
    }
}

impl std::error::Error for XPathError {}

/// A value a caller can bind to a `$variable`.
#[derive(Debug, Clone, PartialEq)]
pub enum XValue {
    Str(String),
    Num(f64),
    Bool(bool),
}

/// The namespaces of an XML document built into a netweir-html tree,
/// which the tree leaves to its host. Ids are the host's; 0 is no
/// namespace.
pub trait Namespaces: Sync {
    /// The namespace id of an element.
    fn ns_of(&self, element: NodeId) -> u32;
    /// The URI with id `id`; "" for 0.
    fn uri(&self, id: u32) -> &str;
    /// The id of `uri` in this document, None if nothing in it has that
    /// namespace. "" is 0.
    fn id_of(&self, uri: &str) -> Option<u32>;
}

/// The document a query runs over: its tree, the tree's index, and for
/// an XML document its namespaces.
#[derive(Clone, Copy)]
pub struct Doc<'a> {
    pub(crate) tree: &'a Document,
    pub(crate) index: &'a Index,
    pub(crate) names: Option<&'a dyn Namespaces>,
}

impl<'a> Doc<'a> {
    /// An HTML document. `index` must be of `tree` as it is now.
    pub fn html(tree: &'a Document, index: &'a Index) -> Doc<'a> {
        Doc {
            tree,
            index,
            names: None,
        }
    }

    /// An XML document, whose elements are [`netweir_html::Ns::Xml`] and
    /// whose attributes' namespaces are [`netweir_html::AttrNs::Host`] ids.
    pub fn xml(tree: &'a Document, index: &'a Index, names: &'a dyn Namespaces) -> Doc<'a> {
        Doc {
            tree,
            index,
            names: Some(names),
        }
    }

    pub fn tree(&self) -> &'a Document {
        self.tree
    }

    pub fn is_xml(&self) -> bool {
        self.names.is_some()
    }

    /// An attribute's value by name: exact in XML, and in HTML compared
    /// ignoring ASCII case, as HTML names are lowercase.
    pub fn attr(&self, node: NodeId, name: &str) -> Option<&'a str> {
        let mut attrs = self.tree.attrs(node);
        if self.is_xml() {
            return attrs.find(|(_, n, _)| *n == name).map(|(_, _, v)| v);
        }
        attrs
            .find(|(_, n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, _, v)| v)
    }
}

/// A node in XPath's data model: a tree node, or an attribute (which the
/// tree keeps on its element rather than as a node of its own).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Item<'a> {
    Node(NodeId),
    Attr {
        owner: NodeId,
        /// Its place among the element's attributes.
        index: u32,
        /// As written, with any prefix.
        name: &'a str,
        /// The namespace id (see [`Namespaces`]); 0 for none.
        ns: u32,
        value: &'a str,
    },
}

/// What an expression evaluates to. A node-set is in document order,
/// without duplicates.
#[derive(Debug, Clone)]
pub enum Value<'a> {
    Nodes(Vec<Item<'a>>),
    Str(Cow<'a, str>),
    Num(f64),
    Bool(bool),
}

/// Functions beyond XPath 1.0's core library, which a caller provides. A
/// core function of the same name wins.
pub trait Extensions: Sync {
    /// Works out `name()`, or None if this has no function of that name.
    fn call<'a>(&self, name: &str, args: &Args<'_, 'a>) -> Option<Result<Value<'a>, String>>;

    /// Whether `name()` only ever gives a number or a yes or no, so a
    /// predicate's call of it can be kept per node (see `memoize`).
    fn scalar(&self, _name: &str) -> bool {
        false
    }
}

/// No functions beyond the core library.
pub struct NoExtensions;

impl Extensions for NoExtensions {
    fn call<'a>(&self, _: &str, _: &Args<'_, 'a>) -> Option<Result<Value<'a>, String>> {
        None
    }
}

/// A compiled XPath expression. It holds no state of any document, so one
/// can be shared by many threads.
#[derive(Debug, Clone)]
pub struct XPath {
    src: String,
    expr: parser::Expr,
    /// The `$variables` the expression uses, found once here rather than
    /// on every run.
    variables: Vec<String>,
}

impl XPath {
    pub fn new(src: &str) -> Result<XPath, XPathError> {
        XPath::with_extensions(src, &NoExtensions)
    }

    /// [`XPath::new`], for running with `extensions`.
    pub fn with_extensions(src: &str, extensions: &dyn Extensions) -> Result<XPath, XPathError> {
        let tokens = lexer::tokenize(src)?;
        let mut expr = parser::parse(src, tokens)?;
        parser::memoize(&mut expr, &|name| extensions.scalar(name));
        let mut variables = Vec::new();
        expr.variables(&mut variables);
        Ok(XPath {
            src: src.to_string(),
            expr,
            variables,
        })
    }

    /// The expression as written.
    pub fn source(&self) -> &str {
        &self.src
    }

    /// Evaluates with `scope` as the context node, `vars` giving the
    /// `$variables` and `namespaces` binding prefixes to URIs as
    /// `(prefix, uri)` (a later pair for a prefix wins). `extensions`
    /// should be what the expression was compiled with.
    pub fn evaluate<'a>(
        &self,
        doc: Doc<'a>,
        scope: NodeId,
        vars: &[(String, XValue)],
        namespaces: &[(String, String)],
        extensions: &dyn Extensions,
    ) -> Result<Value<'a>, XPathError> {
        self.evaluate_within(doc, scope, vars, namespaces, extensions, WORK_LIMIT)
    }

    /// [`XPath::evaluate`], giving up after `limit` units of work rather
    /// than [`WORK_LIMIT`].
    pub fn evaluate_within<'a>(
        &self,
        doc: Doc<'a>,
        scope: NodeId,
        vars: &[(String, XValue)],
        namespaces: &[(String, String)],
        extensions: &dyn Extensions,
        limit: u64,
    ) -> Result<Value<'a>, XPathError> {
        if let Some(missing) = self
            .variables
            .iter()
            .find(|v| !vars.iter().any(|(k, _)| k == *v))
        {
            return Err(XPathError {
                query: self.src.clone(),
                reason: format!("no value given for ${missing}"),
            });
        }
        eval::run(&self.expr, doc, scope, vars, namespaces, extensions, limit).map_err(|reason| {
            XPathError {
                query: self.src.clone(),
                reason,
            }
        })
    }
}

// A compiled expression is shared between threads; this fails the build
// if a change ever stops that.
const _: () = {
    const fn shareable<T: Send + Sync>() {}
    shareable::<XPath>();
    shareable::<Index>();
};
