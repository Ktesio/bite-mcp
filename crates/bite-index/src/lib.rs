pub mod store;

use std::path::Path;

pub use store::{
    EntityIndex, Hit, IndexStats, IngestStats, LockError, Record, SearchQuery, SearchResult,
    WriteGuard,
};

/// Ingest every un-ingested `*.jsonl` batch in the staging dir (deleting each
/// file on success — merge-insert makes re-ingest idempotent). Returns the
/// number of batches ingested, the total rows written, and the number of
/// unreadable batches quarantined (moved to `<staging>/quarantine/`).
pub fn ingest_staged(
    index: &EntityIndex,
    staging: &Path,
) -> Result<(usize, usize, usize), store::IndexError> {
    let mut batches = 0usize;
    let mut rows = 0usize;
    let mut quarantined = 0usize;
    let mut files: Vec<_> = std::fs::read_dir(staging)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    files.sort();
    // a single poison batch must not wedge the whole pipeline forever:
    // quarantine unreadable files and keep ingesting the rest
    let quarantine = staging.join("quarantine");
    for file in files {
        match store::parse_jsonl(&file) {
            Ok(records) => {
                let stats = index.ingest(&records)?;
                batches += 1;
                rows += stats.rows;
                std::fs::remove_file(&file).ok();
            }
            Err(e) => {
                eprintln!(
                    "bite-index: quarantining unreadable batch {}: {e}",
                    file.display()
                );
                if std::fs::create_dir_all(&quarantine).is_ok() {
                    let dest = quarantine.join(file.file_name().unwrap_or_default());
                    match std::fs::rename(&file, dest) {
                        Ok(_) => quarantined += 1,
                        Err(re) => {
                            // leave the file in place — it will be retried
                            // (and re-fail visibly) on the next ingest
                            eprintln!("bite-index: could not quarantine {}: {re}", file.display());
                        }
                    }
                }
                // retention cap: keep quarantine/ bounded at 200 files,
                // deleting the OLDEST first by modification time (filename
                // order lies once seq >= 10: "-10" sorts before "-9")
                let mut qfiles: Vec<_> = std::fs::read_dir(&quarantine)
                    .map(|it| {
                        it.filter_map(|e| e.ok())
                            .map(|e| e.path())
                            .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                qfiles.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
                if qfiles.len() > 200 {
                    for p in &qfiles[..qfiles.len() - 200] {
                        std::fs::remove_file(p).ok();
                    }
                }
            }
        }
    }
    Ok((batches, rows, quarantined))
}
