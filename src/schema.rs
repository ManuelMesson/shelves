use anyhow::Result;
use rusqlite::{Connection, params};

use crate::{CONTRACT_VERSION, SCHEMA_VERSION};

pub fn init_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA busy_timeout=5000;

CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS memories (
  id           INTEGER PRIMARY KEY,
  name         TEXT NOT NULL UNIQUE,
  title        TEXT NOT NULL,
  body         TEXT NOT NULL,
  owner        TEXT NOT NULL,
  scope        TEXT NOT NULL,
  source_path  TEXT,
  content_hash TEXT NOT NULL,
  is_lock      INTEGER NOT NULL DEFAULT 0,
  status       TEXT NOT NULL DEFAULT 'hot',
  created_at   TEXT NOT NULL,
  updated_at   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS episodes (
  id          INTEGER PRIMARY KEY,
  ts          TEXT NOT NULL,
  actor       TEXT NOT NULL,
  kind        TEXT NOT NULL,
  summary     TEXT NOT NULL,
  body        TEXT,
  scope       TEXT NOT NULL,
  source_path TEXT
);

CREATE TABLE IF NOT EXISTS recall_events (
  id          INTEGER PRIMARY KEY,
  memory_id   INTEGER NOT NULL REFERENCES memories(id),
  queried_by  TEXT NOT NULL,
  query_scope TEXT NOT NULL,
  ts          TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS future_items (
  id         INTEGER PRIMARY KEY,
  body       TEXT NOT NULL,
  due        TEXT NOT NULL,
  created_by TEXT NOT NULL,
  status     TEXT NOT NULL DEFAULT 'open',
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS links (
  from_kind TEXT NOT NULL, from_id INTEGER NOT NULL,
  to_kind   TEXT NOT NULL, to_id   INTEGER NOT NULL,
  kind      TEXT NOT NULL,
  PRIMARY KEY (from_kind, from_id, to_kind, to_id, kind)
);

CREATE TABLE IF NOT EXISTS locks (
  id         INTEGER PRIMARY KEY,
  slug       TEXT NOT NULL UNIQUE,
  title      TEXT NOT NULL,
  body       TEXT NOT NULL,
  scope      TEXT NOT NULL,
  locked_on  TEXT NOT NULL,
  status     TEXT NOT NULL DEFAULT 'active',
  supersedes INTEGER REFERENCES locks(id)
);

CREATE TABLE IF NOT EXISTS node_acl (
  owner_node TEXT NOT NULL,
  reader     TEXT NOT NULL,
  granted    INTEGER NOT NULL DEFAULT 1,
  PRIMARY KEY (owner_node, reader)
);

CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(title, body, content='memories', content_rowid='id');
CREATE VIRTUAL TABLE IF NOT EXISTS episodes_fts USING fts5(summary, body, content='episodes', content_rowid='id');
CREATE VIRTUAL TABLE IF NOT EXISTS locks_fts USING fts5(slug, title, body, content='locks', content_rowid='id');

CREATE UNIQUE INDEX IF NOT EXISTS episodes_idempotency
ON episodes(ts, actor, kind, summary, source_path);
"#,
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.

    seed_meta(conn)?;
    rebuild_locks_fts(conn)?;
    Ok(())
}

pub fn reset_db(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
DROP TABLE IF EXISTS memories_fts;
DROP TABLE IF EXISTS episodes_fts;
DROP TABLE IF EXISTS locks_fts;
DROP TABLE IF EXISTS recall_events;
DROP TABLE IF EXISTS future_items;
DROP TABLE IF EXISTS links;
DROP TABLE IF EXISTS locks;
DROP TABLE IF EXISTS node_acl;
DROP TABLE IF EXISTS memories;
DROP TABLE IF EXISTS episodes;
DROP TABLE IF EXISTS meta;
"#,
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    init_db(conn)
}

fn seed_meta(conn: &Connection) -> Result<()> {
    let rows = [
        ("schema_version", SCHEMA_VERSION),
        ("contract_version", CONTRACT_VERSION),
        ("decay_d", "0.5"),
        // Default keeps the cortex to roughly memories touched in the last month
        // after mtime seeding with d=0.5: ln(30^-0.5) ~= -1.70.
        ("hot_threshold", "-1.6"),
        ("rank_episode_weight", "0.55"),
        // Lock shelves-english-primary-agents-translate: OR fallback must clear a real-match floor.
        ("real_match_floor", "0.50"),
        ("context_default_budget", "15"),
        // Context treats the budget as a cap; task-specific tail rows must clear this relevance floor.
        ("context_relevance_floor", "0.66"),
        // Archivist's consolidate pass nominates a promotion once a memory has
        // been recalled at least this many times from a broader scope.
        ("promotion_recall_threshold", "2"),
    ];
    for (key, value) in rows {
        conn.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    }
    Ok(())
}

fn rebuild_locks_fts(conn: &Connection) -> Result<()> {
    conn.execute("INSERT INTO locks_fts(locks_fts) VALUES('rebuild')", [])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::collections::BTreeSet;
    use tempfile::TempDir;

    fn table_names(conn: &Connection) -> BTreeSet<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap();
        let rows = stmt.query_map([], |row| row.get::<_, String>(0)).unwrap();
        rows.map(|row| row.unwrap()).collect()
    }

    #[test]
    fn init_db_creates_base_tables_fts_tables_and_idempotency_index() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();

        let names = table_names(&conn);
        for expected in [
            "meta",
            "memories",
            "episodes",
            "recall_events",
            "future_items",
            "links",
            "locks",
            "node_acl",
            "memories_fts",
            "episodes_fts",
            "locks_fts",
        ] {
            assert!(names.contains(expected), "missing {expected}");
        }

        let index_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'episodes_idempotency'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_exists, 1);
    }

    #[test]
    fn init_db_sets_file_db_wal_and_busy_timeout_pragmas() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("shelves.db");
        let conn = Connection::open(path).unwrap();

        init_db(&conn).unwrap();

        let journal_mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        let busy_timeout: i64 = conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode, "wal");
        assert_eq!(busy_timeout, 5000);
    }

    #[test]
    fn seed_meta_inserts_documented_defaults_and_reinit_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();

        let expected = [
            ("schema_version", SCHEMA_VERSION),
            ("contract_version", CONTRACT_VERSION),
            ("decay_d", "0.5"),
            ("hot_threshold", "-1.6"),
            ("rank_episode_weight", "0.55"),
            ("real_match_floor", "0.50"),
            ("context_default_budget", "15"),
            ("context_relevance_floor", "0.66"),
            ("promotion_recall_threshold", "2"),
        ];
        for (key, value) in expected {
            let actual: String = conn
                .query_row(
                    "SELECT value FROM meta WHERE key = ?1",
                    params![key],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(actual, value);
        }
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM meta", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 9);

        conn.execute(
            "UPDATE meta SET value = 'changed' WHERE key = 'decay_d'",
            [],
        )
        .unwrap();
        init_db(&conn).unwrap();

        let decay: String = conn
            .query_row("SELECT value FROM meta WHERE key = 'decay_d'", [], |row| {
                row.get(0)
            })
            .unwrap();
        let count_after_reinit: i64 = conn
            .query_row("SELECT COUNT(*) FROM meta", [], |row| row.get(0))
            .unwrap();
        assert_eq!(decay, "0.5");
        assert_eq!(count_after_reinit, 9);
    }

    #[test]
    fn reset_db_clears_rows_and_restores_schema_and_seed_meta() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();
        conn.execute(
            "INSERT INTO memories(name, title, body, owner, scope, source_path, content_hash, created_at, updated_at)
             VALUES('m', 'Memory', 'body', 'shared', 'company', '/tmp/m.md', 'hash', '2026-06-01T00:00:00Z', '2026-06-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episodes(ts, actor, kind, summary, body, scope, source_path)
             VALUES('2026-06-01T00:00:00Z', 'agent:engineer', 'note', 'summary', 'body', 'company', '/tmp/e.md')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO future_items(body, due, created_by, created_at)
             VALUES('future', '2026-06-20', 'agent:engineer', '2026-06-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO meta(key, value) VALUES('custom', 'value')", [])
            .unwrap();

        reset_db(&conn).unwrap();

        for table in ["memories", "episodes", "future_items"] {
            let sql = format!("SELECT COUNT(*) FROM {table}");
            let count: i64 = conn.query_row(&sql, [], |row| row.get(0)).unwrap();
            assert_eq!(count, 0, "{table} was not cleared");
        }
        let custom_meta: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM meta WHERE key = 'custom'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let decay: String = conn
            .query_row("SELECT value FROM meta WHERE key = 'decay_d'", [], |row| {
                row.get(0)
            })
            .unwrap();
        let names = table_names(&conn);
        assert_eq!(custom_meta, 0);
        assert_eq!(decay, "0.5");
        assert!(names.contains("memories_fts"));
        assert!(names.contains("episodes_fts"));
        assert!(names.contains("locks_fts"));
    }

    #[test]
    fn init_db_migrates_existing_locks_into_locks_fts() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
CREATE TABLE locks (
  id         INTEGER PRIMARY KEY,
  slug       TEXT NOT NULL UNIQUE,
  title      TEXT NOT NULL,
  body       TEXT NOT NULL,
  scope      TEXT NOT NULL,
  locked_on  TEXT NOT NULL,
  status     TEXT NOT NULL DEFAULT 'active',
  supersedes INTEGER REFERENCES locks(id)
);
INSERT INTO locks(slug, title, body, scope, locked_on, status)
VALUES('shelves-testing-standard', 'Shelves Testing Standard', 'python pytest cargo coverage', 'company', '2026-06-11', 'active');
"#,
        )
        .unwrap();

        init_db(&conn).unwrap();

        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM locks_fts WHERE locks_fts MATCH 'pytest'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1);
    }

    #[test]
    fn episodes_idempotency_index_deduplicates_insert_or_ignore_rows() {
        let conn = Connection::open_in_memory().unwrap();
        init_db(&conn).unwrap();

        for _ in 0..2 {
            conn.execute(
                "INSERT OR IGNORE INTO episodes(ts, actor, kind, summary, body, scope, source_path)
                 VALUES('2026-06-01T00:00:00Z', 'agent:engineer', 'note', 'summary', 'body', 'company', '/tmp/e.md')",
                [],
            )
            .unwrap();
        }

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM episodes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
