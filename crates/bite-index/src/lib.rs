pub mod store;
#[cfg(test)]
pub mod test_support;

use std::path::Path;

pub use store::{
    EntityIndex, Hit, IndexStats, IngestStats, LockError, Record, SearchQuery, SearchResult,
    WriteGuard,
};

/// One recorded `index_forget` scope. Records whose (app, container) match a
/// recorded scope are SKIPPED at ingest, so a deliberate purge survives the
/// full re-crawls that would otherwise re-import the same rows from Apple
/// (`index_rebuild full:true`, `index_wipe` + rebuild, the 365-day backfill).
///
/// Matching mirrors the purge's delete filter exactly: a `None` field matches
/// any value, a `Some` field is an exact (case-sensitive) equality, and a
/// `Some(container)` never matches a record with no container — so "rows a
/// scope filter would delete" and "rows the scope skips at ingest" are the
/// same set. `recorded_ms` is operator context only (never consulted for
/// matching), and `index_wipe` clears the whole set: it is the complete
/// reset, including the purge record.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PurgeScope {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_ms: Option<i64>,
}

impl PurgeScope {
    /// True when a record with this app/container falls inside the scope.
    pub fn matches(&self, app: &str, container: Option<&str>) -> bool {
        if let Some(a) = &self.app {
            if a != app {
                return false;
            }
        }
        if let Some(c) = &self.container {
            if container != Some(c.as_str()) {
                return false;
            }
        }
        true
    }

    /// Same app+container (the identity that matters — `recorded_ms` is not
    /// part of it, so re-forgetting a scope never duplicates the entry).
    pub fn same_scope(&self, app: Option<&str>, container: Option<&str>) -> bool {
        self.app.as_deref() == app && self.container.as_deref() == container
    }
}

/// Total-order sort key for batch files: `(job_ts, seq, path)`. Batch names
/// are `crawl-<job_ts>-<seq>.jsonl`; ordering by the job timestamp FIRST
/// keeps a dead crashed job's leftovers (`crawl-<T1>-40`) from out-ranking
/// the live job's batches (`crawl-<T2>-0`) after a rebuild — seq-first
/// would invert them and keep-last dedup would keep the DEAD job's row for
/// overlapping keys. Within a job the numeric seq orders correctly
/// (lexicographic filename order lies once seq >= 10). Names that don't
/// parse as `<…>-<int>-<int>` sort LAST (u64::MAX sentinels). The key is a
/// TOTAL order, so `sort_by_key` can never panic on an inconsistent
/// comparator (Rust >= 1.81 aborts non-total `sort_by`s — a panic here
/// would poison the caller's INDEX mutex).
fn batch_order_key(path: &Path) -> (u64, u64, std::path::PathBuf) {
    const UNPARSED: u64 = u64::MAX;
    let (job_ts, seq) = path
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|stem| {
            // last TWO `-<int>` groups of the stem: (timestamp, seq)
            let (head, seq) = stem.rsplit_once('-')?;
            let (_, ts) = head.rsplit_once('-')?;
            Some((ts.parse::<u64>().ok()?, seq.parse::<u64>().ok()?))
        })
        .unwrap_or((UNPARSED, UNPARSED));
    (job_ts, seq, path.to_path_buf())
}

