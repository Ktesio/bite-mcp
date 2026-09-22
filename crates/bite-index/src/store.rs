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

use arrow_array::{
    builder::{BooleanBuilder, Int32Builder, Int64Builder, StringBuilder},
    Array, BooleanArray, Int64Array, RecordBatch, RecordBatchReader, StringArray,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
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
    pub read: Option<bool>,
    pub flagged: Option<bool>,
    pub junk: Option<bool>,
    pub completed: Option<bool>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub limit: usize,
}

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
    /// Exact count when the query has no text component (filter-only COUNT);
    /// null for FTS queries where only the requested page was materialized.
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
    Arrow(#[from] arrow_schema::ArrowError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

pub struct EntityIndex {
    dir: PathBuf,
    rt: tokio::runtime::Runtime,
    /// exclusive writer lock, held for the store's lifetime
    _lock: std::fs::File,
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
    /// Open (creating if needed) with an exclusive writer lock. The lock file
    /// lives inside the private data dir.
    pub fn open(dir: &Path) -> Result<Self, IndexError> {
        bite_core::fsops::ensure_private_dir(dir)?;
        let lock_path = dir.join(".writer.lock");
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;
        fs2::FileExt::try_lock_exclusive(&lock_file).map_err(|e| IndexError::Other(format!(
            "another bite process is writing the index ({e})"
        )))?;
        bite_core::fsops::tighten_file(&lock_path).ok();

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let dir_str = dir.to_string_lossy().to_string();
        let conn = rt.block_on(async { lancedb::connect(dir_str.as_str()).execute().await })?;

        let index = Self { dir: dir.to_path_buf(), rt, _lock: lock_file, conn };
        index.rt.block_on(async { index.ensure_table().await })?;
        Ok(index)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
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

    /// Upsert records keyed on (app, id). Idempotent per batch — re-ingesting
    /// the same batch file is a no-op.
    pub fn ingest(&self, records: &[Record]) -> Result<IngestStats, IndexError> {
        if records.is_empty() {
            return Ok(IngestStats::default());
        }
        self.rt.block_on(async {
            let table = self.table().await?;
            let mut updated_rows: usize = 0;
            for chunk in records.chunks(512) {
                let batch = record_batch(chunk)?;
                let reader =
                    arrow_array::RecordBatchIterator::new(vec![Ok(batch)], schema());
                let mut mi = table.merge_insert(&["app", "id"]);
                mi.when_matched_update_all(None)
                    .when_not_matched_insert_all();
                let result = mi.execute(Box::new(reader)).await?;
                updated_rows += (result.num_updated_rows + result.num_inserted_rows) as usize;
            }
            // First-time FTS index creation once rows exist; afterwards the
            // index is refreshed lazily by the crawler job completion path.
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
            Ok(table.count_rows(filter.as_ref().map(|f| f.to_string())).await?)
        })
    }

    /// Search with optional full-text + scalar filters. `total` is exact for
    /// filter-only queries; None for FTS (page-capped).
    pub fn search(&self, q: &SearchQuery) -> Result<SearchResult, IndexError> {
        self.rt.block_on(async {
            let table = self.table().await?;
            let filter = filter_sql(q);

            let mut total = None;
            if q.text.is_none() {
                total = Some(table.count_rows(filter.as_ref().map(|f| f.to_string())).await?);
            }

            let mut builder = table.query();
            if let Some(f) = &filter {
                builder = builder.only_if(f.clone());
            }
            if let Some(text) = &q.text {
                builder = builder.full_text_search(
                    FullTextSearchQuery::new(text.clone())
                        .with_columns(&["search_text".to_string()])
                        .map_err(|e| IndexError::Other(e.to_string()))?,
                );
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
            let total = table.count_rows(None).await?;
            let mut per_app = Vec::new();
            let mut newest = None;
            for app in ["mail", "calendar", "reminders", "contacts", "notes", "messages"] {
                let n = table
                    .count_rows(Some(format!("app = {}", sql_str(app))))
                    .await?;
                if n > 0 {
                    per_app.push((app.to_string(), n));
                }
            }
            let stream = table
                .query()
                .select(lancedb::query::Select::Columns(vec![
                    "updated_ms".to_string()
                ]))
                .limit(1)
                .execute()
                .await?;
            use futures::StreamExt;
            let mut stream = stream;
            while let Some(batch) = stream.next().await {
                let batch = batch.map_err(IndexError::from)?;
                let col = batch.column(0);
                if let Some(arr) = col.as_any().downcast_ref::<Int64Array>() {
                    if arr.len() > 0 && !arr.is_null(0) {
                        newest = Some(arr.value(0));
                    }
                }
            }
            let fts_indexed = self.has_fts().await?;
            Ok(IndexStats { total, per_app, newest_updated_ms: newest, fts_indexed })
        })
    }

    /// Destroy all rows and indexes (the `index wipe` command). The store
    /// remains usable afterwards.
    pub fn wipe(&self) -> Result<(), IndexError> {
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
    if parts.is_empty() { None } else { Some(parts.join(" AND ")) }
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
        account.append_option(r.account.as_deref());
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
            batch.schema().index_of(name).ok().map(|i| batch.column(i).as_ref())
        };
        let as_str = |name: &str, i: usize| -> Option<String> {
            col(name).and_then(|a| a.as_any().downcast_ref::<StringArray>())
                .filter(|a| !a.is_null(i))
                .map(|a| a.value(i).to_string())
        };
        let as_bool = |name: &str, i: usize| -> Option<bool> {
            col(name).and_then(|a| a.as_any().downcast_ref::<BooleanArray>())
                .filter(|a| !a.is_null(i))
                .map(|a| a.value(i))
        };
        let as_i64 = |name: &str, i: usize| -> Option<i64> {
            col(name).and_then(|a| a.as_any().downcast_ref::<Int64Array>())
                .filter(|a| !a.is_null(i))
                .map(|a| a.value(i))
        };
        for i in 0..n {
            let content = as_str("content", i);
            let snippet = content.as_ref().map(|c| {
                let t = c.trim();
                if t.chars().count() <= 200 { t.to_string() } else { format!("{}…", t.chars().take(200).collect::<String>()) }
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
        records.push(serde_json::from_str::<Record>(line).map_err(|e| {
            IndexError::Other(format!("{}:{}: {e}", path.display(), i + 1))
        })?);
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

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "bite-index-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        d
    }

    #[test]
    fn ingest_search_upsert_delete() {
        let dir = temp_dir("basic");
        let idx = EntityIndex::open(&dir).unwrap();

        idx.ingest(&[
            record("mail", "m1", "Invoice from Acme", "your invoice for March is attached", Some(false)),
            record("mail", "m2", "Hello", "plain greeting body", Some(true)),
            record("calendar", "c1", "Dentist", "appointment notes", None),
        ])
        .unwrap();

        // filter-only search: exact total
        let res = idx.search(&SearchQuery {
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
            .search(&SearchQuery { text: Some("invoice".into()), limit: 10, ..Default::default() })
            .unwrap();
        assert!(res.hits.iter().any(|h| h.id == "m1"), "fts should hit m1");

        // cross-app scope
        let res = idx
            .search(&SearchQuery { app: Some("calendar".into()), limit: 10, ..Default::default() })
            .unwrap();
        assert_eq!(res.hits.len(), 1);
        assert_eq!(res.hits[0].app, "calendar");

        // upsert: same (app, id) updates, does not duplicate
        idx.ingest(&[record("mail", "m2", "Hello (edited)", "plain greeting body", Some(true))])
            .unwrap();
        assert_eq!(idx.count(Some("app = 'mail'")).unwrap(), 2);
        let res = idx
            .search(&SearchQuery { app: Some("mail".into()), limit: 10, ..Default::default() })
            .unwrap();
        assert!(res.hits.iter().any(|h| h.title.as_deref() == Some("Hello (edited)")));

        // delete_where for cache invalidation
        idx.delete_where("app = 'calendar'").unwrap();
        assert_eq!(idx.count(Some("app = 'calendar'")).unwrap(), 0);

        // stats
        let stats = idx.stats().unwrap();
        assert_eq!(stats.total, 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fts_sees_rows_added_after_index_creation() {
        let dir = temp_dir("fts");
        let idx = EntityIndex::open(&dir).unwrap();
        idx.ingest(&[record("mail", "a1", "first", "original body text", Some(true))])
            .unwrap();
        idx.refresh_fts().unwrap();

        // new rows arriving after the FTS index was built must still be found
        idx.ingest(&[record("mail", "a2", "second", "zebra unicorn query term", Some(false))])
            .unwrap();

        let res = idx
            .search(&SearchQuery { text: Some("zebra".into()), limit: 10, ..Default::default() })
            .unwrap();
        assert!(
            res.hits.iter().any(|h| h.id == "a2"),
            "FTS must see rows added after index creation"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jsonl_ingest_and_wipe() {
        let dir = temp_dir("jsonl");
        let idx = EntityIndex::open(&dir).unwrap();
        let batch = dir.join("batch-1.jsonl");
        let line1 = serde_json::to_string(&record("notes", "n1", "Groceries", "milk eggs", None)).unwrap();
        let line2 = serde_json::to_string(&record("notes", "n2", "Ideas", "startup idea: bite", None)).unwrap();
        std::fs::write(&batch, format!("{line1}\n{line2}\n")).unwrap();

        let records = parse_jsonl(&batch).unwrap();
        assert_eq!(records.len(), 2);
        idx.ingest(&records).unwrap();
        assert_eq!(idx.count(Some("app = 'notes'")).unwrap(), 2);

        idx.wipe().unwrap();
        assert_eq!(idx.count(None).unwrap(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dir_is_private() {
        let dir = temp_dir("perm");
        let idx = EntityIndex::open(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        drop(idx);
        std::fs::remove_dir_all(&dir).ok();
    }
}
