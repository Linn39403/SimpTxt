//! Reproducible, in-memory performance measurements. No timing assertions.
//!
//! cargo run --release --no-default-features --example performance_probe -- 100000000
//! cargo run --release --no-default-features --example performance_probe -- 1000000000
//! cargo run --release --no-default-features --example performance_probe -- --history
//!
//! Run --history in its own process: allocator reuse after a large-file probe
//! would hide resident-memory growth. Output is JSON; timings are not CI gates.
use simptxt::{
    document::{CHUNK_SIZE, Document, HISTORY_BYTE_LIMIT, resident_document_bytes},
    search,
    syntax::{HighlightIndex, Language, ViewportCache},
};
use std::{
    hint::black_box,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Instant,
};

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn proc_kib(field: &str) -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix(field)
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|value| value.parse::<u64>().ok())
            })
        })
        .map_or_else(|| "null".to_owned(), |value| value.to_string())
}

fn history_sample(doc: &Document, step: usize) -> String {
    format!(
        "{{\"step\":{step},\"logical_bytes\":{},\"tracked_resident_bytes\":{},\"history_bytes\":{},\"history_evictions\":{},\"rss_kib\":{},\"peak_rss_kib\":{}}}",
        doc.len(),
        resident_document_bytes(),
        doc.history_bytes(),
        doc.history_evictions(),
        proc_kib("VmRSS:"),
        proc_kib("VmHWM:")
    )
}

fn history_probe() {
    let a = "a".repeat(8 * 1024 * 1024);
    let b = "b".repeat(a.len());
    let mut doc = Document::from_bytes(a.as_bytes().to_vec()).unwrap();
    let mut samples = vec![history_sample(&doc, 0)];
    for step in 1..=40 {
        let text = if step % 2 == 1 { &b } else { &a };
        doc.replace(0..doc.len(), text).unwrap();
        assert_eq!(doc.read(0..1), text.as_bytes()[..1]);
        if step % 4 == 0 {
            samples.push(history_sample(&doc, step));
        }
    }
    let mut undo_count = 0;
    while doc.undo() {
        undo_count += 1;
    }
    println!(
        "{{\n  \"schema\":1,\n  \"workload\":\"history-fresh-process\",\n  \"history_limit_bytes\":{HISTORY_BYTE_LIMIT},\n  \"replacement_bytes\":{},\n  \"edits\":40,\n  \"samples\":[{}],\n  \"available_undo_count\":{undo_count}\n}}",
        a.len(),
        samples.join(",\n    ")
    );
}

fn main() {
    let argument = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "100000000".to_owned());
    if argument == "--history" {
        history_probe();
        return;
    }
    let n: usize = argument.parse().expect("use a byte count or --history");
    assert!(
        (10_000..=1_000_000_000).contains(&n),
        "probe is capped at 1 GB"
    );
    let seed = b"fn main() { let value = 42; } // needle\n";
    let chunks: Vec<Arc<[u8]>> = (0..n)
        .step_by(CHUNK_SIZE)
        .map(|at| {
            (0..CHUNK_SIZE.min(n - at))
                .map(|j| seed[(at + j) % seed.len()])
                .collect::<Vec<_>>()
                .into()
        })
        .collect();
    let mut doc = Document::from_chunks(chunks).unwrap();
    let cancel = AtomicBool::new(false);
    let progress = AtomicUsize::new(0);
    let start = Instant::now();
    let forward = search::search(&doc, "needle", n - 100, false, &cancel, &progress).unwrap();
    let forward_ms = elapsed_ms(start);
    let forward_inspected = progress.load(Ordering::Relaxed);
    assert!(forward.is_some());
    let start = Instant::now();
    let reverse = search::search(&doc, "needle", n, true, &cancel, &progress).unwrap();
    let reverse_ms = elapsed_ms(start);
    let reverse_inspected = progress.load(Ordering::Relaxed);
    assert!(reverse.is_some());
    let start = Instant::now();
    let mut index = HighlightIndex::build(&doc, Language::Rust, &cancel).unwrap();
    let full_ms = elapsed_ms(start);
    let first = doc.line_count() / 2;
    let rows: Vec<_> = (first..first + 40)
        .map(|line| doc.line_start(line)..doc.line_start(line + 1))
        .collect();
    let start = Instant::now();
    for _ in 0..100 {
        for row in &rows {
            black_box(index.spans(&doc, row.clone()).unwrap());
        }
    }
    let row_ms = elapsed_ms(start) / 100.0;
    let start = Instant::now();
    for _ in 0..100 {
        black_box(index.spans_batch(&doc, &rows).unwrap());
    }
    let batch_ms = elapsed_ms(start) / 100.0;
    let mut cache = ViewportCache::default();
    cache.prepare(&doc, Some(&index), &rows);
    let start = Instant::now();
    for _ in 0..10_000 {
        cache.prepare(&doc, Some(&index), &rows);
        for row in 0..40 {
            black_box(cache.row(row));
        }
    }
    let cached_ms = elapsed_ms(start) / 10_000.0;
    let mut edit_doc = doc.snapshot();
    let start = Instant::now();
    for _ in 0..100 {
        let at = edit_doc.len() / 2;
        edit_doc.replace(at..at, "x").unwrap();
        assert!(edit_doc.undo());
    }
    let insert_undo_ms = elapsed_ms(start) / 100.0;
    drop(edit_doc);
    let at = doc.line_start(first) + 19;
    let mut times: Vec<f64> = Vec::new();
    let mut max_scanned = 0;
    for turn in 0..100 {
        if turn % 2 == 0 {
            doc.replace(at..at, "x").unwrap();
        } else {
            doc.replace(at..at + 1, "").unwrap();
        }
        let start = Instant::now();
        index = HighlightIndex::refresh(&doc, Language::Rust, Some(&index), doc.len(), &cancel)
            .unwrap();
        times.push(elapsed_ms(start));
        max_scanned = max_scanned.max(index.scanned_bytes());
    }
    let incremental_mean = times.iter().sum::<f64>() / times.len() as f64;
    times.sort_by(f64::total_cmp);
    println!(
        "{{\n  \"schema\":1,\n  \"workload\":\"ascii-rust-source\",\n  \"bytes\":{n},\n  \"iterations\":100,\n  \"forward_search_ms\":{forward_ms:.6},\n  \"forward_search_inspected\":{forward_inspected},\n  \"reverse_search_ms\":{reverse_ms:.6},\n  \"reverse_search_inspected\":{reverse_inspected},\n  \"full_syntax_ms\":{full_ms:.6},\n  \"per_row_40_mean_ms\":{row_ms:.6},\n  \"batched_40_mean_ms\":{batch_ms:.6},\n  \"cached_40_mean_ms\":{cached_ms:.6},\n  \"insert_undo_mean_ms\":{insert_undo_ms:.6},\n  \"incremental_mean_ms\":{incremental_mean:.6},\n  \"incremental_p95_ms\":{:.6},\n  \"incremental_max_ms\":{:.6},\n  \"incremental_max_scanned_bytes\":{max_scanned},\n  \"tracked_resident_bytes\":{},\n  \"rss_kib\":{},\n  \"peak_rss_kib\":{}\n}}",
        times[94],
        times[99],
        resident_document_bytes(),
        proc_kib("VmRSS:"),
        proc_kib("VmHWM:")
    );
}
