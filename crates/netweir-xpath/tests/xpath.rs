//! Every axis, node test and core function of XPath 1.0 over a parsed HTML
//! page, with the answers lxml gives for the same queries, plus the hooks
//! (extension functions, namespaces) and the limits on hostile input.

use std::time::{Duration, Instant};

use netweir_html::{self as html, NodeId};
use netweir_xpath::{
    Args, Doc, Extensions, Index, Item, Namespaces, NoExtensions, Value, XPath, XValue,
};

const PAGE: &str = r#"<!DOCTYPE html><html lang="en"><head><title>T</title></head><body>
<div id="a" class="x y"><p>one</p><!--c--><p id="b">two <b>bold</b></p></div>
<ul><li>1</li><li>2</li><li>3</li></ul>
<span lang="en-GB" data-n="4">s</span><svg><circle r="1"/></svg>
</body></html>"#;

struct Page {
    tree: html::Document,
    index: Index,
}

impl Page {
    fn new(src: &str) -> Page {
        let tree = html::parse(src, false);
        let index = Index::build(&tree, &[tree.root()]);
        Page { tree, index }
    }

    fn doc(&self) -> Doc<'_> {
        Doc::html(&self.tree, &self.index)
    }

    fn value(&self, query: &str) -> Value<'_> {
        XPath::new(query)
            .and_then(|x| x.evaluate(self.doc(), self.tree.root(), &[], &[], &NoExtensions))
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// A result as text: each node's name (or `#text:...`, `@name=value`),
    /// or the scalar.
    fn get(&self, query: &str) -> Vec<String> {
        match self.value(query) {
            Value::Nodes(items) => items.iter().map(|i| self.describe(i)).collect(),
            Value::Str(s) => vec![s.into_owned()],
            Value::Num(n) => vec![n.to_string()],
            Value::Bool(b) => vec![b.to_string()],
        }
    }

    fn describe(&self, item: &Item<'_>) -> String {
        match item {
            Item::Attr { name, value, .. } => format!("@{name}={value}"),
            Item::Node(n) => match self.tree.kind(*n) {
                html::Kind::Element { name, .. } => name.to_string(),
                html::Kind::Text(t) => format!("#text:{}", t.trim()),
                html::Kind::Comment(c) => format!("#comment:{c}"),
                html::Kind::Document => "#document".into(),
                html::Kind::Doctype { .. } => "#doctype".into(),
                other => format!("{other:?}"),
            },
        }
    }

    fn one(&self, query: &str) -> String {
        let all = self.get(query);
        assert_eq!(all.len(), 1, "{query}: {all:?}");
        all.into_iter().next().unwrap()
    }
}

fn names(s: &[&str]) -> Vec<String> {
    s.iter().map(|s| s.to_string()).collect()
}

#[test]
fn every_axis() {
    let p = Page::new(PAGE);
    let at_b = "//p[@id='b']";
    for (axis, want) in [
        ("child::node()", &["#text:two", "b"][..]),
        ("descendant::node()", &["#text:two", "b", "#text:bold"]),
        ("descendant-or-self::*", &["p", "b"]),
        ("parent::node()", &["div"]),
        ("ancestor::*", &["html", "body", "div"]),
        ("ancestor-or-self::*", &["html", "body", "div", "p"]),
        ("following-sibling::node()", &[]),
        ("preceding-sibling::node()", &["p", "#comment:c"]),
        ("following::li", &["li", "li", "li"]),
        ("preceding::*", &["head", "title", "p"]),
        ("attribute::*", &["@id=b"]),
        ("self::p", &["p"]),
        ("namespace::*", &[]),
    ] {
        assert_eq!(p.get(&format!("{at_b}/{axis}")), names(want), "{axis}");
    }
    // Reverse axes count from the context node outwards.
    assert_eq!(p.one(&format!("{at_b}/ancestor::*[1]")), "div");
    assert_eq!(
        p.one(&format!("{at_b}/preceding-sibling::node()[1]")),
        "#comment:c"
    );
    assert_eq!(p.one(&format!("{at_b}/preceding::*[1]")), "p");
    // An attribute's parent is its element, and it has no children.
    assert_eq!(p.one("//@data-n/.."), "span");
    assert_eq!(p.get("//@data-n/node()"), names(&[]));
    assert_eq!(p.get("//@data-n/following::*[1]"), names(&["svg"]));
}

