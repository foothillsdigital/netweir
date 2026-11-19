//! The selector tests from web-platform-tests that a static document can
//! answer: the dom/nodes querySelector, matches and closest suites, and the
//! css/selectors cases scripts/wpt-selectors-cases.js could lift out of
//! their JavaScript. The data is in tests/wpt-selectors/cases.json.

use std::path::Path;

use netweir_html::css::{MatchOptions, SelectorList};
use netweir_html::{AttrNs, Document, Kind, NodeId, Ns, parse, parse_fragment};
use serde_json::Value;

mod summary;

const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/wpt-selectors");
/// What the iframe in ParentNode-querySelector-All.html loads; `:target`
/// needs the fragment.
const CONTENT_URL: &str =
    "http://web-platform.test:8000/dom/nodes/ParentNode-querySelector-All-content.html#target";

const TEST_QSA: u64 = 0x01;
const TEST_MATCH: u64 = 0x10;

struct Outcome {
    name: String,
    failure: Option<String>,
}

impl Outcome {
    fn check(name: String, ok: bool, detail: impl FnOnce() -> String) -> Outcome {
        Outcome {
            name,
            failure: (!ok).then(detail),
        }
    }
}

/// Why a case can't run here, going by the selector in its name: what no
/// spec defines, and what a parsed document can't hold.
fn skip_reason(name: &str) -> Option<&'static str> {
    let selector = name.rsplit(" | ").next().unwrap_or(name);
    if selector.contains("-*") {
        Some("class prefix selectors (.foo-*) are a tentative proposal in no spec")
    } else if selector.contains(":has-slotted") {
        Some(":has-slotted is a tentative proposal, with a grammar no spec defines yet")
    } else if selector.contains(":state(") {
        Some(":state() needs custom element states, which live in JavaScript")
    } else if selector.contains("#no-namespace |") {
        Some("an element in no namespace can't come from parsed HTML")
    } else {
        None
    }
}

// ---- documents -------------------------------------------------------------

fn elements(doc: &Document, under: NodeId) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut stack: Vec<NodeId> = doc.children(under).collect();
    stack.reverse();
    while let Some(n) = stack.pop() {
        if matches!(doc.kind(n), Kind::Element { .. }) {
            out.push(n);
        }
        let mut kids: Vec<NodeId> = doc.children(n).collect();
        kids.reverse();
        stack.extend(kids);
    }
    out
}

fn id_of(doc: &Document, n: NodeId) -> Option<&str> {
    doc.attrs(n)
        .find(|(ns, name, _)| *ns == AttrNs::None && *name == "id")
        .map(|(_, _, v)| v)
}

fn by_id(doc: &Document, id: &str) -> Option<NodeId> {
    by_id_under(doc, doc.root(), id)
}

/// `#id` among `under` and its descendants: a detached element isn't under
/// the document.
fn by_id_under(doc: &Document, under: NodeId, id: &str) -> Option<NodeId> {
    std::iter::once(under)
        .chain(elements(doc, under))
        .find(|&n| id_of(doc, n) == Some(id))
}

fn by_tag(doc: &Document, tag: &str) -> Option<NodeId> {
    elements(doc, doc.root())
        .into_iter()
        .find(|&n| doc.lower_name(n) == Some(tag))
}

/// The element a case's `root` names: `document`, `html`, `body` or `#id`.
/// The document root stands for the document.
fn resolve(doc: &Document, root: &str) -> Option<NodeId> {
    match root {
        "document" => Some(doc.root()),
        "html" | "body" => by_tag(doc, root),
        _ => by_id(doc, root.strip_prefix('#')?),
    }
}

fn ids(doc: &Document, nodes: &[NodeId]) -> Vec<String> {
    nodes
        .iter()
        .map(|&n| id_of(doc, n).unwrap_or("").to_string())
        .collect()
}

/// What `closest()` gives: the element or its nearest ancestor matching,
/// with the element as `:scope`.
fn closest(list: &SelectorList, doc: &Document, el: NodeId) -> Option<NodeId> {
    let mut at = Some(el);
    while let Some(n) = at {
        if matches!(doc.kind(n), Kind::Element { .. }) && list.matches(doc, n, Some(el)) {
            return Some(n);
        }
        at = doc.parent(n);
    }
    None
}

/// A selector on one line, so a case's name fits known-failures.txt.
fn label(selector: &str) -> String {
    selector
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn str_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|s| s.as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default()
}

