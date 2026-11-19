//! Elements that look like a given one: the other cards in a listing once
//! you've found one card.
//!
//! An element is similar to the one given when it
//!
//! - is another element with the same tag, in the same tree;
//! - sits at the same depth, under ancestors with the same tags level by
//!   level (`html > body > div > ul > li` for an `<li>`); and
//! - shares at least `threshold` of its attribute features, counted as
//!   Jaccard similarity: matching features over all features of the two.
//!   A feature is an attribute's name, or one class from `class`; values
//!   other than classes are ignored, since they differ from card to card
//!   (`href`, `data-id`, `src`). Two elements with no attributes at all
//!   match fully.
//!
//! The tag and ancestor checks run on the document index as integer
//! comparisons, so only real candidates have their attributes read.

use netweir_xpath::Index;

use crate::document::{Node, NodeKind};

/// The share of attribute features a similar element needs by default.
/// `class="card"` against `class="card sale"` is 0.5 and counts;
/// against `class="card sale new"` it is a third and doesn't.
pub const SIMILAR: f64 = 0.5;

/// An element's attribute names and classes, sorted and distinct. The
/// flag marks a class, so a class named like an attribute doesn't match it.
fn features<'a>(node: Node<'a>) -> Vec<(bool, &'a str)> {
    let mut out = Vec::new();
    for (name, value) in node.attrs() {
        if name == "class" {
            out.extend(value.split_ascii_whitespace().map(|c| (true, c)));
        } else {
            out.push((false, name));
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Matching features over all features of two sorted, distinct lists.
fn jaccard<T: Ord>(a: &[T], b: &[T]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let (mut i, mut j, mut shared) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                shared += 1;
                i += 1;
                j += 1;
            }
        }
    }
    shared as f64 / (a.len() + b.len() - shared) as f64
}

impl<'a> Node<'a> {
    /// The other elements that look like this one, in document order (see
    /// the module notes for what "look like" means). Empty for anything
    /// but an element.
    pub fn similar(&self, threshold: f64) -> Vec<Node<'a>> {
        if self.kind() != NodeKind::Element {
            return Vec::new();
        }
        let mine = features(*self);
        same_path(self.doc.index(), self.order())
            .into_iter()
            .map(|raw| Node::from_raw(self.doc, raw))
            .filter(|&n| jaccard(&features(n), &mine) >= threshold)
            .collect()
    }
}

/// The nodes in `at`'s tree, other than `at`, of its kind and tag and
/// with the same tags all the way up, in document order. Tags are compared
/// as ids, so the scan is integer work.
fn same_path(index: &Index, at: u32) -> Vec<u32> {
    let mut top = at;
    while index.parent_of(top) != u32::MAX {
        top = index.parent_of(top);
    }
    (top..index.end_of(top))
        .filter(|&j| {
            j != at
                && index.tag_at(j) == index.tag_at(at)
                && index.kind_at(j) == index.kind_at(at)
                && same_ancestors(index, j, at)
        })
        .map(|j| index.node_at(j))
        .collect()
}

/// Whether the ancestors of `x` and `y` have the same tags, level by
/// level, up to the root of their tree.
fn same_ancestors(index: &Index, mut x: u32, mut y: u32) -> bool {
    loop {
        let (px, py) = (index.parent_of(x), index.parent_of(y));
        // Siblings and cousins meet at a shared ancestor, and the rest
        // of the way up is the same for both.
        if px == py {
            return true;
        }
        if px == u32::MAX || py == u32::MAX {
            return false;
        }
        (x, y) = (px, py);
        if index.tag_at(x) != index.tag_at(y) || index.kind_at(x) != index.kind_at(y) {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Document;

    fn names(nodes: &[Node<'_>]) -> Vec<String> {
        nodes.iter().map(|n| n.text().trim().to_string()).collect()
    }

    const LISTING: &str = r#"<body>
        <aside><div class="card">aside</div></aside>
        <main><section>
          <div class="card"><h2>a</h2></div>
          <div class="card sale"><h2>b</h2></div>
          <div class="card sale new"><h2>c</h2></div>
          <div class="banner">d</div>
          <p class="card">e</p>
        </section>
        <section><div class="card"><h2>f</h2></div></section></main>
        </body>"#;

    #[test]
    fn same_tag_path_and_enough_shared_attributes() {
        let doc = Document::parse(LISTING);
        let a = doc
            .root()
            .descendants()
            .find(|n| n.tag() == Some("div") && n.text() == "a")
            .unwrap();
        // The aside's card has a different path; the banner too few
        // classes in common; the <p> another tag; "c" a third.
        assert_eq!(names(&a.similar(SIMILAR)), ["b", "f"]);
        assert_eq!(names(&a.similar(0.3)), ["b", "c", "f"]);
        assert_eq!(names(&a.similar(1.0)), ["f"]);
        // Every element on the path, at 0.
        assert_eq!(names(&a.similar(0.0)), ["b", "c", "d", "f"]);
    }

    #[test]
    fn jaccard_counts_shared_over_all() {
        let none: [&str; 0] = [];
        assert_eq!(jaccard(&none, &none), 1.0);
        assert_eq!(jaccard(&[".a"], &[".a", ".b"]), 0.5);
        assert_eq!(jaccard(&[".a", "href"], &["href"]), 0.5);
        assert_eq!(jaccard(&[".a"], &["href"]), 0.0);
    }

    #[test]
    fn text_and_documents_have_nothing_similar() {
        let doc = Document::parse("<p>x</p><p>y</p>");
        assert!(doc.root().similar(0.0).is_empty());
        let text = doc
            .root()
            .descendants()
            .find(|n| n.kind() == NodeKind::Text);
        assert!(text.unwrap().similar(0.0).is_empty());
    }
}