#[test]
fn node_tests() {
    let p = Page::new(PAGE);
    assert_eq!(p.get("//div/node()").len(), 3);
    assert_eq!(p.get("//div/*"), names(&["p", "p"]));
    assert_eq!(p.get("//p/text()").len(), 2);
    assert_eq!(p.get("//div/comment()"), names(&["#comment:c"]));
    assert_eq!(p.get("//processing-instruction()"), names(&[]));
    assert_eq!(p.get("/node()"), names(&["#doctype", "html"]));
    assert_eq!(p.one("/"), "#document");
    // Names are the parser's lowercase ones, in no namespace.
    assert_eq!(p.get("//DIV"), names(&[]));
    assert_eq!(p.get("//@ID"), names(&[]));
    assert_eq!(p.one("//svg/circle/@r"), "@r=1");
    assert!(XPath::new("//svg:circle").is_ok());
}

#[test]
fn predicates_and_document_order() {
    let p = Page::new(PAGE);
    assert_eq!(p.get("//li[2]/text()"), names(&["#text:2"]));
    assert_eq!(p.get("//li[last()]/text()"), names(&["#text:3"]));
    assert_eq!(p.get("//li[position() > 1][1]/text()"), names(&["#text:2"]));
    assert_eq!(p.get("(//li)[2]/text()"), names(&["#text:2"]));
    assert_eq!(
        p.get("//li[. = '3' or . = '1']/text()"),
        names(&["#text:1", "#text:3"])
    );
    // A union comes back in document order without duplicates.
    assert_eq!(
        p.get("//span | //p | //div | //p"),
        names(&["div", "p", "p", "span"])
    );
    assert_eq!(
        p.get("//*[@id] | //@id"),
        names(&["div", "@id=a", "p", "@id=b"])
    );
}

#[test]
fn number_string_and_boolean_rules() {
    let p = Page::new(PAGE);
    for (q, want) in [
        ("1 + 2 * 3", "7"),
        ("7 mod -2", "1"),
        ("-7 mod 2", "-1"),
        ("1 div 0", "inf"),
        ("number('x')", "NaN"),
        ("count(//li) = 3", "true"),
        ("//li = 2", "true"),
        ("//li != 2", "true"),
        ("//li > 2", "true"),
        ("//li > 3", "false"),
        ("//nothing = //nothing", "false"),
        ("//li = //span/@data-n", "false"),
        ("true() = //li", "true"),
        ("'a' < 'b'", "false"),
        ("string(1 div 0)", "Infinity"),
        ("string(-0)", "0"),
        ("string(2.50)", "2.5"),
        ("boolean('')", "false"),
        ("boolean(//li)", "true"),
        ("number(true())", "1"),
        ("sum(//li)", "6"),
    ] {
        assert_eq!(p.one(q), want, "{q}");
    }
}

#[test]
fn every_core_function() {
    let p = Page::new(PAGE);
    for (q, want) in [
        ("last()", "1"),
        ("position()", "1"),
        ("count(//li)", "3"),
        ("name(id('b'))", "p"),
        ("count(id('a b zz'))", "2"),
        ("local-name(//svg)", "svg"),
        ("namespace-uri(//svg)", ""),
        ("name(//@data-n)", "data-n"),
        ("string(//p[2])", "two bold"),
        ("concat('a', 1, true())", "a1true"),
        ("starts-with('abc', 'ab')", "true"),
        ("contains('abc', 'bd')", "false"),
        ("substring-before('1999/04/01', '/')", "1999"),
        ("substring-after('1999/04/01', '/')", "04/01"),
        ("substring('12345', 1.5, 2.6)", "234"),
        ("substring('12345', 0, 3)", "12"),
        ("substring('12345', 0 div 0, 3)", ""),
        ("substring('12345', -42, 1 div 0)", "12345"),
        ("string-length('añb')", "3"),
        ("normalize-space('  a \n b  ')", "a b"),
        ("translate('bar', 'abc', 'ABC')", "BAr"),
        ("translate('--aaa--', 'abc-', 'ABC')", "AAA"),
        ("boolean(0)", "false"),
        ("not(0)", "true"),
        ("true()", "true"),
        ("false()", "false"),
        ("count(//*[lang('en')])", "15"),
        ("count(//span[lang('en')])", "1"),
        ("count(//span[lang('en-gb')])", "1"),
        ("count(//span[lang('fr')])", "0"),
        ("number(' 12 ')", "12"),
        ("sum(//@data-n)", "4"),
        ("floor(-1.5)", "-2"),
        ("ceiling(-1.5)", "-1"),
        ("round(-1.5)", "-1"),
        ("round(2.5)", "3"),
    ] {
        assert_eq!(p.one(q), want, "{q}");
    }
    // Context-node defaults.
    assert_eq!(
        p.get("//li[string-length() = 1][number() = 2]/text()"),
        names(&["#text:2"])
    );
    assert_eq!(
        p.get("//p[normalize-space() = 'two bold']/@id"),
        names(&["@id=b"])
    );
    assert_eq!(p.get("//p[string() = 'one']"), names(&["p"]));
    assert_eq!(p.get("//*[local-name() = 'circle']"), names(&["circle"]));
}

