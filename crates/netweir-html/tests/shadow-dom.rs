//! The declarative shadow DOM tests from web-platform-tests'
//! shadow-dom/declarative, the ones a parser without script can answer:
//! the trees in tests/shadow-dom/declarative.dat, which carry the markup
//! the JavaScript tests parse and the tree it should make, and the
//! attachment, getHTML and ordering tests, generated here from the same
//! lists of elements and options the JavaScript loops over.

use netweir_html::css::SelectorList;
use netweir_html::{
    Document, Kind, NodeId, Ns, Options, SerializeOptions, ShadowRoot, ShadowRootMode,
    parse_fragment_with, parse_with,
};

mod summary;

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/shadow-dom");

/// html/resources/common.js's HTML5_ELEMENTS.
const HTML5_ELEMENTS: &[&str] = &[
    "a",
    "abbr",
    "address",
    "area",
    "article",
    "aside",
    "audio",
    "b",
    "base",
    "bdi",
    "bdo",
    "blockquote",
    "body",
    "br",
    "button",
    "canvas",
    "caption",
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
    "div",
    "dl",
    "dt",
    "em",
    "embed",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "html",
    "i",
    "iframe",
    "img",
    "input",
    "ins",
    "kbd",
    "label",
    "legend",
    "li",
    "link",
    "main",
    "map",
    "mark",
    "menu",
    "meta",
    "meter",
    "nav",
    "noscript",
    "object",
    "ol",
    "optgroup",
    "option",
    "output",
    "p",
    "param",
    "pre",
    "progress",
    "q",
    "rp",
    "rt",
    "ruby",
    "s",
    "samp",
    "script",
    "search",
    "section",
    "select",
    "slot",
    "small",
    "source",
    "span",
    "strong",
    "style",
    "sub",
    "sup",
    "summary",
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
    "u",
    "ul",
    "var",
    "video",
    "wbr",
];

/// HTML5_SHADOW_ALLOWED_ELEMENTS.
const SHADOW_ALLOWED: &[&str] = &[
    "article",
    "aside",
    "blockquote",
    "body",
    "div",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "main",
    "nav",
    "p",
    "section",
    "span",
];

/// `wrapper.setHTMLUnsafe(html)`: a fragment in a div, shadow roots on.
fn set_html_unsafe(html: &str) -> Document {
    let options = Options {
        declarative_shadow_roots: Some(true),
        ..Options::default()
    };
    parse_fragment_with(html, Ns::Html, "div", options).unwrap()
}

/// Every node: the tree, template contents and shadow roots.
fn all_nodes(doc: &Document) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut stack = vec![doc.root()];
    while let Some(n) = stack.pop() {
        out.push(n);
        stack.extend(doc.children(n));
        stack.extend(doc.template_contents(n));
        stack.extend(doc.shadow_root(n));
    }
    out
}

fn first(doc: &Document, css: &str) -> Option<NodeId> {
    let list = SelectorList::parse(css).unwrap();
    all_nodes(doc)
        .into_iter()
        .filter(|&n| is_element(doc, n))
        .find(|&n| list.matches(doc, n, None))
}

fn is_element(doc: &Document, n: NodeId) -> bool {
    matches!(doc.kind(n), Kind::Element { .. })
}

fn dump(doc: &Document) -> String {
    #[cfg(debug_assertions)]
    doc.check_tree();
    doc.serialize_with(
        doc.root(),
        &SerializeOptions {
            html5test: true,
            ..SerializeOptions::default()
        },
    )
}

// ---- the trees ---------------------------------------------------------------

struct Case {
    description: String,
    data: String,
    fragment: Option<String>,
    shadow_roots: Option<bool>,
    document: String,
}

fn cases() -> Vec<Case> {
    let text = std::fs::read_to_string(format!("{DIR}/declarative.dat")).unwrap();
    let mut out = Vec::new();
    for test in text.split("\n\n#data\n") {
        let test = test.strip_prefix("#data\n").unwrap_or(test);
        let mut fields: Vec<(&str, Vec<&str>)> = vec![("data", Vec::new())];
        for line in test.lines() {
            match line {
                "#description" | "#document-fragment" | "#shadow-roots" | "#document" => {
                    fields.push((&line[1..], Vec::new()))
                }
                _ => fields.last_mut().unwrap().1.push(line),
            }
        }
        let get = |name: &str| {
            fields
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.join("\n"))
        };
        out.push(Case {
            description: get("description").unwrap_or_default(),
            data: get("data").unwrap(),
            fragment: get("document-fragment"),
            shadow_roots: get("shadow-roots").map(|s| s == "on"),
            document: get("document").unwrap().trim_end().to_string(),
        });
    }
    out
}

