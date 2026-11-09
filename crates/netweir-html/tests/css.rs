//! CSS selectors: every feature Netweir's own tests use, the rest of what
//! lexbor offered, and where browsers and lexbor part ways, the browsers.

use netweir_html::css::SelectorList;
use netweir_html::{Document, Kind, NodeId, parse};

const PAGE: &str = r#"<!DOCTYPE html><html><head><title>Shop</title></head><body>
<div id="main" class="Wide page">
  <h1>Books</h1>
  <ul>
    <li class="item first" data-id="17"><a href="/p/1" title="Book A, B">A</a></li>
    <li class="item sold" data-id="27"><a href="/p/2">B</a><span class="price">£2</span></li>
    <li class="item" data-id="30"><a href="https://x.example/p/3" hreflang="en-GB">C</a></li>
    <li class="item last"><p></p><p> </p><p><!--c--></p></li>
  </ul>
  <form><fieldset disabled><legend><input name="in-legend"></legend><input name="off"></fieldset>
    <input type="checkbox" checked name="box"><input type="TEXT" required readonly name="ro">
    <select><optgroup disabled><option>x</option></optgroup><option selected>y</option></select>
    <textarea placeholder="say"></textarea><input placeholder="p" value="v">
  </form>
  <svg viewBox="0 0 1 1"><clipPath id="cp"></clipPath><rect/></svg>
</div></body></html>"#;

fn names(doc: &Document, nodes: &[NodeId]) -> Vec<String> {
    nodes
        .iter()
        .map(|&n| {
            let Kind::Element { name, .. } = doc.kind(n) else {
                unreachable!()
            };
            let id = doc
                .attrs(n)
                .find(|(_, a, _)| *a == "id" || *a == "name" || *a == "data-id")
                .map(|(_, _, v)| format!("#{v}"))
                .unwrap_or_default();
            format!("{name}{id}")
        })
        .collect()
}

fn select(html: &str, css: &str) -> Vec<String> {
    let doc = parse(html, false);
    let list = SelectorList::parse(css).unwrap_or_else(|e| panic!("{css}: {e}"));
    names(&doc, &list.select(&doc, doc.root()))
}

fn page(css: &str) -> Vec<String> {
    select(PAGE, css)
}

#[test]
fn simple_selectors_and_combinators() {
    assert_eq!(page("h1"), ["h1"]);
    assert_eq!(page("li.item.sold"), ["li#27"]);
    assert_eq!(page("#main > h1"), ["h1"]);
    assert_eq!(page("ul a"), ["a", "a", "a"]);
    assert_eq!(page("li.first + li"), ["li#27"]);
    assert_eq!(page("li.first ~ li"), ["li#27", "li#30", "li"]);
    assert_eq!(page("*").len(), page("html, html *").len());
    assert_eq!(page("h1, #main > h1, h1:not(p)"), ["h1"]);
    assert_eq!(page(r"#a\:b, .w-1\/2, #\39 lives"), Vec::<String>::new());
}

#[test]
fn attribute_selectors() {
    assert_eq!(page("[data-id]"), ["li#17", "li#27", "li#30"]);
    assert_eq!(page("a[href^='/']"), ["a", "a"]);
    assert_eq!(page("[data-id$='7']"), ["li#17", "li#27"]);
    assert_eq!(page("a[href*='/p/']").len(), 3);
    assert_eq!(page("[class~=sold]"), ["li#27"]);
    assert_eq!(page("[class~='']"), Vec::<String>::new());
    assert_eq!(page("[hreflang|=en]"), ["a"]);
    assert_eq!(page("a[title='Book A, B']"), ["a"]);
    assert_eq!(page("[data-id=\"17\"]"), ["li#17"]);
    assert_eq!(page("[class='ITEM FIRST' i]"), ["li#17"]);
    assert_eq!(page("[href^='']"), Vec::<String>::new());
}

#[test]
fn html_lists_attributes_whose_values_ignore_case() {
    assert_eq!(page("input[type=text]"), ["input#ro"]);
    assert_eq!(page("input[type=text s]"), Vec::<String>::new());
    assert_eq!(page("[data-id='17' s]"), ["li#17"]);
}

