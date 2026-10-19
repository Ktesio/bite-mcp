//! LanceDB-backed global entity index.
//!
//! One shared `entities` table for every app's searchable corpus. Rust owns
//! the store; the Swift helper only produces JSONL batches that get ingested
//! here (see docs/protocol.md — batch handoff).
//!
//! Privacy: the database lives in bite's own data dir, which is forced to
//! 0700 (see bite-core::fsops). Full-body FTS requires plaintext at index
//! time — the `content` column is omitted entirely when the store-body
//! posture is off (rebuild required after switching).

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// Use the arrow line re-exported by lancedb (`lancedb::arrow`) instead of
// declaring direct arrow-array/arrow-schema dependencies: lancedb's traits
// (Scannable, RecordBatchReader) are implemented only for ITS arrow version,
// so a direct dep that resolves to any other arrow release splits the crate
// graph in two and the store stops compiling (lancedb/lancedb#3575).
use lancedb::arrow::arrow_array::{
    builder::{BooleanBuilder, Int32Builder, Int64Builder, StringBuilder},
    Array, BooleanArray, Int64Array, RecordBatch, StringArray,
};
use lancedb::arrow::arrow_schema::{DataType, Field, Schema, SchemaRef};
use lancedb::index::scalar::FullTextSearchQuery;
use lancedb::index::Index;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::Connection;
use serde::{Deserialize, Serialize};

/// One searchable entity. All app-specific semantics map onto these fields
/// (mail message / calendar event / contact / note / chat message).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Record {
    /// App namespace: "mail" | "calendar" | "reminders" | "contacts" | "notes" | "messages"
    pub app: String,
    /// App-native stable identifier (Mail message id, EK identifier, CN identifier, …)
    pub id: String,
    /// Owning account. Optional in the JSONL contract, but NORMALIZED to
    /// Some("") before dedup/merge (in-memory only): Mail ids are
    /// account-scoped, so the merge/dedup key is (app, account, id) and a
    /// NULL key value would never match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Mailbox / calendar / list / folder / chat
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Full body text. Omitted entirely when the store-body posture is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Participants: for mail `"sender, to1, to2, …"` (sender first, `", "`
    /// joined, capped at 20 addresses; rows indexed before recipient
    /// enrichment carry the sender only). Other apps use their natural
    /// participant list (e.g. event attendees).
    pub participants: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flagged: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub junk: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    /// App-specific extras as a JSON object string
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub props: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    pub text: Option<String>,
    pub app: Option<String>,
    pub container: Option<String>,
    pub account: Option<String>,
    pub read: Option<bool>,
    pub flagged: Option<bool>,
    pub junk: Option<bool>,
    pub completed: Option<bool>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub limit: usize,
    /// Case-insensitive substring match against participants. For mail the
    /// crawl now stores `"sender, to1, to2, …"` (to-recipient addresses
    /// appended since the recipient-enrichment change; rows indexed before
    /// it carry the sender only until a full reindex backfills them).
    /// Applied as `ILIKE '%…%'` with LIKE wildcards escaped, so the
    /// needle is always a literal substring.
    pub participant_contains: Option<String>,
    /// Case-insensitive substring match against title
    pub title_contains: Option<String>,
    /// Case-insensitive substring match against content
    pub content_contains: Option<String>,
    /// Filter-only queries normally pay a second `count_rows` scan so
    /// `total` is exact. Set to skip that scan; `total` then comes back
    /// `None` — the same shape full-text queries already return — and the
    /// caller saves one full filtered pass over the table. Callers that
    /// surface `total` or derive "are there more?" from it must keep this
    /// unset (the default).
    pub skip_exact_count: bool,
}

/// Columns `hits_from_stream` reads — the exact projection `search()`
/// requests. Keep in sync with the Hit reader: a column read there but
/// missing from this list silently degrades to a None/default Hit field.
const HIT_COLUMNS: &[&str] = &[
    "app",
    "id",
    "account",
    "container",
    "title",
    "content",
    "participants",
    "start_ms",
    "updated_ms",
    "read",
    "flagged",
    "junk",
    "completed",
];