fn tree_cases(failed: &mut Vec<String>) -> usize {
    let cases = cases();
    for case in &cases {
        let options = Options {
            declarative_shadow_roots: case.shadow_roots,
            ..Options::default()
        };
        let doc = match &case.fragment {
            Some(context) => parse_fragment_with(&case.data, Ns::Html, context, options),
            None => parse_with(&case.data, options),
        }
        .unwrap();
        let got = dump(&doc);
        if got != case.document {
            failed.push(format!(
                "{}\n--- input\n{}\n--- expected\n{}\n--- got\n{got}\n",
                case.description, case.data, case.document
            ));
        }
    }
    cases.len()
}

// ---- declarative-shadow-dom-attachment.html --------------------------------

/// Each element, with each mode and delegatesFocus: a safelisted one
/// becomes the host of a shadow root holding the template's contents, and
/// keeps one light child; any other keeps the template.
fn attachment_cases(failed: &mut Vec<String>) -> usize {
    const SHADOW: &str = "<span>Shadow tree</span><slot></slot>";
    // Elements the markup can't put children in.
    const NO_CHILDREN: &[&str] = &[
        "iframe", "noscript", "script", "select", "style", "textarea", "title",
    ];
    let mut count = 0;
    for delegates in [false, true] {
        for mode in ["open", "closed", "invalid"] {
            for &name in HTML5_ELEMENTS {
                if name == "body" || name == "template" {
                    continue;
                }
                count += 1;
                let focus = if delegates {
                    " shadowrootdelegatesfocus"
                } else {
                    ""
                };
                let html = format!(
                    "<{name} id=theelement><template shadowrootmode={mode}{focus}>{SHADOW}</template>\n     \
                     <span class='lightdom'>Light DOM</span></{name}>"
                );
                let doc = set_html_unsafe(&html);
                #[cfg(debug_assertions)]
                doc.check_tree();
                let allowed = SHADOW_ALLOWED.contains(&name) && mode != "invalid";
                let element = first(&doc, "#theelement");
                let ok = if allowed {
                    element.is_some_and(|el| {
                        let kids: Vec<_> =
                            doc.children(el).filter(|&c| is_element(&doc, c)).collect();
                        let root = doc.shadow_root(el);
                        let init = root.map(|r| doc.kind(r));
                        first(&doc, "template").is_none()
                            && kids.len() == 1
                            && doc.text(kids[0]) == "Light DOM"
                            && root.is_some_and(|r| doc.inner_html(r) == SHADOW)
                            && matches!(init, Some(Kind::ShadowRoot(i))
                                if i.delegates_focus == delegates
                                    && (i.mode == ShadowRootMode::Open) == (mode == "open"))
                    })
                } else if NO_CHILDREN.contains(&name) {
                    true
                } else {
                    let template = first(&doc, "template[shadowrootmode]");
                    let attr = template.and_then(|t| {
                        doc.attrs(t)
                            .find(|a| a.1 == "shadowrootmode")
                            .map(|a| a.2.to_string())
                    });
                    attr.as_deref() == Some(mode)
                        && first(&doc, "span.lightdom").is_some_and(|s| doc.text(s) == "Light DOM")
                        && element.is_none_or(|e| doc.shadow_root(e).is_none())
                };
                if !ok {
                    failed.push(format!(
                        "attachment: <{name}> mode={mode} delegatesFocus={delegates}\n{}\n",
                        dump(&doc)
                    ));
                }
            }
        }
    }
    count
}

// ---- gethtml.html ----------------------------------------------------------