#[test]
fn ids_and_classes_compare_exactly_unless_quirks() {
    assert_eq!(page(".wide"), Vec::<String>::new());
    assert_eq!(page(".Wide"), ["div#main"]);
    let quirky = "<div id=X class=Y></div>";
    assert_eq!(select(quirky, "#x"), ["div#X"]);
    assert_eq!(select(quirky, ".y"), ["div#X"]);
    assert_eq!(
        select(&format!("<!DOCTYPE html>{quirky}"), "#x"),
        Vec::<String>::new()
    );
}

#[test]
fn type_selectors_ignore_case_for_html_only() {
    assert_eq!(page("H1"), ["h1"]);
    assert_eq!(page("clipPath"), ["clipPath#cp"]);
    assert_eq!(page("clippath"), Vec::<String>::new());
    assert_eq!(page("[viewBox]"), ["svg"]);
    assert_eq!(page("[viewbox]"), Vec::<String>::new());
    assert_eq!(page("*|rect"), ["rect"]);
    assert_eq!(page("|rect, |*"), Vec::<String>::new());
}

/// As in a browser, `[href]` means an href in no namespace; SVG's
/// `xlink:href` takes `[*|href]`. Lexbor matched both.
#[test]
fn plain_attribute_names_mean_no_namespace() {
    let svg = "<svg><use xlink:href=#a></use><a href=b></a></svg>";
    assert_eq!(select(svg, "[href]"), ["a"]);
    assert_eq!(select(svg, "[*|href]"), ["use", "a"]);
}

#[test]
fn child_index_pseudo_classes() {
    assert_eq!(page("li:nth-child(2)"), ["li#27"]);
    assert_eq!(page("li:nth-child(odd)"), ["li#17", "li#30"]);
    assert_eq!(page("li:nth-child(2n + 2)"), ["li#27", "li"]);
    assert_eq!(page("li:nth-child(-n+2)"), ["li#17", "li#27"]);
    assert_eq!(page("li:nth-last-child(1)"), ["li"]);
    assert_eq!(page("li:nth-child(2 of .item:not(.sold))"), ["li#30"]);
    assert_eq!(page("li:first-child"), ["li#17"]);
    assert_eq!(page("li:last-child"), ["li"]);
    assert_eq!(page("p:nth-of-type(2)"), ["p"]);
    assert_eq!(page("p:last-of-type"), ["p"]);
    assert_eq!(page("h1:only-of-type, title:only-child"), ["title", "h1"]);
    assert_eq!(page("li:nth-child(0)"), Vec::<String>::new());
}

#[test]
fn logical_pseudo_classes() {
    assert_eq!(page(":is(h1, li.sold)"), ["h1", "li#27"]);
    assert_eq!(page(":where(h1)"), ["h1"]);
    assert_eq!(page(":is(h1, ::nonsense, )"), ["h1"]);
    assert_eq!(page("li:not(.sold):not(.last)"), ["li#17", "li#30"]);
    assert_eq!(page("li:has(.price)"), ["li#27"]);
    assert_eq!(page("li:has(> a[href^=http])"), ["li#30"]);
    assert_eq!(page("li:has(+ li.sold)"), ["li#17"]);
    assert_eq!(page("li:has(~ .last)").len(), 3);
    assert_eq!(page("ul:has(p:empty)"), ["ul"]);
}

#[test]
fn state_and_structure_pseudo_classes() {
    assert_eq!(page(":root"), ["html"]);
    assert_eq!(page("p:empty").len(), 2);
    assert_eq!(page("a:any-link, a:link").len(), 3);
    assert_eq!(page(":checked"), ["input#box", "option"]);
    assert_eq!(page("input:disabled"), ["input#off"]);
    assert_eq!(page("option:disabled"), ["option"]);
    assert_eq!(page("fieldset:disabled"), ["fieldset"]);
    assert_eq!(page("input:enabled").len(), 4);
    assert_eq!(page(":required"), ["input#ro"]);
    assert_eq!(
        page("input:read-only"),
        ["input#off", "input#box", "input#ro"]
    );
    assert_eq!(page(":placeholder-shown"), ["textarea"]);
    assert_eq!(page("a:hover, a:focus, a:visited"), Vec::<String>::new());
}

