//! One parsed document read from many threads at once.

use netweir_html::css::SelectorList;
use netweir_html::parse;

#[test]
fn one_document_answers_queries_from_many_threads() {
    let html = "<ul>".to_string() + &"<li class=item><a href=/x>x</a></li>".repeat(500) + "</ul>";
    let doc = parse(&html, false);
    let list = SelectorList::parse("li.item > a").unwrap();
    let alone = list.select(&doc, doc.root());
    assert_eq!(alone.len(), 500);
    let seen: Vec<Vec<_>> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..8)
            .map(|_| s.spawn(|| list.select(&doc, doc.root())))
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    assert!(seen.iter().all(|found| *found == alone));
}
