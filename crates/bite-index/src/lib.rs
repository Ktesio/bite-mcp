pub mod store;

use std::path::Path;

pub use store::{
    EntityIndex, Hit, IndexStats, IngestStats, LockError, Record, SearchQuery, SearchResult,
    WriteGuard,
};

/// Numeric trailing seq of a batch filename (`crawl-<job>-<seq>.jsonl` →
/// seq). Lexicographic filename order LIES once seq >= 10 (`crawl-…-10`
/// sorts before `crawl-…-9`), so batch order is decided from the number —
/// keep-last dedup below would otherwise keep the OLDER batch's row.
/// `None` for files without a `-<int>` suffix.
fn batch_seq(path: &Path) -> Option<u64> {
    let stem = path.file_stem()?.to_str()?;
    let (_, seq) = stem.rsplit_once('-')?;
    seq.parse::<u64>().ok()
}

/// Ingest every un-ingested `*.jsonl` batch in the staging dir (deleting each
/// file on success — merge-insert makes re-ingest idempotent). Returns the
/// number of batches ingested, the total rows written, and the number of
/// unreadable batches quarantined (moved to `<staging>/quarantine/`).
pub fn ingest_staged(
    index: &EntityIndex,
    staging: &Path,
) -> Result<(usize, usize, usize), store::IndexError> {
    let mut files: Vec<_> = std::fs::read_dir(staging)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .map(|e| e.path())
                // is_file(): a `*.jsonl`-named DIRECTORY must be skipped, not
                // read-failed and quarantined as a poison batch
                .filter(|p| p.is_file() && p.extension().map(|x| x == "jsonl").unwrap_or(false))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // Oldest batch first, by NUMERIC trailing seq (see `batch_seq`); files
    // without a parseable seq fall back to lexicographic order.
    files.sort_by(|a, b| match (batch_seq(a), batch_seq(b)) {
        (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.cmp(b)),
        _ => a.cmp(b),
    });
    // a single poison batch must not wedge the whole pipeline forever:
    // quarantine unreadable files and keep ingesting the rest
    let quarantine = staging.join("quarantine");
    let mut staged_files: Vec<std::path::PathBuf> = Vec::new();
    let mut all_records: Vec<Record> = Vec::new();
    let mut quarantined = 0usize;
    for file in files {
        match store::parse_jsonl(&file) {
            Ok(records) => {
                staged_files.push(file.clone());
                all_records.extend(records);
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
    // Normalize `account` before dedup/merge: Mail message ids are
    // ACCOUNT-scoped integers, so the merge key must include the account.
    // The JSONL contract keeps `account` optional, but a missing account is
    // normalized to "" (never NULL) here — NULL values in a merge-insert
    // key column never match, so a NULL-account row could never be upserted.
    for r in &mut all_records {
        if r.account.is_none() {
            r.account = Some(String::new());
        }
    }
    // Cross-batch dedup by (app, account, id), keeping the LAST occurrence:
    // the crawler legitimately produces duplicate keys across batch files
    // WITHIN one account (overlapping windows, restarts), and merge-insert
    // PROHIBITS two source rows matching the same target key — without
    // this, one duplicated message wedges the entire ingest forever. The
    // account is part of the key: two accounts holding id "4127" are two
    // distinct messages, and the old (app, id) key silently collapsed
    // cross-account mail into one row. "Last" is the NEWEST batch because
    // files are ordered by numeric seq above. First-seen order is preserved
    // for determinism.
    let mut key_index: std::collections::HashMap<(&str, &str, &str), usize> =
        std::collections::HashMap::new();
    let mut unique: Vec<Record> = Vec::with_capacity(all_records.len());
    for r in &all_records {
        let key = (
            r.app.as_str(),
            r.account.as_deref().unwrap_or(""),
            r.id.as_str(),
        );
        match key_index.get(&key) {
            Some(&i) => unique[i] = r.clone(),
            None => {
                key_index.insert(key, unique.len());
                unique.push(r.clone());
            }
        }
    }
    let dedup_dropped = all_records.len() - unique.len();
    if dedup_dropped > 0 {
        eprintln!(
            "bite-index: deduplicated {dedup_dropped} duplicate (app,account,id) rows across batches"
        );
    }
    let stats = index.ingest(&unique)?;
    for file in &staged_files {
        std::fs::remove_file(file).ok();
    }
    let batches = staged_files.len();
    Ok((batches, stats.rows, quarantined))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{EntityIndex, Record, SearchQuery};

    fn mail_record(id: &str, account: Option<&str>, title: &str) -> Record {
        Record {
            app: "mail".into(),
            id: id.into(),
            account: account.map(String::from),
            container: Some("INBOX".into()),
            title: Some(title.into()),
            content: None,
            participants: None,
            start_ms: Some(1_700_000_000_000),
            end_ms: None,
            updated_ms: Some(1_700_000_000_000),
            read: Some(false),
            flagged: Some(false),
            junk: None,
            completed: None,
            priority: None,
            props: None,
        }
    }

    fn temp_staging(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "bite-ingest-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_batch(staging: &Path, name: &str, records: &[Record]) {
        let lines: Vec<String> = records
            .iter()
            .map(|r| serde_json::to_string(r).unwrap())
            .collect();
        std::fs::write(staging.join(name), format!("{}\n", lines.join("\n"))).unwrap();
    }

    fn mail_hits(idx: &EntityIndex) -> Vec<(String, String)> {
        let res = idx
            .search(&SearchQuery {
                app: Some("mail".into()),
                limit: 50,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(res.hits.len(), idx.count(Some("app = 'mail'")).unwrap());
        res.hits
            .iter()
            .map(|h| {
                (
                    h.account.clone().unwrap_or_default(),
                    h.title.clone().unwrap_or_default(),
                )
            })
            .collect()
    }

    #[test]
    fn cross_account_same_id_stays_two_rows() {
        // Mail AppleScript ids are ACCOUNT-scoped integers: account "Work"
        // and account "iCloud" both holding id "4127" are two distinct
        // messages. The old (app, id) merge/dedup key collapsed them.
        let staging = temp_staging("xacct");
        let idx = EntityIndex::open(&staging.join("index.lance")).unwrap();
        write_batch(
            &staging,
            "crawl-1-0.jsonl",
            &[
                mail_record("4127", Some("Work"), "from Work"),
                mail_record("4127", Some("iCloud"), "from iCloud"),
            ],
        );
        let (batches, _rows, quarantined) = ingest_staged(&idx, &staging).unwrap();
        assert_eq!((batches, quarantined), (1, 0));
        assert_eq!(idx.count(Some("app = 'mail'")).unwrap(), 2);

        // Re-ingest with the records in REVERSE order (both keys refreshed
        // by a later crawl): still 2 rows, each carrying its per-account
        // value — neither account's copy may absorb the other's.
        write_batch(
            &staging,
            "crawl-1-1.jsonl",
            &[
                mail_record("4127", Some("iCloud"), "from iCloud v2"),
                mail_record("4127", Some("Work"), "from Work v2"),
            ],
        );
        let _ = ingest_staged(&idx, &staging).unwrap();
        assert_eq!(idx.count(Some("app = 'mail'")).unwrap(), 2);
        let mut got = mail_hits(&idx);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("Work".to_string(), "from Work v2".to_string()),
                ("iCloud".to_string(), "from iCloud v2".to_string()),
            ]
        );
        std::fs::remove_dir_all(&staging).ok();
    }

    #[test]
    fn dedup_keep_last_orders_by_numeric_seq() {
        // `crawl-…-10` sorts BEFORE `crawl-…-9` lexicographically; keep-last
        // dedup must still keep the -10 (NEWER) batch's row.
        let staging = temp_staging("seq");
        let idx = EntityIndex::open(&staging.join("index.lance")).unwrap();
        // write the OLDER batch last so read_dir order cannot mask the fix
        write_batch(
            &staging,
            "crawl-1-10.jsonl",
            &[mail_record("77", Some("Work"), "newer")],
        );
        write_batch(
            &staging,
            "crawl-1-9.jsonl",
            &[mail_record("77", Some("Work"), "older")],
        );
        let _ = ingest_staged(&idx, &staging).unwrap();
        assert_eq!(idx.count(Some("app = 'mail'")).unwrap(), 1);
        let got = mail_hits(&idx);
        assert_eq!(got, vec![("Work".to_string(), "newer".to_string())]);
        std::fs::remove_dir_all(&staging).ok();
    }

    #[test]
    fn missing_account_normalizes_to_empty_string() {
        // JSONL keeps `account` optional; a record without one must still be
        // upsertable — normalized to "" (NULL merge-key values never match).
        let staging = temp_staging("acct");
        let idx = EntityIndex::open(&staging.join("index.lance")).unwrap();
        let line = r#"{"app":"mail","id":"5","title":"no account"}"#;
        std::fs::write(staging.join("crawl-1-0.jsonl"), format!("{line}\n")).unwrap();
        let _ = ingest_staged(&idx, &staging).unwrap();
        // re-ingest the same record (fresh file — merge key must match)
        std::fs::write(staging.join("crawl-1-1.jsonl"), format!("{line}\n")).unwrap();
        let _ = ingest_staged(&idx, &staging).unwrap();
        assert_eq!(idx.count(Some("app = 'mail'")).unwrap(), 1);
        assert_eq!(idx.count(Some("account = ''")).unwrap(), 1);
        std::fs::remove_dir_all(&staging).ok();
    }

    #[test]
    fn jsonl_named_directory_is_skipped_not_quarantined() {
        let staging = temp_staging("dir");
        let idx = EntityIndex::open(&staging.join("index.lance")).unwrap();
        std::fs::create_dir_all(staging.join("weird.jsonl")).unwrap();
        let (batches, _rows, quarantined) = ingest_staged(&idx, &staging).unwrap();
        assert_eq!((batches, quarantined), (0, 0));
        assert!(staging.join("weird.jsonl").is_dir(), "directory untouched");
        std::fs::remove_dir_all(&staging).ok();
    }
}
