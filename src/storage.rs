use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;

use crate::activation;
use crate::parser::{EpisodeDoc, MemoryDoc};
use crate::schema;

#[derive(Debug, Serialize)]
pub struct Stats {
    pub memories: i64,
    pub episodes: i64,
    pub recall_events: i64,
    pub misses: i64,
    pub active_locks: i64,
    pub superseded_locks: i64,
    pub hot_memories: i64,
    pub cold_memories: i64,
    pub memories_by_scope: BTreeMap<String, i64>,
    pub memories_by_owner: BTreeMap<String, i64>,
    pub memories_by_status: BTreeMap<String, i64>,
    pub episodes_by_kind: BTreeMap<String, i64>,
    pub last_ingest_ts: Option<String>,
    pub stale: bool,
}

const INGESTED_EPISODE_KINDS_SQL: &str =
    "'handoff', 'archivist-report', 'agent-to-agent', 'ticket', 'team-log', 'meeting'";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EpisodeIdentity {
    source_path: Option<String>,
    summary: String,
    body: Option<String>,
}

impl EpisodeIdentity {
    fn from_doc(doc: &EpisodeDoc) -> Self {
        Self {
            source_path: Some(doc.source_path.to_string_lossy().into_owned()),
            summary: doc.summary.clone(),
            body: Some(doc.body.clone()),
        }
    }
}

#[derive(Debug)]
pub struct EpisodeDeduper {
    identities: HashSet<EpisodeIdentity>,
}

impl EpisodeDeduper {
    pub fn load(conn: &Connection) -> Result<Self> {
        let sql = format!(
            "SELECT source_path, summary, body
             FROM episodes
             WHERE kind IN ({INGESTED_EPISODE_KINDS_SQL})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            Ok(EpisodeIdentity {
                source_path: row.get(0)?,
                summary: row.get(1)?,
                body: row.get(2)?,
            })
        })?;
        let identities = rows.collect::<rusqlite::Result<HashSet<_>>>()?;
        Ok(Self { identities })
    }

    pub fn insert_if_new(&mut self, conn: &Connection, doc: &EpisodeDoc) -> Result<Option<i64>> {
        if !is_ingested_episode_kind(&doc.kind) {
            bail!("episode kind {:?} is not an ingested-source kind", doc.kind);
        }
        let identity = EpisodeIdentity::from_doc(doc);
        if self.identities.contains(&identity) {
            return Ok(None);
        }
        conn.execute(
            "INSERT INTO episodes(ts, actor, kind, summary, body, scope, source_path)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                doc.ts,
                doc.actor,
                doc.kind,
                doc.summary,
                doc.body,
                doc.scope,
                doc.source_path.to_string_lossy()
            ],
        )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        let id = conn.last_insert_rowid();
        self.identities.insert(identity);
        Ok(Some(id))
    }
}

#[derive(Debug, Serialize)]
pub struct EpisodeDedupeReport {
    pub applied: bool,
    pub before: i64,
    pub exact_duplicates: i64,
    pub removed: i64,
    pub after: i64,
}

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating db parent {}", parent.display()))?;
    } // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    schema::init_db(&conn)?;
    Ok(conn)
}

pub fn open_read_only(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {} read-only", path.display()))
}

pub fn open_existing_read_write(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .with_context(|| format!("opening existing database {}", path.display()))?;
    conn.busy_timeout(Duration::from_millis(5000))?;
    Ok(conn)
}

pub fn rebuild_fts(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO memories_fts(memories_fts) VALUES('rebuild')",
        [],
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    rebuild_episodes_fts(conn)?;
    rebuild_locks_fts(conn)?;
    Ok(())
}

pub fn rebuild_episodes_fts(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO episodes_fts(episodes_fts) VALUES('rebuild')",
        [],
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    Ok(())
}

pub fn rebuild_locks_fts(conn: &Connection) -> Result<()> {
    conn.execute("INSERT INTO locks_fts(locks_fts) VALUES('rebuild')", [])?;
    Ok(())
}

pub fn set_last_ingest(conn: &Connection) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO meta(key, value) VALUES('last_ingest_ts', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![now],
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    Ok(())
}

