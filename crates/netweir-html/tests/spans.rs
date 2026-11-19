//! Source positions, checked on every tree-construction input: each span
//! a node or attribute has is a slice of the input that reads as it.

use netweir_html::{Document, Kind, NodeId, Options, parse_fragment_with, parse_with};

mod inputs;

fn parse(data: &str, fragment: &Option<(netweir_html::Ns, String)>) -> Document {
    let options = Options {
        spans: true,
        ..Options::default()
    };
    match fragment {
        Some((ns, context)) => parse_fragment_with(data, *ns, context, options),
        None => parse_with(data, options),
    }
    .unwrap()
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// What's wrong with the spans of `doc`, parsed from `input`, or how
/// many it checked.
fn check(doc: &Document, input: &str) -> Result<usize, String> {
    let slice = |r: std::ops::Range<usize>| -> Result<&str, String> {
        if r.start > r.end || r.end > input.len() {
            return Err(format!("{r:?} is out of bounds"));
        }
        input
            .get(r.clone())
            .ok_or_else(|| format!("{r:?} splits a character"))
    };
    let (mut last_start, mut spans) = (0, 0);
    for id in 0..doc.len() as NodeId {
        let Some(span) = doc.span(id) else {
            continue;
        };
        spans += 1;
        // The parser makes nodes as it reads the tokens they come from.
        if span.start < last_start {
            return Err(format!(
                "node {id} starts at {span:?}, before node {}",
                id - 1
            ));
        }
        last_start = span.start;
        let s = slice(span.clone())?;
        match doc.kind(id) {
            Kind::Element { name, .. } => {
                // `<image>` is read as an img.
                let tag = if name == "img" && starts_with_ci(s, "<image") {
                    "image"
                } else {
                    name
                };
                if !starts_with_ci(s, &format!("<{tag}")) || !s.ends_with('>') {
                    return Err(format!("<{name}> at {span:?} reads {s:?}"));
                }
                let written: Vec<_> = doc.attrs_written(id).map(|(_, n, _)| n).collect();
                for (i, name) in written.iter().enumerate() {
                    let Some(at) = doc.attr_span(id, i) else {
                        return Err(format!("<{tag}>'s {name} has no span"));
                    };
                    let a = slice(at.clone())?;
                    // Not always inside the tag: a second `<body>` can
                    // give the first one attributes.
                    if !(name.contains('\u{FFFD}') || starts_with_ci(a, name)) {
                        return Err(format!("<{tag}>'s {name} at {at:?} reads {a:?}"));
                    }
                }
            }
            Kind::Text(text) => {
                if s.is_empty() {
                    return Err(format!("text {text:?} has an empty span"));
                }
                // Text read as it was written is in the slice, in order:
                // all of it, or what the tree builder kept (the spaces
                // of `a b` in a frameset).
                let mut rest = s.chars();
                let kept = text.chars().all(|c| rest.any(|r| r == c));
                if !s.contains(['&', '<', '\0', '\r']) && !kept {
                    return Err(format!("text {text:?} at {span:?} reads {s:?}"));
                }
            }
            Kind::Comment(_) | Kind::ProcessingInstruction { .. } | Kind::Doctype { .. }
                if !s.starts_with('<') || s.len() < 2 =>
            {
                return Err(format!("{:?} at {span:?} reads {s:?}", doc.kind(id)));
            }
            _ => {}
        }
        if let Some(end) = doc.end_span(id) {
            let e = slice(end.clone())?;
            if !e.starts_with("</") || !e.ends_with('>') || end.start < span.end {
                return Err(format!("end tag of node {id} at {end:?} reads {e:?}"));
            }
        }
    }
    Ok(spans)
}

#[test]
fn every_span_reads_as_its_node_in_every_tree_construction_input() {
    let mut failures = Vec::new();
    let (mut checked, mut spans) = (0, 0);
    for case in inputs::tree_inputs() {
        // As written, and with every LF a CRLF: the spans are offsets
        // into what was passed, not into the parser's copy.
        for input in [case.data.clone(), case.data.replace('\n', "\r\n")] {
            let doc = parse(&input, &case.fragment);
            checked += 1;
            match check(&doc, &input) {
                Ok(n) => spans += n,
                Err(e) => failures.push(format!("{input:?}: {e}")),
            }
        }
    }
    eprintln!(
        "spans: {checked} parses, {spans} spans checked, {} failed",
        failures.len()
    );
    assert!(
        spans > 3 * checked,
        "only {spans} spans in {checked} parses"
    );
    assert!(
        failures.is_empty(),
        "{} failures, first 20:\n{}",
        failures.len(),
        failures[..failures.len().min(20)].join("\n")
    );
}

/// The first node `doc` has that `want` picks.
fn find(doc: &Document, want: impl Fn(Kind<'_>) -> bool) -> NodeId {
    (0..doc.len() as NodeId)
        .find(|&id| want(doc.kind(id)))
        .expect("the node")
}

fn element(doc: &Document, name: &str) -> NodeId {
    find(
        doc,
        |k| matches!(k, Kind::Element { name: n, .. } if n == name),
    )
}

fn text(doc: &Document, t: &str) -> NodeId {
    find(doc, |k| k == Kind::Text(t))
}

fn read(input: &str, span: Option<std::ops::Range<usize>>) -> Option<&str> {
    span.map(|r| &input[r])
}

#[test]
fn elements_have_their_start_and_end_tags_and_implied_ones_none() {
    let input = "<p class=a>One</p><div><b>x</div>";
    let doc = parse(input, &None);
    let p = element(&doc, "p");
    assert_eq!(read(input, doc.span(p)), Some("<p class=a>"));
    assert_eq!(read(input, doc.end_span(p)), Some("</p>"));
    let div = element(&doc, "div");
    assert_eq!(read(input, doc.end_span(div)), Some("</div>"));
    // `</div>` closed the b too, but it wasn't b's end tag.
    assert_eq!(doc.end_span(element(&doc, "b")), None);
    for implied in ["html", "head", "body"] {
        assert_eq!(doc.span(element(&doc, implied)), None, "{implied}");
    }
    assert_eq!(read(input, doc.span(text(&doc, "One"))), Some("One"));
    // A reopened formatting element is the parser's, not the page's.
    let input = "<b>1<p>2</b>3</p>";
    let doc = parse(input, &None);
    let bs: Vec<_> = (0..doc.len() as NodeId)
        .filter(|&id| doc.lower_name(id) == Some("b"))
        .collect();
    assert_eq!(bs.len(), 2);
    assert_eq!(read(input, doc.span(bs[0])), Some("<b>"));
    assert_eq!(doc.span(bs[1]), None);
}

#[test]
fn attributes_span_name_to_value_quoted_or_not() {
    let input = "<a href=\"/x\" id=y title='t' hidden data-v=\"&amp;\">a</a>";
    let doc = parse(input, &None);
    let a = element(&doc, "a");
    let spans: Vec<_> = (0..5).map(|i| read(input, doc.attr_span(a, i))).collect();
    assert_eq!(
        spans,
        [
            Some("href=\"/x\""),
            Some("id=y"),
            Some("title='t'"),
            Some("hidden"),
            Some("data-v=\"&amp;\"")
        ]
    );
    assert_eq!(doc.attr_span(a, 5), None);
    // The state machine's way, past a character reference.
    let input = "<p X=a&amp;b y = 'z'>";
    let doc = parse(input, &None);
    let p = element(&doc, "p");
    assert_eq!(read(input, doc.attr_span(p, 0)), Some("X=a&amp;b"));
    assert_eq!(read(input, doc.attr_span(p, 1)), Some("y = 'z'"));
}

#[test]
fn text_spans_cover_character_references_and_foster_parented_runs() {
    let input = "<p>a &amp; b&lt;</p>";
    let doc = parse(input, &None);
    assert_eq!(
        read(input, doc.span(text(&doc, "a & b<"))),
        Some("a &amp; b&lt;")
    );
    // Text in a table goes before it, and both runs into one node.
    let input = "<table>x<tr><td>y</td></tr>z</table>";
    let doc = parse(input, &None);
    assert_eq!(
        read(input, doc.span(text(&doc, "xz"))),
        Some("x<tr><td>y</td></tr>z")
    );
    assert_eq!(read(input, doc.span(text(&doc, "y"))), Some("y"));
    assert_eq!(doc.span(element(&doc, "tbody")), None);
}

#[test]
fn offsets_are_into_the_input_as_given_crlf_and_all() {
    let input = "\u{feff}<p>\r\nline\r\n<!-- c --></p>\r<i>é</i>";
    let doc = parse(input, &None);
    let p = element(&doc, "p");
    assert_eq!(read(input, doc.span(p)), Some("<p>"));
    assert_eq!(
        read(input, doc.span(text(&doc, "\nline\n"))),
        Some("\r\nline\r\n")
    );
    let comment = find(&doc, |k| k == Kind::Comment(" c "));
    assert_eq!(read(input, doc.span(comment)), Some("<!-- c -->"));
    assert_eq!(read(input, doc.end_span(p)), Some("</p>"));
    let i = element(&doc, "i");
    assert_eq!(read(input, doc.span(i)), Some("<i>"));
    // A CRLF is one line break and a lone CR another; columns count
    // characters, so the é is one.
    assert_eq!(doc.line_col(doc.span(p).unwrap().start), (1, 1));
    assert_eq!(doc.line_col(doc.span(comment).unwrap().start), (3, 1));
    assert_eq!(doc.line_col(doc.span(i).unwrap().start), (4, 1));
    assert_eq!(doc.line_col(doc.end_span(i).unwrap().start), (4, 5));
    assert_eq!(doc.line_col(doc.end_span(p).unwrap().end), (3, 15));
}

#[test]
fn spans_are_off_unless_asked_for() {
    let doc = netweir_html::parse("<p a=b>x</p>", false);
    assert!((0..doc.len() as NodeId).all(|id| doc.span(id).is_none()));
    assert_eq!(doc.attr_span(element(&doc, "p"), 0), None);
}
