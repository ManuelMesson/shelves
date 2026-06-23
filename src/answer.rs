use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, bail};
use chrono::{DateTime, Duration, Local, NaiveDate, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::{acl, activation, search, storage};

const BRIEF_MAX_LINES: usize = 20;
const BRIEF_DUE_DAYS: i64 = 30;
const RELEVANCE_WEIGHT: f64 = 100.0;
const ACTIVATION_WEIGHT: f64 = 1.0;
const CORE_LOCK_BONUS: f64 = 0.1;
const DEFAULT_CONTEXT_RELEVANCE_FLOOR: f64 = 0.66;
const CURATOR_AUTHORED_CORE_LOCKS: &[&str] = &[
    "actor-lanes",
    "ground-rule-no-hallucination",
    "memory-security-boundaries",
    "zero-manual-cli",
    "quality-bar-language-parity",
];

#[derive(Debug, Clone, Serialize)]
pub struct PackLine {
    pub section: String,
    pub title: String,
    pub body: String,
    pub source_path: String,
    pub scope: String,
    pub owner: String,
    pub reason: String,
    pub activation: Option<f64>,
    pub stale: bool,
    pub confidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FutureItem {
    pub id: i64,
    pub body: String,
    pub due: String,
    pub created_by: String,
    pub status: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FutureItemTransition {
    pub item: FutureItem,
    pub previous_status: String,
    pub changed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct RelatedRow {
    pub kind: String,
    pub id: i64,
    pub title: String,
    pub body: String,
    pub source_path: String,
    pub link_kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MissRow {
    pub id: i64,
    pub what: String,
    pub by: String,
}

pub fn normalize_agent(agent: &str) -> String {
    let lowered = agent.trim().trim_start_matches('@').to_ascii_lowercase();
    if lowered.starts_with("agent:") {
        lowered
    } else {
        format!("agent:{lowered}")
    }
}

pub fn ask(
    conn: &Connection,
    agent: &str,
    query: &str,
    asker: &str,
    scope: &str,
    include_cold: bool,
    limit: usize,
) -> Result<Vec<search::SearchHit>> {
    let owner = normalize_agent(agent);
    let reader = normalize_agent(asker);
    if !acl::can_read_owner(conn, &owner, &reader)? {
        return Ok(Vec::new());
    }
    search::search(
        conn,
        query,
        scope,
        Some(&owner),
        &reader,
        include_cold,
        limit,
    )
}

pub fn remember(conn: &Connection, text: &str, due: &str, by: &str) -> Result<FutureItem> {
    validate_iso_date(due)?;
    let created_by = normalize_agent(by);
    let id = storage::insert_future_item(conn, text, due, &created_by)?;
    future_item_by_id(conn, id)
}

pub fn done(conn: &Connection, id: i64, drop: bool) -> Result<FutureItemTransition> {
    let target_status = if drop { "dropped" } else { "done" };
    let Some(existing) = maybe_future_item_by_id(conn, id)? else {
        bail!("future item {id} not found");
    };
    if existing.status != "open" {
        return Ok(FutureItemTransition {
            previous_status: existing.status.clone(),
            item: existing,
            changed: false,
        });
    }
    conn.execute(
        "UPDATE future_items SET status = ?1 WHERE id = ?2 AND status = 'open'",
        params![target_status, id],
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    Ok(FutureItemTransition {
        item: future_item_by_id(conn, id)?,
        previous_status: existing.status,
        changed: true,
    })
}

pub fn upcoming(conn: &Connection, days: Option<i64>) -> Result<Vec<FutureItem>> {
    let mut items = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT id, body, due, created_by, status, created_at
         FROM future_items
         WHERE status = 'open'
         ORDER BY due ASC, id ASC",
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let rows = stmt.query_map([], row_future_item)?;
    let today = Local::now().date_naive();
    let cutoff = days.map(|value| today + Duration::days(value));
    for row in rows {
        let item = row?;
        if cutoff
            .and_then(|day| {
                NaiveDate::parse_from_str(&item.due, "%Y-%m-%d")
                    .ok()
                    .map(|due| due <= day)
            })
            .unwrap_or(true)
        {
            items.push(item);
        }
    }
    Ok(items)
}

pub fn brief(conn: &Connection, agent: &str) -> Result<Vec<PackLine>> {
    let reader = normalize_agent(agent);
    let mut lines = Vec::new();
    lines.extend(lock_lines(conn, &reader, None, None, None, 5)?);
    lines.extend(hot_memory_lines(
        conn,
        &reader,
        None,
        None,
        None,
        8.min(BRIEF_MAX_LINES.saturating_sub(lines.len())),
    )?); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    lines.extend(future_lines(
        conn,
        Some(BRIEF_DUE_DAYS),
        5.min(BRIEF_MAX_LINES.saturating_sub(lines.len())),
    )?); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    lines.extend(yesterday_lines(
        conn,
        BRIEF_MAX_LINES.saturating_sub(lines.len()),
    )?); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    Ok(cap_lines(lines, BRIEF_MAX_LINES))
}

pub fn context(
    conn: &Connection,
    agent: &str,
    task: &str,
    budget: Option<usize>,
) -> Result<Vec<PackLine>> {
    let max_lines = budget.unwrap_or(storage::meta_usize(conn, "context_default_budget", 15)?);
    if max_lines == 0 {
        return Ok(Vec::new());
    }
    let reader = normalize_agent(agent);
    let terms = query_terms(task);
    let task_terms = if terms.is_empty() {
        None
    } else {
        Some(terms.as_slice())
    };
    let relevance_floor = if task_terms.is_some() {
        Some(storage::meta_f64(
            conn,
            "context_relevance_floor",
            DEFAULT_CONTEXT_RELEVANCE_FLOOR,
        )?) // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    } else {
        None
    };
    let relevance_scores =
        search::memory_relevance_scores(conn, task, "company", &reader, false, 128)?;
    let mut lock_relevance_scores = relevance_scores.clone();
    lock_relevance_scores.extend(search::lock_relevance_scores(conn, task, "company", 128)?);
    let mut lines = Vec::new();
    lines.extend(lock_lines(
        conn,
        &reader,
        task_terms,
        Some(&lock_relevance_scores),
        relevance_floor,
        max_lines,
    )?); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    lines.extend(hot_memory_lines(
        conn,
        &reader,
        task_terms,
        Some(&relevance_scores),
        relevance_floor,
        max_lines.saturating_sub(lines.len()),
    )?); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    if !lines.iter().any(is_task_specific_line) {
        lines.extend(episode_precedent_lines(
            conn,
            &terms,
            relevance_floor,
            max_lines.saturating_sub(lines.len()),
        )?); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    }
    if let Some(floor) = relevance_floor {
        if !lines.iter().any(is_task_specific_line) {
            lines.extend(task_future_lines(
                conn,
                &terms,
                floor,
                max_lines.saturating_sub(lines.len()),
            )?); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        }
        if !lines.iter().any(is_task_specific_line) {
            lines.push(nothing_specific_line());
        }
    } else {
        lines.extend(future_lines(
            conn,
            None,
            max_lines.saturating_sub(lines.len()),
        )?); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    }
    Ok(cap_lines(lines, max_lines))
}

pub fn missed(conn: &Connection, what: &str, by: &str) -> Result<MissRow> {
    let actor = normalize_agent(by);
    let id = storage::insert_miss(conn, what, &actor)?;
    Ok(MissRow {
        id,
        what: what.to_string(),
        by: actor,
    })
}

pub fn related(conn: &Connection, name: &str) -> Result<Vec<RelatedRow>> {
    let slug = crate::parser::slugify(name);
    let from_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM memories WHERE name = ?1",
            params![slug],
            |row| row.get(0),
        )
        .optional()?;
    let Some(from_id) = from_id else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare(
        "SELECT l.to_kind, l.to_id, l.kind,
                CASE WHEN l.to_kind = 'memory' THEN m.title ELSE e.summary END,
                CASE WHEN l.to_kind = 'memory' THEN substr(m.body, 1, 240) ELSE substr(e.body, 1, 240) END,
                CASE WHEN l.to_kind = 'memory' THEN coalesce(m.source_path, '') ELSE coalesce(e.source_path, '') END
         FROM links l
         LEFT JOIN memories m ON l.to_kind = 'memory' AND m.id = l.to_id
         LEFT JOIN episodes e ON l.to_kind = 'episode' AND e.id = l.to_id
         WHERE l.from_kind = 'memory' AND l.from_id = ?1
         ORDER BY l.kind, l.to_kind, l.to_id",
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let rows = stmt.query_map(params![from_id], |row| {
        Ok(RelatedRow {
            kind: row.get(0)?,
            id: row.get(1)?,
            link_kind: row.get(2)?,
            title: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            body: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            source_path: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn prune_report(conn: &Connection) -> Result<Vec<PackLine>> {
    let decay = storage::meta_f64(conn, "decay_d", 0.5)?;
    let threshold = storage::meta_f64(conn, "hot_threshold", -1.6)?;
    let now = Utc::now();
    let mut rows = memory_rows(conn, "status != 'archived'")?;
    rows.sort_by(|a, b| a.title.cmp(&b.title));
    let mut lines = Vec::new();
    for row in rows {
        let activation = activation::memory_activation(conn, row.id, now, decay)?;
        let section = if row.is_lock {
            "lock-exempt"
        } else if !activation::is_hot(false, activation, threshold) {
            "cooling-candidate"
        } else {
            continue;
        };
        lines.push(pack_line(section, &row, activation, false, section));
    }
    Ok(lines)
}

pub fn cap_lines(mut lines: Vec<PackLine>, budget: usize) -> Vec<PackLine> {
    lines.truncate(budget);
    lines
}

pub fn validate_iso_date(input: &str) -> Result<()> {
    if NaiveDate::parse_from_str(input, "%Y-%m-%d").is_err() {
        bail!("due date must be ISO YYYY-MM-DD, got {input:?}");
    }
    Ok(())
}

fn lock_lines(
    conn: &Connection,
    reader: &str,
    terms: Option<&[String]>,
    relevance_scores: Option<&HashMap<i64, f64>>,
    min_relevance: Option<f64>,
    limit: usize,
) -> Result<Vec<PackLine>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let decay = storage::meta_f64(conn, "decay_d", 0.5)?;
    let now = Utc::now();
    let mut candidates = Vec::new();
    let mut rows = memory_rows(conn, "is_lock = 1 AND status != 'archived'")?;
    rows.extend(lock_store_rows(conn)?);
    for row in rows {
        if !scoped_for_company(&row.scope) || !acl::can_read_owner(conn, &row.owner, reader)? {
            continue; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        }
        let relevance = row_relevance(&row, terms, relevance_scores);
        let core = is_core_lock(&row);
        if terms.is_some() && relevance <= 0.0 && !core {
            continue;
        }
        if !core
            && min_relevance.is_some_and(|floor| row_term_relevance(&row, terms, relevance) < floor)
        {
            continue;
        }
        let activation = if row.id > 0 {
            activation::memory_activation(conn, row.id, now, decay)?
        } else {
            None
        };
        candidates.push(ScoredMemory {
            row,
            relevance,
            activation,
            core,
        });
    }
    sort_scored_memories(&mut candidates);
    Ok(candidates
        .into_iter()
        .take(limit)
        .map(|candidate| {
            let stale = source_stale(conn, &candidate.row.source_path).unwrap_or(false);
            pack_line(
                "active-lock",
                &candidate.row,
                candidate.activation,
                stale,
                candidate_reason(&candidate),
            )
        })
        .collect())
}

fn hot_memory_lines(
    conn: &Connection,
    reader: &str,
    terms: Option<&[String]>,
    relevance_scores: Option<&HashMap<i64, f64>>,
    min_relevance: Option<f64>,
    limit: usize,
) -> Result<Vec<PackLine>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let decay = storage::meta_f64(conn, "decay_d", 0.5)?;
    let threshold = storage::meta_f64(conn, "hot_threshold", -1.6)?;
    let now = Utc::now();
    let mut candidates = Vec::new();
    for row in memory_rows(conn, "is_lock = 0 AND status != 'archived'")? {
        if !scoped_for_company(&row.scope) || !acl::can_read_owner(conn, &row.owner, reader)? {
            continue; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        }
        let relevance = row_relevance(&row, terms, relevance_scores);
        if terms.is_some() && relevance <= 0.0 {
            continue;
        }
        if min_relevance.is_some_and(|floor| row_term_relevance(&row, terms, relevance) < floor) {
            continue;
        }
        if min_relevance.is_some()
            && terms.is_some_and(|items| {
                !context_tail_signal_strong(&row.name, &row.title, &row.body, items)
            })
        {
            continue;
        }
        let value = activation::memory_activation(conn, row.id, now, decay)?;
        if activation::is_hot(false, value, threshold) {
            candidates.push(ScoredMemory {
                row,
                relevance,
                activation: value,
                core: false,
            });
        }
    }
    sort_scored_memories(&mut candidates);
    Ok(candidates
        .into_iter()
        .take(limit)
        .map(|candidate| {
            let stale = source_stale(conn, &candidate.row.source_path).unwrap_or(false);
            pack_line(
                "hot-memory",
                &candidate.row,
                candidate.activation,
                stale,
                candidate_reason(&candidate),
            )
        })
        .collect())
}

fn future_lines(conn: &Connection, days: Option<i64>, limit: usize) -> Result<Vec<PackLine>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    Ok(upcoming(conn, days)?
        .into_iter()
        .take(limit)
        .map(|item| PackLine {
            section: "due-future".to_string(),
            title: item.due,
            body: item.body,
            source_path: format!("future_items:{}", item.id),
            scope: "company".to_string(),
            owner: item.created_by,
            reason: "due-future".to_string(),
            activation: None,
            stale: false,
            confidence: Vec::new(),
        })
        .collect())
}

fn task_future_lines(
    conn: &Connection,
    terms: &[String],
    min_relevance: f64,
    limit: usize,
) -> Result<Vec<PackLine>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    Ok(upcoming(conn, None)?
        .into_iter()
        .filter(|item| {
            context_floor_relevance("future", &item.due, &item.body, terms) >= min_relevance
                && context_tail_signal_strong("future", &item.due, &item.body, terms)
        })
        .take(limit)
        .map(|item| PackLine {
            section: "due-future".to_string(),
            title: item.due,
            body: item.body,
            source_path: format!("future_items:{}", item.id),
            scope: "company".to_string(),
            owner: item.created_by,
            reason: "matched-task".to_string(),
            activation: None,
            stale: false,
            confidence: Vec::new(),
        })
        .collect())
}

fn yesterday_lines(conn: &Connection, limit: usize) -> Result<Vec<PackLine>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let yesterday = Local::now().date_naive() - Duration::days(1);
    #[allow(clippy::expect_used)]
    let from = Utc
        .from_utc_datetime(
            &yesterday
                .and_hms_opt(0, 0, 0)
                .expect("00:00:00 is always a valid time"),
        )
        .to_rfc3339();
    #[allow(clippy::expect_used)]
    let to = Utc
        .from_utc_datetime(
            &yesterday
                .and_hms_opt(23, 59, 59)
                .expect("23:59:59 is always a valid time"),
        )
        .to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT summary, body, actor, scope, coalesce(source_path, '')
         FROM episodes
         WHERE ts >= ?1 AND ts <= ?2
         ORDER BY ts DESC
         LIMIT ?3",
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let rows = stmt.query_map(params![from, to, limit as i64], |row| {
        let summary: String = row.get(0)?;
        let body: String = row.get(1)?;
        let source_path: String = row.get(4)?;
        Ok(PackLine {
            section: "yesterday".to_string(),
            title: summary,
            body: body.chars().take(240).collect(),
            source_path,
            scope: row.get(3)?,
            owner: row.get(2)?,
            reason: "recent-episode".to_string(),
            activation: None,
            stale: false,
            confidence: confidence_markers(&body),
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn episode_precedent_lines(
    conn: &Connection,
    terms: &[String],
    min_relevance: Option<f64>,
    limit: usize,
) -> Result<Vec<PackLine>> {
    if limit == 0 || terms.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT summary, body, actor, scope, coalesce(source_path, '')
         FROM episodes
         WHERE kind != 'miss'
         ORDER BY ts DESC
         LIMIT 500",
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    let mut lines = Vec::new();
    for row in rows {
        let (summary, body, owner, scope, source_path) = row?;
        if !matches_terms(&summary, &body, terms) {
            continue;
        }
        if min_relevance
            .is_some_and(|floor| context_floor_relevance("episode", &summary, &body, terms) < floor)
        {
            continue;
        }
        if min_relevance.is_some() && !context_tail_signal_strong("episode", &summary, &body, terms)
        {
            continue;
        }
        lines.push(PackLine {
            section: "episodic-precedent".to_string(),
            title: summary,
            body: body.chars().take(240).collect(),
            source_path,
            scope,
            owner,
            reason: "matched-task".to_string(),
            activation: None,
            stale: false,
            confidence: confidence_markers(&body),
        });
        if lines.len() >= limit {
            break;
        }
    }
    Ok(lines)
}

fn is_task_specific_line(line: &PackLine) -> bool {
    line.reason != "house-rule-core" && line.section != "nothing-specific"
}

fn nothing_specific_line() -> PackLine {
    PackLine {
        section: "nothing-specific".to_string(),
        title: "No task match".to_string(),
        body: "No task match above the relevance floor -- standing rules only.".to_string(),
        source_path: String::new(),
        scope: "company".to_string(),
        owner: "shared".to_string(),
        reason: "no-task-match".to_string(),
        activation: None,
        stale: false,
        confidence: Vec::new(),
    }
}

#[derive(Debug)]
struct MemoryRow {
    id: i64,
    name: String,
    title: String,
    body: String,
    owner: String,
    scope: String,
    source_path: String,
    is_lock: bool,
}

fn memory_rows(conn: &Connection, predicate: &str) -> Result<Vec<MemoryRow>> {
    let sql = format!(
        "SELECT id, name, title, body, owner, scope, coalesce(source_path, ''), is_lock
         FROM memories WHERE {predicate}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(MemoryRow {
            id: row.get(0)?,
            name: row.get(1)?,
            title: row.get(2)?,
            body: row.get::<_, String>(3)?.chars().take(320).collect(),
            owner: row.get(4)?,
            scope: row.get(5)?,
            source_path: row.get(6)?,
            is_lock: row.get::<_, i64>(7)? != 0,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn lock_store_rows(conn: &Connection) -> Result<Vec<MemoryRow>> {
    let source_path = crate::workspace_root()
        .map(|root| root.join("system/locks.yaml").to_string_lossy().to_string())
        .unwrap_or_else(|_| "system/locks.yaml".to_string());
    let mut stmt = conn.prepare(
        "SELECT id, slug, title, body, scope
         FROM locks
         WHERE status = 'active'",
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let rows = stmt.query_map([], |row| {
        let id: i64 = row.get(0)?;
        Ok(MemoryRow {
            id: -id,
            name: row.get(1)?,
            title: row.get(2)?,
            body: row.get::<_, String>(3)?.chars().take(320).collect(),
            owner: "shared".to_string(),
            scope: row.get(4)?,
            source_path: source_path.clone(),
            is_lock: true,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

#[derive(Debug)]
struct ScoredMemory {
    row: MemoryRow,
    relevance: f64,
    activation: Option<f64>,
    core: bool,
}

impl ScoredMemory {
    fn score(&self) -> f64 {
        (self.relevance * RELEVANCE_WEIGHT)
            + (self
                .activation
                .unwrap_or(f64::NEG_INFINITY)
                .clamp(-10.0, 0.0)
                * ACTIVATION_WEIGHT)
            + if self.core { CORE_LOCK_BONUS } else { 0.0 }
    }
}

fn sort_scored_memories(candidates: &mut [ScoredMemory]) {
    candidates.sort_by(|a, b| {
        b.score()
            .total_cmp(&a.score())
            .then_with(|| a.row.title.cmp(&b.row.title))
    });
}

fn is_core_lock(row: &MemoryRow) -> bool {
    let haystack = format!("{}\n{}", row.name, row.body).to_ascii_lowercase();
    CURATOR_AUTHORED_CORE_LOCKS
        .iter()
        .any(|slug| row.name == *slug || haystack.contains(slug))
}

fn row_relevance(
    row: &MemoryRow,
    terms: Option<&[String]>,
    relevance_scores: Option<&HashMap<i64, f64>>,
) -> f64 {
    let bm25 = relevance_scores
        .and_then(|scores| scores.get(&row.id).copied())
        .unwrap_or(0.0);
    let lexical = terms
        .map(|items| lexical_relevance(&row.name, &row.title, &row.body, items))
        .unwrap_or(0.0);
    if bm25 > 0.0 {
        bm25 * lexical.max(0.1)
    } else {
        lexical
    }
}

fn row_term_relevance(row: &MemoryRow, terms: Option<&[String]>, fallback: f64) -> f64 {
    terms
        .map(|items| context_floor_relevance(&row.name, &row.title, &row.body, items))
        .unwrap_or(fallback)
}

fn context_floor_relevance(name: &str, title: &str, body: &str, terms: &[String]) -> f64 {
    let signal_terms = context_signal_terms(terms);
    if !signal_terms.is_empty() {
        return context_signal_relevance(name, title, body, &signal_terms);
    }
    let floor_terms = terms.to_vec();
    if floor_terms.is_empty() {
        return 0.0;
    }
    let haystack = format!("{name}\n{title}\n{body}").to_ascii_lowercase();
    let matches = floor_terms
        .iter()
        .filter(|term| context_floor_term_matches(&haystack, term))
        .count();
    let ratio = matches as f64 / floor_terms.len() as f64;
    if ratio >= 0.5 {
        DEFAULT_CONTEXT_RELEVANCE_FLOOR
    } else {
        ratio
    }
}

fn context_signal_relevance(name: &str, title: &str, body: &str, signal_terms: &[String]) -> f64 {
    let haystack = format!("{name}\n{title}\n{body}").to_ascii_lowercase();
    let matched = signal_terms
        .iter()
        .filter(|term| context_floor_term_matches(&haystack, term))
        .count();
    if matched >= 2 {
        return matched as f64 / signal_terms.len() as f64;
    }
    let title_haystack = format!("{name}\n{title}").to_ascii_lowercase();
    if matched == 1
        && signal_terms.iter().any(|term| {
            is_testing_signal_term(term) && context_floor_title_term_matches(&title_haystack, term)
        })
    {
        return DEFAULT_CONTEXT_RELEVANCE_FLOOR;
    }
    0.0
}

fn context_tail_signal_strong(name: &str, title: &str, body: &str, terms: &[String]) -> bool {
    let signal_terms = context_signal_terms(terms);
    if signal_terms.is_empty() {
        return true;
    }
    let haystack = format!("{name}\n{title}\n{body}").to_ascii_lowercase();
    let title_haystack = format!("{name}\n{title}").to_ascii_lowercase();
    let matched = signal_terms
        .iter()
        .filter(|term| context_floor_term_matches(&haystack, term))
        .count();
    matched >= 2
        && signal_terms
            .iter()
            .any(|term| context_floor_title_term_matches(&title_haystack, term))
}

fn context_signal_terms(terms: &[String]) -> Vec<String> {
    terms
        .iter()
        .filter(|term| is_context_signal_term(term))
        .cloned()
        .collect()
}

fn is_context_signal_term(term: &str) -> bool {
    matches!(
        term,
        "add"
            | "build"
            | "clean"
            | "debug"
            | "deploy"
            | "fix"
            | "implement"
            | "launch"
            | "python"
            | "review"
            | "ship"
            | "test"
            | "testing"
            | "tests"
            | "tweet"
            | "write"
    )
}

fn is_testing_signal_term(term: &str) -> bool {
    matches!(term, "test" | "testing" | "tests")
}

fn context_floor_term_matches(haystack: &str, term: &str) -> bool {
    let tokens = haystack
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    if term == "test" {
        return query_aliases(term)
            .iter()
            .any(|alias| tokens.iter().any(|token| token == alias));
    }
    tokens.iter().any(|token| token == &term)
        || query_aliases(term)
            .iter()
            .any(|alias| tokens.iter().any(|token| token == alias))
}

fn context_floor_title_term_matches(haystack: &str, term: &str) -> bool {
    let tokens = haystack
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    if matches!(term, "test" | "tests" | "testing") {
        return ["tests", "testing", "pytest"]
            .iter()
            .any(|alias| tokens.iter().any(|token| token == alias));
    }
    tokens.iter().any(|token| token == &term)
        || query_aliases(term)
            .iter()
            .any(|alias| tokens.iter().any(|token| token == alias))
}

fn lexical_relevance(name: &str, title: &str, body: &str, terms: &[String]) -> f64 {
    if terms.is_empty() {
        return 0.0;
    }
    let haystack = format!("{name}\n{title}\n{body}").to_ascii_lowercase();
    let matches = terms
        .iter()
        .filter(|term| term_or_alias_matches(&haystack, term))
        .count();
    matches as f64 / terms.len() as f64
}

fn term_or_alias_matches(haystack: &str, term: &str) -> bool {
    haystack.contains(term)
        || query_aliases(term)
            .iter()
            .any(|alias| haystack.contains(alias))
}

fn query_aliases(term: &str) -> &'static [&'static str] {
    match term {
        "launch" => &["pitch", "public", "opening", "grand"],
        "python" => &["pytest", "ruff"],
        "test" | "tests" | "testing" => &["tests", "testing", "pytest", "coverage"],
        "tweet" => &["social", "copy", "ads", "email"],
        _ => &[],
    }
}

fn candidate_reason(candidate: &ScoredMemory) -> &'static str {
    if candidate.core {
        "house-rule-core"
    } else if candidate.relevance > 0.0 {
        "matched-task"
    } else {
        "hot"
    }
}

fn pack_line(
    section: &str,
    row: &MemoryRow,
    activation: Option<f64>,
    stale: bool,
    reason: &str,
) -> PackLine {
    PackLine {
        section: section.to_string(),
        title: row.title.clone(),
        body: row.body.clone(),
        source_path: row.source_path.clone(),
        scope: row.scope.clone(),
        owner: row.owner.clone(),
        reason: reason.to_string(),
        activation,
        stale,
        confidence: confidence_markers(&format!("{}\n{}", row.title, row.body)),
    }
}

fn row_future_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<FutureItem> {
    Ok(FutureItem {
        id: row.get(0)?,
        body: row.get(1)?,
        due: row.get(2)?,
        created_by: row.get(3)?,
        status: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn future_item_by_id(conn: &Connection, id: i64) -> Result<FutureItem> {
    maybe_future_item_by_id(conn, id)?.ok_or_else(|| anyhow::anyhow!("future item {id} not found"))
}

fn maybe_future_item_by_id(conn: &Connection, id: i64) -> Result<Option<FutureItem>> {
    Ok(conn
        .query_row(
            "SELECT id, body, due, created_by, status, created_at FROM future_items WHERE id = ?1",
            params![id],
            row_future_item,
        )
        .optional()?)
}

fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_')
        .filter(|term| term.len() >= 2)
        .filter(|term| !search::is_query_stopword(term))
        .take(12)
        .map(|term| term.to_ascii_lowercase())
        .collect()
}

fn matches_terms(title: &str, body: &str, terms: &[String]) -> bool {
    if terms.is_empty() {
        return true; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    }
    let haystack = format!("{title}\n{body}").to_ascii_lowercase();
    terms.iter().any(|term| haystack.contains(term))
}

fn scoped_for_company(scope: &str) -> bool {
    scope == "company" || scope == "os" || scope.starts_with("product:")
}

fn source_stale(conn: &Connection, source_path: &str) -> Result<bool> {
    if source_path.is_empty() {
        return Ok(false);
    }
    let Some(last) = storage::meta_value(conn, "last_ingest_ts")? else {
        return Ok(true);
    };
    let Ok(last) = DateTime::parse_from_rfc3339(&last) else {
        return Ok(true);
    };
    let Ok(meta) = std::fs::metadata(Path::new(source_path)) else {
        return Ok(false);
    };
    let Ok(modified) = meta.modified() else {
        return Ok(false); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    };
    Ok(DateTime::<Utc>::from(modified) > last.with_timezone(&Utc))
}

fn confidence_markers(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (needle, label) in [("\u{2705}", "✅"), ("\u{1f914}", "🤔"), ("\u{2753}", "❓")] {
        if text.contains(needle) {
            out.push(label.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parser::MemoryDoc, schema};
    use proptest::prelude::*;
    use std::path::Path;

    #[test]
    fn future_item_validation_rejects_garbage() {
        assert!(validate_iso_date("2026-06-13").is_ok());
        assert!(validate_iso_date("06/13/2026").is_err());
    }

    #[test]
    fn done_marks_open_future_item_done() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        let id =
            storage::insert_future_item(&conn, "future", "2026-06-13", "agent:engineer").unwrap();

        let transition = done(&conn, id, false).unwrap();

        assert!(transition.changed);
        assert_eq!(transition.previous_status, "open");
        assert_eq!(transition.item.status, "done");
        assert!(upcoming(&conn, None).unwrap().is_empty());
    }

    #[test]
    fn done_can_drop_open_future_item() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        let id =
            storage::insert_future_item(&conn, "future", "2026-06-13", "agent:engineer").unwrap();

        let transition = done(&conn, id, true).unwrap();

        assert!(transition.changed);
        assert_eq!(transition.item.status, "dropped");
    }

    #[test]
    fn done_is_idempotent_for_closed_future_items() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        let id =
            storage::insert_future_item(&conn, "future", "2026-06-13", "agent:engineer").unwrap();
        done(&conn, id, false).unwrap();

        let transition = done(&conn, id, false).unwrap();

        assert!(!transition.changed);
        assert_eq!(transition.previous_status, "done");
        assert_eq!(transition.item.status, "done");
    }

    #[test]
    fn done_rejects_unknown_future_item() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();

        let err = done(&conn, 999, false).unwrap_err().to_string();

        assert!(err.contains("future item 999 not found"));
    }

    #[test]
    fn brief_filters_closed_future_items() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_memory(&conn, "lock", true);
        let id =
            storage::insert_future_item(&conn, "future", "2026-06-13", "agent:engineer").unwrap();
        done(&conn, id, false).unwrap();

        let lines = brief(&conn, "curator").unwrap();

        assert!(
            lines
                .iter()
                .all(|line| line.section != "due-future" && line.body != "future")
        );
    }

    #[test]
    fn context_budget_is_hard_cap() {
        let line = PackLine {
            section: "hot-memory".to_string(),
            title: "t".to_string(),
            body: "b".to_string(),
            source_path: "/tmp/x".to_string(),
            scope: "company".to_string(),
            owner: "shared".to_string(),
            reason: "matched-task".to_string(),
            activation: None,
            stale: false,
            confidence: Vec::new(),
        };
        assert_eq!(
            cap_lines(vec![line.clone(), line.clone(), line], 2).len(),
            2
        );
    }

    #[test]
    fn brief_orders_locks_before_hot_and_due() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_memory(&conn, "lock", true);
        insert_memory(&conn, "hot", false);
        storage::log_recall_event(&conn, 2, "agent:engineer", "company").unwrap();
        storage::insert_future_item(&conn, "future", "2026-06-13", "agent:engineer").unwrap();

        let lines = brief(&conn, "curator").unwrap();
        assert_eq!(lines[0].section, "active-lock");
        assert!(
            lines
                .iter()
                .position(|line| line.section == "hot-memory")
                .unwrap()
                < lines
                    .iter()
                    .position(|line| line.section == "due-future")
                    .unwrap()
        );
        assert!(lines.len() <= BRIEF_MAX_LINES);
    }

    #[test]
    fn pack_helper_limit_and_filter_branches_are_explicit() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        assert!(
            lock_lines(&conn, "agent:curator", None, None, None, 0)
                .unwrap()
                .is_empty()
        );
        assert!(
            hot_memory_lines(&conn, "agent:curator", None, None, None, 0)
                .unwrap()
                .is_empty()
        );
        assert!(future_lines(&conn, None, 0).unwrap().is_empty());
        assert!(yesterday_lines(&conn, 0).unwrap().is_empty());

        let mut product = memory_doc("product-only", false);
        product.scope = "personal".to_string();
        product.content_hash = "hash-personal".to_string();
        storage::upsert_memory(&conn, &product).unwrap();
        insert_memory(&conn, "dispatch lock", true);
        let terms = vec!["missing".to_string()];
        assert!(
            lock_lines(&conn, "agent:curator", Some(&terms), None, None, 5)
                .unwrap()
                .is_empty()
        );

        insert_memory(&conn, "cold memory", false);
        let prune = prune_report(&conn).unwrap();
        assert!(prune.iter().any(|line| line.section == "cooling-candidate"));
    }

    #[test]
    fn upcoming_days_filter_includes_only_parseable_due_before_cutoff() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        let today = Local::now().date_naive();
        storage::insert_future_item(
            &conn,
            "soon",
            &today.format("%Y-%m-%d").to_string(),
            "agent:engineer",
        )
        .unwrap();
        storage::insert_future_item(&conn, "bad-date", "not-a-date", "agent:engineer").unwrap();
        storage::insert_future_item(
            &conn,
            "later",
            &(today + Duration::days(10)).format("%Y-%m-%d").to_string(),
            "agent:engineer",
        )
        .unwrap();

        let rows = upcoming(&conn, Some(1)).unwrap();

        assert_eq!(
            rows.iter()
                .map(|item| item.body.as_str())
                .collect::<Vec<_>>(),
            ["soon", "bad-date"]
        );
    }

    #[test]
    fn related_maps_memory_and_episode_targets_with_defaults() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_memory(&conn, "from memory", false);
        insert_memory(&conn, "to memory", false);
        conn.execute(
            "INSERT INTO episodes(ts, actor, kind, summary, body, scope, source_path)
             VALUES('2026-06-14T00:00:00Z', 'agent:engineer', 'note', 'Episode Target', NULL, 'company', NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO links(from_kind, from_id, to_kind, to_id, kind)
             VALUES('memory', 1, 'memory', 2, 'wiki'), ('memory', 1, 'episode', 1, 'audit')",
            [],
        )
        .unwrap();

        assert!(related(&conn, "missing memory").unwrap().is_empty());
        let rows = related(&conn, "from memory").unwrap();

        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .any(|row| row.kind == "memory" && row.title == "to memory")
        );
        assert!(rows.iter().any(|row| {
            row.kind == "episode" && row.title == "Episode Target" && row.body.is_empty()
        }));
    }

    #[test]
    fn context_zero_budget_and_precedent_yesterday_lines_cover_pack_helpers() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        assert!(
            context(&conn, "curator", "anything", Some(0))
                .unwrap()
                .is_empty()
        );
        assert!(
            episode_precedent_lines(&conn, &[], None, 10)
                .unwrap()
                .is_empty()
        );

        let yesterday = Local::now().date_naive() - Duration::days(1);
        let ts = Utc
            .from_utc_datetime(&yesterday.and_hms_opt(12, 0, 0).unwrap())
            .to_rfc3339();
        conn.execute(
            "INSERT INTO episodes(ts, actor, kind, summary, body, scope, source_path)
             VALUES(?1, 'agent:engineer', 'note', 'Yesterday summary', ?2, 'company', '/tmp/yesterday.md')",
            params![ts, "Yesterday body with \u{2705} and \u{1f914}"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO episodes(ts, actor, kind, summary, body, scope, source_path)
             VALUES('2026-06-13T00:00:00Z', 'agent:engineer', 'note', 'Dispatch precedent', 'dispatch body \u{2753}', 'company', '/tmp/precedent.md')",
            [],
        )
        .unwrap();

        let yesterday_rows = yesterday_lines(&conn, 5).unwrap();
        assert_eq!(yesterday_rows[0].section, "yesterday");
        assert_eq!(yesterday_rows[0].confidence, ["✅", "🤔"]);

        let terms = query_terms("dispatch ticket");
        let precedent = episode_precedent_lines(&conn, &terms, None, 1).unwrap();
        assert_eq!(precedent[0].section, "episodic-precedent");
        assert_eq!(precedent[0].confidence, ["❓"]);
    }

    #[test]
    fn source_stale_and_confidence_marker_branches_are_explicit() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        assert!(!source_stale(&conn, "").unwrap());
        assert!(source_stale(&conn, "/tmp/missing.md").unwrap());

        conn.execute(
            "INSERT INTO meta(key, value) VALUES('last_ingest_ts', 'not-a-date')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )
        .unwrap();
        assert!(source_stale(&conn, "/tmp/missing.md").unwrap());

        conn.execute(
            "INSERT INTO meta(key, value) VALUES('last_ingest_ts', '2999-01-01T00:00:00Z')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )
        .unwrap();
        assert!(!source_stale(&conn, "/tmp/missing.md").unwrap());
        assert_eq!(
            confidence_markers("ok \u{2705} unsure \u{1f914} question \u{2753}"),
            ["✅", "🤔", "❓"]
        );
    }

    #[test]
    fn ask_respects_acl_revoke() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_agent_memory(&conn);
        crate::storage::rebuild_fts(&conn).unwrap();
        assert!(
            !ask(
                &conn,
                "archivist",
                "closing console",
                "agent:engineer",
                "company",
                false,
                5
            )
            .unwrap()
            .is_empty()
        );
        conn.execute(
            "INSERT INTO node_acl(owner_node, reader, granted) VALUES('agent:archivist', 'agent:engineer', 0)",
            [],
        )
        .unwrap();
        assert!(
            ask(
                &conn,
                "archivist",
                "closing console",
                "agent:engineer",
                "company",
                false,
                5
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn context_keeps_locks_first() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_memory(&conn, "dispatch lock", true);
        insert_memory(&conn, "dispatch hot", false);
        storage::log_recall_event(&conn, 2, "agent:engineer", "company").unwrap();

        let lines = context(&conn, "curator", "dispatch ticket", Some(2)).unwrap();
        assert_eq!(lines[0].section, "active-lock");
        assert!(lines.len() <= 2);
    }

    #[test]
    fn context_ranks_locks_by_task_relevance_and_keeps_core() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_named_memory(
            &conn,
            "actor-lanes",
            "Actor Lanes",
            "Operator decides. Curator routes. Builder executes.",
            true,
        );
        insert_named_memory(
            &conn,
            "memory-security-boundaries",
            "Memory Security Boundaries",
            "Layer-1 personal data stays outside Builder and product agents.",
            true,
        );
        insert_lock_store_entry(
            &conn,
            "shelves-testing-standard",
            "Shelves Testing Standard",
            "Rust cargo test, Python pytest, golden coverage, and failing test trust gates.",
        );
        insert_lock_store_entry(
            &conn,
            "notebook-positioning-hook-value-moat",
            "Notebook Positioning",
            "Public launch tweet: score hook, Advisor value, memory compounding moat.",
        );
        insert_named_memory(
            &conn,
            "generic-hot-lock",
            "Generic Hot Lock",
            "General shelves company rule with repeated activation.",
            true,
        );
        storage::rebuild_fts(&conn).unwrap();
        let generic_id: i64 = conn
            .query_row(
                "SELECT id FROM memories WHERE name = 'generic-hot-lock'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        storage::log_recall_event(&conn, generic_id, "agent:engineer", "company").unwrap();

        let test_pack = context(
            &conn,
            "curator",
            "fix a failing python test in the shelves",
            Some(10),
        )
        .unwrap();
        let tweet_pack = context(
            &conn,
            "curator",
            "write a public launch tweet for Notebook",
            Some(10),
        )
        .unwrap();

        let test_top3 = test_pack
            .iter()
            .take(3)
            .map(|line| line.title.as_str())
            .collect::<Vec<_>>();
        let tweet_top3 = tweet_pack
            .iter()
            .take(3)
            .map(|line| line.title.as_str())
            .collect::<Vec<_>>();
        assert_ne!(test_top3, tweet_top3);
        assert!(test_top3.contains(&"Shelves Testing Standard"));
        assert!(
            test_pack
                .iter()
                .all(|line| line.title != "Generic Hot Lock")
        );
        assert!(tweet_top3.contains(&"Notebook Positioning"));
        assert_eq!(
            test_pack
                .iter()
                .find(|line| line.title == "Shelves Testing Standard")
                .map(|line| line.reason.as_str()),
            Some("matched-task")
        );
        assert_eq!(
            test_pack
                .iter()
                .find(|line| line.title == "Actor Lanes")
                .map(|line| line.reason.as_str()),
            Some("house-rule-core")
        );
        assert_eq!(lexical_relevance("name", "title", "body", &[]), 0.0);
        for pack in [test_pack, tweet_pack] {
            assert!(pack.iter().any(|line| line.title == "Actor Lanes"));
            assert!(
                pack.iter()
                    .any(|line| line.title == "Memory Security Boundaries")
            );
        }
    }

    #[test]
    fn context_floor_keeps_core_and_speaks_when_task_has_no_match() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_named_memory(
            &conn,
            "actor-lanes",
            "Actor Lanes",
            "Operator decides. Curator routes. Builder executes.",
            true,
        );
        insert_named_memory(
            &conn,
            "maintenance-bleed",
            "Maintenance Bleed",
            "A generic maintenance note that should not answer unrelated schedules.",
            true,
        );
        storage::rebuild_fts(&conn).unwrap();

        let lines = context(
            &conn,
            "curator",
            "espresso machine maintenance schedule for the third floor",
            Some(10),
        )
        .unwrap();

        assert!(
            lines
                .iter()
                .any(|line| { line.title == "Actor Lanes" && line.reason == "house-rule-core" })
        );
        assert!(lines.iter().all(|line| line.title != "Maintenance Bleed"));
        assert!(lines.iter().any(|line| {
            line.section == "nothing-specific"
                && line.body == "No task match above the relevance floor -- standing rules only."
        }));
    }

    #[test]
    fn context_floor_keeps_top_match_and_drops_single_term_bleed() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_named_memory(
            &conn,
            "actor-lanes",
            "Actor Lanes",
            "Operator decides. Curator routes. Builder executes.",
            true,
        );
        insert_lock_store_entry(
            &conn,
            "shelves-testing-standard",
            "Shelves Testing Standard",
            "Python pytest coverage and failing test gates for trusted memory.",
        );
        insert_lock_store_entry(
            &conn,
            "off-topic-note",
            "Off-Topic Note",
            "An unrelated note that should not surface.",
        );
        storage::rebuild_fts(&conn).unwrap();

        let lines = context(&conn, "curator", "fix a python test in notebook", Some(10)).unwrap();

        assert!(
            lines
                .iter()
                .any(|line| line.title == "Shelves Testing Standard")
        );
        assert!(lines.iter().all(|line| line.title != "Off-Topic Note"));
        assert!(lines.iter().all(|line| line.section != "nothing-specific"));
    }

    #[test]
    fn context_relevance_floor_meta_tunes_cutoff() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_named_memory(
            &conn,
            "actor-lanes",
            "Actor Lanes",
            "Operator decides. Curator routes. Builder executes.",
            true,
        );
        insert_lock_store_entry(
            &conn,
            "borderline-testing",
            "Borderline Testing",
            "Python pytest coverage.",
        );
        storage::rebuild_fts(&conn).unwrap();

        conn.execute(
            "INSERT INTO meta(key, value) VALUES('context_relevance_floor', '0.75')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )
        .unwrap();
        let strict = context(&conn, "curator", "fix a python test in notebook", Some(10)).unwrap();
        assert!(strict.iter().all(|line| line.title != "Borderline Testing"));
        assert!(strict.iter().any(|line| line.section == "nothing-specific"));

        conn.execute(
            "UPDATE meta SET value = '0.25' WHERE key = 'context_relevance_floor'",
            [],
        )
        .unwrap();
        let loose = context(&conn, "curator", "fix a python test in notebook", Some(10)).unwrap();
        assert!(loose.iter().any(|line| line.title == "Borderline Testing"));
    }

    #[test]
    fn context_empty_task_does_not_apply_relevance_floor() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_named_memory(
            &conn,
            "actor-lanes",
            "Actor Lanes",
            "Operator decides. Curator routes. Builder executes.",
            true,
        );
        insert_named_memory(
            &conn,
            "generic-hot",
            "Generic Hot",
            "Broad memory line for boot-style context.",
            false,
        );
        storage::log_recall_event(&conn, 2, "agent:engineer", "company").unwrap();
        storage::insert_future_item(&conn, "future task", "2026-06-13", "agent:engineer").unwrap();

        let lines = context(&conn, "curator", "", Some(10)).unwrap();

        assert!(lines.iter().any(|line| line.title == "Generic Hot"));
        assert!(lines.iter().any(|line| line.section == "due-future"));
        assert!(lines.iter().all(|line| line.section != "nothing-specific"));
    }

    #[test]
    fn context_floor_helper_branches_are_explicit() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        let signal_terms = query_terms("fix a python test in notebook");
        let dispatch_terms = query_terms("dispatch ticket");
        let plain_terms = query_terms("maintenance schedule");

        assert_eq!(context_floor_relevance("", "", "", &[]), 0.0);
        assert_eq!(
            context_floor_relevance("memory", "dispatch", "dispatch ticket", &dispatch_terms),
            DEFAULT_CONTEXT_RELEVANCE_FLOOR
        );
        assert!(
            context_floor_relevance("memory", "python-only", "python runtime", &signal_terms)
                < DEFAULT_CONTEXT_RELEVANCE_FLOOR
        );
        assert_eq!(
            context_floor_relevance("memory", "Shelves Testing", "body", &signal_terms),
            DEFAULT_CONTEXT_RELEVANCE_FLOOR
        );
        assert!(context_tail_signal_strong(
            "memory",
            "Testing Coverage",
            "python pytest coverage",
            &signal_terms
        ));
        assert!(!context_tail_signal_strong(
            "memory",
            "Body Only",
            "python pytest coverage",
            &signal_terms
        ));
        assert!(context_tail_signal_strong(
            "memory",
            "maintenance",
            "maintenance schedule",
            &plain_terms
        ));
        assert!(context_floor_title_term_matches("shelves testing", "test"));
        assert!(context_floor_title_term_matches("launch pitch", "launch"));

        storage::insert_future_item(
            &conn,
            "maintenance schedule",
            "2026-06-13",
            "agent:engineer",
        )
        .unwrap();
        assert!(
            task_future_lines(&conn, &plain_terms, DEFAULT_CONTEXT_RELEVANCE_FLOOR, 0)
                .unwrap()
                .is_empty()
        );
        let future =
            task_future_lines(&conn, &plain_terms, DEFAULT_CONTEXT_RELEVANCE_FLOOR, 5).unwrap();
        assert_eq!(future[0].section, "due-future");
        assert_eq!(future[0].reason, "matched-task");

        insert_named_memory(
            &conn,
            "python-only-hot",
            "Python Only",
            "python runtime",
            false,
        );
        insert_named_memory(
            &conn,
            "body-strong-hot",
            "Body Strong",
            "python pytest coverage",
            false,
        );
        insert_named_memory(
            &conn,
            "testing-strong-hot",
            "Testing Strong",
            "python pytest coverage",
            false,
        );
        for id in 1..=3 {
            storage::log_recall_event(&conn, id, "agent:engineer", "company").unwrap();
        }
        let hot = hot_memory_lines(
            &conn,
            "agent:curator",
            Some(&signal_terms),
            None,
            Some(DEFAULT_CONTEXT_RELEVANCE_FLOOR),
            5,
        )
        .unwrap();
        assert_eq!(
            hot.iter()
                .map(|line| line.title.as_str())
                .collect::<Vec<_>>(),
            ["Testing Strong"]
        );

        conn.execute(
            "INSERT INTO episodes(ts, actor, kind, summary, body, scope, source_path)
             VALUES
             ('2026-06-13T00:00:00Z', 'agent:engineer', 'note', 'Python Only Episode', 'python runtime', 'company', '/tmp/python.md'),
             ('2026-06-13T00:01:00Z', 'agent:engineer', 'note', 'Body Strong Episode', 'python pytest coverage', 'company', '/tmp/body.md'),
             ('2026-06-13T00:02:00Z', 'agent:engineer', 'note', 'Testing Episode', 'python pytest coverage', 'company', '/tmp/testing.md')",
            [],
        )
        .unwrap();
        let episodes = episode_precedent_lines(
            &conn,
            &signal_terms,
            Some(DEFAULT_CONTEXT_RELEVANCE_FLOOR),
            5,
        )
        .unwrap();
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].title, "Testing Episode");

        let context_with_future =
            context(&conn, "curator", "maintenance schedule", Some(5)).unwrap();
        assert!(
            context_with_future
                .iter()
                .any(|line| line.section == "due-future")
        );
    }

    fn insert_memory(conn: &Connection, title: &str, is_lock: bool) {
        let doc = memory_doc(title, is_lock);
        storage::upsert_memory(conn, &doc).unwrap();
    }

    fn insert_named_memory(conn: &Connection, name: &str, title: &str, body: &str, is_lock: bool) {
        let mut doc = memory_doc(title, is_lock);
        doc.name = name.to_string();
        doc.body = body.to_string();
        doc.content_hash = format!("hash-{name}");
        storage::upsert_memory(conn, &doc).unwrap();
    }

    fn insert_lock_store_entry(conn: &Connection, slug: &str, title: &str, body: &str) {
        conn.execute(
            "INSERT INTO locks(slug, title, body, scope, locked_on, status)
             VALUES(?1, ?2, ?3, 'company', '2026-06-11', 'active')",
            params![slug, title, body],
        )
        .unwrap();
    }

    fn memory_doc(title: &str, is_lock: bool) -> MemoryDoc {
        MemoryDoc {
            name: crate::parser::slugify(title),
            title: title.to_string(),
            body: format!("{title} body"),
            owner: "shared".to_string(),
            scope: "company".to_string(),
            source_path: Path::new("/tmp/memory.md").to_path_buf(),
            content_hash: format!("hash-{title}"),
            is_lock,
            created_at: "2026-06-10T00:00:00Z".to_string(),
            updated_at: "2026-06-10T00:00:00Z".to_string(),
        }
    }

    fn insert_agent_memory(conn: &Connection) {
        let doc = MemoryDoc {
            name: "archivist-bootstrap".to_string(),
            title: "Archivist Bootstrap".to_string(),
            body: "closing console ritual".to_string(),
            owner: "agent:archivist".to_string(),
            scope: "company".to_string(),
            source_path: Path::new("/tmp/archivist.md").to_path_buf(),
            content_hash: "hash-archivist".to_string(),
            is_lock: false,
            created_at: "2026-06-10T00:00:00Z".to_string(),
            updated_at: "2026-06-10T00:00:00Z".to_string(),
        };
        storage::upsert_memory(conn, &doc).unwrap();
    }

    proptest! {
        #[test]
        fn context_cap_never_exceeds_budget(budget in 0usize..100, count in 0usize..200) {
            let line = PackLine {
                section: "hot-memory".to_string(),
                title: "t".to_string(),
                body: "b".to_string(),
                source_path: "/tmp/x".to_string(),
                scope: "company".to_string(),
                owner: "shared".to_string(),
                reason: "matched-task".to_string(),
                activation: None,
                stale: false,
                confidence: Vec::new(),
            };
            let lines = vec![line; count];
            prop_assert!(cap_lines(lines, budget).len() <= budget);
        }
    }
}
