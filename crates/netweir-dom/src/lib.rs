//! HTML and XML parsing, CSS and XPath queries, and declarative extraction
//! for netweir.
//!
//! HTML is parsed by netweir-html, Netweir's own parser for the HTML
//! Standard. XML is read with quick-xml and built into the same kind of
//! tree (see [`xml`]). [`Node`] is a borrowed handle into the tree.

pub mod auto;
mod cache;
pub mod chunk;
pub mod content;
pub mod css_path;
pub mod date;
mod document;
pub mod extract;
pub mod gtin;
pub mod js;
pub mod markdown;
pub mod metadata;
pub mod price;
mod query;
pub mod records;
pub mod similar;
pub mod track;
pub mod xml;
mod xpath;

/// The HTML parser's name and version, as `netweir version` reports it.
pub const PARSER: &str = concat!("netweir-html ", env!("CARGO_PKG_VERSION"));

pub use cache::{QUERY_CACHE_SIZE, QUERY_CACHE_TEXT};
pub use document::{Content, Document, EditError, Node, NodeId, NodeKind, ParseTimeout, Reading};
pub use query::{Hit, OutOfMemory, Output, Query, QueryError};
pub use xml::{ParseXmlError, XmlError, XmlNodes};
pub use xpath::{XPath, XPathError, XValue};
