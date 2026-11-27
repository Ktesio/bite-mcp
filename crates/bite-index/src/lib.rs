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

/// Cross-batch keep-last by (app, account, id): the crawler legitimately
/// produces duplicate keys across batch files WITHIN one account
/// (overlapping windows, restarts), and merge-insert PROHIBITS two source
/// rows matching the same target key — without this, one duplicated message
/// wedges the entire ingest forever. The account is part of the key: two
/// accounts holding id "4127" are two distinct messages, and the old
/// (app, id) key silently collapsed cross-account mail into one row.
/// "Last" is the NEWEST batch because the caller orders the batch files by
/// numeric seq before the key pass.
///
/// Returns the POSITIONS of the surviving occurrences among `keys` (the
/// last occurrence of each key). Positions, not records: the write pass
/// filters by them, so neither pass ever holds a record's body beyond the
/// file it is reading. What this replaces held every parsed record of the
/// whole crawl in memory until the first write — and, before that, a deep
/// clone of every survivor beside it.
fn keep_last_positions(keys: &[DedupKey]) -> std::collections::HashSet<usize> {
    let mut last_seen: std::collections::HashMap<&DedupKey, usize> =
        std::collections::HashMap::with_capacity(keys.len());
    for (i, key) in keys.iter().enumerate() {
        last_seen.insert(key, i);
    }
    last_seen.into_values().collect()
}

/// Dedup key of one staged record: `(app, account, id)`, a missing account
/// normalized to `""` exactly as the write normalizes it.
type DedupKey = (String, String, String);

/// Rows carried between staged files before one write. Equal to the
/// `merge_insert` chunk inside `EntityIndex::ingest`, and always flushed in
/// exact multiples, so a crawl costs the same number of write transactions
/// as the single whole-crawl `ingest` call it replaces.
const FLUSH_ROWS: usize = 512;

/// The key pass for ONE staged file: parse every non-empty line (validating
/// it exactly as the write pass will) and keep only each record's
/// [`DedupKey`]. A malformed line yields no key and is reported by line
/// number instead of failing the file; a read failure surfaces before any
/// of the file's keys is used, so a poison batch can never stand as the
/// newest occurrence of a key — quarantining it must not take the good row
/// of an earlier batch along with it.
fn key_pass(file: &Path) -> Result<(Vec<DedupKey>, Vec<usize>), store::IndexError> {
    let parsed = store::parse_jsonl(file)?;
    let lines = parsed.bad_lines.iter().map(|bad| bad.line).collect();
    let keys = parsed
        .records
        .into_iter()
        .map(|r| (r.app, r.account.unwrap_or_default(), r.id))
        .collect();
    Ok((keys, lines))
}

/// One stderr note per partially malformed batch: the file and the line
/// numbers that were skipped (the first few). Line CONTENT is never printed
/// — a malformed mail record is still mail.
fn note_malformed(file: &Path, bad_lines: &[usize]) {
    const SHOWN: usize = 5;
    let mut listed: Vec<String> = bad_lines
        .iter()
        .take(SHOWN)
        .map(std::string::ToString::to_string)
        .collect();
    if bad_lines.len() > SHOWN {
        listed.push("…".to_string());
    }
    eprintln!(
        "bite-index: skipped {} malformed line(s) in {}: lines {}",
        bad_lines.len(),
        file.display(),
        listed.join(", ")
    );
}

/// Move one unreadable batch to `<staging>/quarantine/` (created 0700) and
/// enforce the 200-file retention cap there. Returns whether the file was
/// moved.
fn quarantine_batch(file: &Path, quarantine: &Path) -> bool {
    let mut moved = false;
    if std::fs::create_dir_all(quarantine).is_ok() {
        // creation-time privacy: the MCP server process may long outlive
        // its startup enforcement pass, so a quarantine/ first created
        // mid-session must not sit at umask defaults until the next startup
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(quarantine, std::fs::Permissions::from_mode(0o700));
        let dest = quarantine.join(file.file_name().unwrap_or_default());
        match std::fs::rename(file, dest) {
            Ok(_) => moved = true,
            Err(re) => {
                // leave the file in place — it will be retried
                // (and re-fail visibly) on the next ingest
                eprintln!("bite-index: could not quarantine {}: {re}", file.display());
            }
        }
    }
    // retention cap: keep quarantine/ bounded at 200 files, deleting the
    // OLDEST first by modification time (filename order lies once seq >= 10:
    // "-10" sorts before "-9")
    let mut qfiles: Vec<_> = std::fs::read_dir(quarantine)
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
    moved
}