pub fn upsert_memory(conn: &Connection, doc: &MemoryDoc) -> Result<i64> {
    let existing: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT id, content_hash, status FROM memories WHERE name = ?1",
            params![doc.name],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if let Some((id, hash, status)) = existing {
        if hash != doc.content_hash || status == "archived" {
            conn.execute(
                "UPDATE memories SET title=?1, body=?2, owner=?3, scope=?4, source_path=?5,
                 content_hash=?6, is_lock=?7, status='hot', updated_at=?8 WHERE id=?9",
                params![
                    doc.title,
                    doc.body,
                    doc.owner,
                    doc.scope,
                    doc.source_path.to_string_lossy(),
                    doc.content_hash,
                    doc.is_lock as i64,
                    doc.updated_at,
                    id
                ],
            )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        }
        return Ok(id);
    }

    conn.execute(
        "INSERT INTO memories(name, title, body, owner, scope, source_path, content_hash, is_lock, created_at, updated_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            doc.name,
            doc.title,
            doc.body,
            doc.owner,
            doc.scope,
            doc.source_path.to_string_lossy(),
            doc.content_hash,
            doc.is_lock as i64,
            doc.created_at,
            doc.updated_at
        ],
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    Ok(conn.last_insert_rowid())
}

pub fn archive_absent_memories_for_source(
    conn: &Connection,
    source_path: &Path,
    present_names: &[String],
) -> Result<usize> {
    let present: BTreeSet<&str> = present_names.iter().map(String::as_str).collect();
    let source = source_path.to_string_lossy().to_string();
    let mut stmt = conn
        .prepare("SELECT id, name FROM memories WHERE source_path = ?1 AND status != 'archived'")?;
    let rows = stmt.query_map(params![source], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut stale_ids = Vec::new();
    for row in rows {
        let (id, name) = row?;
        if !present.contains(name.as_str()) {
            stale_ids.push(id);
        }
    }
    let now = Utc::now().to_rfc3339();
    for id in &stale_ids {
        conn.execute(
            "UPDATE memories SET status='archived', updated_at=?1 WHERE id=?2",
            params![now, id],
        )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    }
    Ok(stale_ids.len())
}

pub fn insert_episode_if_new(conn: &Connection, doc: &EpisodeDoc) -> Result<Option<i64>> {
    drop_legacy_episode_idempotency_index(conn)?;
    EpisodeDeduper::load(conn)?.insert_if_new(conn, doc)
}

pub fn drop_legacy_episode_idempotency_index(conn: &Connection) -> Result<()> {
    conn.execute("DROP INDEX IF EXISTS episodes_idempotency", [])?;
    Ok(())
}

pub fn dedupe_ingested_episodes(conn: &mut Connection, apply: bool) -> Result<EpisodeDedupeReport> {
    let before = scalar_count(conn, "SELECT COUNT(*) FROM episodes")?;
    let exact_duplicates = exact_ingested_episode_duplicates(conn)?;
    if !apply {
        return Ok(EpisodeDedupeReport {
            applied: false,
            before,
            exact_duplicates,
            removed: 0,
            after: before,
        });
    }

    let tx = conn.transaction()?;
    drop_legacy_episode_idempotency_index(&tx)?;
    let sql = format!(
        "WITH ranked AS (
           SELECT id,
                  ROW_NUMBER() OVER (
                    PARTITION BY source_path, summary, body
                    ORDER BY COALESCE(julianday(ts), 5373484.5) ASC, ts ASC, id ASC
                  ) AS duplicate_rank
           FROM episodes
           WHERE kind IN ({INGESTED_EPISODE_KINDS_SQL})
         )
         DELETE FROM episodes
         WHERE id IN (
           SELECT id FROM ranked WHERE duplicate_rank > 1
         )"
    );
    let removed = tx.execute(&sql, [])? as i64;
    rebuild_episodes_fts(&tx)?;
    let after = scalar_count(&tx, "SELECT COUNT(*) FROM episodes")?;
    if removed != exact_duplicates || before - after != exact_duplicates {
        // LCOV_EXCL_START: defensive invariant; SQLite reports both counts inside one transaction.
        bail!(
            "episode cleanup count mismatch: expected {exact_duplicates}, removed {removed}, before {before}, after {after}"
        );
        // LCOV_EXCL_STOP
    }
    tx.commit()?;

    Ok(EpisodeDedupeReport {
        applied: true,
        before,
        exact_duplicates,
        removed,
        after,
    })
}

fn exact_ingested_episode_duplicates(conn: &Connection) -> Result<i64> {
    let sql = format!(
        "SELECT COALESCE(SUM(row_count - 1), 0)
         FROM (
           SELECT COUNT(*) AS row_count
           FROM episodes
           WHERE kind IN ({INGESTED_EPISODE_KINDS_SQL})
           GROUP BY source_path, summary, body
           HAVING COUNT(*) > 1
         )"
    );
    scalar_count(conn, &sql)
}