#[test]
fn scope_is_the_element_a_query_runs_from() {
    let doc = parse(PAGE, false);
    let ul = SelectorList::parse("ul").unwrap().select(&doc, doc.root())[0];
    let direct = SelectorList::parse(":scope > li.sold").unwrap();
    assert_eq!(names(&doc, &direct.select(&doc, ul)), ["li#27"]);
    // Combinators look past the scope, as in a browser.
    let past = SelectorList::parse("div li.sold").unwrap();
    assert_eq!(names(&doc, &past.select(&doc, ul)), ["li#27"]);
    // The scope itself isn't a result.
    assert!(
        SelectorList::parse("ul")
            .unwrap()
            .select(&doc, ul)
            .is_empty()
    );
}

#[test]
fn bad_selectors_are_errors() {
    for bad in [
        "",
        "   ",
        "div[",
        "p >",
        "##x",
        "!!",
        "a,",
        ", a",
        "a,,b",
        "a::before",
        "svg|rect",
        "[xlink|href]",
        ":nth-child(x)",
        ":nth-of-type(2 of p)",
        ":not()",
        ":has()",
        ":nope",
        ":lexbor-contains('x')",
        "a || b",
        "[a=b c]",
        "[a!=b]",
    ] {
        assert!(SelectorList::parse(bad).is_err(), "{bad:?} parsed");
    }
}