#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub app: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// First ~200 chars of content — full body via the app's get tool
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub participants: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flagged: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub junk: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResult {
    pub hits: Vec<Hit>,
    /// Exact count when the query has no text component and
    /// `skip_exact_count` is unset (filter-only COUNT); null for FTS
    /// queries where only the requested page was materialized, and for
    /// any query where the caller skipped the count scan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexStats {
    pub total: usize,
    pub per_app: Vec<(String, usize)>,
    pub newest_updated_ms: Option<i64>,
    pub fts_indexed: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("lancedb: {0}")]
    Lance(#[from] lancedb::Error),
    #[error("arrow: {0}")]
    Arrow(#[from] lancedb::arrow::arrow_schema::ArrowError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

/// Held only for the duration of a write. Multiple bite processes (MCP
/// sessions, CLI) may hold read access concurrently; this lock just
/// serializes writers and releases as soon as the write completes.
pub struct WriteGuard {
    _file: std::fs::File,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another bite process is writing the index")]
    Busy,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl From<LockError> for IndexError {
    fn from(e: LockError) -> Self {
        match e {
            LockError::Busy => IndexError::Other(e.to_string()),
            LockError::Io(io) => IndexError::Io(io),
        }
    }
}

pub struct EntityIndex {
    dir: PathBuf,
    rt: tokio::runtime::Runtime,
    conn: Connection,
}

const TABLE: &str = "entities";

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("app", DataType::Utf8, false),
        Field::new("id", DataType::Utf8, false),
        Field::new("account", DataType::Utf8, true),
        Field::new("container", DataType::Utf8, true),
        Field::new("title", DataType::Utf8, true),
        Field::new("content", DataType::Utf8, true),
        Field::new("participants", DataType::Utf8, true),
        Field::new("start_ms", DataType::Int64, true),
        Field::new("end_ms", DataType::Int64, true),
        Field::new("updated_ms", DataType::Int64, true),
        Field::new("read", DataType::Boolean, true),
        Field::new("flagged", DataType::Boolean, true),
        Field::new("junk", DataType::Boolean, true),
        Field::new("completed", DataType::Boolean, true),
        Field::new("priority", DataType::Int32, true),
        Field::new("props", DataType::Utf8, true),
        // title + "\n" + content — the FTS target (not user-facing)
        Field::new("search_text", DataType::Utf8, false),
    ]))
}

/// Single-quote escape for SQL filter literals.
pub fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

impl EntityIndex {
    /// Open (creating if needed). Does NOT take the writer lock — reads never
    /// block other bite processes; writers acquire the lock per operation.
    pub fn open(dir: &Path) -> Result<Self, IndexError> {
        bite_core::fsops::ensure_private_dir(dir)?;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let dir_str = dir.to_string_lossy().to_string();
        let conn = rt.block_on(async { lancedb::connect(dir_str.as_str()).execute().await })?;

        let index = Self {
            dir: dir.to_path_buf(),
            rt,
            conn,
        };
        index.rt.block_on(async { index.ensure_table().await })?;
        Ok(index)
    }

