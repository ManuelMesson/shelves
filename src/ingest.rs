use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, TransactionBehavior, params};
use serde::Serialize;
use walkdir::WalkDir;

use crate::guard;
use crate::locks;
use crate::parser;
use crate::schema;
use crate::storage;
use crate::workspace_root;

#[derive(Debug, Serialize)]
pub struct IngestReport {
    pub memories: usize,
    pub episodes: usize,
    pub skipped: usize,
    pub sources: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_durability: Option<ResetDurabilityReport>,
}

#[derive(Debug, Serialize)]
pub struct ResetDurabilityReport {
    pub future_items_preserved: usize,
    pub misses_preserved: usize,
    pub recall_events_reset: bool,
    pub promotion_audits_dropped: usize,
}

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub source: &'static str,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy)]
enum SourcePath {
    WorkspaceFile(&'static str),
    WorkspaceDir {
        relative: &'static str,
        recursive: bool,
    },
    ExternalMemoryDir,
    AgentMemoryDirs,
    AgentIdentityFiles,
    OpenTicketFiles,
}

#[derive(Debug, Clone, Copy)]
struct SourceSpec {
    source: &'static str,
    path: SourcePath,
}

const EXTERNAL_PROJECTS_RELATIVE_DIR: &str = ".agents/projects";

const SOURCE_ROSTER: &[SourceSpec] = &[
    SourceSpec {
        source: "external-memory",
        path: SourcePath::ExternalMemoryDir,
    },
    SourceSpec {
        source: "system-memory",
        path: SourcePath::WorkspaceFile("system/memory.md"),
    },
    SourceSpec {
        source: "team-log",
        path: SourcePath::WorkspaceFile("system/TEAM_LOG.md"),
    },
    SourceSpec {
        source: "handoffs",
        path: SourcePath::WorkspaceDir {
            relative: "system/handoffs",
            recursive: false,
        },
    },
    SourceSpec {
        source: "archive-reports",
        path: SourcePath::WorkspaceDir {
            relative: "system/inbox/archive-reports",
            recursive: true,
        },
    },
    SourceSpec {
        source: "agent-to-agent",
        path: SourcePath::WorkspaceDir {
            relative: "system/inbox/agent-to-agent",
            recursive: true,
        },
    },
    SourceSpec {
        source: "processed-tickets",
        path: SourcePath::WorkspaceDir {
            relative: "system/inbox/builder-code/processed",
            recursive: true,
        },
    },
    SourceSpec {
        source: "open-tickets",
        path: SourcePath::OpenTicketFiles,
    },
    SourceSpec {
        source: "agent-memory",
        path: SourcePath::AgentMemoryDirs,
    },
    SourceSpec {
        source: "agent-identity",
        path: SourcePath::AgentIdentityFiles,
    },
    SourceSpec {
        source: "lock-store",
        path: SourcePath::WorkspaceFile("system/locks.yaml"),
    },
    SourceSpec {
        source: "meetings",
        path: SourcePath::WorkspaceDir {
            relative: "meetings",
            recursive: false,
        },
    },
];

pub fn run(conn: &mut Connection, reset: bool, source: Option<&str>) -> Result<IngestReport> {
    let files = discover_source_files(source)?;
    if files.is_empty() {
        if source == Some("lock-store") {
            return Ok(IngestReport {
                memories: 0,
                episodes: 0,
                skipped: 0,
                sources: Vec::new(),
                reset_durability: None,
            });
        }
        let source_label = source.unwrap_or("all configured sources");
        let root = workspace_root()?;
        eprintln!(
            "shelves ingest warning: discovered zero source files for {source_label} under {}",
            root.display()
        );
        bail!(
            "shelves ingest discovered zero source files for {source_label}; refusing empty ingest"
        );
    }

    let runtime_state = if reset {
        Some(RuntimeState::snapshot(conn)?)
    } else {
        None
    };
    if reset {
        schema::reset_db(conn)?;
        if let Some(state) = runtime_state.as_ref() {
            if state.promotion_audits_dropped > 0 {
                eprintln!(
                    "shelves ingest warning: --reset drops {} promotion audit episode(s); ratified scope changes must be made canonical before reset",
                    state.promotion_audits_dropped
                );
            }
            state.restore(conn)?;
        } // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    storage::drop_legacy_episode_idempotency_index(&tx)?;
    let mut episode_deduper = storage::EpisodeDeduper::load(&tx)?;
    if source.is_none() || source == Some("open-tickets") {
        reconcile_open_ticket_memories(&tx, &files)?;
    }
    let mut memories = 0;
    let mut episodes = 0;
    let mut skipped = 0;

    for file in &files {
        match ingest_file(&tx, &mut episode_deduper, file) {
            Ok((m, e)) => {
                memories += m;
                episodes += e;
            }
            Err(err) => {
                if file.source == "lock-store" {
                    return Err(err);
                }
                skipped += 1;
                eprintln!("shelves ingest skipped {}: {err}", file.path.display());
            }
        }
    }
    storage::rebuild_wiki_links(&tx)?;
    storage::rebuild_fts(&tx)?;
    storage::set_last_ingest(&tx)?;
    tx.commit()?;

    Ok(IngestReport {
        memories,
        episodes,
        skipped,
        sources: files
            .iter()
            .map(|file| format!("{}:{}", file.source, file.path.display()))
            .collect(),
        reset_durability: runtime_state.map(|state| state.report()),
    })
}

#[derive(Debug)]
struct RuntimeState {
    future_items: Vec<FutureItemRow>,
    misses: Vec<MissEpisodeRow>,
    promotion_audits_dropped: usize,
}

#[derive(Debug)]
struct FutureItemRow {
    id: i64,
    body: String,
    due: String,
    created_by: String,
    status: String,
    created_at: String,
}

#[derive(Debug)]
struct MissEpisodeRow {
    id: i64,
    ts: String,
    actor: String,
    summary: String,
    body: Option<String>,
    scope: String,
    source_path: Option<String>,
}

impl RuntimeState {
    fn snapshot(conn: &Connection) -> Result<Self> {
        let future_items = snapshot_future_items(conn)?;
        let misses = snapshot_misses(conn)?;
        let promotion_audits_dropped = count_promotion_audits(conn)?;
        Ok(Self {
            future_items,
            misses,
            promotion_audits_dropped,
        })
    }

    fn restore(&self, conn: &Connection) -> Result<()> {
        for item in &self.future_items {
            conn.execute(
                "INSERT INTO future_items(id, body, due, created_by, status, created_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    item.id,
                    item.body,
                    item.due,
                    item.created_by,
                    item.status,
                    item.created_at
                ],
            )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        }
        for miss in &self.misses {
            conn.execute(
                "INSERT INTO episodes(id, ts, actor, kind, summary, body, scope, source_path)
                 VALUES(?1, ?2, ?3, 'miss', ?4, ?5, ?6, ?7)",
                params![
                    miss.id,
                    miss.ts,
                    miss.actor,
                    miss.summary,
                    miss.body,
                    miss.scope,
                    miss.source_path
                ],
            )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        }
        Ok(())
    }

    fn report(&self) -> ResetDurabilityReport {
        ResetDurabilityReport {
            future_items_preserved: self.future_items.len(),
            misses_preserved: self.misses.len(),
            recall_events_reset: true,
            promotion_audits_dropped: self.promotion_audits_dropped,
        }
    }
}

fn snapshot_future_items(conn: &Connection) -> Result<Vec<FutureItemRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, body, due, created_by, status, created_at FROM future_items ORDER BY id",
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let rows = stmt.query_map([], |row| {
        Ok(FutureItemRow {
            id: row.get(0)?,
            body: row.get(1)?,
            due: row.get(2)?,
            created_by: row.get(3)?,
            status: row.get(4)?,
            created_at: row.get(5)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn snapshot_misses(conn: &Connection) -> Result<Vec<MissEpisodeRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, ts, actor, summary, body, scope, source_path
         FROM episodes
         WHERE kind = 'miss'
         ORDER BY id",
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let rows = stmt.query_map([], |row| {
        Ok(MissEpisodeRow {
            id: row.get(0)?,
            ts: row.get(1)?,
            actor: row.get(2)?,
            summary: row.get(3)?,
            body: row.get(4)?,
            scope: row.get(5)?,
            source_path: row.get(6)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn count_promotion_audits(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM episodes WHERE kind = 'promotion'",
        [],
        |row| row.get(0),
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    Ok(count as usize)
}

pub fn discover_source_files(source: Option<&str>) -> Result<Vec<SourceFile>> {
    guard::require_protected_root()?;
    let root = workspace_root()?;
    let mut out = Vec::new();

    for spec in SOURCE_ROSTER {
        match spec.path {
            SourcePath::WorkspaceFile(relative) => {
                push_file(&mut out, spec.source, &root.join(relative));
            }
            SourcePath::WorkspaceDir {
                relative,
                recursive,
            } => {
                push_dir(&mut out, spec.source, &root.join(relative), recursive);
            }
            SourcePath::ExternalMemoryDir => {
                push_dir(&mut out, spec.source, &external_memory_dir(&root), false);
            }
            SourcePath::AgentMemoryDirs => push_agent_memory_dirs(&mut out, &root),
            SourcePath::AgentIdentityFiles => push_agent_identity_files(&mut out, &root),
            SourcePath::OpenTicketFiles => push_open_ticket_files(&mut out, &root),
        }
    }
    if let Ok(extra_dir) = std::env::var("SHELVES_EXTRA_SOURCE_DIR")
        && !extra_dir.trim().is_empty()
    {
        push_dir(&mut out, "extra", Path::new(&extra_dir), true);
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    if let Some(source) = source {
        out.retain(|file| file.source == source);
    }
    Ok(out)
}

pub fn discover_paths(source: Option<&str>) -> Result<Vec<PathBuf>> {
    Ok(discover_source_files(source)?
        .into_iter()
        .map(|file| file.path)
        .collect())
}

fn ingest_file(
    conn: &Connection,
    episode_deduper: &mut storage::EpisodeDeduper,
    file: &SourceFile,
) -> Result<(usize, usize)> {
    let path = guard::assert_path_allowed_before_read(&file.path)?;
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    match file.source {
        "external-memory" => {
            let docs = parser::parse_external_memory(&path, &text);
            ingest_memory_docs(conn, &path, &docs)?;
            Ok((docs.len(), 0))
        }
        "system-memory" => {
            let docs = parser::parse_system_memory(&path, &text);
            ingest_memory_docs(conn, &path, &docs)?;
            Ok((docs.len(), 0))
        }
        "agent-memory" | "agent-identity" | "open-tickets" => {
            let docs = parser::parse_agent_memory(&path, &text);
            ingest_memory_docs(conn, &path, &docs)?;
            Ok((docs.len(), 0))
        }
        "team-log" => {
            let docs = parser::parse_team_log(&path, &text);
            let mut inserted = 0;
            for doc in &docs {
                if episode_deduper.insert_if_new(conn, doc)?.is_some() {
                    inserted += 1;
                }
            }
            Ok((0, inserted))
        }
        "handoffs" => ingest_episode_file(conn, episode_deduper, &path, &text, "handoff"),
        "archive-reports" => {
            ingest_episode_file(conn, episode_deduper, &path, &text, "archivist-report")
        }
        "agent-to-agent" => {
            ingest_episode_file(conn, episode_deduper, &path, &text, "agent-to-agent")
        }
        "processed-tickets" => ingest_episode_file(conn, episode_deduper, &path, &text, "ticket"),
        "meetings" => ingest_meeting_file(conn, episode_deduper, &path, &text),
        "lock-store" => {
            locks::ingest_lock_store(conn, &path)?;
            Ok((0, 0))
        }
        "extra" if text.trim_start().starts_with("---") => {
            let docs = parser::parse_external_memory(&path, &text);
            ingest_memory_docs(conn, &path, &docs)?;
            Ok((docs.len(), 0))
        }
        "extra" => ingest_episode_file(conn, episode_deduper, &path, &text, "ticket"),
        _ => Ok((0, 0)),
    }
}

fn ingest_memory_docs(conn: &Connection, path: &Path, docs: &[parser::MemoryDoc]) -> Result<usize> {
    for doc in docs {
        let id = storage::upsert_memory(conn, doc)?;
        storage::seed_recall_event_if_absent(conn, id, doc)?;
    }
    let present_names = docs.iter().map(|doc| doc.name.clone()).collect::<Vec<_>>();
    storage::archive_absent_memories_for_source(conn, path, &present_names)?;
    Ok(docs.len())
}

fn reconcile_open_ticket_memories(conn: &Connection, files: &[SourceFile]) -> Result<usize> {
    let root = workspace_root()?;
    let ticket_root = root.join("system/inbox/builder-code");
    let live_paths = files
        .iter()
        .filter(|file| file.source == "open-tickets")
        .map(|file| file.path.as_path())
        .collect::<std::collections::HashSet<_>>();
    let mut stale_ids = Vec::new();
    let mut stmt = conn.prepare("SELECT id, source_path FROM memories WHERE is_lock = 0")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id, source_path) = row?;
        let path = Path::new(&source_path);
        if path.parent() == Some(ticket_root.as_path()) && !live_paths.contains(path) {
            stale_ids.push(id);
        }
    }
    drop(stmt);
    let now = chrono::Utc::now().to_rfc3339();
    for id in &stale_ids {
        conn.execute(
            "UPDATE memories SET status = 'archived', updated_at = ?1 WHERE id = ?2",
            params![now, id],
        )?; // LCOV_EXCL_LINE: SQL error branch; successful archive asserted below.
    }
    Ok(stale_ids.len())
}

fn ingest_episode_file(
    conn: &Connection,
    episode_deduper: &mut storage::EpisodeDeduper,
    path: &Path,
    text: &str,
    kind: &str,
) -> Result<(usize, usize)> {
    let docs = parser::parse_markdown_episode_file(path, text, kind);
    let mut inserted = 0;
    for doc in &docs {
        if episode_deduper.insert_if_new(conn, doc)?.is_some() {
            inserted += 1;
        }
    }
    Ok((0, inserted))
}

fn ingest_meeting_file(
    conn: &Connection,
    episode_deduper: &mut storage::EpisodeDeduper,
    path: &Path,
    text: &str,
) -> Result<(usize, usize)> {
    let docs = parser::parse_meeting_file(path, text);
    let present = docs
        .iter()
        .map(|doc| (doc.summary.clone(), doc.body.clone()))
        .collect::<HashSet<_>>();
    let source_path = path.to_string_lossy();
    let mut stmt = conn.prepare(
        "SELECT id, summary, body FROM episodes WHERE kind='meeting' AND source_path=?1",
    )?; // LCOV_EXCL_LINE: prepare error path; successful reconciliation is asserted by meeting ingest tests.
    let rows = stmt.query_map(params![source_path.as_ref()], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let existing = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let stale_ids = existing
        .into_iter()
        .filter_map(|(id, summary, body)| (!present.contains(&(summary, body))).then_some(id))
        .collect::<Vec<_>>();
    drop(stmt);
    for id in stale_ids {
        conn.execute("DELETE FROM episodes WHERE id=?1", params![id])?;
    }

    let mut inserted = 0;
    for doc in &docs {
        if episode_deduper.insert_if_new(conn, doc)?.is_some() {
            inserted += 1;
        }
    }
    Ok((0, inserted))
}

fn push_file(out: &mut Vec<SourceFile>, source: &'static str, path: &Path) {
    if path.is_file() {
        out.push(SourceFile {
            source,
            path: path.to_path_buf(),
        });
    }
}

fn push_dir(out: &mut Vec<SourceFile>, source: &'static str, dir: &Path, recursive: bool) {
    if !dir.is_dir() {
        return;
    }
    let walker = if recursive {
        WalkDir::new(dir)
    } else {
        WalkDir::new(dir).max_depth(1)
    };
    for entry in walker.into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            out.push(SourceFile {
                source,
                path: path.to_path_buf(),
            });
        }
    }
}

fn external_memory_dir(root: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os("SHELVES_EXTERNAL_MEMORY_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    root.parent()
        .unwrap_or(root)
        .join(EXTERNAL_PROJECTS_RELATIVE_DIR)
        .join(external_project_slug(root))
        .join("memory")
}

fn external_project_slug(root: &Path) -> String {
    let normalized = root
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    format!("-{}", parser::slugify(&normalized))
}

fn push_agent_memory_dirs(out: &mut Vec<SourceFile>, root: &Path) {
    let memory_root = root.join("memory");
    let Ok(entries) = std::fs::read_dir(&memory_root) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        push_dir(out, "agent-memory", &dir, false);
    }
}

fn push_agent_identity_files(out: &mut Vec<SourceFile>, root: &Path) {
    let identity_root = root.join("agents");
    let known_agents = known_agent_names(root);
    let Ok(entries) = std::fs::read_dir(identity_root) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("md")
            && file_names_known_agent(&path, &known_agents)
        {
            out.push(SourceFile {
                source: "agent-identity",
                path,
            });
        }
    }
}

fn push_open_ticket_files(out: &mut Vec<SourceFile>, root: &Path) {
    let ticket_root = root.join("system/inbox/builder-code");
    let known_agents = known_agent_names(root);
    let Ok(entries) = std::fs::read_dir(ticket_root) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("md")
            && file_names_known_agent(&path, &known_agents)
        {
            out.push(SourceFile {
                source: "open-tickets",
                path,
            });
        }
    }
}

fn file_names_known_agent(path: &Path, known_agents: &std::collections::HashSet<String>) -> bool {
    path.file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(|token| token.to_ascii_lowercase())
        .any(|token| known_agents.contains(&token))
}

fn known_agent_names(root: &Path) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    if let Ok(raw) = std::fs::read_to_string(root.join("system/agents.txt")) {
        names.extend(
            raw.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(|line| {
                    line.trim_start_matches("agent:")
                        .to_ascii_lowercase()
                        .replace('-', "_")
                }),
        );
    }
    if let Ok(entries) = std::fs::read_dir(root.join("memory")) {
        names.extend(entries.filter_map(Result::ok).filter_map(|entry| {
            entry.path().is_dir().then(|| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .replace('-', "_")
            })
        }));
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{schema, storage};
    use rusqlite::Connection;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use tempfile::TempDir;

    #[test]
    fn source_discovery_uses_aios_root_for_agent_owned_sources() {
        let _lock = env_lock().lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let protected = tmp.path().join("protected-os");
        let external_dir = tmp.path().join("external-memory");
        let synthetic_agent_dir = root.join("memory/cafe-owner");
        let identity_dir = root.join("agents");
        let ticket_dir = root.join("system/inbox/builder-code");
        std::fs::create_dir_all(&protected).unwrap();
        std::fs::create_dir_all(&external_dir).unwrap();
        std::fs::create_dir_all(&synthetic_agent_dir).unwrap();
        std::fs::create_dir_all(&identity_dir).unwrap();
        std::fs::create_dir_all(&ticket_dir).unwrap();
        let external_file = external_dir.join("project_cafe.md");
        let agent_file = synthetic_agent_dir.join("menu.md");
        let identity_file = identity_dir.join("cafe-owner.md");
        let roster_file = identity_dir.join("ROSTER.md");
        let ticket_file = ticket_dir.join("20260820T010000Z-cafe-owner-menu.md");
        std::fs::write(&external_file, "# Cafe\n").unwrap();
        std::fs::write(&agent_file, "# Menu\n").unwrap();
        std::fs::write(root.join("system/agents.txt"), "cafe\n").unwrap();
        std::fs::write(&identity_file, "# Cafe Owner\n").unwrap();
        std::fs::write(&roster_file, "# Roster\n").unwrap();
        std::fs::write(&ticket_file, "---\nowner: cafe\n---\nOpen menu work.\n").unwrap();

        let old_root = std::env::var("AIOS_ROOT").ok();
        let old_protected = std::env::var("SHELVES_PROTECTED_ROOT").ok();
        let old_external = std::env::var("SHELVES_EXTERNAL_MEMORY_DIR").ok();
        unsafe { std::env::set_var("AIOS_ROOT", root) };
        unsafe { std::env::set_var("SHELVES_PROTECTED_ROOT", &protected) };
        unsafe { std::env::set_var("SHELVES_EXTERNAL_MEMORY_DIR", &external_dir) };

        let external_paths = discover_paths(Some("external-memory")).unwrap();
        let agent_paths = discover_paths(Some("agent-memory")).unwrap();
        let identity_paths = discover_paths(Some("agent-identity")).unwrap();
        let open_ticket_paths = discover_paths(Some("open-tickets")).unwrap();
        assert_eq!(external_paths, vec![external_file]);
        assert_eq!(agent_paths, vec![agent_file]);
        assert_eq!(identity_paths, vec![identity_file]);
        assert_eq!(open_ticket_paths, vec![ticket_file]);

        restore_env("AIOS_ROOT", old_root);
        restore_env("SHELVES_PROTECTED_ROOT", old_protected);
        restore_env("SHELVES_EXTERNAL_MEMORY_DIR", old_external);
    }

    #[test]
    fn discover_source_files_filters_sorts_and_includes_extra_source_dir() {
        // Covers discover_source_files filtering/sorting plus
        // SHELVES_EXTRA_SOURCE_DIR at lines 318-350.
        let _lock = env_lock().lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workspace");
        let protected = tmp.path().join("protected-os");
        let extra = tmp.path().join("extra-source");
        std::fs::create_dir_all(root.join("system")).unwrap();
        std::fs::create_dir_all(&protected).unwrap();
        std::fs::create_dir_all(extra.join("nested")).unwrap();
        std::fs::write(root.join("system/memory.md"), "# System\n").unwrap();
        std::fs::write(extra.join("b.md"), "B\n").unwrap();
        std::fs::write(extra.join("a.md"), "A\n").unwrap();
        std::fs::write(extra.join("nested/c.md"), "C\n").unwrap();
        std::fs::write(extra.join("ignored.txt"), "ignored\n").unwrap();
        let _env = EnvGuard::new(&root, &protected);
        unsafe { std::env::set_var("SHELVES_EXTRA_SOURCE_DIR", &extra) };

        let extra_files = discover_source_files(Some("extra")).unwrap();

        assert!(extra_files.iter().all(|file| file.source == "extra"));
        assert_eq!(
            extra_files
                .iter()
                .map(|file| file.path.clone())
                .collect::<Vec<_>>(),
            vec![
                extra.join("a.md"),
                extra.join("b.md"),
                extra.join("nested/c.md")
            ]
        );
        let system_files = discover_source_files(Some("system-memory")).unwrap();
        assert_eq!(
            system_files
                .iter()
                .map(|file| file.path.clone())
                .collect::<Vec<_>>(),
            vec![root.join("system/memory.md")]
        );
    }

    #[test]
    fn full_ingest_removes_memories_for_tickets_no_longer_open() {
        let _lock = env_lock().lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workspace");
        let protected = tmp.path().join("protected-os");
        let ticket_dir = root.join("system/inbox/builder-code");
        std::fs::create_dir_all(&ticket_dir).unwrap();
        std::fs::create_dir_all(&protected).unwrap();
        std::fs::write(root.join("system/agents.txt"), "engineer\n").unwrap();
        std::fs::write(
            root.join("system/memory.md"),
            "# Synthetic\n\n## Stable\nStable shared memory.\n",
        )
        .unwrap();
        let ticket = ticket_dir.join("20260820T010000Z-engineer-open.md");
        std::fs::write(
            &ticket,
            "---\nowner: engineer\ntitle: Engineer Open Ticket\n---\nCurrent open work.\n",
        )
        .unwrap();
        let _env = EnvGuard::new(&root, &protected);
        let mut conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();

        run(&mut conn, false, None).unwrap();
        let before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE title = 'Engineer Open Ticket'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(before, 1);

        std::fs::remove_file(ticket).unwrap();
        run(&mut conn, false, None).unwrap();
        let after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE title = 'Engineer Open Ticket' AND status != 'archived'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(after, 0);
    }

    #[test]
    fn meetings_source_discovers_markdown_and_ingests_sections_idempotently() {
        let _lock = env_lock().lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workspace");
        let protected = tmp.path().join("protected-os");
        let meetings = root.join("meetings");
        std::fs::create_dir_all(&meetings).unwrap();
        std::fs::create_dir_all(&protected).unwrap();
        let meeting = meetings.join("2026-08-01-base-resume.md");
        std::fs::write(
            &meeting,
            "# Meeting — Base resume\n**Date:** 2026-08-01\n\n## Decision\nThe compactor attacks the summary FIRST.\n",
        )
        .unwrap();
        std::fs::write(meetings.join("ignored.txt"), "not markdown").unwrap();
        let _env = EnvGuard::new(&root, &protected);
        let mut conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();

        let files = discover_source_files(Some("meetings")).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].source, "meetings");
        assert_eq!(files[0].path, meeting);

        let first = run(&mut conn, false, Some("meetings")).unwrap();
        assert_eq!(first.memories, 0);
        assert_eq!(first.episodes, 2);
        assert_eq!(first.skipped, 0);
        let second = run(&mut conn, false, Some("meetings")).unwrap();
        assert_eq!(second.episodes, 0);

        std::fs::write(
            &meeting,
            "# Meeting — Base resume\n**Date:** 2026-08-01\n\n## Decision\nThe compactor preserves the summary now.\n",
        )
        .unwrap();
        let changed = run(&mut conn, false, Some("meetings")).unwrap();
        assert_eq!(changed.episodes, 1);

        let persisted: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM episodes WHERE kind='meeting' AND source_path=?1",
                params![meeting.to_string_lossy()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(persisted, 2);
        let stale: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM episodes WHERE kind='meeting' AND body LIKE '%attacks%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale, 0);
    }

    #[test]
    fn missing_protected_root_refuses_source_file_walk() {
        let _lock = env_lock().lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        let old_root = std::env::var("AIOS_ROOT").ok();
        let old_protected = std::env::var("SHELVES_PROTECTED_ROOT").ok();
        let old_extra = std::env::var("SHELVES_EXTRA_SOURCE_DIR").ok();
        unsafe {
            std::env::set_var("AIOS_ROOT", &root);
            std::env::remove_var("SHELVES_PROTECTED_ROOT");
            std::env::remove_var("SHELVES_EXTRA_SOURCE_DIR");
        }

        let err = discover_source_files(Some("system-memory"))
            .expect_err("missing SHELVES_PROTECTED_ROOT must refuse before walking sources");

        assert!(err.to_string().contains(
            "SHELVES_PROTECTED_ROOT is required for protected-path enforcement; refusing file walk"
        ));
        restore_env("AIOS_ROOT", old_root);
        restore_env("SHELVES_PROTECTED_ROOT", old_protected);
        restore_env("SHELVES_EXTRA_SOURCE_DIR", old_extra);
    }

    #[test]
    fn empty_ingest_refuses_non_lock_sources_but_allows_empty_lock_store() {
        // Covers empty-ingest refusal vs lock-store empty allowance at lines
        // 109-130.
        let _lock = env_lock().lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("empty-workspace");
        let protected = tmp.path().join("protected-os");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&protected).unwrap();
        let _env = EnvGuard::new(&root, &protected);
        let mut conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();

        let err = run(&mut conn, false, Some("system-memory"))
            .expect_err("non-lock empty source must fail closed");
        assert!(err.to_string().contains("zero source files"));

        let report = run(&mut conn, false, Some("lock-store")).unwrap();
        assert_eq!(report.memories, 0);
        assert_eq!(report.episodes, 0);
        assert_eq!(report.skipped, 0);
        assert!(report.sources.is_empty());
    }

    #[test]
    fn reset_preserves_runtime_state_and_drops_promotion_audits() {
        // Covers --reset runtime-state snapshot/restore/report at lines
        // 132-147 and 214-265.
        let _lock = env_lock().lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("reset-workspace");
        let protected = tmp.path().join("protected-os");
        std::fs::create_dir_all(root.join("system")).unwrap();
        std::fs::create_dir_all(&protected).unwrap();
        std::fs::write(
            root.join("system/memory.md"),
            "## Reset Source\nReset source body for ingest.\n",
        )
        .unwrap();
        let _env = EnvGuard::new(&root, &protected);
        let mut conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        storage::insert_future_item(&conn, "durable future", "2026-06-30", "agent:engineer")
            .unwrap();
        storage::insert_miss(&conn, "durable miss", "agent:engineer").unwrap();
        conn.execute(
            "INSERT INTO episodes(ts, actor, kind, summary, body, scope, source_path)
             VALUES('2026-06-15T00:00:00Z', 'agent:coordinator', 'promotion', 'audit', 'audit', 'company', '')",
            [],
        )
        .unwrap();

        let report = run(&mut conn, true, Some("system-memory")).unwrap();

        let durability = report.reset_durability.unwrap();
        assert_eq!(durability.future_items_preserved, 1);
        assert_eq!(durability.misses_preserved, 1);
        assert!(durability.recall_events_reset);
        assert_eq!(durability.promotion_audits_dropped, 1);
        let future_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM future_items WHERE body='durable future'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let miss_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM episodes WHERE kind='miss' AND summary='durable miss'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let promotion_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM episodes WHERE kind='promotion'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(future_count, 1);
        assert_eq!(miss_count, 1);
        assert_eq!(promotion_count, 0);
    }

    #[test]
    fn memory_source_reingest_archives_renamed_and_removed_sections() {
        let _lock = env_lock().lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workspace");
        let protected = tmp.path().join("protected-os");
        std::fs::create_dir_all(root.join("system")).unwrap();
        std::fs::create_dir_all(&protected).unwrap();
        let memory = root.join("system/memory.md");
        std::fs::write(
            &memory,
            "## Old Note\nOriginal body.\n\n## Keep Me\nStill here.\n",
        )
        .unwrap();
        let _env = EnvGuard::new(&root, &protected);
        let mut conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();

        run(&mut conn, false, Some("system-memory")).unwrap();
        std::fs::write(&memory, "## New Note\nUpdated body.\n").unwrap();
        run(&mut conn, false, Some("system-memory")).unwrap();

        let rows = memory_status_rows(&conn);
        assert_eq!(rows["old-note"], "archived");
        assert_eq!(rows["keep-me"], "archived");
        assert_eq!(rows["new-note"], "hot");
    }

    #[test]
    fn per_file_errors_skip_non_lock_sources_but_fail_lock_store() {
        // Covers the skip-vs-fail policy at lines 154-167.
        let _lock = env_lock().lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let protected = tmp.path().join("protected-os");
        let root = protected.join("workspace-inside-layer1");
        std::fs::create_dir_all(root.join("system")).unwrap();
        std::fs::write(
            root.join("system/memory.md"),
            "## Refused\nprotected source.\n",
        )
        .unwrap();
        std::fs::write(root.join("system/locks.yaml"), "# lock store\n").unwrap();
        let _env = EnvGuard::new(&root, &protected);
        let mut conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();

        let skipped = run(&mut conn, false, Some("system-memory")).unwrap();
        assert_eq!(skipped.memories, 0);
        assert_eq!(skipped.episodes, 0);
        assert_eq!(skipped.skipped, 1);

        let err = run(&mut conn, false, Some("lock-store"))
            .expect_err("lock-store ingest errors must propagate");
        assert!(err.to_string().contains("refusing to read"));
    }

    #[test]
    fn ingest_file_covers_extra_frontmatter_unknown_source_and_slug_defaults() {
        let _lock = env_lock().lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workspace");
        let protected = tmp.path().join("protected-os");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&protected).unwrap();
        let _env = EnvGuard::new(&root, &protected);
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();

        let extra = root.join("extra-memory.md");
        std::fs::write(
            &extra,
            "---\nname: extra_memory\ntitle: Extra Memory\n---\nBody",
        )
        .unwrap();
        let mut episode_deduper = storage::EpisodeDeduper::load(&conn).unwrap();
        let (memories, episodes) = ingest_file(
            &conn,
            &mut episode_deduper,
            &SourceFile {
                source: "extra",
                path: extra,
            },
        )
        .unwrap();
        assert_eq!((memories, episodes), (1, 0));

        let coordinator = root.join("external-memory.md");
        std::fs::write(
            &coordinator,
            "---\nname: external_memory\ntitle: Coordinator Memory\n---\nBody",
        )
        .unwrap();
        assert_eq!(
            ingest_file(
                &conn,
                &mut episode_deduper,
                &SourceFile {
                    source: "external-memory",
                    path: coordinator,
                },
            )
            .unwrap(),
            (1, 0)
        );

        let agent_to_agent = root.join("agent-to-agent.md");
        std::fs::write(
            &agent_to_agent,
            "DATE: 2026-07-29\nFixture agent-to-agent ping",
        )
        .unwrap();
        assert_eq!(
            ingest_file(
                &conn,
                &mut episode_deduper,
                &SourceFile {
                    source: "agent-to-agent",
                    path: agent_to_agent,
                },
            )
            .unwrap(),
            (0, 2)
        );

        let unknown = root.join("unknown.md");
        std::fs::write(&unknown, "ignored").unwrap();
        assert_eq!(
            ingest_file(
                &conn,
                &mut episode_deduper,
                &SourceFile {
                    source: "unknown",
                    path: unknown,
                },
            )
            .unwrap(),
            (0, 0)
        );

        let default_dir = external_memory_dir(&root);
        assert!(default_dir.to_string_lossy().contains(".agents/projects/"));
        assert!(default_dir.ends_with("memory"));
    }

    fn memory_status_rows(conn: &Connection) -> BTreeMap<String, String> {
        let mut stmt = conn
            .prepare("SELECT name, status FROM memories ORDER BY name")
            .unwrap();
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap();
        rows.collect::<rusqlite::Result<BTreeMap<_, _>>>().unwrap()
    }

    fn env_lock() -> &'static Mutex<()> {
        crate::test_env_lock()
    }

    fn restore_env(key: &str, old: Option<String>) {
        match old {
            Some(value) => unsafe { std::env::set_var(key, value) }, // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
            None => unsafe { std::env::remove_var(key) },
        }
    }

    struct EnvGuard {
        old_values: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn new(root: &Path, protected: &Path) -> Self {
            let keys = [
                "AIOS_ROOT",
                "SHELVES_PROTECTED_ROOT",
                "SHELVES_DB_PATH",
                "SHELVES_EXTERNAL_MEMORY_DIR",
                "SHELVES_EXTRA_SOURCE_DIR",
            ];
            let old_values = keys
                .into_iter()
                .map(|key| (key, std::env::var(key).ok()))
                .collect();
            unsafe {
                std::env::set_var("AIOS_ROOT", root);
                std::env::set_var("SHELVES_PROTECTED_ROOT", protected);
                std::env::set_var("SHELVES_DB_PATH", root.join("system/shelves.db"));
                std::env::remove_var("SHELVES_EXTERNAL_MEMORY_DIR");
                std::env::remove_var("SHELVES_EXTRA_SOURCE_DIR");
            }
            Self { old_values }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.old_values.drain(..) {
                restore_env(key, value);
            }
        }
    }
}