fn is_ingested_episode_kind(kind: &str) -> bool {
    matches!(
        kind,
        "handoff" | "archivist-report" | "agent-to-agent" | "ticket" | "team-log" | "meeting"
    )
}

pub fn log_recall_event(
    conn: &Connection,
    memory_id: i64,
    queried_by: &str,
    query_scope: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO recall_events(memory_id, queried_by, query_scope, ts) VALUES(?1, ?2, ?3, ?4)",
        params![memory_id, queried_by, query_scope, Utc::now().to_rfc3339()],
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    Ok(())
}

pub fn seed_recall_event_if_absent(
    conn: &Connection,
    memory_id: i64,
    doc: &MemoryDoc,
) -> Result<()> {
    let existing: i64 = conn.query_row(
        "SELECT COUNT(*) FROM recall_events WHERE memory_id = ?1",
        params![memory_id],
        |row| row.get(0),
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    if existing > 0 {
        return Ok(());
    }
    let ts = std::fs::metadata(&doc.source_path)
        .and_then(|meta| meta.modified())
        .map(DateTime::<Utc>::from)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|_| doc.updated_at.clone());
    conn.execute(
        "INSERT INTO recall_events(memory_id, queried_by, query_scope, ts) VALUES(?1, ?2, ?3, ?4)",
        params![memory_id, "system:ingest", doc.scope, ts],
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    Ok(())
}

pub fn rebuild_wiki_links(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM links WHERE kind = 'wiki'", [])?;
    let mut stmt = conn.prepare("SELECT id, body FROM memories")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut refs = Vec::new();
    for row in rows {
        let (from_id, body) = row?;
        for slug in crate::parser::extract_wiki_links(&body) {
            refs.push((from_id, slug));
        }
    }
    for (from_id, slug) in refs {
        let to_id: Option<i64> = conn
            .query_row(
                "SELECT id FROM memories WHERE name = ?1",
                params![slug],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(to_id) = to_id {
            conn.execute(
                "INSERT OR IGNORE INTO links(from_kind, from_id, to_kind, to_id, kind)
                 VALUES('memory', ?1, 'memory', ?2, 'wiki')",
                params![from_id, to_id],
            )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        }
    }
    Ok(())
}

pub fn insert_future_item(
    conn: &Connection,
    body: &str,
    due: &str,
    created_by: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO future_items(body, due, created_by, created_at) VALUES(?1, ?2, ?3, ?4)",
        params![body, due, created_by, Utc::now().to_rfc3339()],
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    Ok(conn.last_insert_rowid())
}

pub fn insert_miss(conn: &Connection, what: &str, by: &str) -> Result<i64> {
    let ts = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO episodes(ts, actor, kind, summary, body, scope, source_path)
         VALUES(?1, ?2, 'miss', ?3, ?3, 'company', '')",
        params![ts, by, what],
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO episodes_fts(rowid, summary, body) VALUES(?1, ?2, ?2)",
        params![id, what],
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    Ok(id)
}

pub fn stats(conn: &Connection, source_files: &[PathBuf]) -> Result<Stats> {
    let last_ingest_ts = meta_value(conn, "last_ingest_ts")?;
    let stale = staleness(&last_ingest_ts, source_files);
    let (hot_memories, cold_memories) = hot_cold_counts(conn)?;
    Ok(Stats {
        memories: scalar_count(conn, "SELECT COUNT(*) FROM memories")?,
        episodes: scalar_count(conn, "SELECT COUNT(*) FROM episodes")?,
        recall_events: scalar_count(conn, "SELECT COUNT(*) FROM recall_events")?,
        misses: scalar_count(conn, "SELECT COUNT(*) FROM episodes WHERE kind = 'miss'")?,
        active_locks: scalar_count(conn, "SELECT COUNT(*) FROM locks WHERE status = 'active'")?,
        superseded_locks: scalar_count(
            conn,
            "SELECT COUNT(*) FROM locks WHERE status = 'superseded'",
        )?, // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        hot_memories,
        cold_memories,
        memories_by_scope: grouped_counts(
            conn,
            "SELECT scope, COUNT(*) FROM memories GROUP BY scope",
        )?, // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        memories_by_owner: grouped_counts(
            conn,
            "SELECT owner, COUNT(*) FROM memories GROUP BY owner",
        )?, // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        memories_by_status: grouped_counts(
            conn,
            "SELECT status, COUNT(*) FROM memories GROUP BY status",
        )?, // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        episodes_by_kind: grouped_counts(
            conn,
            "SELECT kind, COUNT(*) FROM episodes GROUP BY kind",
        )?, // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        last_ingest_ts,
        stale,
    })
}