/// Each nesting recursed in the parser, so a deep enough selector
/// overflowed the stack and aborted the process; it's an error instead.
#[test]
fn deeply_nested_selectors_are_errors_not_crashes() {
    let result = std::thread::Builder::new()
        .stack_size(512 * 1024)
        .spawn(|| {
            let not = format!("{}a{}", ":not(".repeat(20_000), ")".repeat(20_000));
            let is = format!("{}a{}", ":is(".repeat(5_000), ")".repeat(5_000));
            {
                // :is() forgives what it can't parse, so the too-deep part
                // is dropped rather than failing the list: what matters is
                // that neither overflows.
                let _ = SelectorList::parse(&is);
                (SelectorList::parse(&not).is_err(), true)
            }
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(result, (true, true));
    // Reasonable nesting still works.
    let ok = format!("{}a{}", ":not(".repeat(20), ")".repeat(20));
    assert!(SelectorList::parse(&ok).is_ok());
}

/// Places the parser was looser than CSS (parsec-html/parsec#37). Lexbor
/// reads comments the same way, and rejects `+ 5`, but accepts `2 n`,
/// `#1a` and `:has()` inside `:has()`.
#[test]
fn selectors_read_as_css_reads_them() {
    // A comment is no whitespace: `li/**/.sold` is one compound.
    assert_eq!(page("li/**/.sold"), ["li#27"]);
    assert_eq!(page("li /**/.price"), ["span"]);
    assert_eq!(page("li /**/ .price"), ["span"]);
    assert_eq!(page("li/**/ .price"), ["span"]);
    assert_eq!(page("ul/**/>/**/li").len(), 4);
    assert!(SelectorList::parse("*/**/").is_ok());
    // Two type selectors in one compound, not a descendant selector.
    assert!(SelectorList::parse("a/**/b").is_err());

    // An+B has no whitespace inside a term, only around the sign before B.
    for bad in [
        ":nth-child(2 n)",
        ":nth-child(+ 5)",
        ":nth-child(- n+1)",
        ":nth-child(2n 1)",
    ] {
        assert!(SelectorList::parse(bad).is_err(), "{bad:?} parsed");
    }
    for good in [
        ":nth-child(2n + 1)",
        ":nth-child( 2n- 1 )",
        ":nth-child(-n +3)",
        ":nth-child(+5)",
    ] {
        assert!(SelectorList::parse(good).is_ok(), "{good:?} didn't parse");
    }

    // An ID is an identifier: no leading digit, unless it's escaped.
    for (bad, why) in [
        ("#1a", "an ID can't start with a digit"),
        ("#-1", "an ID can't start with a hyphen and a digit"),
        ("#", "a # with no ID after it"),
        ("p#", "a # with no ID after it"),
    ] {
        let e = SelectorList::parse(bad).expect_err(bad);
        assert!(e.to_string().contains(why), "{bad:?}: {e}");
    }
    assert!(SelectorList::parse(r"#\31 a").is_ok());
    assert!(SelectorList::parse("#--x").is_ok());

    // :has() doesn't nest, however deep the inner one is.
    for bad in [
        ":has(:has(a))",
        ":has(a, :has(b))",
        ":has(:nth-child(1 of :has(a)))",
        ":has(:not(:has(a)))",
    ] {
        assert!(SelectorList::parse(bad).is_err(), "{bad:?} parsed");
    }
    assert!(SelectorList::parse(":has(a):has(b)").is_ok());
    assert!(SelectorList::parse(":is(:has(a))").is_ok());
}

#[test]
fn lang_matches_the_nearest_language_by_extended_filtering() {
    let html = r#"<div id="ch" lang="de-CH"><p id="in">x</p></div>
<p id="latn" lang="de-Latn-CH">a</p><p id="en" lang="EN-gb">b</p><p id="fr" lang="fr">c</p>
<p id="none">d</p><p id="private" lang="de-x-CH">e</p>
<div id="outer" lang="de"><p id="inner" lang="en">f</p></div>
<svg id="svg" xml:lang="fr" lang="de"></svg>"#;
    assert_eq!(
        select(html, ":lang(de)"),
        ["div#ch", "p#in", "p#latn", "p#private", "div#outer"]
    );
    assert_eq!(select(html, ":lang(de-CH)"), ["div#ch", "p#in", "p#latn"]);
    assert_eq!(select(html, ":lang('*-CH')"), ["div#ch", "p#in", "p#latn"]);
    assert_eq!(
        select(html, ":lang(en, fr)"),
        ["p#en", "p#fr", "p#inner", "svg#svg"]
    );
    assert_eq!(select(html, ":lang(EN-GB)"), ["p#en"]);
    assert_eq!(select(html, "p:lang(e)"), Vec::<String>::new());
    // xml:lang wins over lang on the same element.
    assert_eq!(select(html, "svg:lang(de)"), Vec::<String>::new());
    assert!(SelectorList::parse(":lang()").is_err());
}

#[test]
fn default_and_indeterminate_follow_the_markup() {
    let html = r#"<form id="f">
<input type="text" name="t"><button id="first" type="button">no</button>
<button id="go">go</button><input type="submit" id="second">
<input type="checkbox" id="c1" checked><input type="checkbox" id="c2">
<input type="radio" id="a1" name="a"><input type="radio" id="a2" name="a" checked>
<input type="radio" id="b1" name="b"><input type="radio" id="b2" name="b">
<input type="radio" id="alone">
<select><option id="o1">x</option><option id="o2" selected>y</option></select>
</form>
<form id="g"></form><input type="submit" id="outside" form="g">
<input type="radio" id="b3" name="b" checked>
<progress id="busy"></progress><progress id="half" value="0.5"></progress>"#;
    assert_eq!(
        select(html, ":default"),
        [
            "button#go",
            "input#c1",
            "input#a2",
            "option#o2",
            "input#outside",
            "input#b3"
        ]
    );
    // b3 is checked, but outside the form, so form f's group b has none.
    assert_eq!(
        select(html, ":indeterminate"),
        ["input#b1", "input#b2", "input#alone", "progress#busy"]
    );
}

#[test]
fn in_range_and_out_of_range_follow_min_max_and_the_value() {
    let html = r#"<input id="n1" type="number" min="1" max="10" value="5">
<input id="n2" type="number" min="1" max="10" value="11"><input id="n3" type="number" min="1" value="0.5e0">
<input id="n4" type="number" min="1" max="10"><input id="n5" type="number" min="1" value="ten">
<input id="n6" type="number" value="5"><input id="n7" type="number" min="1" value="0" disabled>
<input id="n8" type="number" min="1" value="0" readonly><datalist><input id="n9" type="number" min="1" value="0"></datalist>
<input id="r1" type="range" value="500"><input id="r2" type="range" readonly value="-5">
<input id="d1" type="date" min="2024-01-01" value="2024-02-29"><input id="d2" type="date" max="2023-12-31" value="2024-01-01">
<input id="d3" type="date" min="2024-01-01" value="2023-02-29">
<input id="m1" type="month" max="2024-06" value="2024-07"><input id="w1" type="week" min="2025-W53" value="2025-W01">
<input id="w2" type="week" min="2020-W53" value="2020-W53">
<input id="t1" type="time" min="22:00" max="06:00" value="23:30"><input id="t2" type="time" min="22:00" max="06:00" value="12:00">
<input id="dt" type="datetime-local" max="2024-01-01T12:00" value="2024-01-01 12:00:01">
<input id="text" type="text" min="1" value="0"><input id="dot" type="number" min="1." value="0">
<input id="n10" type="number" min="1" value="1.">"#;
    assert_eq!(
        select(html, ":in-range"),
        [
            "input#n1",
            "input#n4",
            "input#n5",
            "input#r1",
            "input#r2",
            "input#d1",
            "input#d3",
            "input#w2",
            "input#t1",
            "input#n10"
        ]
    );
    assert_eq!(
        select(html, ":out-of-range"),
        [
            "input#n2", "input#n3", "input#d2", "input#m1", "input#t2", "input#dt"
        ]
    );
}

#[test]
fn valid_and_invalid_follow_the_constraints_the_markup_sets() {
    let html = r#"<form id="f">
<input id="req" required><input id="req-ok" required value="x"><input id="plain">
<input id="mail" type="email" value="a@b.example"><input id="bad-mail" type="email" value="a@-b">
<input id="mails" type="email" multiple value=" a@b.example , c@d.example "><input id="url" type="url" value="https://x.test/">
<input id="bad-url" type="url" value="not a url"><input id="num" type="number" min="1" max="10" step="2" value="5">
<input id="off-step" type="number" min="1" step="2" value="4"><input id="over" type="number" max="3" value="4">
<input id="any" type="number" min="0" step="any" value="0.5"><input id="wk" type="week" min="2020-W53" step="2" value="2021-W02">
<input id="box" type="checkbox" required><input id="file" type="file" required>
<input id="r1" type="radio" name="g" required><input id="r2" type="radio" name="g">
<textarea id="ta" required></textarea>
<select id="sel" required><option value="">Pick</option><option>A</option></select>
<select id="sel-ok" required><option value="">Pick</option><option selected>A</option></select>
<input id="ro" required readonly><input id="off" required disabled><input id="hid" type="hidden" required>
<button id="go">go</button><button id="nope" type="button">x</button>
</form>
<form id="g"><input id="fine" value="x"></form>
<fieldset id="fs"><input id="in-fs" type="email" value="x"></fieldset>"#;
    assert_eq!(
        select(html, "input:invalid, textarea:invalid, select:invalid"),
        [
            "input#req",
            "input#bad-mail",
            "input#bad-url",
            "input#off-step",
            "input#over",
            "input#box",
            "input#file",
            "input#r1",
            "input#r2",
            "textarea#ta",
            "select#sel",
            "input#in-fs"
        ]
    );
    assert_eq!(
        select(html, "input:valid, select:valid, button:valid"),
        [
            "input#req-ok",
            "input#plain",
            "input#mail",
            "input#mails",
            "input#url",
            "input#num",
            "input#any",
            "input#wk",
            "select#sel-ok",
            "button#go",
            "input#fine"
        ]
    );
    assert_eq!(
        select(html, "form:invalid, fieldset:invalid"),
        ["form#f", "fieldset#fs"]
    );
    assert_eq!(select(html, "form:valid"), ["form#g"]);
}

#[test]
fn valid_reads_attributes_as_the_standard_parses_them() {
    let html = r#"<input id="one-mail" type="email" value="a@b.example,c@d.example">
<input id="two-mails" type="email" multiple value="a@b.example,c@d.example">
<select id="list" required size="3"><option>A</option></select>
<input id="step-ws" type="number" min="0" step=" 2 " value="3">
<input id="step-unit" type="number" min="0" step="2px" value="4">
<select id="px" required size="2px"><option value="" selected>None</option><option>A</option></select>"#;
    assert_eq!(
        select(html, ":invalid"),
        ["input#one-mail", "select#list", "input#step-ws"]
    );
    assert_eq!(
        select(html, ":valid"),
        ["input#two-mails", "input#step-unit", "select#px"]
    );
}
