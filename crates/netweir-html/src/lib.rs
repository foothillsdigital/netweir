//! An HTML parser that follows the WHATWG HTML Standard, written for
//! Netweir and for speed.

pub mod css;
mod dom;
mod edit;
mod entities;
mod serialize;
mod tags;
pub mod tokenizer;
mod tree;

pub use dom::{AttrNs, Document, Kind, MAX_INPUT, NodeId, Ns, Quirks};
pub use edit::EditError;
pub use tree::{Options, ParseError, parse, parse_fragment, parse_fragment_with, parse_with};

// A parsed document and a parsed selector can be read from many threads at
// once (writing takes `&mut`, so one at a time). This fails the build if a
// change ever makes either of them not Send or not Sync.
const _: () = {
    const fn shareable<T: Send + Sync>() {}
    shareable::<Document>();
    shareable::<css::SelectorList>();
};

/// The character(s) HTML's named reference `name` stands for, without the
/// `&` and with or without the `;` as the table has it: `nbsp;` gives
/// U+00A0.
pub fn named_reference(name: &str) -> Option<&'static str> {
    entities::ENTITIES
        .binary_search_by(|(n, _)| n.as_bytes().cmp(name.as_bytes()))
        .ok()
        .map(|i| entities::ENTITIES[i].1)
}

/// The number [`Document::known_tag`] gives elements named `name`
/// (lowercase), if the parser knows the tag.
pub fn tag_number(name: &str) -> Option<u8> {
    let tag = tags::Tag::from_name(name);
    (tag != tags::Tag::Other).then_some(tag as u8)
}

/// Whether `name` (in any case) names an element of HTML, current or
/// obsolete, or the root of SVG or MathML: `div`, `marquee`, `svg`; not
/// `rss`.
pub fn is_html_element(name: &str) -> bool {
    const NAMES: &[&str] = &[
        "a",
        "abbr",
        "acronym",
        "address",
        "applet",
        "area",
        "article",
        "aside",
        "audio",
        "b",
        "base",
        "basefont",
        "bdi",
        "bdo",
        "bgsound",
        "big",
        "blink",
        "blockquote",
        "body",
        "br",
        "button",
        "canvas",
        "caption",
        "center",
        "cite",
        "code",
        "col",
        "colgroup",
        "data",
        "datalist",
        "dd",
        "del",
        "details",
        "dfn",
        "dialog",
        "dir",
        "div",
        "dl",
        "dt",
        "em",
        "embed",
        "fieldset",
        "figcaption",
        "figure",
        "font",
        "footer",
        "form",
        "frame",
        "frameset",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "head",
        "header",
        "hgroup",
        "hr",
        "html",
        "i",
        "iframe",
        "image",
        "img",
        "input",
        "ins",
        "isindex",
        "kbd",
        "keygen",
        "label",
        "legend",
        "li",
        "link",
        "listing",
        "main",
        "map",
        "mark",
        "marquee",
        "math",
        "menu",
        "menuitem",
        "meta",
        "meter",
        "multicol",
        "nav",
        "nextid",
        "nobr",
        "noembed",
        "noframes",
        "noscript",
        "object",
        "ol",
        "optgroup",
        "option",
        "output",
        "p",
        "param",
        "picture",
        "plaintext",
        "pre",
        "progress",
        "q",
        "rb",
        "rp",
        "rt",
        "rtc",
        "ruby",
        "s",
        "samp",
        "script",
        "search",
        "section",
        "select",
        "selectedcontent",
        "slot",
        "small",
        "source",
        "spacer",
        "span",
        "strike",
        "strong",
        "style",
        "sub",
        "summary",
        "sup",
        "svg",
        "table",
        "tbody",
        "td",
        "template",
        "textarea",
        "tfoot",
        "th",
        "thead",
        "time",
        "title",
        "tr",
        "track",
        "tt",
        "u",
        "ul",
        "var",
        "video",
        "wbr",
        "xmp",
    ];
    let lower = name.to_ascii_lowercase();
    NAMES.binary_search(&lower.as_str()).is_ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn named_references_and_html_element_names() {
        assert_eq!(super::named_reference("nbsp;"), Some("\u{a0}"));
        assert_eq!(super::named_reference("amp"), Some("&"));
        assert_eq!(super::named_reference("nope;"), None);
        assert!(super::is_html_element("DIV"));
        assert!(super::is_html_element("marquee"));
        assert!(!super::is_html_element("rss"));
    }
}