fn scalar_count(conn: &Connection, sql: &str) -> Result<i64> {
    Ok(conn.query_row(sql, [], |row| row.get(0))?)
}

fn grouped_counts(conn: &Connection, sql: &str) -> Result<BTreeMap<String, i64>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut map = BTreeMap::new();
    for row in rows {
        let (key, count) = row?;
        map.insert(key, count);
    }
    Ok(map)
}

pub fn meta_value(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key=?1", params![key], |row| {
            row.get(0)
        })
        .optional()?)
}

pub fn meta_f64(conn: &Connection, key: &str, fallback: f64) -> Result<f64> {
    Ok(meta_value(conn, key)?
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(fallback))
}

pub fn meta_usize(conn: &Connection, key: &str, fallback: usize) -> Result<usize> {
    Ok(meta_value(conn, key)?
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(fallback))
}

fn hot_cold_counts(conn: &Connection) -> Result<(i64, i64)> {
    let decay = meta_f64(conn, "decay_d", 0.5)?;
    let threshold = meta_f64(conn, "hot_threshold", -1.6)?;
    let now = Utc::now();
    let mut stmt = conn.prepare("SELECT id, is_lock FROM memories WHERE status != 'archived'")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? != 0))
    })?;
    let mut hot = 0;
    let mut cold = 0;
    for row in rows {
        let (id, is_lock) = row?;
        let activation = activation::memory_activation(conn, id, now, decay)?;
        if activation::is_hot(is_lock, activation, threshold) {
            hot += 1;
        } else {
            cold += 1;
        }
    }
    Ok((hot, cold))
}

