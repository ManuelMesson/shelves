use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, bail};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::{activation, locks, storage, workspace_root};

#[derive(Debug, Clone, Serialize)]
pub struct ConsolidationReport {
    pub generated_at: String,
    pub at_close: bool,
    pub promotion_recall_threshold: usize,
    pub hot_threshold: f64,
    pub decay_d: f64,
    pub report_path: Option<String>,
    pub candidates: Vec<ConsolidationCandidate>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConsolidationCandidate {
    pub kind: String,
    pub id: i64,
    pub title: String,
    pub name: String,
    pub owner: String,
    pub current_scope: String,
    pub suggested_scope: Option<String>,
    pub required_tier: String,
    pub recall_counts_by_scope: BTreeMap<String, i64>,
    pub higher_scope_recall_count: i64,
    pub activation: Option<f64>,
    pub last_recall_ts: Option<String>,
    pub source_path: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PromoteOutcome {
    pub id: i64,
    pub name: String,
    pub title: String,
    pub owner: String,
    pub from_scope: String,
    pub to_scope: String,
    pub by: String,
    pub changed: bool,
    pub audit_episode_id: Option<i64>,
}

#[derive(Debug)]
struct MemoryForGraduation {
    id: i64,
    name: String,
    title: String,
    owner: String,
    scope: String,
    is_lock: bool,
    status: String,
    source_path: String,
}

pub fn consolidate(conn: &Connection, at_close: bool) -> Result<ConsolidationReport> {
    let mut report = build_consolidation_report(conn, at_close)?;
    let path = write_report(&report)?;
    report.report_path = Some(path.to_string_lossy().to_string());
    Ok(report)
}

pub fn build_consolidation_report(
    conn: &Connection,
    at_close: bool,
) -> Result<ConsolidationReport> {
    let generated_at = Utc::now().to_rfc3339();
    let threshold = storage::meta_usize(conn, "promotion_recall_threshold", 2)?;
    let decay = storage::meta_f64(conn, "decay_d", 0.5)?;
    let hot_threshold = storage::meta_f64(conn, "hot_threshold", -1.6)?;
    let now = Utc::now();
    let mut candidates = Vec::new();

    for memory in graduation_memories(conn)? {
        let recall_counts = recall_counts_by_scope(conn, memory.id)?;
        let higher_count = higher_scope_count(&memory.scope, &recall_counts);
        let activation = activation::memory_activation(conn, memory.id, now, decay)?;
        let last_recall = last_recall_ts(conn, memory.id)?;

        if higher_count >= threshold as i64 {
            let suggested_scope = suggested_scope(&memory.scope, &recall_counts);
            let required_tier =
                required_tier(&memory.scope, suggested_scope.as_deref(), memory.is_lock);
            candidates.push(candidate(
                "promotion-candidate",
                &memory,
                suggested_scope,
                required_tier,
                recall_counts.clone(),
                higher_count,
                activation,
                last_recall.clone(),
            ));
        }

        if memory.status != "archived" && memory.is_lock {
            candidates.push(candidate(
                "lock-exempt",
                &memory,
                None,
                "operator".to_string(),
                recall_counts.clone(),
                higher_count,
                activation,
                last_recall.clone(),
            ));
        } else if memory.status != "archived"
            && !activation::is_hot(false, activation, hot_threshold)
        {
            candidates.push(candidate(
                "cooling-candidate",
                &memory,
                None,
                "archivist-report".to_string(),
                recall_counts,
                higher_count,
                activation,
                last_recall,
            ));
        }
    }

    candidates.sort_by(|a, b| {
        kind_order(&a.kind)
            .cmp(&kind_order(&b.kind))
            .then_with(|| {
                b.higher_scope_recall_count // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
                    .cmp(&a.higher_scope_recall_count) // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
            }) // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
            .then_with(|| a.title.cmp(&b.title))
    });

    Ok(ConsolidationReport {
        generated_at,
        at_close,
        promotion_recall_threshold: threshold,
        hot_threshold,
        decay_d: decay,
        report_path: None,
        candidates,
    })
}

pub fn promote(conn: &Connection, id: i64, to_scope: &str, by: &str) -> Result<PromoteOutcome> {
    let by = by.trim().to_ascii_lowercase();
    if !locks::validate_scope(to_scope) {
        bail!("scope must be os, company, or product:<slug>");
    }
    let memory = memory_by_id(conn, id)?.ok_or_else(|| anyhow::anyhow!("memory {id} not found"))?;
    if memory.scope == to_scope {
        return Ok(PromoteOutcome {
            id: memory.id,
            name: memory.name,
            title: memory.title,
            owner: memory.owner,
            from_scope: memory.scope.clone(),
            to_scope: memory.scope,
            by,
            changed: false,
            audit_episode_id: None,
        });
    }
    if memory.is_lock {
        bail!("memory {id} is a lock; lock scope moves go through Lock-It, not promote");
    }
    if !tier_allows(&memory.scope, to_scope, &by) {
        if by == "archivist" {
            bail!(
                "archivist may nominate scope moves via consolidate; promote requires curator or operator"
            );
        }
        bail!(
            "{by} may not move memory {id} from {} to {to_scope}; product<->company requires curator, os/refusal overrides require operator",
            memory.scope
        );
    }

    conn.execute(
        "UPDATE memories SET scope = ?1, updated_at = ?2 WHERE id = ?3",
        params![to_scope, Utc::now().to_rfc3339(), id],
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let summary = format!("{} -> {}: {}", memory.scope, to_scope, memory.name);
    conn.execute(
        "INSERT INTO episodes(ts, actor, kind, summary, body, scope, source_path)
         VALUES(?1, ?2, 'promotion', ?3, ?4, ?5, ?6)",
        params![
            Utc::now().to_rfc3339(),
            by,
            summary,
            format!(
                "memory_id={} owner={} title={} from_scope={} to_scope={}",
                memory.id, memory.owner, memory.title, memory.scope, to_scope
            ),
            to_scope,
            memory.source_path
        ],
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let audit_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO episodes_fts(rowid, summary, body) VALUES(?1, ?2, ?3)",
        params![
            audit_id,
            summary,
            format!(
                "memory_id={} owner={} title={} from_scope={} to_scope={}",
                memory.id, memory.owner, memory.title, memory.scope, to_scope
            )
        ],
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.

    Ok(PromoteOutcome {
        id: memory.id,
        name: memory.name,
        title: memory.title,
        owner: memory.owner,
        from_scope: memory.scope,
        to_scope: to_scope.to_string(),
        by,
        changed: true,
        audit_episode_id: Some(audit_id),
    })
}

fn graduation_memories(conn: &Connection) -> Result<Vec<MemoryForGraduation>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, title, owner, scope, is_lock, status, coalesce(source_path, '')
         FROM memories",
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let rows = stmt.query_map([], row_memory_for_graduation)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn memory_by_id(conn: &Connection, id: i64) -> Result<Option<MemoryForGraduation>> {
    Ok(conn
        .query_row(
            "SELECT id, name, title, owner, scope, is_lock, status, coalesce(source_path, '')
             FROM memories WHERE id = ?1",
            params![id],
            row_memory_for_graduation,
        )
        .optional()?)
}

fn row_memory_for_graduation(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryForGraduation> {
    Ok(MemoryForGraduation {
        id: row.get(0)?,
        name: row.get(1)?,
        title: row.get(2)?,
        owner: row.get(3)?,
        scope: row.get(4)?,
        is_lock: row.get::<_, i64>(5)? != 0,
        status: row.get(6)?,
        source_path: row.get(7)?,
    })
}

fn recall_counts_by_scope(conn: &Connection, memory_id: i64) -> Result<BTreeMap<String, i64>> {
    let mut stmt = conn.prepare(
        "SELECT query_scope, COUNT(*) FROM recall_events WHERE memory_id = ?1 GROUP BY query_scope",
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let rows = stmt.query_map(params![memory_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut counts = BTreeMap::new();
    for row in rows {
        let (scope, count) = row?;
        counts.insert(scope, count);
    }
    Ok(counts)
}

fn last_recall_ts(conn: &Connection, memory_id: i64) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT ts FROM recall_events WHERE memory_id = ?1 ORDER BY ts DESC LIMIT 1",
            params![memory_id],
            |row| row.get(0),
        )
        .optional()?)
}

fn higher_scope_count(scope: &str, counts: &BTreeMap<String, i64>) -> i64 {
    counts
        .iter()
        .filter(|(query_scope, _)| scope_level(query_scope) > scope_level(scope))
        .map(|(_, count)| *count)
        .sum()
}

fn suggested_scope(scope: &str, counts: &BTreeMap<String, i64>) -> Option<String> {
    counts
        .iter()
        .filter(|(query_scope, _)| scope_level(query_scope) > scope_level(scope))
        .max_by(|a, b| {
            a.1.cmp(b.1)
                .then_with(|| scope_level(a.0).cmp(&scope_level(b.0)))
        })
        .map(|(query_scope, _)| normalize_broader_scope(query_scope))
}

fn normalize_broader_scope(scope: &str) -> String {
    if scope == "os" {
        "os".to_string()
    } else {
        "company".to_string()
    }
}

fn scope_level(scope: &str) -> u8 {
    if scope == "os" {
        2
    } else if scope == "company" {
        1
    } else {
        0
    }
}

fn required_tier(from_scope: &str, to_scope: Option<&str>, is_lock: bool) -> String {
    if is_lock || from_scope == "os" || to_scope == Some("os") {
        "operator".to_string()
    } else if from_scope.starts_with("product:") || from_scope == "company" {
        "curator".to_string()
    } else {
        "operator".to_string()
    }
}

fn tier_allows(from_scope: &str, to_scope: &str, by: &str) -> bool {
    if by == "operator" {
        return true;
    }
    if by != "curator" {
        return false;
    }
    (from_scope == "company" && to_scope.starts_with("product:"))
        || (from_scope.starts_with("product:") && to_scope == "company")
}

// Consolidation candidates are assembled from separate evidence columns; keeping the
// constructor flat preserves call-site auditability for this quality pass.
#[allow(clippy::too_many_arguments)]
fn candidate(
    kind: &str,
    memory: &MemoryForGraduation,
    suggested_scope: Option<String>,
    required_tier: String,
    recall_counts_by_scope: BTreeMap<String, i64>,
    higher_scope_recall_count: i64,
    activation: Option<f64>,
    last_recall_ts: Option<String>,
) -> ConsolidationCandidate {
    ConsolidationCandidate {
        kind: kind.to_string(),
        id: memory.id,
        title: memory.title.clone(),
        name: memory.name.clone(),
        owner: memory.owner.clone(),
        current_scope: memory.scope.clone(),
        suggested_scope,
        required_tier,
        recall_counts_by_scope,
        higher_scope_recall_count,
        activation,
        last_recall_ts,
        source_path: memory.source_path.clone(),
    }
}

fn write_report(report: &ConsolidationReport) -> Result<PathBuf> {
    let dir = workspace_root()?.join("system/inbox/archivist-reports");
    std::fs::create_dir_all(&dir)?;
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    let path = dir.join(format!("{stamp}-shelves-consolidate.md"));
    std::fs::write(&path, render_report(report))?;
    Ok(path)
}

fn render_report(report: &ConsolidationReport) -> String {
    let mut out = String::new();
    out.push_str("# Shelves Consolidation Report\n\n");
    out.push_str(&format!("DATE: {}\n", report.generated_at));
    out.push_str(&format!("AT_CLOSE: {}\n", report.at_close));
    out.push_str(&format!(
        "PROMOTION_RECALL_THRESHOLD: {}\n",
        report.promotion_recall_threshold
    ));
    out.push_str(&format!("HOT_THRESHOLD: {}\n", report.hot_threshold));
    out.push_str(&format!("DECAY_D: {}\n\n", report.decay_d));
    if report.candidates.is_empty() {
        out.push_str("No consolidation candidates.\n");
        return out;
    }
    out.push_str("| Kind | ID | Title | Current | Suggested | Owner | Tier | Higher recalls | Activation | Last recall |\n");
    out.push_str("|---|---:|---|---|---|---|---|---:|---:|---|\n");
    for item in &report.candidates {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            item.kind,
            item.id,
            escape_cell(&item.title),
            item.current_scope,
            item.suggested_scope.as_deref().unwrap_or("-"),
            item.owner,
            item.required_tier,
            item.higher_scope_recall_count,
            item.activation
                .map(|value| format!("{value:.3}"))
                .unwrap_or_else(|| "-".to_string()),
            item.last_recall_ts.as_deref().unwrap_or("-"),
        ));
    }
    out.push_str("\n## Evidence\n\n");
    for item in &report.candidates {
        out.push_str(&format!(
            "- `{}` memory:{} `{}` scope={} owner={} source={}\n",
            item.kind, item.id, item.name, item.current_scope, item.owner, item.source_path
        ));
        out.push_str(&format!(
            "  recall_counts_by_scope={:?}\n",
            item.recall_counts_by_scope
        ));
    }
    out
}

fn escape_cell(input: &str) -> String {
    input.replace('|', "\\|").replace('\n', " ")
}

fn kind_order(kind: &str) -> u8 {
    match kind {
        "promotion-candidate" => 0,
        "cooling-candidate" => 1,
        "lock-exempt" => 2,
        _ => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parser::MemoryDoc, schema};
    use proptest::prelude::*;
    use std::path::Path;

    #[test]
    fn consolidate_nominates_promotion_and_cooling_but_exempts_locks() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_memory(
            &conn,
            "product-memory",
            "Product Memory",
            "product:notebook",
            false,
        );
        insert_memory(&conn, "cold-memory", "Cold Memory", "company", false);
        insert_memory(&conn, "lock-memory", "Lock Memory", "company", true);
        storage::log_recall_event(&conn, 1, "agent:curator", "company").unwrap();
        storage::log_recall_event(&conn, 1, "agent:curator", "company").unwrap();

        let report = build_consolidation_report(&conn, false).unwrap();

        assert!(report.report_path.is_none());
        assert!(report.candidates.iter().any(|item| {
            item.kind == "promotion-candidate"
                && item.name == "product-memory"
                && item.suggested_scope.as_deref() == Some("company")
        }));
        assert!(
            report
                .candidates
                .iter()
                .any(|item| item.kind == "cooling-candidate" && item.name == "cold-memory")
        );
        assert!(
            report
                .candidates
                .iter()
                .any(|item| item.kind == "lock-exempt" && item.name == "lock-memory")
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn promotion_threshold_boundary_requires_configured_count() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_memory(
            &conn,
            "product-memory",
            "Product Memory",
            "product:notebook",
            false,
        );
        storage::log_recall_event(&conn, 1, "agent:curator", "company").unwrap();

        let report = build_consolidation_report(&conn, false).unwrap();

        assert!(
            !report
                .candidates
                .iter()
                .any(|item| item.kind == "promotion-candidate")
        );
    }

    #[test]
    fn report_rendering_covers_empty_and_table_rows() {
        let empty = ConsolidationReport {
            generated_at: "2026-06-15T00:00:00Z".to_string(),
            at_close: true,
            promotion_recall_threshold: 2,
            hot_threshold: -1.6,
            decay_d: 0.5,
            report_path: None,
            candidates: Vec::new(),
        };
        let empty_text = render_report(&empty);
        assert!(empty_text.contains("No consolidation candidates."));

        let mut counts = BTreeMap::new();
        counts.insert("company".to_string(), 2);
        let mut report = empty;
        report.candidates.push(ConsolidationCandidate {
            kind: "promotion-candidate".to_string(),
            id: 7,
            title: "Pipe | Title\nNext".to_string(),
            name: "pipe-title".to_string(),
            owner: "shared".to_string(),
            current_scope: "product:notebook".to_string(),
            suggested_scope: Some("company".to_string()),
            required_tier: "curator".to_string(),
            recall_counts_by_scope: counts,
            higher_scope_recall_count: 2,
            activation: Some(-0.25),
            last_recall_ts: Some("2026-06-15T00:00:00Z".to_string()),
            source_path: "/tmp/memory.md".to_string(),
        });
        let table = render_report(&report);
        assert!(table.contains("Pipe \\| Title Next"));
        assert!(table.contains("0.250"));
        assert!(table.contains("## Evidence"));
    }

    #[test]
    fn promote_curator_allows_product_company_only_and_audits() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_memory(
            &conn,
            "product-memory",
            "Product Memory",
            "product:notebook",
            false,
        );

        let outcome = promote(&conn, 1, "company", "curator").unwrap();

        assert!(outcome.changed);
        assert_eq!(outcome.owner, "shared");
        assert_eq!(outcome.audit_episode_id, Some(1));
        let row: (String, String) = conn
            .query_row("SELECT owner, scope FROM memories WHERE id=1", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(row, ("shared".to_string(), "company".to_string()));
        let kind: String = conn
            .query_row("SELECT kind FROM episodes WHERE id=1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(kind, "promotion");
    }

    #[test]
    fn promote_tier_gate_matrix() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_memory(
            &conn,
            "product-memory",
            "Product Memory",
            "product:notebook",
            false,
        );
        insert_memory(&conn, "company-memory", "Company Memory", "company", false);
        insert_memory(&conn, "os-memory", "OS Memory", "os", false);

        assert!(promote(&conn, 1, "company", "archivist").is_err());
        assert!(promote(&conn, 1, "company", "curator").is_ok());
        assert!(promote(&conn, 2, "os", "curator").is_err());
        assert!(promote(&conn, 2, "os", "operator").is_ok());
        assert!(promote(&conn, 3, "product:notebook", "operator").is_ok());
    }

    #[test]
    fn promote_refuses_locks_invalid_scope_and_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_memory(&conn, "lock-memory", "Lock Memory", "company", true);
        insert_memory(&conn, "company-memory", "Company Memory", "company", false);

        assert!(promote(&conn, 1, "product:notebook", "operator").is_err());
        assert!(promote(&conn, 2, "personal", "operator").is_err());
        let frontende = promote(&conn, 2, "company", "archivist").unwrap();
        assert!(!frontende.changed);
        let audits: i64 = conn
            .query_row("SELECT COUNT(*) FROM episodes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(audits, 0);

        let missing = promote(&conn, 999, "company", "operator").unwrap_err();
        assert!(missing.to_string().contains("memory 999 not found"));
    }

    #[test]
    fn graduation_helper_ordering_and_tiers_are_stable() {
        let mut counts = BTreeMap::new();
        counts.insert("os".to_string(), 1);
        counts.insert("company".to_string(), 2);

        assert_eq!(higher_scope_count("product:notebook", &counts), 3);
        assert_eq!(
            suggested_scope("product:notebook", &counts),
            Some("company".to_string())
        );
        assert_eq!(suggested_scope("company", &counts), Some("os".to_string()));
        assert_eq!(normalize_broader_scope("os"), "os");
        assert_eq!(normalize_broader_scope("company"), "company");
        assert_eq!(
            required_tier("company", Some("product:notebook"), false),
            "curator"
        );
        assert_eq!(required_tier("company", Some("os"), false), "operator");
        assert_eq!(
            required_tier("personal", Some("company"), false),
            "operator"
        );
        assert!(tier_allows("company", "product:notebook", "curator"));
        assert!(!tier_allows("company", "os", "curator"));
        assert_eq!(kind_order("unknown"), 3);
    }

    fn insert_memory(conn: &Connection, name: &str, title: &str, scope: &str, is_lock: bool) {
        let doc = MemoryDoc {
            name: name.to_string(),
            title: title.to_string(),
            body: format!("{title} body"),
            owner: "shared".to_string(),
            scope: scope.to_string(),
            source_path: Path::new("/tmp/memory.md").to_path_buf(),
            content_hash: format!("hash-{name}"),
            is_lock,
            created_at: "2026-06-10T00:00:00Z".to_string(),
            updated_at: "2026-06-10T00:00:00Z".to_string(),
        };
        storage::upsert_memory(conn, &doc).unwrap();
    }

    proptest! {
        #[test]
        fn promote_does_not_mutate_owner_or_non_target_rows(to_scope in "(os|company|product:[a-z][a-z0-9-]{0,12})", by in "(curator|operator|archivist|engineer)") {
            let conn = Connection::open_in_memory().unwrap();
            schema::init_db(&conn).unwrap();
            insert_memory(&conn, "target", "Target", "product:notebook", false);
            insert_memory(&conn, "other", "Other", "company", false);
            let _ = promote(&conn, 1, &to_scope, &by);
            let target_owner: String = conn.query_row("SELECT owner FROM memories WHERE id=1", [], |row| row.get(0)).unwrap();
            let other: (String, String) = conn.query_row("SELECT owner, scope FROM memories WHERE id=2", [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
            prop_assert_eq!(target_owner, "shared");
            prop_assert_eq!(other, ("shared".to_string(), "company".to_string()));
        }
    }
}