    /// Acquire the short-lived writer lock (non-blocking). Callers treat
    /// LockError::Busy as "skip this round, retry later".
    pub fn lock_writer(&self) -> Result<WriteGuard, LockError> {
        let path = self.dir.join(".writer.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?;
        fs2::FileExt::try_lock_exclusive(&file).map_err(|_| LockError::Busy)?;
        bite_core::fsops::tighten_file(&path)?;
        Ok(WriteGuard { _file: file })
    }

    async fn ensure_table(&self) -> Result<(), IndexError> {
        let names = self.conn.table_names().execute().await?;
        if names.iter().any(|n| n == TABLE) {
            return Ok(());
        }
        let empty = RecordBatch::new_empty(schema());
        self.conn.create_table(TABLE, vec![empty]).execute().await?;
        Ok(())
    }

    async fn table(&self) -> Result<lancedb::Table, IndexError> {
        Ok(self.conn.open_table(TABLE).execute().await?)
    }

    /// Upsert records keyed on (app, account, id). The account is part of
    /// the key because Mail message ids are ACCOUNT-scoped integers — two
    /// accounts holding id "4127" are two distinct messages, and the old
    /// (app, id) key silently collapsed cross-account mail. `account` is
    /// normalized to "" (never NULL) on write: NULL values in a merge key
    /// column never match, so a NULL-account row could never be upserted.
    /// Idempotent per batch — re-ingesting the same batch file is a no-op.
    /// (Legacy rows written under the old 2-column key have a NULL account,
    /// and merge-insert can never match a NULL key column against the
    /// normalized "" — so those rows persist alongside their re-ingested
    /// replacements until `index_wipe` removes them; no crawl rewrites
    /// them.)
    pub fn ingest(&self, records: &[Record]) -> Result<IngestStats, IndexError> {
        if records.is_empty() {
            return Ok(IngestStats::default());
        }
        self.rt.block_on(async {
            let table = self.table().await?;
            let mut updated_rows: usize = 0;
            for chunk in records.chunks(512) {
                let batch = record_batch(chunk)?;
                let reader = lancedb::arrow::arrow_array::RecordBatchIterator::new(
                    vec![Ok(batch)],
                    schema(),
                );
                let mut mi = table.merge_insert(&["app", "account", "id"]);
                mi.when_matched_update_all(None)
                    .when_not_matched_insert_all();
                let result = mi.execute(Box::new(reader)).await?;
                updated_rows += (result.num_updated_rows + result.num_inserted_rows) as usize;
            }
            // First-time FTS index creation once rows exist; afterwards the
            // control plane folds new rows in via `refresh_fts` once the
            // crawl settles (see bite-mcp index.rs refresh_fts_if_crawl_settled).
            if !self.has_fts().await? {
                self.create_fts().await?;
            }
            Ok(IngestStats {
                rows: updated_rows,
                fts_created_now: false,
            })
        })
    }

    /// Refresh indexes over current rows (adds unindexed rows to the FTS
    /// index; called after large ingests).
    pub fn refresh_fts(&self) -> Result<(), IndexError> {
        self.rt.block_on(async {
            use lancedb::table::OptimizeAction;
            let table = self.table().await?;
            table.optimize(OptimizeAction::All).await?;
            Ok(())
        })
    }

    async fn has_fts(&self) -> Result<bool, IndexError> {
        let table = self.table().await?;
        let indices = table.list_indices().await?;
        Ok(indices
            .iter()
            .any(|i| i.index_type == lancedb::index::IndexType::FTS))
    }

    async fn create_fts(&self) -> Result<(), IndexError> {
        let table = self.table().await?;
        table
            .create_index(&["search_text"], Index::FTS(Default::default()))
            .execute()
            .await?;
        Ok(())
    }

    /// Remove rows matching a SQL filter (used for cache invalidation).
    pub fn delete_where(&self, filter: &str) -> Result<(), IndexError> {
        self.rt.block_on(async {
            let table = self.table().await?;
            table.delete(filter).await?;
            Ok(())
        })
    }

    pub fn count(&self, filter: Option<&str>) -> Result<usize, IndexError> {
        self.rt.block_on(async {
            let table = self.table().await?;
            Ok(table
                .count_rows(filter.as_ref().map(|f| f.to_string()))
                .await?)
        })
    }

    /// Search with optional full-text + scalar filters. `total` is exact for
    /// filter-only queries unless `skip_exact_count` is set (scan skipped,
    /// `None` returned); always `None` for FTS (page-capped).
    pub fn search(&self, q: &SearchQuery) -> Result<SearchResult, IndexError> {
        self.rt.block_on(async {
            let table = self.table().await?;
            let filter = filter_sql(q);

            let mut total = None;
            if q.text.is_none() && !q.skip_exact_count {
                total = Some(
                    table
                        .count_rows(filter.as_ref().map(|f| f.to_string()))
                        .await?,
                );
            }

            let mut builder = table.query();
            // Project only the Hit-consumed columns: `search_text`
            // duplicates title+content for the FTS index and `props` can
            // carry a large JSON blob — neither is read on this path, so
            // hauling them per hit wastes scan and IO.
            builder = builder.select(lancedb::query::Select::Columns(
                HIT_COLUMNS.iter().map(|c| c.to_string()).collect(),
            ));
            if let Some(f) = &filter {
                builder = builder.only_if(f.clone());
            }
            if let Some(text) = &q.text {
                builder = builder.full_text_search(
                    FullTextSearchQuery::new(text.clone())
                        .with_columns(&["search_text".to_string()])
                        .map_err(|e| IndexError::Other(e.to_string()))?,
                );
            } else {
                // Filter-only queries have no relevance ranking, so without
                // an explicit ordering the limit-N page was an arbitrary
                // scan-order subset — while both the CLI help and the
                // Apple-side `messages_search` document newest-first. Nulls
                // sink: rows without an updated_ms must never win a slot.
                // FTS queries deliberately stay unordered so BM25 relevance
                // outranks recency there.
                builder =
                    builder.order_by(Some(vec![lancedb::query::ColumnOrdering::desc_nulls_last(
                        "updated_ms".to_string(),
                    )]));
            }
            builder = builder.limit(q.limit.max(1));
            let stream = builder.execute().await?;
            let hits = hits_from_stream(stream).await?;
            Ok(SearchResult { hits, total })
        })
    }

    pub fn stats(&self) -> Result<IndexStats, IndexError> {
        self.rt.block_on(async {
            let table = self.table().await?;
            // One projected scan computes total, per-app counts and the
            // newest `updated_ms` together. The previous shape issued one
            // full-table `count_rows` per app plus an unfiltered total
            // (7 scans) and an ordered limit-1 lookup on top (8 queries).
            let stream = table
                .query()
                .select(lancedb::query::Select::Columns(vec![
                    "app".to_string(),
                    "updated_ms".to_string(),
                ]))
                .execute()
                .await?;
            use futures::StreamExt;
            let mut stream = stream;
            let mut counts: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            let mut newest: Option<i64> = None;
            while let Some(batch) = stream.next().await {
                let batch = batch.map_err(IndexError::from)?;
                // `app` is non-nullable Utf8 and `updated_ms` nullable Int64
                // in the schema; a column that is missing or mistyped means
                // the store is corrupt, not empty.
                let (Some(apps), Some(timestamps)) = (
                    batch
                        .column_by_name("app")
                        .and_then(|a| a.as_any().downcast_ref::<StringArray>()),
                    batch
                        .column_by_name("updated_ms")
                        .and_then(|a| a.as_any().downcast_ref::<Int64Array>()),
                ) else {
                    return Err(IndexError::Other(
                        "stats: app/updated_ms columns missing from scan".into(),
                    ));
                };
                for i in 0..batch.num_rows() {
                    *counts.entry(apps.value(i).to_string()).or_insert(0) += 1;
                    if !timestamps.is_null(i) {
                        // Nulls sink: rows without an updated_ms must never win.
                        newest = newest.max(Some(timestamps.value(i)));
                    }
                }
            }
            // Unknown app values still count toward `total`; per_app keeps
            // the fixed app order and omits zero-count apps.
            let total = counts.values().sum();
            let mut per_app = Vec::new();
            for app in [
                "mail",
                "calendar",
                "reminders",
                "contacts",
                "notes",
                "messages",
            ] {
                if let Some(n) = counts.get(app) {
                    per_app.push((app.to_string(), *n));
                }
            }
            let fts_indexed = self.has_fts().await?;
            Ok(IndexStats {
                total,
                per_app,
                newest_updated_ms: newest,
                fts_indexed,
            })
        })
    }

    /// Destroy all rows and indexes (the `index wipe` command). The store
    /// remains usable afterwards.
    pub fn wipe(&self) -> Result<(), IndexError> {
        let _guard = self.lock_writer()?;
        self.rt.block_on(async {
            let names = self.conn.table_names().execute().await?;
            if names.iter().any(|n| n == TABLE) {
                self.conn.drop_table(TABLE, &[]).await?;
            }
            self.ensure_table().await
        })
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct IngestStats {
    /// rows inserted or updated
    pub rows: usize,
    pub fts_created_now: bool,
}

fn filter_sql(q: &SearchQuery) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(app) = &q.app {
        parts.push(format!("app = {}", sql_str(app)));
    }
    if let Some(c) = &q.container {
        parts.push(format!("container = {}", sql_str(c)));
    }
    if let Some(a) = &q.account {
        parts.push(format!("account = {}", sql_str(a)));
    }
    if let Some(v) = q.read {
        parts.push(format!("read IS NOT NULL AND read = {v}"));
    }
    if let Some(v) = q.flagged {
        parts.push(format!("flagged IS NOT NULL AND flagged = {v}"));
    }
    if let Some(v) = q.junk {
        parts.push(format!("junk IS NOT NULL AND junk = {v}"));
    }
    if let Some(v) = q.completed {
        parts.push(format!("completed IS NOT NULL AND completed = {v}"));
    }
    if let Some(v) = q.since_ms {
        parts.push(format!("start_ms >= {v}"));
    }
    if let Some(v) = q.until_ms {
        parts.push(format!("start_ms < {v}"));
    }
    if let Some(needle) = &q.participant_contains {
        parts.push(contains_clause("participants", needle));
    }
    if let Some(needle) = &q.title_contains {
        parts.push(contains_clause("title", needle));
    }
    if let Some(needle) = &q.content_contains {
        parts.push(contains_clause("content", needle));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" AND "))
    }
}

