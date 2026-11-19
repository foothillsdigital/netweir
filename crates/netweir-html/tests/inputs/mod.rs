//! Every input the conformance suites have, for tests that check a
//! property of the parse rather than the tree each case expects.

use netweir_html::Ns;

/// A tree-construction case's input: its `#data`, and its context
/// element when it's a fragment case.
pub struct TreeInput {
    pub data: String,
    pub fragment: Option<(Ns, String)>,
}

pub fn tree_inputs() -> Vec<TreeInput> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/tree-construction");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "dat"))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        // `#data` runs to the `#errors` line after it; a fragment case
        // names its context after `#document-fragment`.
        for case in text.split("#data\n").skip(1) {
            let Some((data, rest)) = case.split_once("\n#errors") else {
                continue;
            };
            let fragment = rest
                .split_once("#document-fragment\n")
                .and_then(|(_, f)| f.lines().next())
                .map(|context| match context.trim().split_once(' ') {
                    Some(("svg", n)) => (Ns::Svg, n.to_string()),
                    Some(("math", n)) => (Ns::MathMl, n.to_string()),
                    _ => (Ns::Html, context.trim().to_string()),
                });
            out.push(TreeInput {
                data: data.to_string(),
                fragment,
            });
        }
    }
    out
}