/// `getHTML()` with each set of options, on a div or on a shadow root,
/// around each element, with a shadow root made by markup or by
/// `attachShadow`.
fn get_html_cases(failed: &mut Vec<String>) -> usize {
    let mut count = 0;
    let safelisted: Vec<&str> = SHADOW_ALLOWED
        .iter()
        .copied()
        .filter(|&e| e != "body")
        .collect();
    let all: Vec<&str> = HTML5_ELEMENTS
        .iter()
        .copied()
        .chain(["htmlunknown"])
        .collect();
    let get_html = |doc: &Document, n: NodeId, serializable: bool, roots: Vec<NodeId>| {
        doc.inner_html_with(
            n,
            &SerializeOptions {
                serializable_shadow_roots: serializable,
                shadow_roots: roots,
                ..SerializeOptions::default()
            },
        )
    };
    for &name in &all {
        let allows = safelisted.contains(&name);
        for on_shadow_root in [false, true] {
            for light in ["", "<span>light</span>"] {
                let mut variants = Vec::new();
                if allows {
                    for declarative in [false, true] {
                        for delegates in [false, true] {
                            for clonable in [false, true] {
                                for mode in ["open", "closed"] {
                                    for serializable in [None, Some(false), Some(true)] {
                                        for empty in [false, true] {
                                            variants.push(Some((
                                                declarative,
                                                delegates,
                                                clonable,
                                                mode,
                                                serializable,
                                                empty,
                                            )));
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else {
                    variants.push(None);
                }
                for variant in variants {
                    count += 1;
                    let mut doc = set_html_unsafe("");
                    // The wrapper: a div, or a div's open shadow root.
                    let host = doc.create_element(Ns::Html, "div");
                    doc.append(doc.root(), host).unwrap();
                    let wrapper = if on_shadow_root {
                        doc.attach_shadow(host, ShadowRoot::open()).unwrap()
                    } else {
                        host
                    };
                    let shadow: Option<NodeId>;
                    let mut expected = None;
                    if let Some((declarative, delegates, clonable, mode, serializable, empty)) =
                        variant
                    {
                        let init = ShadowRoot {
                            mode: if mode == "open" {
                                ShadowRootMode::Open
                            } else {
                                ShadowRootMode::Closed
                            },
                            delegates_focus: delegates,
                            clonable,
                            serializable: serializable == Some(true),
                            ..ShadowRoot::open()
                        };
                        let delegates_attr = if delegates {
                            " shadowrootdelegatesfocus=\"\""
                        } else {
                            ""
                        };
                        let serializable_attr = if serializable == Some(true) {
                            " shadowrootserializable=\"\""
                        } else {
                            ""
                        };
                        let clonable_attr = if clonable {
                            " shadowrootclonable=\"\""
                        } else {
                            ""
                        };
                        let element = if declarative {
                            let html = format!(
                                "<{name}>{light}<template shadowrootmode={mode}{delegates_attr}\
                                 {serializable_attr}{clonable_attr}>"
                            );
                            let parsed = set_html_unsafe(&html);
                            // Moved over by markup, as setHTMLUnsafe would.
                            let made = copy_in(&mut doc, &parsed, wrapper);
                            // attachShadow gives the declared root back, emptied.
                            let el = made[0];
                            let root = doc.attach_shadow(el, init);
                            shadow = root.ok();
                            el
                        } else {
                            let el = doc.create_element(Ns::Html, name);
                            doc.append(wrapper, el).unwrap();
                            let parsed = set_html_unsafe(light);
                            copy_in(&mut doc, &parsed, el);
                            shadow = doc.attach_shadow(el, init).ok();
                            el
                        };
                        let Some(root) = shadow else {
                            failed.push(format!("getHTML: <{name}> got no shadow root"));
                            continue;
                        };
                        if !empty {
                            let slot = doc.create_element(Ns::Html, "slot");
                            doc.append(root, slot).unwrap();
                        }
                        let correct_shadow = format!(
                            "<template shadowrootmode=\"{mode}\"{delegates_attr}{serializable_attr}\
                             {clonable_attr}>{}</template>",
                            if empty { "" } else { "<slot></slot>" }
                        );
                        let correct = format!("<{name}>{correct_shadow}{light}</{name}>");
                        let plain = format!("<{name}>{light}</{name}>");
                        let mut checks = vec![
                            (
                                get_html(&doc, wrapper, true, vec![]),
                                if serializable == Some(true) {
                                    correct.clone()
                                } else {
                                    plain
                                },
                            ),
                            (get_html(&doc, wrapper, true, vec![root]), correct.clone()),
                            (get_html(&doc, wrapper, false, vec![root]), correct.clone()),
                        ];
                        if serializable == Some(true) {
                            checks.push((
                                get_html(&doc, element, true, vec![]),
                                format!("{correct_shadow}{light}"),
                            ));
                        }
                        expected = Some(checks);
                    } else {
                        let el = doc.create_element(Ns::Html, name);
                        doc.append(wrapper, el).unwrap();
                        let parsed = set_html_unsafe(light);
                        copy_in(&mut doc, &parsed, el);
                    }
                    let inner = doc.inner_html(wrapper);
                    let mut checks = expected.unwrap_or_else(|| {
                        vec![(get_html(&doc, wrapper, true, vec![]), inner.clone())]
                    });
                    checks.push((get_html(&doc, wrapper, false, vec![]), inner.clone()));
                    #[cfg(debug_assertions)]
                    doc.check_tree();
                    for (got, want) in checks {
                        if got != want {
                            failed.push(format!(
                                "getHTML: <{name}> {variant:?} on a shadow root {on_shadow_root}\n\
                                 --- expected\n{want}\n--- got\n{got}\n"
                            ));
                            break;
                        }
                    }
                }
            }
        }
    }
    count
}

/// Copies `from`'s children, with their attributes and text, under
/// `parent`, and any shadow roots their elements host, as an import
/// would; returns the copies of the top ones.
fn copy_in(doc: &mut Document, from: &Document, parent: NodeId) -> Vec<NodeId> {
    let mut made = Vec::new();
    let mut stack: Vec<(NodeId, NodeId)> = from
        .children(from.root())
        .map(|c| (c, parent))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    while let Some((n, into)) = stack.pop() {
        let copy = match from.kind(n) {
            Kind::Element { ns, name } => {
                let el = doc.create_element(ns, name);
                for (ns, name, value) in from.attrs_written(n) {
                    doc.add_attr(el, ns, name, value);
                }
                el
            }
            Kind::Text(t) => doc.create_text(t),
            Kind::Comment(c) => doc.create_comment(c),
            _ => continue,
        };
        if into == parent {
            made.push(copy);
        }
        doc.append(into, copy).unwrap();
        if let Some(root) = from.shadow_root(n)
            && let Kind::ShadowRoot(init) = from.kind(root)
        {
            let to = doc
                .attach_shadow(
                    copy,
                    ShadowRoot {
                        declarative: true,
                        ..init
                    },
                )
                .unwrap();
            for c in from.children(root).collect::<Vec<_>>().into_iter().rev() {
                stack.push((c, to));
            }
        }
        for c in from.children(n).collect::<Vec<_>>().into_iter().rev() {
            stack.push((c, copy));
        }
    }
    made
}

// ---- declarative-shadow-dom-slot-assignment-serialization.html and
// gethtml-ordering.html ----------------------------------------------------

fn serialization_cases(failed: &mut Vec<String>) -> usize {
    let cases: &[(&str, &str)] = &[
        (
            r#"<div id="host"><template shadowrootmode="open" shadowrootserializable shadowrootslotassignment="manual"></template></div>"#,
            r#"<template shadowrootmode="open" shadowrootserializable="" shadowrootslotassignment="manual"></template>"#,
        ),
        (
            r#"<div id="host"><template shadowrootmode="open" shadowrootserializable shadowrootslotassignment="named"></template></div>"#,
            r#"<template shadowrootmode="open" shadowrootserializable=""></template>"#,
        ),
        (
            r#"<div id="host"><template shadowrootmode="open" shadowrootdelegatesfocus shadowrootserializable shadowrootclonable shadowrootslotassignment="manual"></template></div>"#,
            r#"<template shadowrootmode="open" shadowrootdelegatesfocus="" shadowrootserializable="" shadowrootslotassignment="manual" shadowrootclonable=""></template>"#,
        ),
    ];
    for (html, want) in cases {
        let doc = set_html_unsafe(html);
        let host = first(&doc, "#host").unwrap();
        let got = doc.inner_html_with(
            host,
            &SerializeOptions {
                serializable_shadow_roots: true,
                ..SerializeOptions::default()
            },
        );
        if got != *want {
            failed.push(format!("{html}\n--- expected\n{want}\n--- got\n{got}\n"));
        }
    }
    // gethtml-ordering.html: where the template and its attributes are
    // written doesn't change the serialization.
    let ordering = std::fs::read_to_string(format!("{DIR}/gethtml-ordering.html")).unwrap();
    let mut doc = parse_with(&ordering, Options::default()).unwrap();
    // removeWhitespaceNodes.
    for n in all_nodes(&doc) {
        if matches!(doc.kind(n), Kind::Text(t) if t.trim().is_empty()) {
            doc.remove(n);
        }
    }
    let hosts: Vec<NodeId> = SelectorList::parse("#tests > div")
        .unwrap()
        .select(&doc, doc.root());
    let serialize = |host: NodeId| {
        let root = doc.shadow_root(host).unwrap();
        let options = SerializeOptions {
            shadow_roots: vec![root],
            ..SerializeOptions::default()
        };
        doc.inner_html_with(host, &options)
    };
    let correct = serialize(hosts[0]);
    for &host in &hosts[1..] {
        let got = serialize(host);
        if got != correct {
            failed.push(format!(
                "gethtml-ordering\n--- expected\n{correct}\n--- got\n{got}\n"
            ));
        }
    }
    cases.len() + hosts.len() - 1
}

#[test]
fn wpt_declarative_shadow_dom() {
    let mut failed = Vec::new();
    let mut passed = 0;
    for run in [
        tree_cases,
        attachment_cases,
        get_html_cases,
        serialization_cases,
    ] {
        let before = failed.len();
        let ran = run(&mut failed);
        passed += ran - (failed.len() - before);
    }
    eprintln!(
        "declarative shadow DOM: {passed} passed, {} failed",
        failed.len()
    );
    summary::write(&summary::Counts {
        suite: "wpt-declarative-shadow-dom",
        passed,
        failed: failed.len(),
        skipped: 0,
        skipped_because: "",
        data: std::path::Path::new(DIR),
    });
    assert!(
        failed.is_empty(),
        "{} failures, first 10:\n{}",
        failed.len(),
        failed[..failed.len().min(30)].join("\n")
    );
}
