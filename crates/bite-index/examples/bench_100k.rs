//! M1 acceptance: 100k-row ingest + filtered FTS latency.
//! Run: cargo run -p bite-index --example bench_100k --release

use bite_index::{EntityIndex, Record, SearchQuery};
use std::time::Instant;

fn main() {
    let dir = std::env::temp_dir().join(format!("bite-bench-100k-{}", std::process::id()));
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(100_000);

    println!("opening index at {}", dir.display());
    let idx = EntityIndex::open(&dir).unwrap();

    println!("generating + ingesting {n} synthetic records (5k batches)…");
    let t0 = Instant::now();
    for batch_start in (0..n).step_by(5_000) {
        let batch_end = (batch_start + 5_000).min(n);
        let records: Vec<Record> = (batch_start..batch_end)
            .map(|i| Record {
                app: "mail".into(),
                id: format!("m{i}"),
                account: Some("iCloud".into()),
                container: Some(if i % 10 == 0 { "Archive".into() } else { "INBOX".into() }),
                title: Some(format!("Quarterly report number {i}")),
                content: Some(format!(
                    "Message {i}: the quick brown fox discusses invoice {i} with acme corp about payments and scheduling for quarter {}",
                    i % 4
                )),
                participants: Some(format!("sender{i}@example.com")),
                start_ms: Some(1_700_000_000_000 + i as i64 * 60_000),
                end_ms: None,
                updated_ms: Some(1_700_000_000_000 + i as i64 * 60_000),
                read: Some(i % 3 == 0),
                flagged: Some(i % 50 == 0),
                junk: Some(i % 100 == 0),
                completed: None,
                priority: None,
                props: None,
            })
            .collect();
        idx.ingest(&records).unwrap();
        print!("\r  {batch_end}/{n}");
    }
    let ingest = t0.elapsed();
    println!("\ningest: {ingest:.1?} total");

    idx.refresh_fts().unwrap();
    println!("FTS index refreshed");

    let mut times = Vec::new();
    for q in ["invoice 4242", "quarter 3 payments", "zebra"] {
        let t = Instant::now();
        let res = idx.search(&SearchQuery {
            text: Some(q.into()),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
        times.push((q, t.elapsed(), res.hits.len()));
    }
    for (q, d, hits) in &times {
        println!("FTS '{q}': {d:.1?} ({hits} hits)");
    }

    let t = Instant::now();
    let res = idx
        .search(&SearchQuery {
            app: Some("mail".into()),
            read: Some(false),
            limit: 20,
            ..Default::default()
        })
        .unwrap();
    println!(
        "filter-only (unread mail): {:#?} — exact total {} in {:.1?}",
        t.elapsed(),
        res.total.unwrap(),
        t.elapsed()
    );

    let stats = idx.stats().unwrap();
    println!("stats: total={} per_app={:?}", stats.total, stats.per_app);

    std::fs::remove_dir_all(&dir).ok();
    let _ = ingest;
}