#[test]
fn errors_say_what_is_wrong() {
    let p = Page::new(PAGE);
    let run = |q: &str| {
        XPath::new(q)
            .and_then(|x| x.evaluate(p.doc(), p.tree.root(), &[], &[], &NoExtensions))
            .map(|_| ())
            .unwrap_err()
            .reason
    };
    assert!(run("count()").contains("count() takes 1 arguments, got 0"));
    assert!(run("nope()").contains("unknown function nope()"));
    assert!(run("has-class('x')").contains("unknown function"));
    assert!(run("//x:y").contains("undefined namespace prefix \"x\""));
    assert!(run("$v").contains("no value given for $v"));
    assert!(run("1 | //p").contains("node-sets"));
    assert!(run("//p[").contains("expected"));
}

#[test]
fn variables_and_bound_prefixes() {
    let p = Page::new(PAGE);
    let x = XPath::new("//li[. = $n]/text() | //svg[$flag]").unwrap();
    let vars = [
        ("n".to_string(), XValue::Num(2.0)),
        ("flag".to_string(), XValue::Bool(false)),
    ];
    let Value::Nodes(found) = x
        .evaluate(p.doc(), p.tree.root(), &vars, &[], &NoExtensions)
        .unwrap()
    else {
        panic!("not a node-set")
    };
    assert_eq!(found.len(), 1);
    // HTML has no namespaces: a bound prefix is accepted and matches
    // nothing.
    let ns = [("s".to_string(), "http://www.w3.org/2000/svg".to_string())];
    let x = XPath::new("count(//s:svg)").unwrap();
    let got = x
        .evaluate(p.doc(), p.tree.root(), &[], &ns, &NoExtensions)
        .unwrap();
    assert!(matches!(got, Value::Num(n) if n == 0.0));
}

/// An extension function that reads the context node and its arguments.
struct Shout;

impl Extensions for Shout {
    fn call<'a>(&self, name: &str, args: &Args<'_, 'a>) -> Option<Result<Value<'a>, String>> {
        if name != "shout" {
            return None;
        }
        Some((|| {
            args.arity(0, 1)?;
            let text = if args.is_empty() {
                match args.context() {
                    Item::Node(n) => args.doc().attr(n, "id").unwrap_or("").to_string(),
                    Item::Attr { value, .. } => value.to_string(),
                }
            } else {
                args.string(0)?
            };
            Ok(Value::Str(text.to_uppercase().into()))
        })())
    }

    fn scalar(&self, _: &str) -> bool {
        false
    }
}

#[test]
fn extension_functions_come_from_the_caller() {
    let p = Page::new(PAGE);
    let run = |q: &str| {
        let x = XPath::with_extensions(q, &Shout).unwrap();
        match x.evaluate(p.doc(), p.tree.root(), &[], &[], &Shout) {
            Ok(Value::Str(s)) => s.into_owned(),
            Ok(Value::Nodes(n)) => n.len().to_string(),
            other => format!("{other:?}"),
        }
    };
    assert_eq!(run("shout('hi')"), "HI");
    assert_eq!(run("//*[shout() = 'B']"), "1");
    assert!(run("shout(1, 2)").contains("shout() takes 0 to 1 arguments, got 2"));
    // A core function's name can't be taken over.
    assert_eq!(run("string(count(//li))"), "3");
}

/// Namespaces for a page whose SVG elements are said to be in one, as
/// an XML host would keep them.
struct SvgInOne<'t>(&'t html::Document);

impl Namespaces for SvgInOne<'_> {
    fn ns_of(&self, element: NodeId) -> u32 {
        match self.0.kind(element) {
            html::Kind::Element {
                ns: html::Ns::Svg, ..
            } => 1,
            _ => 0,
        }
    }

    fn uri(&self, id: u32) -> &str {
        if id == 1 { "urn:svg" } else { "" }
    }

    fn id_of(&self, uri: &str) -> Option<u32> {
        match uri {
            "" => Some(0),
            "urn:svg" => Some(1),
            _ => None,
        }
    }
}

#[test]
fn namespaces_come_from_the_host() {
    let p = Page::new(PAGE);
    let names = SvgInOne(&p.tree);
    let doc = Doc::xml(&p.tree, &p.index, &names);
    let ns = [("s".to_string(), "urn:svg".to_string())];
    let count = |q: &str| match XPath::new(q)
        .unwrap()
        .evaluate(doc, p.tree.root(), &[], &ns, &NoExtensions)
        .unwrap()
    {
        Value::Num(n) => n,
        Value::Nodes(n) => n.len() as f64,
        other => panic!("{other:?}"),
    };
    assert_eq!(count("count(//s:*)"), 2.0);
    assert_eq!(count("//s:circle"), 1.0);
    assert_eq!(count("//circle"), 0.0, "no prefix is no namespace");
    assert_eq!(count("//div"), 1.0);
    assert_eq!(count("count(//*[namespace-uri() = 'urn:svg'])"), 2.0);
}