/// Case-insensitive literal-substring predicate (ILIKE with LIKE wildcards
/// escaped), matching the live path's `lowercased().contains(needle)`.
/// NULL columns never match: a NULL title/participants/content row is
/// excluded, which is the same answer the live Mail path gives.
fn contains_clause(column: &str, needle: &str) -> String {
    // escape LIKE wildcards so a user needle containing % or _ stays literal
    let escaped = needle
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("{column} ILIKE {}", sql_str(&format!("%{escaped}%")))
}

fn record_batch(records: &[Record]) -> Result<RecordBatch, IndexError> {
    let schema = schema();
    let n = records.len();
    let mut app = StringBuilder::with_capacity(n, 128);
    let mut id = StringBuilder::with_capacity(n, 128);
    let mut account = StringBuilder::new();
    let mut container = StringBuilder::new();
    let mut title = StringBuilder::with_capacity(n, 256);
    let mut content = StringBuilder::with_capacity(n, 4096);
    let mut participants = StringBuilder::new();
    let mut start_ms = Int64Builder::with_capacity(n);
    let mut end_ms = Int64Builder::with_capacity(n);
    let mut updated_ms = Int64Builder::with_capacity(n);
    let mut read = BooleanBuilder::with_capacity(n);
    let mut flagged = BooleanBuilder::with_capacity(n);
    let mut junk = BooleanBuilder::with_capacity(n);
    let mut completed = BooleanBuilder::with_capacity(n);
    let mut priority = Int32Builder::with_capacity(n);
    let mut props = StringBuilder::with_capacity(n, 512);
    let mut search_text = StringBuilder::with_capacity(n, 4096);

    for r in records {
        app.append_value(&r.app);
        id.append_value(&r.id);
        // account normalized to "" (never NULL) at the write boundary:
        // merge-insert key columns must not carry NULLs (they never match)
        account.append_option(Some(r.account.as_deref().unwrap_or("")));
        container.append_option(r.container.as_deref());
        title.append_option(r.title.as_deref());
        content.append_option(r.content.as_deref());
        participants.append_option(r.participants.as_deref());
        start_ms.append_option(r.start_ms);
        end_ms.append_option(r.end_ms);
        updated_ms.append_option(r.updated_ms);
        read.append_option(r.read);
        flagged.append_option(r.flagged);
        junk.append_option(r.junk);
        completed.append_option(r.completed);
        priority.append_option(r.priority);
        props.append_option(r.props.as_deref());
        let mut st = r.title.clone().unwrap_or_default();
        if let Some(c) = &r.content {
            if !st.is_empty() {
                st.push('\n');
            }
            st.push_str(c);
        }
        search_text.append_value(st);
    }

    let batch = RecordBatch::try_new(
        schema,
        vec![
            Arc::new(app.finish()),
            Arc::new(id.finish()),
            Arc::new(account.finish()),
            Arc::new(container.finish()),
            Arc::new(title.finish()),
            Arc::new(content.finish()),
            Arc::new(participants.finish()),
            Arc::new(start_ms.finish()),
            Arc::new(end_ms.finish()),
            Arc::new(updated_ms.finish()),
            Arc::new(read.finish()),
            Arc::new(flagged.finish()),
            Arc::new(junk.finish()),
            Arc::new(completed.finish()),
            Arc::new(priority.finish()),
            Arc::new(props.finish()),
            Arc::new(search_text.finish()),
        ],
    )?;
    Ok(batch)
}