/// Ingest every un-ingested `*.jsonl` batch in the staging dir (deleting each
/// file on success — merge-insert makes re-ingest idempotent). Returns the
/// number of batches ingested, the total rows written, the number of
/// batches quarantined (moved to `<staging>/quarantine/`), the rows dropped
/// because they matched a recorded purge scope (`index_forget`) — purged
/// scopes must not come back with the next full crawl — and the lines
/// skipped because they did not parse as a record.
///
/// A batch is quarantined only when it cannot be read as a whole (missing,
/// unreadable, not valid UTF-8) or when EVERY non-empty line is malformed.
/// A batch with malformed lines among valid ones is ingested without them:
/// the crawl watermark has already moved past it, so quarantining it whole
/// would drop its valid records from search for good.
///
/// Two passes over the staged files keep memory bounded by ONE staged file
/// plus [`FLUSH_ROWS`] records, however large the crawl is:
///
/// 1. **Key pass** — every line is parsed (so validated) and reduced to its
///    [`DedupKey`]; `keep_last_positions` then settles WHICH occurrence of
///    each key survives. Keys hold no bodies, so this pass costs a second
///    parse of every line in the write pass, not memory.
/// 2. **Write pass** — each file is parsed again and only the surviving
///    occurrences are written, [`FLUSH_ROWS`] at a time.
///
/// Both passes see the same records in the same order, so the dedup,
/// purge-scope and quarantine outcomes are exactly those of the former
/// parse-everything-then-write shape — which held every parsed record of
/// the whole crawl in memory before the first write.
pub fn ingest_staged(
    index: &EntityIndex,
    staging: &Path,
    purge_scopes: &[PurgeScope],
) -> Result<(usize, usize, usize, usize, usize), store::IndexError> {
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
    let quarantine = staging.join("quarantine");
    let mut readable: Vec<std::path::PathBuf> = Vec::new();
    let mut all_keys: Vec<DedupKey> = Vec::new();
    let mut quarantined = 0usize;
    let mut skipped_malformed = 0usize;
    // a single poison batch must not wedge the whole pipeline forever:
    // quarantine unreadable files and keep ingesting the rest
    for file in files {
        match key_pass(&file) {
            Ok((keys, bad_lines)) => {
                if keys.is_empty() && !bad_lines.is_empty() {
                    // nothing in this batch is ingestable: keep the file
                    // for inspection, exactly like an unreadable one
                    eprintln!(
                        "bite-index: quarantining malformed batch {}: lines {}",
                        file.display(),
                        bad_lines
                            .iter()
                            .take(5)
                            .map(std::string::ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    quarantined += usize::from(quarantine_batch(&file, &quarantine));
                    continue;
                }
                if !bad_lines.is_empty() {
                    skipped_malformed += bad_lines.len();
                    note_malformed(&file, &bad_lines);
                }
                all_keys.extend(keys);
                readable.push(file);
            }
            Err(e) => {
                eprintln!(
                    "bite-index: quarantining unreadable batch {}: {e}",
                    file.display()
                );
                quarantined += usize::from(quarantine_batch(&file, &quarantine));
            }
        }
    }
    let survivors = keep_last_positions(&all_keys);
    let dedup_dropped = all_keys.len() - survivors.len();
    drop(all_keys);
    if dedup_dropped > 0 {
        eprintln!(
            "bite-index: deduplicated {dedup_dropped} duplicate (app,account,id) rows across batches"
        );
    }
    // Purge-scope tombstones: drop rows inside a recorded `index_forget`
    // scope BEFORE the upsert. Applied to the dedup SURVIVOR alone (the
    // superseded occurrences are never written) so a newer batch can
    // never resurrect a purged key, and an older batch's copy of a purged
    // message is dropped too. Empty slice = no tombstones recorded = the
    // historical behavior (everything ingests).
    let mut skipped_purged = 0usize;
    let mut rows = 0usize;
    let mut buffered: Vec<Record> = Vec::with_capacity(FLUSH_ROWS);
    // running occurrence position — the same numbering the key pass saw
    let mut pos = 0usize;
    for file in &readable {
        for mut r in store::parse_jsonl(file)?.records {
            // Only the surviving occurrence of a key is written. The account
            // is normalized before the merge: Mail message ids are
            // ACCOUNT-scoped integers, and NULL values in a merge-insert
            // key column never match (a NULL-account row could never be
            // upserted).
            if survivors.contains(&pos) {
                if r.account.is_none() {
                    r.account = Some(String::new());
                }
                if purge_scopes
                    .iter()
                    .any(|s| s.matches(&r.app, r.container.as_deref()))
                {
                    skipped_purged += 1;
                } else {
                    buffered.push(r);
                }
            }
            pos += 1;
        }
        while buffered.len() >= FLUSH_ROWS {
            let tail = buffered.split_off(FLUSH_ROWS);
            rows += index.ingest(&buffered)?.rows;
            buffered = tail;
        }
    }
    if !buffered.is_empty() {
        rows += index.ingest(&buffered)?.rows;
    }
    if skipped_purged > 0 {
        eprintln!(
            "bite-index: skipped {skipped_purged} rows matching recorded purge scopes (index_forget)"
        );
    }
    for file in &readable {
        std::fs::remove_file(file).ok();
    }
    let batches = readable.len();
    Ok((
        batches,
        rows,
        quarantined,
        skipped_purged,
        skipped_malformed,
    ))
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
        let (batches, _rows, quarantined, _skipped, _malformed) =
            ingest_staged(&idx, &staging, &[]).unwrap();
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
    fn dedup_gather_keeps_last_of_interleaved_keys() {
        // The survivors are gathered in place from source indices, so pin
        // a permutation whose sources are NOT ascending: `a` is first seen
        // at 0 and last at 5, `b` first at 1 and last at 3, with singles in
        // between. A gather that read a slot after overwriting it, or kept
        // a stale first occurrence, shows up here as a wrong title.
        // Keep-last and no-loss only: the hits come back through `search`
        // and are sorted below, so the survivors' first-seen order is NOT
        // observable here.
        let staging = temp_staging("gather");
        let idx = EntityIndex::open(&staging.join("index.lance")).unwrap();
        write_batch(
            &staging,
            "crawl-1-0.jsonl",
            &[
                mail_record("a", Some("Work"), "a first"),
                mail_record("b", Some("Work"), "b first"),
                mail_record("c", Some("Work"), "c only"),
                mail_record("b", Some("Work"), "b last"),
            ],
        );
        write_batch(
            &staging,
            "crawl-1-1.jsonl",
            &[
                mail_record("d", Some("Work"), "d only"),
                mail_record("a", Some("Work"), "a last"),
            ],
        );
        let (batches, rows, quarantined, skipped, _malformed) =
            ingest_staged(&idx, &staging, &[]).unwrap();
        assert_eq!((batches, rows, quarantined, skipped), (2, 4, 0, 0));
        let mut got = mail_hits(&idx);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("Work".to_string(), "a last".to_string()),
                ("Work".to_string(), "b last".to_string()),
                ("Work".to_string(), "c only".to_string()),
                ("Work".to_string(), "d only".to_string()),
            ]
        );
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
        let (batches, _rows, _quarantined, _skipped, _malformed) =
            ingest_staged(&idx, &staging, &[]).unwrap();
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
        let (batches, _rows, quarantined, _skipped, _malformed) =
            ingest_staged(&idx, &staging, &[]).unwrap();
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
        let (batches, rows, quarantined, skipped, _malformed) =
            ingest_staged(&idx, &staging, &scopes).unwrap();
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
        let (_b, rows2, _q, skipped2, _malformed) = ingest_staged(&idx, &staging, &scopes).unwrap();
        assert_eq!((rows2, skipped2), (1, 1));
        assert_eq!(idx.count(Some("app = 'mail'")).unwrap(), 1);
        assert!(mail_hits(&idx)[0].1.contains("v2"), "kept row refreshed");

        // no recorded scopes (the index_wipe shape) → the row CAN come back
        write_batch(&staging, "crawl-3-0.jsonl", &[junk_record("11", "back")]);
        let _ = ingest_staged(&idx, &staging, &[]).unwrap();
        assert_eq!(idx.count(Some("container = 'Junk'")).unwrap(), 1);
    }

    /// A staged file whose first line is a valid record and whose second
    /// line fails to parse — a poison batch that still carries a good key.
    fn write_poison_batch(staging: &Path, name: &str, head: &Record) {
        std::fs::write(
            staging.join(name),
            format!(
                "{}\n{{\"app\":\"mail\",\"id\":\"12\",\"read\":\"yes\"}}\n",
                serde_json::to_string(head).unwrap()
            ),
        )
        .unwrap();
    }

    /// Staged `*.jsonl` files left at the top level of the staging dir.
    fn staged_batches(staging: &Path) -> usize {
        std::fs::read_dir(staging)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
            .count()
    }

    #[test]
    fn write_pass_keeps_only_survivors_across_the_flush_boundary() {
        // More rows than one write chunk, and the newest occurrence of key
        // a0000 sits in the SECOND file. Keep-last is settled for the whole
        // crawl before the first write, so file one's superseded row is
        // never written even though file two is only parsed after file
        // one's rows have left the buffer.
        let staging = temp_staging("flush");
        let idx = EntityIndex::open(&staging.join("index.lance")).unwrap();
        let ids: Vec<String> = (0..FLUSH_ROWS).map(|i| format!("a{i:04}")).collect();
        let first: Vec<Record> = ids
            .iter()
            .map(|id| mail_record(id, Some("Work"), "older"))
            .collect();
        write_batch(&staging, "crawl-1-0.jsonl", &first);
        write_batch(
            &staging,
            "crawl-1-1.jsonl",
            &[
                mail_record("a0000", Some("Work"), "newer"),
                mail_record("b0000", Some("Work"), "only in the second file"),
            ],
        );
        let (batches, rows, quarantined, skipped, _malformed) =
            ingest_staged(&idx, &staging, &[]).unwrap();
        assert_eq!(
            (batches, rows, quarantined, skipped),
            (2, FLUSH_ROWS + 1, 0, 0)
        );
        assert_eq!(idx.count(Some("app = 'mail'")).unwrap(), FLUSH_ROWS + 1);
        assert_eq!(
            idx.count(Some("id = 'a0000' AND title = 'newer'")).unwrap(),
            1
        );
        assert_eq!(idx.count(Some("title = 'older'")).unwrap(), FLUSH_ROWS - 1);
        assert_eq!(staged_batches(&staging), 0, "both files consumed");
    }

    #[test]
    fn partially_malformed_batch_keeps_its_valid_rows_newest_wins() {
        // The third batch repeats key 10 with a newer title and THEN fails
        // to parse. Its valid line is a real record of a newer crawl: it is
        // written (the newest occurrence wins) and only the torn line is
        // skipped and counted. Quarantining the batch whole, as ingest used
        // to, dropped that record for good.
        let staging = temp_staging("poison");
        let idx = EntityIndex::open(&staging.join("index.lance")).unwrap();
        write_batch(
            &staging,
            "crawl-1-0.jsonl",
            &[mail_record("10", Some("Work"), "first")],
        );
        write_batch(
            &staging,
            "crawl-2-0.jsonl",
            &[mail_record("10", Some("Work"), "second")],
        );
        write_poison_batch(
            &staging,
            "crawl-3-0.jsonl",
            &mail_record("10", Some("Work"), "third"),
        );
        let (batches, rows, quarantined, skipped, malformed) =
            ingest_staged(&idx, &staging, &[]).unwrap();
        assert_eq!(
            (batches, rows, quarantined, skipped, malformed),
            (3, 1, 0, 0, 1)
        );
        assert_eq!(idx.count(Some("app = 'mail'")).unwrap(), 1);
        assert!(mail_hits(&idx)[0].1.contains("third"));
        assert!(!staging.join("quarantine").join("crawl-3-0.jsonl").exists());
        assert_eq!(staged_batches(&staging), 0, "all three files consumed");
    }

    #[test]
    fn a_torn_line_drops_only_itself() {
        // good, torn, good in ONE batch: both good rows are indexed and the
        // torn one is counted — the shape a truncated or type-mismatched
        // record has in a real batch.
        let staging = temp_staging("torn");
        let idx = EntityIndex::open(&staging.join("index.lance")).unwrap();
        let before = serde_json::to_string(&mail_record("7", Some("Work"), "before")).unwrap();
        let after = serde_json::to_string(&mail_record("8", Some("Work"), "after")).unwrap();
        std::fs::write(
            staging.join("crawl-1-0.jsonl"),
            format!(
                "{before}\n{{\"app\":\"mail\",\"id\":\"9\",\"read\":\"not-a-bool\"}}\n{after}\n"
            ),
        )
        .unwrap();
        let (batches, rows, quarantined, skipped, malformed) =
            ingest_staged(&idx, &staging, &[]).unwrap();
        assert_eq!(
            (batches, rows, quarantined, skipped, malformed),
            (1, 2, 0, 0, 1)
        );
        let mut titles: Vec<String> = mail_hits(&idx).into_iter().map(|(_, t)| t).collect();
        titles.sort();
        assert_eq!(titles, vec!["after".to_string(), "before".to_string()]);
        assert!(!staging.join("quarantine").join("crawl-1-0.jsonl").exists());
        assert_eq!(staged_batches(&staging), 0);
    }

    #[test]
    fn batch_of_only_malformed_lines_is_quarantined() {
        // nothing ingestable: the file is kept for inspection like an
        // unreadable one, and its lines are not counted as skipped rows of
        // an ingested batch. Blank lines are not malformed.
        let staging = temp_staging("allbad");
        let idx = EntityIndex::open(&staging.join("index.lance")).unwrap();
        std::fs::write(
            staging.join("crawl-1-0.jsonl"),
            "{\"app\":\"mail\",\"id\":\"9\",\"read\":\"not-a-bool\"}\n\n   \nnot json at all\n",
        )
        .unwrap();
        let (batches, rows, quarantined, skipped, malformed) =
            ingest_staged(&idx, &staging, &[]).unwrap();
        assert_eq!(
            (batches, rows, quarantined, skipped, malformed),
            (0, 0, 1, 0, 0)
        );
        assert_eq!(idx.count(Some("app = 'mail'")).unwrap(), 0);
        assert!(staging.join("quarantine").join("crawl-1-0.jsonl").is_file());
        assert_eq!(staged_batches(&staging), 0);
    }

    #[test]
    fn keep_last_positions_keeps_the_last_occurrence_of_each_key() {
        let key = |id: &str| ("mail".to_string(), "Work".to_string(), id.to_string());
        // a, b, a, c, b — the survivors are positions 2 (a), 3 (c) and 4 (b)
        let keys = [key("a"), key("b"), key("a"), key("c"), key("b")];
        let survivors = keep_last_positions(&keys);
        let set = |positions: &[usize]| {
            positions
                .iter()
                .copied()
                .collect::<std::collections::HashSet<usize>>()
        };
        assert_eq!(survivors, set(&[2, 3, 4]));
        assert_eq!(keys.len() - survivors.len(), 2, "two occurrences dropped");

        // Same id in two accounts is two keys: both survive.
        let keys = [
            key("x"),
            ("mail".to_string(), "Home".to_string(), "x".to_string()),
        ];
        assert_eq!(keep_last_positions(&keys), set(&[0, 1]));
        assert!(keep_last_positions(&[]).is_empty());
    }
}