#[test]
fn a_removed_subtree_is_its_own_tree() {
    let mut tree = html::parse("<div><p>a</p><p>b</p></div><i>c</i>", false);
    let div = (0..tree.len() as NodeId)
        .find(|&n| tree.lower_name(n) == Some("div"))
        .unwrap();
    let p = tree.first_child(div).unwrap();
    tree.remove(div);
    let index = Index::build(&tree, &[tree.root(), div]);
    let doc = Doc::html(&tree, &index);
    let count = |q: &str| match XPath::new(q)
        .unwrap()
        .evaluate(doc, p, &[], &[], &NoExtensions)
        .unwrap()
    {
        Value::Num(n) => n,
        other => panic!("{other:?}"),
    };
    assert_eq!(count("count(/descendant::p)"), 2.0);
    assert_eq!(count("count(following::*)"), 1.0);
    assert_eq!(count("count(//i)"), 0.0);
}

#[test]
fn processing_instructions_by_target() {
    // HTML parsing makes them comments; a host building XML keeps them.
    let mut tree = html::parse("<p>", false);
    let body = (0..tree.len() as NodeId)
        .find(|&n| tree.lower_name(n) == Some("body"))
        .unwrap();
    for target in ["a", "b"] {
        let pi = tree.create_processing_instruction(target, "data");
        tree.append(body, pi).unwrap();
    }
    let page = Page {
        index: Index::build(&tree, &[tree.root()]),
        tree,
    };
    assert_eq!(page.one("count(//processing-instruction())"), "2");
    assert_eq!(page.one("count(//processing-instruction('b'))"), "1");
    assert_eq!(page.one("name(//processing-instruction()[1])"), "a");
    assert_eq!(page.one("string(//processing-instruction('b'))"), "data");
}

fn timed<'p>(page: &'p Page, q: &str) -> (Result<Value<'p>, String>, Duration) {
    let start = Instant::now();
    let got = XPath::new(q)
        .unwrap()
        .evaluate(page.doc(), page.tree.root(), &[], &[], &NoExtensions)
        .map_err(|e| e.reason);
    (got, start.elapsed())
}

#[test]
fn steps_from_many_context_nodes_are_linear() {
    // Each of these walked the same nodes once per context node.
    let nested = Page::new(&("<div>".repeat(20_000) + "x"));
    let flat = Page::new(&"<p>x".repeat(50_000));
    for (page, q, n) in [
        (&nested, "//div//div", 19_999),
        (&nested, "//div//div//div", 19_998),
        (&nested, "//div/descendant::div", 19_999),
        (&nested, "//div/ancestor::div", 19_999),
        (&flat, "//p/following::p", 49_999),
        (&flat, "//p/preceding::p", 49_999),
        (&flat, "//p/following-sibling::p", 49_999),
        (&flat, "//p/preceding-sibling::p", 49_999),
        (&flat, "//p/following::text()", 49_999),
    ] {
        let (got, took) = timed(page, q);
        match got {
            Ok(Value::Nodes(found)) => assert_eq!(found.len(), n, "{q}"),
            other => panic!("{q}: {other:?}"),
        }
        assert!(took < Duration::from_secs(2), "{q} took {took:?}");
    }
}

#[test]
fn quadratic_queries_stop_at_the_work_limit() {
    let flat = Page::new(&"<p>x".repeat(20_000));
    let within = |q: &str, limit: u64| {
        XPath::new(q)
            .unwrap()
            .evaluate_within(flat.doc(), flat.tree.root(), &[], &[], &NoExtensions, limit)
            .map(|v| match v {
                Value::Nodes(n) => n.len(),
                other => panic!("{other:?}"),
            })
            .map_err(|e| e.reason)
    };
    // Each of the 20,000 tests walks the rest of the page: 200 million
    // nodes in all, which a limit of a million stops early.
    let start = Instant::now();
    let reason = within("//p[following::p]", 1 << 20).unwrap_err();
    assert!(reason.contains("more than 1048576 steps"), "{reason}");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    // Linear queries cost a few units a node.
    assert_eq!(within("//p/following::p", 8 * 40_000), Ok(19_999));
    assert_eq!(
        within("//p[preceding-sibling::*[1]]", 8 * 40_000),
        Ok(19_999)
    );
    // The default leaves real queries plenty of room.
    let small = Page::new(&"<p>x".repeat(2_000));
    let (got, _) = timed(&small, "//p[following::p][position() < 3]");
    assert!(matches!(got, Ok(Value::Nodes(n)) if n.len() == 2));
}
