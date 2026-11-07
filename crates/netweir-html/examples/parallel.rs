//! Parses a file on every core at once and compares the rate with one
//! thread: `cargo run --release --example parallel FILE`. Each thread
//! parses its own copy; nothing is shared but the input text.

use std::time::Instant;

fn rate(html: &str, threads: usize, rounds: usize) -> f64 {
    let t = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                for _ in 0..rounds {
                    std::hint::black_box(netweir_html::parse(html, false));
                }
            });
        }
    });
    (threads * rounds) as f64 / t.elapsed().as_secs_f64()
}

fn main() {
    let html = std::fs::read_to_string(std::env::args().nth(1).expect("a file")).unwrap();
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    let rounds = 200;
    let one = rate(&html, 1, rounds);
    let all = rate(&html, cores, rounds);
    println!("1 thread: {one:.0} pages/s");
    println!("{cores} threads: {all:.0} pages/s ({:.1}x)", all / one);
}