async fn hits_from_stream(
    mut stream: lancedb::arrow::SendableRecordBatchStream,
) -> Result<Vec<Hit>, IndexError> {
    let mut hits = Vec::new();
    use futures::StreamExt;
    loop {
        let batch = match stream.next().await {
            Some(Ok(b)) => b,
            Some(Err(e)) => return Err(IndexError::from(e)),
            None => break,
        };
        {
            let n = batch.num_rows();
            let col = |name: &str| -> Option<&dyn Array> {
                batch
                    .schema()
                    .index_of(name)
                    .ok()
                    .map(|i| batch.column(i).as_ref())
            };
            let as_str = |name: &str, i: usize| -> Option<String> {
                col(name)
                    .and_then(|a| a.as_any().downcast_ref::<StringArray>())
                    .filter(|a| !a.is_null(i))
                    .map(|a| a.value(i).to_string())
            };
            let as_bool = |name: &str, i: usize| -> Option<bool> {
                col(name)
                    .and_then(|a| a.as_any().downcast_ref::<BooleanArray>())
                    .filter(|a| !a.is_null(i))
                    .map(|a| a.value(i))
            };
            let as_i64 = |name: &str, i: usize| -> Option<i64> {
                col(name)
                    .and_then(|a| a.as_any().downcast_ref::<Int64Array>())
                    .filter(|a| !a.is_null(i))
                    .map(|a| a.value(i))
            };
            for i in 0..n {
                let content = as_str("content", i);
                let snippet = content.as_ref().map(|c| {
                    let t = c.trim();
                    if t.chars().count() <= 200 {
                        t.to_string()
                    } else {
                        format!("{}…", t.chars().take(200).collect::<String>())
                    }
                });
                hits.push(Hit {
                    app: as_str("app", i).unwrap_or_default(),
                    id: as_str("id", i).unwrap_or_default(),
                    account: as_str("account", i),
                    container: as_str("container", i),
                    title: as_str("title", i),
                    snippet,
                    participants: as_str("participants", i),
                    start_ms: as_i64("start_ms", i),
                    updated_ms: as_i64("updated_ms", i),
                    read: as_bool("read", i),
                    flagged: as_bool("flagged", i),
                    junk: as_bool("junk", i),
                    completed: as_bool("completed", i),
                    score: None, // BM25 _score available via Select::Dynamic when needed
                });
            }
        }
    }
    Ok(hits)
}