// ---- dom/nodes: ParentNode-querySelector-All, Element-matches ----------------

/// ParentNode-querySelector-All.js's setupSpecialElements: the elements the
/// markup can't express. The element with no namespace is in the host's
/// namespace too, which this DOM has no way to tell apart.
fn setup_special_elements(doc: &mut Document, root: NodeId) {
    for name in ["null", "undefined"] {
        let el = doc.create_element(Ns::Html, name);
        doc.append(root, el).unwrap();
    }
    for group in ["any-namespace", "no-namespace"] {
        let holder = doc.create_element(Ns::Html, "div");
        doc.set_attr(holder, "id", group);
        for (i, ns) in [Ns::Html, Ns::Html, Ns::Xml, Ns::Xml]
            .into_iter()
            .enumerate()
        {
            let div = doc.create_element(ns, "div");
            doc.set_attr(div, "id", &format!("{group}-div{}", i + 1));
            doc.append(holder, div).unwrap();
        }
        doc.append(root, holder).unwrap();
    }
    let span = by_id(doc, "attr-presence-i1").unwrap();
    doc.add_attr(span, AttrNs::Host(1), "title", "");
}

struct Context {
    /// WPT's name: `document`, `element`, `detached` or `fragment`.
    name: &'static str,
    doc: Document,
    /// Where querySelectorAll runs.
    scope: NodeId,
    url: Option<&'static str>,
}

fn contexts() -> Vec<Context> {
    let html = std::fs::read_to_string(
        Path::new(DIR).join("dom/nodes/ParentNode-querySelector-All-content.html"),
    )
    .unwrap();
    let content = || {
        let mut doc = parse(&html, false);
        let root = by_id(&doc, "root").unwrap();
        setup_special_elements(&mut doc, root);
        (doc, root)
    };
    let (doc, root) = content();
    let (in_document, in_document_root) = content();
    // The clones: the fragment holds one, and the detached element is one
    // taken out of its own document.
    let fragment = parse_fragment(&doc.outer_html(root), Ns::Html, "body", false);
    let (mut detached_doc, detached) = content();
    detached_doc.detach(detached);

    vec![
        Context {
            name: "document",
            scope: doc.root(),
            doc,
            url: Some(CONTENT_URL),
        },
        Context {
            name: "element",
            scope: in_document_root,
            doc: in_document,
            url: Some(CONTENT_URL),
        },
        Context {
            name: "detached",
            scope: detached,
            doc: detached_doc,
            url: None,
        },
        Context {
            name: "fragment",
            scope: fragment.root(),
            doc: fragment,
            url: None,
        },
    ]
}

fn excluded(case: &Value, context: &str) -> bool {
    str_list(&case["exclude"])
        .iter()
        .any(|e| e == context || e == "html")
}

