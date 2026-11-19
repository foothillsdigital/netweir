//! libxml2's XPath expression tests (test/XPath/expr, with the answers in
//! result/XPath/expr), vendored at the commit in tests/libxml2/COMMIT
//! under libxml2's MIT licence (tests/libxml2/Copyright). They check the
//! operators, the number, string and boolean rules and the core functions
//! without a document, so each runs against an empty one.

use netweir_xpath::{Doc, Index, Value, XPath};

const FILES: [&str; 6] = [
    "base",
    "compare",
    "equality",
    "floats",
    "functions",
    "strings",
];

/// What libxml2 printed for an expression.
#[derive(Debug)]
enum Expected {
    Number(String),
    Str(String),
    Bool(bool),
    Error,
}

fn cases(name: &str) -> Vec<(String, Expected)> {
    let path = format!(
        "{}/tests/libxml2/result/XPath/expr/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut out = Vec::new();
    for block in text.split("========================\n").skip(1) {
        let mut lines = block.lines();
        let expr = lines
            .next()
            .and_then(|l| l.strip_prefix("Expression: "))
            .unwrap_or_else(|| panic!("{name}: no expression in {block:?}"))
            .to_string();
        let result = lines.next().unwrap_or("");
        let expected = if let Some(n) = result.strip_prefix("Object is a number : ") {
            Expected::Number(n.to_string())
        } else if let Some(s) = result.strip_prefix("Object is a string : ") {
            Expected::Str(s.to_string())
        } else if let Some(b) = result.strip_prefix("Object is a Boolean : ") {
            Expected::Bool(b == "true")
        } else if result == "Object is empty (NULL)" {
            Expected::Error
        } else {
            panic!("{name}: {expr}: can't read {result:?}");
        };
        out.push((expr, expected));
    }
    out
}

/// libxml2 prints numbers as C's `%g` does (`1e+20`, `1.23457e+19`), so
/// they are compared as numbers to the digits printed.
fn same_number(got: f64, want: &str) -> bool {
    let wanted: f64 = match want {
        "NaN" => return got.is_nan(),
        "Infinity" => f64::INFINITY,
        "-Infinity" => f64::NEG_INFINITY,
        w => w.parse().unwrap_or_else(|_| panic!("not a number: {w:?}")),
    };
    if got == wanted {
        return true;
    }
    let mantissa = want.split('e').next().unwrap_or(want);
    let digits = mantissa
        .trim_start_matches(['-', '0', '.'])
        .chars()
        .filter(char::is_ascii_digit)
        .count() as i32;
    ((got - wanted) / wanted).abs() <= 0.5 * 10f64.powi(1 - digits)
}

/// Why libxml2's answer isn't XPath 1.0's, for the cases where it isn't.
fn libxml2_only(expr: &str) -> Option<&'static str> {
    // `1e3`: digits, then an exponent.
    let b = expr.as_bytes();
    let exponent = (1..b.len().saturating_sub(1)).any(|i| {
        b[i] == b'e' && b[i - 1].is_ascii_digit() && (b[i + 1].is_ascii_digit() || b[i + 1] == b'-')
    });
    if exponent {
        return Some("XPath 1.0 numbers have no exponent; libxml2 reads one");
    }
    // Section 4.2: a number becomes a string without an exponent.
    if expr.starts_with("string(") && expr.contains("12345678901234567890") {
        return Some("libxml2 writes large numbers with an exponent; XPath 1.0 spells them out");
    }
    None
}

#[test]
fn libxml2_expressions() {
    let tree = netweir_html::parse("", false);
    let index = Index::build(&tree, &[tree.root()]);
    let doc = Doc::html(&tree, &index);
    let (mut passed, mut skipped, mut failed) = (0, 0, Vec::new());
    for file in FILES {
        for (expr, expected) in cases(file) {
            if libxml2_only(&expr).is_some() {
                skipped += 1;
                continue;
            }
            let got = XPath::new(&expr)
                .and_then(|x| x.evaluate(doc, tree.root(), &[], &[], &netweir_xpath::NoExtensions));
            let ok = match (&got, &expected) {
                (Err(_), Expected::Error) => true,
                (Ok(Value::Num(n)), Expected::Number(want)) => same_number(*n, want),
                (Ok(Value::Str(s)), Expected::Str(want)) => s == want,
                (Ok(Value::Bool(b)), Expected::Bool(want)) => b == want,
                _ => false,
            };
            if ok {
                passed += 1;
            } else {
                failed.push(format!(
                    "{file}: {expr}: expected {expected:?}, got {got:?}"
                ));
            }
        }
    }
    eprintln!(
        "libxml2 XPath expressions: {passed} passed, {} failed, {skipped} skipped (libxml2's own extensions)",
        failed.len()
    );
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}