/// Parse a JSONL batch file (one Record per line) written by a Swift crawler.
pub fn parse_jsonl(path: &Path) -> Result<Vec<Record>, IndexError> {
    let text = std::fs::read_to_string(path)?;
    let mut records = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        records.push(
            serde_json::from_str::<Record>(line)
                .map_err(|e| IndexError::Other(format!("{}:{}: {e}", path.display(), i + 1)))?,
        );
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn record(app: &str, id: &str, title: &str, content: &str, read: Option<bool>) -> Record {
        Record {
            app: app.into(),
            id: id.into(),
            account: Some("iCloud".into()),
            container: Some("INBOX".into()),
            title: Some(title.into()),
            content: Some(content.into()),
            participants: Some("a@b.c".into()),
            start_ms: Some(1_700_000_000_000),
            end_ms: None,
            updated_ms: Some(1_700_000_000_000),
            read,
            flagged: Some(false),
            junk: Some(false),
            completed: None,
            priority: None,
            props: None,
        }
    }

    /// Self-cleaning scratch dir (removed on drop — panics can't leak it).
    fn temp_dir(tag: &str) -> crate::test_support::ScratchDir {
        crate::test_support::scratch(&format!("bite-index-{tag}-"))
    }

    #[test]
    #[should_panic(expected = "boom")]
    fn panicking_test_cleans_up_its_temp_dir() {
        // Proves the Drop guard runs during panic unwind: after this test
        // (and any panicking sibling), no bite-index-* dir is left in $TMPDIR.
        let _dir = temp_dir("panic");
        panic!("boom");
    }

    #[test]
    fn ingest_search_upsert_delete() {
        let dir = temp_dir("basic");
        let idx = EntityIndex::open(&dir).unwrap();

        idx.ingest(&[
            record(
                "mail",
                "m1",
                "Invoice from Acme",
                "your invoice for March is attached",
                Some(false),
            ),
            record("mail", "m2", "Hello", "plain greeting body", Some(true)),
            record("calendar", "c1", "Dentist", "appointment notes", None),
        ])
        .unwrap();

        // filter-only search: exact total
        let res = idx
            .search(&SearchQuery {
                app: Some("mail".into()),
                read: Some(false),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(res.total, Some(1));
        assert_eq!(res.hits[0].title.as_deref(), Some("Invoice from Acme"));

        // full-text search hits body text
        let res = idx
            .search(&SearchQuery {
                text: Some("invoice".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert!(res.hits.iter().any(|h| h.id == "m1"), "fts should hit m1");

        // cross-app scope
        let res = idx
            .search(&SearchQuery {
                app: Some("calendar".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(res.hits.len(), 1);
        assert_eq!(res.hits[0].app, "calendar");

        // upsert: same (app, id) updates, does not duplicate
        idx.ingest(&[record(
            "mail",
            "m2",
            "Hello (edited)",
            "plain greeting body",
            Some(true),
        )])
        .unwrap();
        assert_eq!(idx.count(Some("app = 'mail'")).unwrap(), 2);
        let res = idx
            .search(&SearchQuery {
                app: Some("mail".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert!(res
            .hits
            .iter()
            .any(|h| h.title.as_deref() == Some("Hello (edited)")));

        // delete_where for cache invalidation
        idx.delete_where("app = 'calendar'").unwrap();
        assert_eq!(idx.count(Some("app = 'calendar'")).unwrap(), 0);

        // stats
        let stats = idx.stats().unwrap();
        assert_eq!(stats.total, 2);
    }

    #[test]
    fn stats_newest_updated_ms_is_max_not_scan_order() {
        // regression: the limit-1 `updated_ms` query had no ORDER BY, so
        // `newest_updated_ms` echoed whichever row the scan happened to
        // return first (oldest here, by insertion order) instead of the max
        let dir = temp_dir("stats-ordering");
        let idx = EntityIndex::open(&dir).unwrap();

        let mut oldest = record("mail", "m1", "first", "oldest body", Some(true));
        oldest.updated_ms = Some(1_700_000_000_000);
        let mut middle = record("mail", "m2", "second", "middle body", Some(true));
        middle.updated_ms = Some(1_700_000_100_000);
        let mut no_timestamp = record("notes", "n1", "null", "no updated_ms", None);
        no_timestamp.updated_ms = None;
        let mut newest = record("mail", "m3", "third", "newest body", Some(false));
        newest.updated_ms = Some(1_700_000_200_000);
        // ingest oldest-first so scan order ≠ max
        idx.ingest(&[oldest, middle, no_timestamp, newest]).unwrap();

        let stats = idx.stats().unwrap();
        assert_eq!(stats.total, 4);
        assert_eq!(
            stats.newest_updated_ms,
            Some(1_700_000_200_000),
            "newest_updated_ms must be the max updated_ms, not an arbitrary row"
        );
    }

    #[test]
    fn stats_single_scan_contract_total_per_app_newest() {
        // stats() computes everything in one projected scan now; pin the
        // observable contract: total counts every row (unknown app values
        // included), per_app keeps the fixed app order and omits zero-count
        // apps, and newest skips rows without an updated_ms.
        let dir = temp_dir("stats-single-scan");
        let idx = EntityIndex::open(&dir).unwrap();

        let mut older = record("mail", "m1", "one", "body one", Some(true));
        older.updated_ms = Some(1_700_000_050_000);
        let mut no_timestamp = record("mail", "m2", "two", "body two", None);
        no_timestamp.updated_ms = None;
        let mut newest = record("notes", "n1", "note", "note body", None);
        newest.updated_ms = Some(1_700_000_150_000);
        let mut unknown_app = record("future-app", "x1", "mystery", "??", None);
        unknown_app.updated_ms = Some(9);
        idx.ingest(&[older, no_timestamp, newest, unknown_app])
            .unwrap();

        let stats = idx.stats().unwrap();
        assert_eq!(stats.total, 4, "unknown app values count toward total");
        assert_eq!(
            stats.per_app,
            vec![("mail".to_string(), 2), ("notes".to_string(), 1)],
            "per_app keeps fixed order, skips zero-count and unknown apps"
        );
        assert_eq!(stats.newest_updated_ms, Some(1_700_000_150_000));
    }

    #[test]
    fn search_contains_filters_apply() {
        // regression: *_contains used to be declared on SearchQuery but never
        // reached filter_sql — subject/body/from/to filters were silently
        // ignored by every index-backed search
        let dir = temp_dir("contains");
        let idx = EntityIndex::open(&dir).unwrap();
        let mut rec = record(
            "mail",
            "m1",
            "Invoice from Acme",
            "please pay soon",
            Some(false),
        );
        rec.participants = Some("billing@acme.example".into());
        let mut rec2 = record("mail", "m2", "Hello", "greeting body", Some(true));
        rec2.participants = Some("friend@elsewhere.example".into());
        // m3: NULL participants/title-adjacent shape — must never match
        let mut rec3 = record("mail", "m3", "No participants here", "some body", None);
        rec3.participants = None;
        idx.ingest(&[rec, rec2, rec3]).unwrap();

        // case-insensitive title substring
        let res = idx
            .search(&SearchQuery {
                title_contains: Some("INVOICE".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(res.total, Some(1));
        assert_eq!(res.hits[0].id, "m1");

        // participants substring (the `from` filter at the MCP boundary)
        let res = idx
            .search(&SearchQuery {
                participant_contains: Some("acme.example".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(res.total, Some(1));
        assert_eq!(res.hits[0].id, "m1");

        // content substring (the `body` filter)
        let res = idx
            .search(&SearchQuery {
                content_contains: Some("greeting".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(res.total, Some(1));
        assert_eq!(res.hits[0].id, "m2");

        // combining with a scalar filter still applies both
        let res = idx
            .search(&SearchQuery {
                participant_contains: Some("example".into()),
                read: Some(true),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(res.total, Some(1));
        assert_eq!(res.hits[0].id, "m2");

        // no match → empty, not everything
        let res = idx
            .search(&SearchQuery {
                title_contains: Some("nonexistent".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(res.total, Some(0));
        assert!(res.hits.is_empty());
    }

    #[test]
    fn search_contains_needles_with_like_wildcards_stay_literal() {
        // a needle containing % or _ must not widen into a LIKE pattern
        let dir = temp_dir("contains-wildcard");
        let idx = EntityIndex::open(&dir).unwrap();
        let mut rec = record("mail", "m1", "100% done_report", "body", Some(false));
        rec.participants = Some("a_b@c.example".into());
        let mut rec2 = record("mail", "m2", "plain title", "body", Some(false));
        rec2.participants = Some("axb@c.example".into());
        idx.ingest(&[rec, rec2]).unwrap();

        // literal "_" must NOT match both a_b and axb
        let res = idx
            .search(&SearchQuery {
                participant_contains: Some("a_b@".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(res.total, Some(1));
        assert_eq!(res.hits[0].id, "m1");

        // literal "%" must not act as a wildcard
        let res = idx
            .search(&SearchQuery {
                title_contains: Some("100%".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(res.total, Some(1));
        assert_eq!(res.hits[0].id, "m1");
    }

    #[test]
    fn search_filter_only_returns_newest_first() {
        // regression: filter-only queries had no ORDER BY, so a limit-N page
        // was an arbitrary scan-order subset — the CLI help and the
        // Apple-side `messages_search` both document newest-first
        let dir = temp_dir("filter-order");
        let idx = EntityIndex::open(&dir).unwrap();
        // ingest in shuffled order so physical/scan order ≠ recency order
        let stamps: [(&str, Option<i64>); 5] = [
            ("m3", Some(3_000)),
            ("m1", Some(1_000)),
            ("m4", None), // no updated_ms — must sink below every stamped row
            ("m5", Some(5_000)),
            ("m2", Some(2_000)),
        ];
        let recs: Vec<Record> = stamps
            .iter()
            .map(|(id, updated)| {
                let mut r = record("mail", id, "title", "content", Some(false));
                r.updated_ms = *updated;
                r
            })
            .collect();
        idx.ingest(&recs).unwrap();

        // limit below the row count: exactly the newest 3, descending
        let res = idx
            .search(&SearchQuery {
                limit: 3,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(res.total, Some(5));
        let ids: Vec<&str> = res.hits.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, vec!["m5", "m3", "m2"]);
        // the unstamped row never wins a slot while stamped rows exist
        assert!(!ids.contains(&"m4"));
    }

    #[test]
    fn search_fts_stays_relevance_ordered_not_newest_first() {
        // the ordering above must not leak into the FTS path: BM25 relevance
        // outranks recency there. m_old repeats the needle far more densely
        // (clearly higher score) but is the OLDEST row — if the order_by had
        // leaked into FTS, m_new would come first.
        let dir = temp_dir("fts-order");
        let idx = EntityIndex::open(&dir).unwrap();
        let mut old = record(
            "mail",
            "m_old",
            "alpha alpha alpha",
            "alpha alpha alpha",
            Some(false),
        );
        old.updated_ms = Some(1_000);
        let mut new = record("mail", "m_new", "beta gamma", "alpha", Some(false));
        new.updated_ms = Some(9_000);
        idx.ingest(&[old, new]).unwrap();

        let res = idx
            .search(&SearchQuery {
                text: Some("alpha".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(res.hits.len(), 2);
        assert_eq!(res.hits[0].id, "m_old");
    }

    #[test]
    fn search_skip_exact_count_returns_none_total_for_filter_queries() {
        // `skip_exact_count` trades the exact `total` (a second full
        // filtered COUNT scan) for latency: total comes back None while
        // the hits themselves are identical. The default stays exact.
        let dir = temp_dir("skip-count");
        let idx = EntityIndex::open(&dir).unwrap();
        idx.ingest(&[
            record("mail", "m1", "Invoice", "pay me", Some(false)),
            record("mail", "m2", "Hello", "hi there", Some(true)),
            record("notes", "n1", "Note", "remember", None),
        ])
        .unwrap();

        // default: filter-only query pays the count scan → exact total
        let exact = idx
            .search(&SearchQuery {
                app: Some("mail".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(exact.total, Some(2));
        assert_eq!(exact.hits.len(), 2);

        // skipped: same hits, no count scan → total is None
        let skipped = idx
            .search(&SearchQuery {
                app: Some("mail".into()),
                limit: 10,
                skip_exact_count: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(skipped.total, None);
        // (row order is not this test's subject — with newest-first ordering
        // both rows have no updated_ms, so compare the SET of hits)
        let mut got: Vec<_> = skipped.hits.iter().map(|h| h.id.clone()).collect();
        let mut want: Vec<_> = exact.hits.iter().map(|h| h.id.clone()).collect();
        got.sort();
        want.sort();
        assert_eq!(
            got, want,
            "skipping the count must not change the returned page"
        );

        // limit still applies with the count skipped
        let page = idx
            .search(&SearchQuery {
                app: Some("mail".into()),
                limit: 1,
                skip_exact_count: true,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.total, None);
        assert_eq!(page.hits.len(), 1);
    }

    #[test]
    fn fts_sees_rows_added_after_index_creation() {
        let dir = temp_dir("fts");
        let idx = EntityIndex::open(&dir).unwrap();
        idx.ingest(&[record(
            "mail",
            "a1",
            "first",
            "original body text",
            Some(true),
        )])
        .unwrap();
        idx.refresh_fts().unwrap();

        // new rows arriving after the FTS index was built must still be found
        idx.ingest(&[record(
            "mail",
            "a2",
            "second",
            "zebra unicorn query term",
            Some(false),
        )])
        .unwrap();

        let res = idx
            .search(&SearchQuery {
                text: Some("zebra".into()),
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert!(
            res.hits.iter().any(|h| h.id == "a2"),
            "FTS must see rows added after index creation"
        );
    }

    #[test]
    fn jsonl_ingest_and_wipe() {
        let dir = temp_dir("jsonl");
        let idx = EntityIndex::open(&dir).unwrap();
        let batch = dir.join("batch-1.jsonl");
        let line1 =
            serde_json::to_string(&record("notes", "n1", "Groceries", "milk eggs", None)).unwrap();
        let line2 =
            serde_json::to_string(&record("notes", "n2", "Ideas", "startup idea: bite", None))
                .unwrap();
        std::fs::write(&batch, format!("{line1}\n{line2}\n")).unwrap();

        let records = parse_jsonl(&batch).unwrap();
        assert_eq!(records.len(), 2);
        idx.ingest(&records).unwrap();
        assert_eq!(idx.count(Some("app = 'notes'")).unwrap(), 2);

        idx.wipe().unwrap();
        assert_eq!(idx.count(None).unwrap(), 0);
    }

    #[test]
    fn dir_is_private() {
        let dir = temp_dir("perm");
        let idx = EntityIndex::open(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        drop(idx);
    }

    #[test]
    fn search_projection_populates_all_hit_fields() {
        // The search() projection must carry every column hits_from_stream
        // reads: a column dropped from HIT_COLUMNS silently degrades to a
        // None/default Hit field (the reader resolves columns via
        // index_of().ok()). Pin both the filter-only and the FTS path —
        // FTS matches on the indexed search_text but must still return the
        // Hit-consumed columns under a Columns projection.
        let dir = temp_dir("projection");
        let idx = EntityIndex::open(&dir).unwrap();
        // 300 chars: exercises the >200-char snippet truncation, which
        // needs `content` present in the projection.
        let long = "needle word ".repeat(25);
        let mut r = record("mail", "m1", "a title", &long, Some(true));
        r.account = Some("acct".into());
        r.container = Some("INBOX/Sub".into());
        r.participants = Some("x@y.z".into());
        r.start_ms = Some(11);
        r.updated_ms = Some(22);
        r.flagged = Some(true);
        r.junk = Some(false);
        r.completed = Some(false);
        idx.ingest(&[r]).unwrap();

        let check = |h: &Hit| {
            assert_eq!(h.app, "mail");
            assert_eq!(h.id, "m1");
            assert_eq!(h.account.as_deref(), Some("acct"));
            assert_eq!(h.container.as_deref(), Some("INBOX/Sub"));
            assert_eq!(h.title.as_deref(), Some("a title"));
            assert_eq!(h.participants.as_deref(), Some("x@y.z"));
            assert_eq!(h.start_ms, Some(11));
            assert_eq!(h.updated_ms, Some(22));
            assert_eq!(h.read, Some(true));
            assert_eq!(h.flagged, Some(true));
            assert_eq!(h.junk, Some(false));
            assert_eq!(h.completed, Some(false));
            let s = h.snippet.as_deref().expect("snippet needs content column");
            assert_eq!(s.chars().count(), 201, "200 chars + ellipsis");
            assert!(s.ends_with('…'));
            assert!(s.starts_with("needle"));
        };

        // filter-only path
        let res = idx
            .search(&SearchQuery {
                limit: 5,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(res.hits.len(), 1);
        check(&res.hits[0]);

        // FTS path: match on the indexed search_text (title + content)
        let res = idx
            .search(&SearchQuery {
                text: Some("needle".into()),
                limit: 5,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(res.hits.len(), 1, "fts must match under projection");
        check(&res.hits[0]);
    }
}