fn dom_nodes(data: &Value, out: &mut Vec<Outcome>) {
    let contexts = contexts();

    for case in data["invalid"].as_array().unwrap() {
        let selector = case["selector"].as_str().unwrap();
        let shown = label(selector);
        out.push(Outcome::check(
            format!("dom/nodes/ParentNode-querySelector-All.html | invalid | {shown}"),
            SelectorList::parse(selector).is_err(),
            || "parsed, but should be a syntax error".to_string(),
        ));
    }

    for case in data["valid"].as_array().unwrap() {
        let selector = case["selector"].as_str().unwrap();
        let shown = label(selector);
        let flags = case["testType"].as_u64().unwrap();
        let expect = str_list(&case["expect"]);
        if flags & TEST_QSA == 0 {
            continue;
        }
        for cx in &contexts {
            if excluded(case, cx.name) {
                continue;
            }
            let name = format!(
                "dom/nodes/ParentNode-querySelector-All.html | {} | {shown}",
                cx.name
            );
            let options = MatchOptions {
                url: cx.url,
                ..MatchOptions::default()
            };
            // querySelector is the same walk stopped at its first match, so
            // querySelectorAll's result covers both.
            let got = SelectorList::parse(selector)
                .map(|list| ids(&cx.doc, &list.select_with(&cx.doc, cx.scope, &options)))
                .map_err(|e| format!("did not parse: {e}"));
            out.push(Outcome::check(
                name,
                got.as_ref() == Ok(&expect),
                || match got {
                    Ok(all) => format!("expected {expect:?}\n     got {all:?}"),
                    Err(e) => e,
                },
            ));
        }
    }

    // Element.matches(): every expected element matches, every unexpected one
    // doesn't, the element itself being :scope. The scoped list's `ctx` is
    // a second argument browsers ignore.
    for (list_name, file) in [("valid", "Element-matches"), ("scoped", "Element-matches")] {
        for case in data[list_name].as_array().unwrap() {
            let selector = case["selector"].as_str().unwrap();
            let shown = label(selector);
            if case["testType"].as_u64().unwrap() & TEST_MATCH == 0 {
                continue;
            }
            for cx in &contexts {
                if cx.name == "element" || excluded(case, cx.name) {
                    continue;
                }
                let name = format!(
                    "dom/nodes/{file}.html | {list_name} | {} | {shown}",
                    cx.name
                );
                let options = MatchOptions {
                    url: cx.url,
                    ..MatchOptions::default()
                };
                let got = SelectorList::parse(selector)
                    .map_err(|e| format!("did not parse: {e}"))
                    .and_then(|list| {
                        let mut wrong = Vec::new();
                        for (id, want) in str_list(&case["expect"])
                            .iter()
                            .map(|id| (id, true))
                            .chain(str_list(&case["unexpected"]).iter().map(|id| (id, false)))
                        {
                            let el = by_id_under(&cx.doc, cx.scope, id)
                                .ok_or_else(|| format!("no element #{id} in the document"))?;
                            if list.matches_with(&cx.doc, el, Some(el), &options) != want {
                                wrong.push(format!(
                                    "#{id} {}",
                                    if want { "did not match" } else { "matched" }
                                ));
                            }
                        }
                        Ok(wrong)
                    });
                out.push(Outcome::check(
                    name,
                    got.as_ref().is_ok_and(Vec::is_empty),
                    || match got {
                        Ok(wrong) => wrong.join(", "),
                        Err(e) => e,
                    },
                ));
            }
        }
    }
}

// ---- css/selectors and Element-closest: cases lifted from each file ----------

fn file_cases(file: &Value, out: &mut Vec<Outcome>) {
    let path = file["file"].as_str().unwrap();
    let html = std::fs::read_to_string(Path::new(DIR).join(path)).unwrap();
    let doc = parse(&html, false);
    for case in file["cases"].as_array().unwrap() {
        let kind = case["kind"].as_str().unwrap();
        let selector = case["selector"].as_str().unwrap();
        let shown = label(selector);
        let root = case["root"].as_str().unwrap_or("");
        let expect = &case["expect"];
        let name = format!("{path} | {kind} | {root} | {shown}").replace("|  |", "|");
        let parsed = SelectorList::parse(selector);
        if kind == "parse" {
            let valid = expect.as_bool().unwrap();
            out.push(Outcome::check(name, parsed.is_ok() == valid, || {
                if valid {
                    format!("did not parse: {}", parsed.unwrap_err())
                } else {
                    "parsed, but should be a syntax error".to_string()
                }
            }));
            continue;
        }
        let Some(el) = resolve(&doc, root) else {
            out.push(Outcome::check(name, false, || {
                format!("no {root} in the document")
            }));
            continue;
        };
        let list = match parsed {
            Ok(list) => list,
            Err(e) => {
                out.push(Outcome::check(name, false, || {
                    format!("did not parse: {e}")
                }));
                continue;
            }
        };
        let id = |n: Option<NodeId>| n.map(|n| format!("#{}", id_of(&doc, n).unwrap_or("")));
        let (got, want) = match kind {
            "all" => {
                let mut got: Vec<String> = ids(&doc, &list.select(&doc, el))
                    .into_iter()
                    .map(|i| format!("#{i}"))
                    .collect();
                let mut want = str_list(expect);
                if expect.is_array() && case["ordered"] == Value::Bool(false) {
                    got.sort();
                    want.sort();
                }
                (format!("{got:?}"), format!("{want:?}"))
            }
            "count" => (
                list.select(&doc, el).len().to_string(),
                expect.as_u64().unwrap().to_string(),
            ),
            "first" => (
                format!("{:?}", id(list.select_first(&doc, el))),
                format!("{:?}", expect.as_str()),
            ),
            "closest" => (
                format!("{:?}", id(closest(&list, &doc, el))),
                format!("{:?}", expect.as_str()),
            ),
            "matches" => (
                list.matches(&doc, el, Some(el)).to_string(),
                expect.as_bool().unwrap().to_string(),
            ),
            other => panic!("{path}: unknown case kind {other}"),
        };
        out.push(Outcome::check(name, got == want, || {
            format!("expected {want}\n     got {got}")
        }));
    }
}

