//! html5lib-tests' serializer suite. Each test is a token stream; this
//! builds the tree the stream describes and serializes it with the test's
//! options over html5lib's defaults.
//!
//! A stream can be part of a tree: `["EndTag", "p"]` alone closes a `p`
//! that opened before it. Such an element is built with nothing before its
//! end, and its start tag, which the stream never had, is cut from the
//! front of the output; an element still open when the stream ends has its
//! end tag cut from the back.

use netweir_html::{Document, NodeId, Ns, SerializeOptions};
use serde_json::Value;

mod summary;

/// Tests whose expected output changes the tree it describes, with why.
const WRONG: &[(&str, &str)] = &[(
    "p end-tag followed by datagrid start-tag",
    "datagrid left HTML; it doesn't close a p, so leaving out </p> puts it inside one",
)];

fn void(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "basefont"
            | "bgsound"
            | "br"
            | "col"
            | "embed"
            | "frame"
            | "hr"
            | "img"
            | "input"
            | "keygen"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// The element a start-tag token makes, attributes sorted by name, as
/// html5lib's runner sorts them.
fn element(doc: &mut Document, name: &str, attrs: &Value) -> NodeId {
    let el = doc.create_element(Ns::Html, name);
    let mut pairs: Vec<(String, String)> = match attrs {
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
            .collect(),
        Value::Array(list) => list
            .iter()
            .map(|a| {
                assert!(a["namespace"].is_null(), "a namespaced attribute");
                (
                    a["name"].as_str().unwrap().to_string(),
                    a["value"].as_str().unwrap().to_string(),
                )
            })
            .collect(),
        _ => Vec::new(),
    };
    pairs.sort();
    for (k, v) in &pairs {
        doc.set_attr(el, k, v);
    }
    el
}

/// The serialized stream, with what the stream didn't have cut away.
fn run(tokens: &[Value], options: &SerializeOptions) -> String {
    let mut doc = Document::empty();
    let root = doc.root();
    // Open elements, innermost last.
    let mut open: Vec<(NodeId, String)> = Vec::new();
    // Elements closed before they opened, outermost first.
    let mut unopened: Vec<String> = Vec::new();
    for t in tokens {
        let t = t.as_array().unwrap();
        let parent = open.last().map_or(root, |&(id, _)| id);
        let s = |i: usize| t.get(i).and_then(Value::as_str).unwrap_or("");
        match s(0) {
            "StartTag" => {
                assert_eq!(s(1), "http://www.w3.org/1999/xhtml", "a foreign element");
                let el = element(&mut doc, s(2), &t[3]);
                doc.append(parent, el).unwrap();
                if !void(s(2)) {
                    open.push((el, s(2).to_string()));
                }
            }
            "EmptyTag" => {
                let el = element(&mut doc, s(1), &t[2]);
                doc.append(parent, el).unwrap();
                // One that isn't void is a start tag never ended.
                if !void(s(1)) {
                    open.push((el, s(1).to_string()));
                }
            }
            "EndTag" => {
                let name = s(2);
                if let Some(at) = open.iter().rposition(|(_, n)| n == name) {
                    open.truncate(at);
                } else {
                    // It held everything so far.
                    let el = doc.create_element(Ns::Html, name);
                    let kids: Vec<_> = doc.children(root).collect();
                    for k in kids {
                        doc.append(el, k).unwrap();
                    }
                    doc.append(root, el).unwrap();
                    unopened.insert(0, name.to_string());
                    open.clear();
                }
            }
            "Characters" => {
                let text = doc.create_text(s(1));
                doc.append(parent, text).unwrap();
            }
            "Comment" => {
                let c = doc.create_comment(s(1));
                doc.append(parent, c).unwrap();
            }
            "Doctype" => {
                let d = doc.create_doctype(s(1), s(2), s(3));
                doc.append(parent, d).unwrap();
            }
            other => panic!("a {other} token"),
        }
    }
    let mut out = doc.serialize_with(root, options);
    for name in &unopened {
        if let Some(rest) = out.strip_prefix(&format!("<{name}>")) {
            out = rest.to_string();
        }
    }
    for (_, name) in open.iter().rev() {
        if let Some(rest) = out.strip_suffix(&format!("</{name}>")) {
            out = rest.to_string();
        }
    }
    out
}

/// html5lib's serializer defaults, then the test's options.
fn options(test: &Value) -> SerializeOptions {
    let mut o = SerializeOptions {
        quote_attr_values: false,
        use_best_quote_char: true,
        minimize_boolean_attributes: true,
        omit_optional_tags: true,
        // html5lib always writes a doctype's identifiers.
        full_doctype: true,
        ..SerializeOptions::default()
    };
    let Some(given) = test.get("options").and_then(Value::as_object) else {
        return o;
    };
    let flag = |name: &str| given.get(name).and_then(Value::as_bool);
    if let Some(q) = given.get("quote_char").and_then(Value::as_str) {
        o.quote_char = q.chars().next().unwrap();
        // As html5lib: a quote asked for is the quote used.
        o.use_best_quote_char = false;
    }
    o.quote_attr_values = flag("quote_attr_values").unwrap_or(o.quote_attr_values);
    o.use_best_quote_char = flag("use_best_quote_char").unwrap_or(o.use_best_quote_char);
    o.minimize_boolean_attributes =
        flag("minimize_boolean_attributes").unwrap_or(o.minimize_boolean_attributes);
    o.use_trailing_solidus = flag("use_trailing_solidus").unwrap_or(false);
    o.escape_rcdata = flag("escape_rcdata").unwrap_or(false);
    o.omit_optional_tags = flag("omit_optional_tags").unwrap_or(o.omit_optional_tags);
    o.strip_whitespace = flag("strip_whitespace").unwrap_or(false);
    // `<` and `>` are always escaped in attribute values, as the HTML
    // Standard has had it since 2025, so `escape_lt_in_attrs` is always on.
    assert_ne!(flag("escape_lt_in_attrs"), Some(false));
    // html5lib's default is on, but it needs an encoding to write.
    if flag("inject_meta_charset") != Some(false) {
        o.inject_meta_charset = given
            .get("encoding")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    o
}

#[test]
fn serializer_tests() {
    let dir = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/html5lib-tests/serializer"
    );
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "test"))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "the html5lib-tests submodule isn't checked out"
    );
    let (mut passed, mut skipped, mut failed) = (0, 0, Vec::new());
    for path in files {
        // The files have trailing commas JSON doesn't allow.
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace(",\n}", "\n}")
            .replace(", \n}", "\n}");
        let json: Value = serde_json::from_str(&text).unwrap();
        for test in json["tests"].as_array().unwrap() {
            let description = test["description"].as_str().unwrap();
            if WRONG.iter().any(|&(d, _)| d == description) {
                skipped += 1;
                continue;
            }
            let got = run(test["input"].as_array().unwrap(), &options(test));
            let expected: Vec<&str> = test["expected"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e.as_str().unwrap())
                .collect();
            if expected.contains(&got.as_str()) {
                passed += 1;
            } else {
                failed.push(format!(
                    "{} :: {description}\n  expected {expected:?}\n  got      {got:?}",
                    path.file_name().unwrap().to_string_lossy()
                ));
            }
        }
    }
    eprintln!(
        "serializer: {passed} passed, {} failed, {skipped} skipped (expected output changes the tree)",
        failed.len()
    );
    summary::write(&summary::Counts {
        suite: "html5lib-serializer",
        passed,
        failed: failed.len(),
        skipped,
        skipped_because: WRONG[0].1,
        data: std::path::Path::new(dir),
    });
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}
