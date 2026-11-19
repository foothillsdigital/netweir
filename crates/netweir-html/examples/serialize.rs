//! Times `outer_html` of a whole parsed document:
//! `cargo run --release --example serialize FILE`.

use std::time::Instant;

fn main() {
    let html = std::fs::read_to_string(std::env::args().nth(1).expect("a file")).unwrap();
    let doc = netweir_html::parse(&html, false);
    let mut best = f64::MAX;
    let mut len = 0;
    for _ in 0..100 {
        let t = Instant::now();
        let out = doc.outer_html(doc.root());
        best = best.min(t.elapsed().as_secs_f64());
        len = out.len();
    }
    println!("{:.3} ms ({len} bytes)", best * 1000.0);
}