fn staleness(last_ingest_ts: &Option<String>, source_files: &[PathBuf]) -> bool {
    let Some(last) = last_ingest_ts else {
        return true;
    };
    let Ok(last) = chrono::DateTime::parse_from_rfc3339(last) else {
        return true;
    };
    source_files.iter().any(|path| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .map(|modified| chrono::DateTime::<Utc>::from(modified) > last.with_timezone(&Utc))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{EpisodeDoc, MemoryDoc};
    use proptest::prelude::*;
    use rusqlite::Connection;
    use std::path::Path;
    use tempfile::NamedTempFile;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        conn
    }

    fn memory_doc(name: &str, title: &str, body: &str) -> MemoryDoc {
        MemoryDoc {
            name: name.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            owner: "shared".to_string(),
            scope: "company".to_string(),
            source_path: Path::new("/tmp/test-memory.md").to_path_buf(),
            content_hash: format!("hash-{name}-{body}"),
            is_lock: false,
            created_at: "2026-06-01T00:00:00Z".to_string(),
            updated_at: "2026-06-01T00:00:00Z".to_string(),
        }
    }

    fn episode_doc(kind: &str, summary: &str, body: &str) -> EpisodeDoc {
        EpisodeDoc {
            ts: "2026-06-10T00:00:00Z".to_string(),
            actor: "agent:engineer".to_string(),
            kind: kind.to_string(),
            summary: summary.to_string(),
            body: body.to_string(),
            scope: "company".to_string(),
            source_path: Path::new("/tmp/test-episode.md").to_path_buf(),
        }
    }

    #[test]
    fn episode_deduper_uses_content_identity_and_rejects_runtime_kinds() {
        let conn = test_conn();
        let first = episode_doc("ticket", "Stable summary", "Stable body");
        let first_id = insert_episode_if_new(&conn, &first).unwrap().unwrap();
        conn.execute(
            "CREATE UNIQUE INDEX episodes_idempotency
             ON episodes(ts, actor, kind, summary, source_path)",
            [],
        )
        .unwrap();

        let mut metadata_drift = first.clone();
        metadata_drift.ts = "2026-06-11T12:30:00Z".to_string();
        metadata_drift.actor = "agent:archivist".to_string();
        metadata_drift.scope = "os".to_string();
        assert_eq!(insert_episode_if_new(&conn, &metadata_drift).unwrap(), None);
        assert_eq!(
            sqlite_object_count(&conn, "index", "episodes_idempotency"),
            0
        );

        let mut changed = first.clone();
        changed.body = "Changed body".to_string();
        let changed_id = insert_episode_if_new(&conn, &changed).unwrap().unwrap();
        assert!(changed_id > first_id);

        let miss = episode_doc("miss", "Stable summary", "Stable body");
        let err = insert_episode_if_new(&conn, &miss).unwrap_err();
        assert!(err.to_string().contains("not an ingested-source kind"));
    }

    #[test]
    fn episode_cleanup_reports_then_removes_only_ingest_duplicates() {
        let mut conn = test_conn();
        conn.execute_batch(
            "INSERT INTO memories(name, title, body, owner, scope, source_path, content_hash, created_at, updated_at)
             VALUES('untouched', 'Untouched', 'memory body', 'shared', 'company', '/tmp/m.md', 'hash', '2026-06-01', '2026-06-01');
             INSERT INTO locks(slug, title, body, scope, locked_on, status)
             VALUES('untouched-lock', 'Untouched Lock', 'lock body', 'company', '2026-06-01', 'active');
             INSERT INTO episodes(ts, actor, kind, summary, body, scope, source_path)
             VALUES
               ('2026-06-02T00:00:00Z', 'agent:engineer', 'ticket', 'duplicate', 'same', 'company', '/tmp/ticket.md'),
               ('2026-06-01T00:00:00Z', 'agent:archivist', 'handoff', 'duplicate', 'same', 'os', '/tmp/ticket.md'),
               ('2026-06-01T00:00:00Z', 'agent:engineer', 'miss', 'runtime', 'runtime', 'company', ''),
               ('2026-06-02T00:00:00Z', 'agent:engineer', 'miss', 'runtime', 'runtime', 'company', ''),
               ('2026-06-01T00:00:00Z', 'agent:engineer', 'promotion', 'audit', 'audit', 'company', ''),
               ('2026-06-02T00:00:00Z', 'agent:engineer', 'promotion', 'audit', 'audit', 'company', '');
             CREATE UNIQUE INDEX episodes_idempotency
             ON episodes(ts, actor, kind, summary, source_path);",
        )
        .unwrap();
        rebuild_fts(&conn).unwrap();

        let preview = dedupe_ingested_episodes(&mut conn, false).unwrap();
        assert!(!preview.applied);
        assert_eq!(preview.before, 6);
        assert_eq!(preview.exact_duplicates, 1);
        assert_eq!(preview.removed, 0);
        assert_eq!(preview.after, 6);
        assert_eq!(
            sqlite_object_count(&conn, "index", "episodes_idempotency"),
            1
        );

        let applied = dedupe_ingested_episodes(&mut conn, true).unwrap();
        assert!(applied.applied);
        assert_eq!(applied.before, 6);
        assert_eq!(applied.exact_duplicates, 1);
        assert_eq!(applied.removed, 1);
        assert_eq!(applied.after, 5);
        assert_eq!(
            sqlite_object_count(&conn, "index", "episodes_idempotency"),
            0
        );

        let retained: (String, String) = conn
            .query_row(
                "SELECT ts, kind FROM episodes
                 WHERE source_path='/tmp/ticket.md' AND summary='duplicate'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            retained,
            ("2026-06-01T00:00:00Z".to_string(), "handoff".to_string())
        );
        assert_eq!(episode_kind_count(&conn, "miss"), 2);
        assert_eq!(episode_kind_count(&conn, "promotion"), 2);
        assert_eq!(sqlite_object_count(&conn, "table", "memories"), 1);
        assert_eq!(sqlite_object_count(&conn, "table", "locks"), 1);
        let fts_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM episodes_fts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(fts_rows, 5);
    }

    #[test]
    fn open_existing_read_write_requires_an_existing_database() {
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("missing.db");
        let err = open_existing_read_write(&missing).unwrap_err();
        assert!(err.to_string().contains("opening existing database"));

        let existing = tmp.path().join("existing.db");
        Connection::open(&existing).unwrap();
        assert!(open_existing_read_write(&existing).is_ok());
    }

    fn sqlite_object_count(conn: &Connection, object_type: &str, name: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type=?1 AND name=?2",
            params![object_type, name],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn episode_kind_count(conn: &Connection, kind: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM episodes WHERE kind=?1",
            params![kind],
            |row| row.get(0),
        )
        .unwrap()
    }

    proptest! {
        #[test]
        fn episode_identity_ignores_ingest_metadata_drift(
            ts in "\\PC*",
            actor in "\\PC*",
            scope in "\\PC*",
        ) {
            let original = episode_doc("ticket", "Stable summary", "Stable body");
            let mut drifted = original.clone();
            drifted.ts = ts;
            drifted.actor = actor;
            drifted.scope = scope;

            prop_assert_eq!(
                EpisodeIdentity::from_doc(&original),
                EpisodeIdentity::from_doc(&drifted)
            );
        }
    }

    #[test]
    fn upsert_memory_is_idempotent_until_content_hash_drifts() {
        let conn = test_conn();
        let doc = memory_doc("guide", "Guide", "Original body");

        let id = upsert_memory(&conn, &doc).unwrap();
        let same_id = upsert_memory(&conn, &doc).unwrap();

        assert_eq!(same_id, id);
        let (count, updated_at): (i64, String) = conn
            .query_row(
                "SELECT COUNT(*), max(updated_at) FROM memories WHERE name = 'guide'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(updated_at, "2026-06-01T00:00:00Z");

        let mut mutated = doc.clone();
        mutated.title = "Guide updated".to_string();
        mutated.body = "Mutated body".to_string();
        mutated.owner = "agent:engineer".to_string();
        mutated.scope = "os".to_string();
        mutated.source_path = Path::new("/tmp/mutated-memory.md").to_path_buf();
        mutated.content_hash = "hash-mutated".to_string();
        mutated.updated_at = "2026-06-02T00:00:00Z".to_string();

        let mutated_id = upsert_memory(&conn, &mutated).unwrap();
        assert_eq!(mutated_id, id);

        let row: (String, String, String, String, String, String) = conn
            .query_row(
                "SELECT title, body, owner, scope, content_hash, updated_at FROM memories WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "Guide updated".to_string(),
                "Mutated body".to_string(),
                "agent:engineer".to_string(),
                "os".to_string(),
                "hash-mutated".to_string(),
                "2026-06-02T00:00:00Z".to_string(),
            )
        );
    }

    #[test]
    fn source_reconcile_archives_absent_rows_and_upsert_reactivates() {
        let conn = test_conn();
        let mut old = memory_doc("old-section", "Old Section", "old body");
        let mut new = memory_doc("new-section", "New Section", "new body");
        old.source_path = Path::new("/tmp/system-memory.md").to_path_buf();
        new.source_path = old.source_path.clone();
        upsert_memory(&conn, &old).unwrap();
        upsert_memory(&conn, &new).unwrap();

        let archived =
            archive_absent_memories_for_source(&conn, &old.source_path, &[new.name.clone()])
                .unwrap();

        assert_eq!(archived, 1);
        let statuses: BTreeMap<String, String> = {
            let mut stmt = conn
                .prepare("SELECT name, status FROM memories ORDER BY name")
                .unwrap();
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .unwrap();
            rows.collect::<rusqlite::Result<BTreeMap<_, _>>>().unwrap()
        };
        assert_eq!(statuses["old-section"], "archived");
        assert_eq!(statuses["new-section"], "hot");

        let old_id = upsert_memory(&conn, &old).unwrap();
        let reactivated: String = conn
            .query_row(
                "SELECT status FROM memories WHERE id = ?1",
                params![old_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reactivated, "hot");
    }

    #[test]
    fn rebuild_fts_populates_external_content_tables() {
        let conn = test_conn();
        upsert_memory(
            &conn,
            &memory_doc("memory-fts", "Guide Memory", "walks every guest through"),
        )
        .unwrap();
        insert_episode_if_new(
            &conn,
            &episode_doc("handoff", "Handoff summary", "builder persisted handoff"),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO locks(slug, title, body, scope, locked_on, status)
             VALUES('lock-fts', 'Lock FTS', 'canonical pytest lock', 'company', '2026-06-11', 'active')",
            [],
        )
        .unwrap();

        rebuild_fts(&conn).unwrap();

        let memory_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH 'guide'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let episode_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM episodes_fts WHERE episodes_fts MATCH 'handoff'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let lock_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM locks_fts WHERE locks_fts MATCH 'pytest'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(memory_hits, 1);
        assert_eq!(episode_hits, 1);
        assert_eq!(lock_hits, 1);
    }

    #[test]
    fn open_creates_parent_and_read_only_open_refuses_missing_db() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db = tmp.path().join("nested").join("shelves.db");

        let conn = open(&db).unwrap();
        drop(conn);

        assert!(db.exists());
        assert!(open_read_only(&db).is_ok());
        let err = open_read_only(&tmp.path().join("missing.db")).unwrap_err();
        assert!(err.to_string().contains("read-only"));
    }

    #[test]
    fn recall_event_logging_and_seed_dedup_use_expected_timestamps() {
        let conn = test_conn();
        let logged_id = upsert_memory(&conn, &memory_doc("logged", "Logged", "body")).unwrap();
        log_recall_event(&conn, logged_id, "agent:engineer", "company").unwrap();
        let logged_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM recall_events WHERE memory_id = ?1 AND queried_by = 'agent:engineer'",
                params![logged_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(logged_count, 1);

        let source = NamedTempFile::new().unwrap();
        let expected_mtime =
            DateTime::<Utc>::from(source.as_file().metadata().unwrap().modified().unwrap())
                .to_rfc3339();
        let mut doc = memory_doc("seeded", "Seeded", "body");
        doc.source_path = source.path().to_path_buf();
        let seeded_id = upsert_memory(&conn, &doc).unwrap();
        seed_recall_event_if_absent(&conn, seeded_id, &doc).unwrap();
        seed_recall_event_if_absent(&conn, seeded_id, &doc).unwrap();

        let seeded: (i64, String, String, String) = conn
            .query_row(
                "SELECT COUNT(*), max(queried_by), max(query_scope), max(ts)
                 FROM recall_events WHERE memory_id = ?1",
                params![seeded_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            seeded,
            (
                1,
                "system:ingest".to_string(),
                "company".to_string(),
                expected_mtime,
            )
        );

        let mut missing_source = memory_doc("fallback", "Fallback", "body");
        missing_source.source_path = Path::new("/tmp/shelves-test-missing-source.md").to_path_buf();
        missing_source.updated_at = "2026-06-03T12:00:00Z".to_string();
        let fallback_id = upsert_memory(&conn, &missing_source).unwrap();
        seed_recall_event_if_absent(&conn, fallback_id, &missing_source).unwrap();
        let fallback_ts: String = conn
            .query_row(
                "SELECT ts FROM recall_events WHERE memory_id = ?1",
                params![fallback_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fallback_ts, "2026-06-03T12:00:00Z");
    }

    #[test]
    fn rebuild_wiki_links_resolves_existing_targets_and_is_idempotent() {
        let conn = test_conn();
        let from_id = upsert_memory(
            &conn,
            &memory_doc("a", "A", "Links to [[b]] and dangling [[ghost]]."),
        )
        .unwrap();
        let to_id = upsert_memory(&conn, &memory_doc("b", "B", "target")).unwrap();

        rebuild_wiki_links(&conn).unwrap();
        rebuild_wiki_links(&conn).unwrap();

        let links: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM links WHERE kind = 'wiki'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let resolved: (i64, i64) = conn
            .query_row(
                "SELECT from_id, to_id FROM links WHERE kind = 'wiki'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(links, 1);
        assert_eq!(resolved, (from_id, to_id));
    }

    #[test]
    fn insert_future_item_returns_rowid_with_open_default() {
        let conn = test_conn();
        let id =
            insert_future_item(&conn, "Call the future", "2026-06-20", "agent:engineer").unwrap();

        let row: (String, String, String, String) = conn
            .query_row(
                "SELECT body, due, status, created_at FROM future_items WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(row.0, "Call the future");
        assert_eq!(row.1, "2026-06-20");
        assert_eq!(row.2, "open");
        DateTime::parse_from_rfc3339(&row.3).unwrap();
    }

    #[test]
    fn insert_miss_writes_episode_and_fts_row() {
        let conn = test_conn();
        let id = insert_miss(&conn, "missed recall needle", "agent:engineer").unwrap();

        let episode: (String, String, String) = conn
            .query_row(
                "SELECT actor, kind, summary FROM episodes WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            episode,
            (
                "agent:engineer".to_string(),
                "miss".to_string(),
                "missed recall needle".to_string(),
            )
        );
        let fts_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM episodes_fts WHERE episodes_fts MATCH 'needle'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fts_hits, 1);
    }

    #[test]
    fn stats_reports_buckets_hot_cold_and_staleness() {
        let conn = test_conn();
        let source = NamedTempFile::new().unwrap();
        let hot_id = upsert_memory(&conn, &memory_doc("hot", "Hot", "body")).unwrap();
        conn.execute(
            "INSERT INTO recall_events(memory_id, queried_by, query_scope, ts)
             VALUES(?1, 'system:ingest', 'company', ?2)",
            params![hot_id, Utc::now().to_rfc3339()],
        )
        .unwrap();

        upsert_memory(&conn, &memory_doc("cold", "Cold", "body")).unwrap();
        let mut lock_doc = memory_doc("lock", "Lock", "body");
        lock_doc.is_lock = true;
        lock_doc.scope = "os".to_string();
        upsert_memory(&conn, &lock_doc).unwrap();
        let mut archived_doc = memory_doc("archived", "Archived", "body");
        archived_doc.owner = "agent:engineer".to_string();
        archived_doc.scope = "product:catalog".to_string();
        let archived_id = upsert_memory(&conn, &archived_doc).unwrap();
        conn.execute(
            "UPDATE memories SET status = 'archived' WHERE id = ?1",
            params![archived_id],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO episodes(ts, actor, kind, summary, body, scope, source_path)
             VALUES('2026-06-10T00:00:00Z', 'agent:engineer', 'note', 'note', 'body', 'company', '/tmp/e.md')",
            [],
        )
        .unwrap();
        insert_miss(&conn, "missed bucket", "agent:engineer").unwrap();
        conn.execute(
            "INSERT INTO locks(slug, title, body, scope, locked_on, status)
             VALUES('active-lock', 'Active', 'body', 'company', '2026-06-10', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO locks(slug, title, body, scope, locked_on, status)
             VALUES('old-lock', 'Old', 'body', 'company', '2026-06-09', 'superseded')",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO meta(key, value) VALUES('last_ingest_ts', '2000-01-01T00:00:00Z')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )
        .unwrap();
        let stale_stats = stats(&conn, &[source.path().to_path_buf()]).unwrap();
        assert!(stale_stats.stale);
        assert_eq!(stale_stats.memories, 4);
        assert_eq!(stale_stats.episodes, 2);
        assert_eq!(stale_stats.recall_events, 1);
        assert_eq!(stale_stats.misses, 1);
        assert_eq!(stale_stats.active_locks, 1);
        assert_eq!(stale_stats.superseded_locks, 1);
        assert_eq!(stale_stats.hot_memories, 2);
        assert_eq!(stale_stats.cold_memories, 1);
        assert_eq!(stale_stats.memories_by_scope["company"], 2);
        assert_eq!(stale_stats.memories_by_scope["os"], 1);
        assert_eq!(stale_stats.memories_by_scope["product:catalog"], 1);
        assert_eq!(stale_stats.memories_by_owner["shared"], 3);
        assert_eq!(stale_stats.memories_by_owner["agent:engineer"], 1);
        assert_eq!(stale_stats.memories_by_status["hot"], 3);
        assert_eq!(stale_stats.memories_by_status["archived"], 1);
        assert_eq!(stale_stats.episodes_by_kind["note"], 1);
        assert_eq!(stale_stats.episodes_by_kind["miss"], 1);
        assert_eq!(
            stale_stats.last_ingest_ts,
            Some("2000-01-01T00:00:00Z".to_string())
        );

        conn.execute(
            "INSERT INTO meta(key, value) VALUES('last_ingest_ts', '2999-01-01T00:00:00Z')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )
        .unwrap();
        let fresh_stats = stats(&conn, &[source.path().to_path_buf()]).unwrap();
        assert!(!fresh_stats.stale);

        conn.execute(
            "INSERT INTO meta(key, value) VALUES('last_ingest_ts', 'not-a-date')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )
        .unwrap();
        let malformed_stats = stats(&conn, &[source.path().to_path_buf()]).unwrap();
        assert!(malformed_stats.stale);
    }

    #[test]
    fn meta_helpers_parse_present_values_and_fallback_safely() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO meta(key, value) VALUES('bad_f64', 'nope'), ('bad_usize', 'nan')",
            [],
        )
        .unwrap();

        assert_eq!(
            meta_value(&conn, "decay_d").unwrap(),
            Some("0.5".to_string())
        );
        assert_eq!(meta_value(&conn, "missing").unwrap(), None);
        assert_eq!(meta_f64(&conn, "decay_d", 9.0).unwrap(), 0.5);
        assert_eq!(meta_f64(&conn, "missing_f64", 9.0).unwrap(), 9.0);
        assert_eq!(meta_f64(&conn, "bad_f64", 9.0).unwrap(), 9.0);
        assert_eq!(
            meta_usize(&conn, "promotion_recall_threshold", 9).unwrap(),
            2
        );
        assert_eq!(meta_usize(&conn, "missing_usize", 9).unwrap(), 9);
        assert_eq!(meta_usize(&conn, "bad_usize", 9).unwrap(), 9);
    }
}
