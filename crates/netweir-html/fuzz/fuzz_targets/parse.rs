//! Any input parses without panicking, as a document and as a fragment,
//! into a tree whose links agree with each other; and fed in pieces, into
//! the same tree.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netweir_html::{
    Document, NodeId, Ns, Options, Parser, SerializeOptions, parse, parse_fragment_with,
};

const CONTEXTS: &[(Ns, &str)] = &[
    (Ns::Html, "body"),
    (Ns::Html, "div"),
    (Ns::Html, "table"),
    (Ns::Html, "tr"),
    (Ns::Html, "select"),
    (Ns::Html, "template"),
    (Ns::Html, "textarea"),
    (Ns::Html, "script"),
    (Ns::Html, "html"),
    (Ns::Svg, "svg"),
    (Ns::Svg, "foreignObject"),
    (Ns::MathMl, "math"),
    (Ns::MathMl, "annotation-xml"),
];

/// Every node reached from the root once (shadow roots and template
/// contents too), each child's parent being the node it was reached
/// from.
fn check(doc: &Document) {
    // cargo fuzz builds with debug assertions: every invariant, then.
    #[cfg(debug_assertions)]
    doc.check_tree();
    let mut seen = vec![false; doc.len()];
    let mut stack: Vec<NodeId> = vec![doc.root()];
    while let Some(node) = stack.pop() {
        assert!(!seen[node as usize], "node {node} reached twice");
        seen[node as usize] = true;
        for child in doc.children(node) {
            assert_eq!(doc.parent(child), Some(node));
            stack.push(child);
        }
        if let Some(root) = doc.shadow_root(node) {
            stack.push(root);
        }
        if let Some(contents) = doc.template_contents(node) {
            stack.push(contents);
        }
        let _ = doc.kind(node);
        let _ = doc.attrs(node).count();
    }
}

/// The whole tree, shadow roots and template contents included.
fn dump(doc: &Document) -> String {
    let options = SerializeOptions {
        html5test: true,
        ..SerializeOptions::default()
    };
    doc.serialize_with(doc.root(), &options)
}

fuzz_target!(|data: &[u8]| {
    let Some((&first, rest)) = data.split_first() else {
        return;
    };
    let html = String::from_utf8_lossy(rest);
    let scripting = first & 1 == 1;
    let whole = parse(&html, scripting);
    check(&whole);
    // In pieces of a size the first byte picks, each ending on a
    // character boundary.
    let size = 1 + (first as usize >> 4);
    let mut parser = Parser::new(Options {
        scripting,
        ..Options::default()
    });
    let mut at = 0;
    while at < html.len() {
        let mut end = (at + size).min(html.len());
        while !html.is_char_boundary(end) {
            end += 1;
        }
        parser.feed(&html[at..end]);
        at = end;
    }
    let pieces = parser.finish().unwrap();
    assert_eq!(
        dump(&pieces),
        dump(&whole)
    );
    let (ns, name) = CONTEXTS[(first as usize >> 1) % CONTEXTS.len()];
    // As innerHTML, or as setHTMLUnsafe with declarative shadow roots.
    let options = Options {
        scripting,
        declarative_shadow_roots: Some(first & 2 != 0),
        ..Options::default()
    };
    check(&parse_fragment_with(&html, ns, name, options).unwrap());
});