/// Ingest every un-ingested `*.jsonl` batch in the staging dir (deleting each
/// file on success — merge-insert makes re-ingest idempotent). Returns the
/// number of batches ingested, the total rows written, the number of
/// unreadable batches quarantined (moved to `<staging>/quarantine/`), and the
/// rows dropped because they matched a recorded purge scope (`index_forget`)
/// — purged scopes must not come back with the next full crawl.
pub fn ingest_staged(
    index: &EntityIndex,
    staging: &Path,
    purge_scopes: &[PurgeScope],
) -> Result<(usize, usize, usize, usize), store::IndexError> {
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
    // Oldest batch first: by job timestamp, then numeric seq within the
    // job (see `batch_order_key` for why both matter).
    files.sort_by_key(|p| batch_order_key(p));
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
                    // creation-time privacy: the MCP server process may long
                    // outlive its startup enforcement pass, so a quarantine/
                    // first created mid-session must not sit at umask
                    // defaults until the next startup
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        &quarantine,
                        std::fs::Permissions::from_mode(0o700),
                    );
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
    // Purge-scope tombstones: drop rows inside a recorded `index_forget`
    // scope BEFORE the upsert. Applied after dedup (fewer comparisons) and
    // to the dedup SURVIVOR so a newer batch can never resurrect a purged
    // key through keep-last. Empty slice = no tombstones recorded = the
    // historical behavior (everything ingests).
    let skipped_purged = if purge_scopes.is_empty() {
        0
    } else {
        let before = unique.len();
        unique.retain(|r| {
            !purge_scopes
                .iter()
                .any(|s| s.matches(&r.app, r.container.as_deref()))
        });
        let skipped = before - unique.len();
        if skipped > 0 {
            eprintln!(
                "bite-index: skipped {skipped} rows matching recorded purge scopes (index_forget)"
            );
        }
        skipped
    };
    let stats = index.ingest(&unique)?;
    for file in &staged_files {
        std::fs::remove_file(file).ok();
    }
    let batches = staged_files.len();
    Ok((batches, stats.rows, quarantined, skipped_purged))
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

    /// Self-cleaning staging dir (removed on drop — panics can't leak it).
    fn temp_staging(tag: &str) -> crate::test_support::ScratchDir {
        crate::test_support::scratch(&format!("bite-ingest-{tag}-"))
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
        let (batches, _rows, quarantined, _skipped) = ingest_staged(&idx, &staging, &[]).unwrap();
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
        let _ = ingest_staged(&idx, &staging, &[]).unwrap();
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
    }

    #[test]
    fn dedup_keep_last_orders_by_numeric_seq() {
        // Within one job, `crawl-…-10` sorts BEFORE `crawl-…-9`
        // lexicographically; keep-last dedup must still keep the -10
        // (NEWER) batch's row.
        let staging = temp_staging("seq");
        let idx = EntityIndex::open(&staging.join("index.lance")).unwrap();
        // write the OLDER batch last so read_dir order cannot mask the fix
        write_batch(
            &staging,
            "crawl-1790958250-10.jsonl",
            &[mail_record("77", Some("Work"), "newer")],
        );
        write_batch(
            &staging,
            "crawl-1790958250-9.jsonl",
            &[mail_record("77", Some("Work"), "older")],
        );
        let _ = ingest_staged(&idx, &staging, &[]).unwrap();
        assert_eq!(idx.count(Some("app = 'mail'")).unwrap(), 1);
        let got = mail_hits(&idx);
        assert_eq!(got, vec![("Work".to_string(), "newer".to_string())]);
    }

    #[test]
    fn dedup_keep_last_orders_dead_job_behind_live_job() {
        // After a crash → rebuild, a dead job's leftover `crawl-<T1>-40`
        // must rank BEHIND the live job's `crawl-<T2>-0` (T2 > T1): a
        // seq-first sort inverted them, and keep-last kept the DEAD job's
        // row for overlapping keys.
        let staging = temp_staging("jobs");
        let idx = EntityIndex::open(&staging.join("index.lance")).unwrap();
        let dead = 1790958250u64;
        let live = dead + 60;
        // write the DEAD job's batch last so read_dir order cannot mask the fix
        write_batch(
            &staging,
            &format!("crawl-{live}-0.jsonl"),
            &[mail_record("77", Some("Work"), "live job")],
        );
        write_batch(
            &staging,
            &format!("crawl-{dead}-40.jsonl"),
            &[mail_record("77", Some("Work"), "dead job")],
        );
        let _ = ingest_staged(&idx, &staging, &[]).unwrap();
        assert_eq!(idx.count(Some("app = 'mail'")).unwrap(), 1);
        let got = mail_hits(&idx);
        assert_eq!(got, vec![("Work".to_string(), "live job".to_string())]);
    }

    #[test]
    fn malformed_batch_names_no_panic_and_sort_last() {
        // A hand-crafted name with a non-numeric tail (`-2zz`) must not be
        // able to break the ordering — a non-total comparator panics
        // `sort_by` on Rust >= 1.81, which would poison the caller's INDEX
        // mutex. `sort_by_key` over a total-order key cannot; unparseable
        // names sort LAST (pinned via keep-last: they win key overlaps).
        let staging = temp_staging("malformed");
        let idx = EntityIndex::open(&staging.join("index.lance")).unwrap();
        let ts = 1790958300u64;
        write_batch(
            &staging,
            &format!("crawl-{ts}-1.jsonl"),
            &[mail_record("5", Some("Work"), "valid")],
        );
        write_batch(
            &staging,
            &format!("crawl-{ts}-2zz.jsonl"),
            &[mail_record("5", Some("Work"), "malformed")],
        );
        write_batch(
            &staging,
            "notes.jsonl",
            &[mail_record("6", Some("Work"), "no seq at all")],
        );
        let (batches, _rows, _quarantined, _skipped) = ingest_staged(&idx, &staging, &[]).unwrap();
        assert_eq!(batches, 3);
        assert_eq!(idx.count(Some("app = 'mail'")).unwrap(), 2);
        let mut got = mail_hits(&idx);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("Work".to_string(), "malformed".to_string()),
                ("Work".to_string(), "no seq at all".to_string()),
            ]
        );
    }

    #[test]
    fn missing_account_normalizes_to_empty_string() {
        // JSONL keeps `account` optional; a record without one must still be
        // upsertable — normalized to "" (NULL merge-key values never match).
        let staging = temp_staging("acct");
        let idx = EntityIndex::open(&staging.join("index.lance")).unwrap();
        let line = r#"{"app":"mail","id":"5","title":"no account"}"#;
        std::fs::write(staging.join("crawl-1-0.jsonl"), format!("{line}\n")).unwrap();
        let _ = ingest_staged(&idx, &staging, &[]).unwrap();
        // re-ingest the same record (fresh file — merge key must match)
        std::fs::write(staging.join("crawl-1-1.jsonl"), format!("{line}\n")).unwrap();
        let _ = ingest_staged(&idx, &staging, &[]).unwrap();
        assert_eq!(idx.count(Some("app = 'mail'")).unwrap(), 1);
        assert_eq!(idx.count(Some("account = ''")).unwrap(), 1);
    }

    #[test]
    fn jsonl_named_directory_is_skipped_not_quarantined() {
        let staging = temp_staging("dir");
        let idx = EntityIndex::open(&staging.join("index.lance")).unwrap();
        std::fs::create_dir_all(staging.join("weird.jsonl")).unwrap();
        let (batches, _rows, quarantined, _skipped) = ingest_staged(&idx, &staging, &[]).unwrap();
        assert_eq!((batches, quarantined), (0, 0));
        assert!(staging.join("weird.jsonl").is_dir(), "directory untouched");
    }

    fn junk_record(id: &str, title: &str) -> Record {
        let mut r = mail_record(id, Some("Work"), title);
        r.container = Some("Junk".into());
        r
    }

    #[test]
    fn purge_scope_matching_mirrors_the_delete_filter() {
        let scope = |app: Option<&str>, container: Option<&str>| PurgeScope {
            app: app.map(String::from),
            container: container.map(String::from),
            recorded_ms: None,
        };
        // app+container: exact both ways
        let s = scope(Some("mail"), Some("Junk"));
        assert!(s.matches("mail", Some("Junk")));
        assert!(!s.matches("mail", Some("INBOX")));
        assert!(!s.matches("notes", Some("Junk")));
        // a Some container never matches a containerless record (same as
        // SQL `container = 'Junk'` never matching NULL)
        assert!(!s.matches("mail", None));
        // app-only scope: any container, including none
        let app_only = scope(Some("mail"), None);
        assert!(app_only.matches("mail", Some("INBOX")));
        assert!(app_only.matches("mail", None));
        assert!(!app_only.matches("calendar", Some("INBOX")));
        // container-only scope: any app
        let c_only = scope(None, Some("Junk"));
        assert!(c_only.matches("mail", Some("Junk")));
        assert!(c_only.matches("notes", Some("Junk")));
        assert!(!c_only.matches("mail", Some("junk")), "case-sensitive");
        // identity ignores recorded_ms: re-forgetting never duplicates
        assert!(scope(Some("mail"), Some("Junk")).same_scope(Some("mail"), Some("Junk")));
        assert!(!scope(Some("mail"), None).same_scope(Some("mail"), Some("Junk")));
    }

    #[test]
    fn recorded_purge_scope_survives_reingest_and_newer_batches() {
        let staging = temp_staging("purge");
        let idx = EntityIndex::open(&staging.join("index.lance")).unwrap();
        write_batch(
            &staging,
            "crawl-1-0.jsonl",
            &[
                mail_record("10", Some("Work"), "kept inbox"),
                junk_record("11", "purged junk"),
            ],
        );
        let scopes = vec![PurgeScope {
            app: Some("mail".into()),
            container: Some("Junk".into()),
            recorded_ms: None,
        }];
        let (batches, rows, quarantined, skipped) = ingest_staged(&idx, &staging, &scopes).unwrap();
        assert_eq!((batches, quarantined, skipped), (1, 0, 1));
        assert_eq!(rows, 1, "only the inbox row was written");
        assert_eq!(idx.count(Some("app = 'mail'")).unwrap(), 1);
        assert_eq!(idx.count(Some("container = 'Junk'")).unwrap(), 0);

        // a LATER full crawl re-delivers the junk row (and re-touches the
        // kept one): the tombstone must still drop it — this is the exact
        // scenario the old "reset, not retention" wording admitted to
        write_batch(
            &staging,
            "crawl-2-0.jsonl",
            &[
                junk_record("11", "purged junk v2"),
                mail_record("10", Some("Work"), "kept inbox v2"),
            ],
        );
        let (_b, rows2, _q, skipped2) = ingest_staged(&idx, &staging, &scopes).unwrap();
        assert_eq!((rows2, skipped2), (1, 1));
        assert_eq!(idx.count(Some("app = 'mail'")).unwrap(), 1);
        assert!(mail_hits(&idx)[0].1.contains("v2"), "kept row refreshed");

        // no recorded scopes (the index_wipe shape) → the row CAN come back
        write_batch(&staging, "crawl-3-0.jsonl", &[junk_record("11", "back")]);
        let _ = ingest_staged(&idx, &staging, &[]).unwrap();
        assert_eq!(idx.count(Some("container = 'Junk'")).unwrap(), 1);
    }
}
