//! Regression guard for the staged-ingest memory bound.
//!
//! `ingest_staged` makes two passes over the staged batch files: a key pass
//! that keeps only each record's `(app, account, id)` key, then a write pass
//! that re-reads one file at a time and writes the keep-last survivors 512
//! rows at a time. Its peak is therefore bounded by ONE staged file plus one
//! write chunk, however many files the crawl produced. The in-repo bench
//! (`examples/bench_100k.rs`) never runs that path — it builds record vectors
//! and calls `EntityIndex::ingest` directly — so a change that buffers the
//! whole crawl again passed every check.
//!
//! What is measured: the peak number of LIVE bytes, counted at the allocator
//! by a `#[global_allocator]` wrapper, between entering and leaving one
//! `ingest_staged` call. Allocator accounting needs no child process and
//! reads the same on every platform; an OS high-water mark (Linux `VmHWM`,
//! macOS `ru_maxrss`) is a process-lifetime maximum in platform-specific
//! units, so it would only ever show the first peak of the test binary and
//! jitter with the runner.
//!
//! The bound is RELATIVE, so it needs no machine-specific byte figure: one
//! call over many staged files may cost at most `FACTOR` times one call over
//! a single file of the same size on a fresh index. The single-file peak is
//! one file parsed and written (the parsed vector, its text and the write
//! chunk); only the key list and the store's own bookkeeping legitimately
//! grow with the file count, and both are a small fraction of one file. A
//! whole-crawl buffer makes the many-file peak about `FILES` times the
//! single-file one, which `FACTOR` rejects.
//!
//! Calibration (one Linux dev box, this file's constants): 1.3x while
//! streaming, 4.8x with the write pass holding the whole crawl until the
//! end — `FACTOR` sits between the two.

use bite_index::{ingest_staged, EntityIndex, Record};
use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Staged files in the crawl run. Each is one write chunk, so a crawl-sized
/// buffer stands out `FILES`-fold against the single-file run.
const FILES: usize = 32;
/// Rows per staged file: exactly one write chunk ([`bite_index`]'s flush
/// size is 512), so the single-file run is one parse plus one write.
const ROWS_PER_FILE: usize = 512;
/// Body size per record. Large enough that one file's parsed records dwarf
/// the per-row key list and the store's bookkeeping.
const BODY_BYTES: usize = 4 * 1024;
/// How many times the single-file peak one crawl-wide call may cost.
const FACTOR: u64 = 3;

static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);

fn grow(by: u64) {
    let live = LIVE.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(live, Ordering::Relaxed);
}

/// Counts live bytes and their peak across every thread of this test binary.
struct PeakAlloc;

unsafe impl GlobalAlloc for PeakAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            grow(layout.size() as u64);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let ptr = System.realloc(ptr, layout, new_size);
        if !ptr.is_null() {
            let (old, new) = (layout.size() as u64, new_size as u64);
            if new >= old {
                grow(new - old);
            } else {
                LIVE.fetch_sub(old - new, Ordering::Relaxed);
            }
        }
        ptr
    }
}

#[global_allocator]
static ALLOC: PeakAlloc = PeakAlloc;

/// Run `f` and report how far the live-byte count peaked above where it
/// stood on entry.
fn peak_live_during<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let start = LIVE.load(Ordering::Relaxed);
    PEAK.store(start, Ordering::Relaxed);
    let out = f();
    let peak = PEAK.load(Ordering::Relaxed);
    (out, peak.saturating_sub(start))
}

fn record(id: usize, body: &str) -> Record {
    Record {
        app: "mail".into(),
        id: format!("m{id}"),
        account: Some("iCloud".into()),
        container: Some("INBOX".into()),
        title: Some(format!("Quarterly report number {id}")),
        content: Some(body.to_string()),
        participants: Some(format!("sender{id}@example.com")),
        start_ms: Some(1_700_000_000_000 + id as i64 * 60_000),
        end_ms: None,
        updated_ms: Some(1_700_000_000_000 + id as i64 * 60_000),
        read: Some(false),
        flagged: Some(false),
        junk: None,
        completed: None,
        priority: None,
        props: None,
    }
}

/// Write `files` staged batch files (`crawl-<job>-0.jsonl`, one chunk each)
/// under `staging`, with distinct record ids across files.
fn stage(staging: &Path, files: usize) {
    std::fs::create_dir_all(staging).unwrap();
    let body = format!("{}{}", "staged body ", "x".repeat(BODY_BYTES));
    for file in 0..files {
        let mut text = String::with_capacity(ROWS_PER_FILE * (BODY_BYTES + 256));
        for row in 0..ROWS_PER_FILE {
            text.push_str(
                &serde_json::to_string(&record(file * ROWS_PER_FILE + row, &body)).unwrap(),
            );
            text.push('\n');
        }
        std::fs::write(
            staging.join(format!("crawl-{}-0.jsonl", 1_700_000_000 + file)),
            text,
        )
        .unwrap();
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

#[test]
fn staged_ingest_peak_is_one_file_not_the_whole_crawl() {
    let single = tempfile::Builder::new()
        .prefix("bite-staged-mem-one-")
        .tempdir()
        .unwrap();
    let crawl = tempfile::Builder::new()
        .prefix("bite-staged-mem-crawl-")
        .tempdir()
        .unwrap();
    // Staging happens before anything is measured, and nothing generated
    // here is alive when the crawl run starts.
    stage(&single.path().join("staging"), 1);
    stage(&crawl.path().join("staging"), FILES);

    let one = EntityIndex::open(&single.path().join("index.lance")).unwrap();
    let (stats, peak_one) =
        peak_live_during(|| ingest_staged(&one, &single.path().join("staging"), &[]).unwrap());
    assert_eq!(stats.1, ROWS_PER_FILE, "the single file wrote its rows");
    drop(one);

    let many = EntityIndex::open(&crawl.path().join("index.lance")).unwrap();
    let (stats, peak_crawl) =
        peak_live_during(|| ingest_staged(&many, &crawl.path().join("staging"), &[]).unwrap());
    assert_eq!(stats.0, FILES, "every staged file was ingested");
    assert_eq!(
        stats.1,
        FILES * ROWS_PER_FILE,
        "every row of the crawl was written"
    );
    let ratio = peak_crawl as f64 / peak_one.max(1) as f64;
    println!(
        "staged ingest peak live bytes: one file {:.1} MiB, {FILES} files {:.1} MiB ({ratio:.2}x, limit {FACTOR}x)",
        mib(peak_one),
        mib(peak_crawl),
    );
    assert!(
        peak_crawl <= peak_one.saturating_mul(FACTOR),
        "one ingest_staged call over the whole crawl cost more than FACTOR times one file: the crawl is being buffered again"
    );
}