// ---- the runner ------------------------------------------------------------

fn known_failures() -> Vec<String> {
    std::fs::read_to_string(Path::new(DIR).join("known-failures.txt"))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

fn report(
    suite: &str,
    outcomes: &[Outcome],
    skipped: usize,
    skipped_because: &str,
    known: &[String],
) -> Vec<String> {
    // Cases nothing here can decide are skipped, tallied by reason.
    let mut reasons: Vec<(&str, usize)> = Vec::new();
    let (skipped_cases, outcomes): (Vec<&Outcome>, Vec<&Outcome>) =
        outcomes.iter().partition(|o| {
            skip_reason(&o.name).is_some_and(|r| {
                match reasons.iter_mut().find(|(k, _)| *k == r) {
                    Some((_, n)) => *n += 1,
                    None => reasons.push((r, 1)),
                }
                true
            })
        });
    let skipped = skipped + skipped_cases.len();
    let skipped_because = reasons
        .iter()
        .map(|(r, n)| format!("{n} cases: {r}"))
        .chain((!skipped_because.is_empty()).then(|| skipped_because.to_string()))
        .collect::<Vec<_>>()
        .join("; ");
    let failed: Vec<&Outcome> = outcomes
        .iter()
        .copied()
        .filter(|o| o.failure.is_some())
        .collect();
    eprintln!(
        "{suite}: {} passed, {} failed, {skipped} skipped ({skipped_because})",
        outcomes.len() - failed.len(),
        failed.len()
    );
    summary::write(&summary::Counts {
        suite,
        passed: outcomes.len() - failed.len(),
        failed: failed.len(),
        skipped,
        skipped_because: &skipped_because,
        data: Path::new(DIR),
    });
    let mut problems = Vec::new();
    for o in &failed {
        if !known.contains(&o.name) {
            problems.push(format!(
                "FAIL {}\n     {}",
                o.name,
                o.failure.as_deref().unwrap()
            ));
        }
    }
    for o in outcomes {
        if o.failure.is_none() && known.contains(&o.name) {
            problems.push(format!(
                "PASSES NOW, remove it from known-failures.txt: {}",
                o.name
            ));
        }
    }
    problems
}

#[test]
fn wpt_selectors() {
    let data: Value =
        serde_json::from_str(&std::fs::read_to_string(Path::new(DIR).join("cases.json")).unwrap())
            .unwrap();
    let known = known_failures();
    let mut problems = Vec::new();

    let mut dom = Vec::new();
    dom_nodes(&data["dom"], &mut dom);
    let mut css = Vec::new();
    let (mut skipped_files, mut skipped_tests) = (0, 0);
    for file in data["files"].as_array().unwrap() {
        if file["skipped_because"].is_string() {
            skipped_files += 1;
        } else {
            skipped_tests += file["skipped"].as_array().map_or(0, Vec::len);
            let out = if file["file"].as_str().unwrap().starts_with("dom/") {
                &mut dom
            } else {
                &mut css
            };
            file_cases(file, out);
        }
    }

    // The suite's checks of the JavaScript API itself (interfaces, NodeList
    // staleness, stringifying null) aren't selector tests and aren't counted.
    problems.extend(report("wpt-dom-selectors", &dom, 0, "", &known));
    problems.extend(report(
        "wpt-css-selectors",
        &css,
        skipped_files + skipped_tests,
        &format!(
            "{skipped_files} files and {skipped_tests} tests need a browser: rendering, \
             computed style, user action, shadow DOM or a DOM changed from script; \
             cases.json has each one's reason"
        ),
        &known,
    ));

    let all: Vec<&Outcome> = dom.iter().chain(&css).collect();
    for k in &known {
        if !all.iter().any(|o| &o.name == k) {
            problems.push(format!(
                "NOT A CASE, remove it from known-failures.txt: {k}"
            ));
        }
    }
    if std::env::var("WPT_SELECTORS_BRIEF").is_err() {
        for o in all
            .iter()
            .filter(|o| o.failure.is_some() && known.contains(&o.name))
        {
            eprintln!("known failure: {}", o.name);
        }
    }
    assert!(
        problems.is_empty(),
        "{} problems:\n{}",
        problems.len(),
        problems.join("\n")
    );
}
