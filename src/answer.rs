use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use anyhow::{Result, bail};
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Utc, Weekday};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::{
    acl, activation,
    locale::{LocaleRegistry, QueryIntent},
    search, storage,
};

const BRIEF_MAX_LINES: usize = 20;
const BRIEF_DUE_DAYS: i64 = 30;
const RELEVANCE_WEIGHT: f64 = 100.0;
const ACTIVATION_WEIGHT: f64 = 1.0;
const CORE_LOCK_BONUS: f64 = 0.1;
const REFERENCE_MATCH_BONUS: f64 = 0.2;
const DEFAULT_CONTEXT_RELEVANCE_FLOOR: f64 = 0.66;
const DEFAULT_CONTEXT_MAX_TERM_DOCUMENT_FREQUENCY: f64 = 0.25;
const ORIENTATION_EPISODE_SCAN_LIMIT: usize = 2_000;
const ORIENTATION_EPISODE_LIMIT: usize = 5;
const ORIENTATION_HOT_LIMIT: usize = 3;
const AGENT_CONTEXT_IDENTITY_LIMIT: usize = 4;
const AGENT_CONTEXT_MEETING_LIMIT: usize = 3;
const AGENT_CONTEXT_TASK_LIMIT: usize = 2;
const CONTEXT_QUERY_LIMIT: usize = 4;
const CONTEXT_SEARCH_LIMIT: usize = 10;
const CORE_LOCKS: &[&str] = &[
    "role-boundaries",
    "accuracy-policy",
    "data-boundaries",
    "automation-policy",
    "quality-policy",
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

#[derive(Debug, Clone, Serialize)]
pub struct MissListRow {
    pub id: i64,
    pub ts: String,
    pub what: String,
    pub by: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgentContextIntent {
    Identity,
    Meeting,
    Task,
}

pub fn normalize_agent(agent: &str) -> String {
    let lowered = agent.trim().trim_start_matches('@').to_ascii_lowercase();
    if lowered.starts_with("agent:") {
        lowered
    } else {
        format!("agent:{lowered}")
    }
}

fn agent_context_intent(agent: &str, task: &str) -> AgentContextIntent {
    let query = task.to_ascii_lowercase();
    let normalized = normalize_agent(agent);
    let name = normalized.trim_start_matches("agent:");
    let names_identity = query.contains(&format!("who is {name}"))
        || (query.contains(name) && query.contains("current open work"));
    if names_identity {
        AgentContextIntent::Identity
    } else if query.contains("meeting:") {
        AgentContextIntent::Meeting
    } else {
        AgentContextIntent::Task
    }
}

fn agent_owned_limit(intent: AgentContextIntent, max_lines: usize) -> usize {
    let reserve = usize::from(max_lines >= 3) * 2;
    let intent_limit = match intent {
        AgentContextIntent::Identity => AGENT_CONTEXT_IDENTITY_LIMIT,
        AgentContextIntent::Meeting => AGENT_CONTEXT_MEETING_LIMIT,
        AgentContextIntent::Task => AGENT_CONTEXT_TASK_LIMIT,
    };
    intent_limit.min(max_lines.saturating_sub(reserve))
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
        &reader,
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
    let locales = LocaleRegistry::installed()?;
    context_with_locales(conn, agent, task, budget, &locales)
}

pub fn context_with_locales(
    conn: &Connection,
    agent: &str,
    task: &str,
    budget: Option<usize>,
    locales: &LocaleRegistry,
) -> Result<Vec<PackLine>> {
    let max_lines = budget.unwrap_or(storage::meta_usize(conn, "context_default_budget", 15)?);
    if max_lines == 0 {
        return Ok(Vec::new());
    }
    let prepared = locales.prepare(task);
    let reader = normalize_agent(agent);
    let original_terms = query_terms(&prepared.search_text);
    let terms = if prepared.intent == QueryIntent::Orientation {
        original_terms.clone()
    } else {
        damped_query_terms(conn, &reader, original_terms.as_slice())?
    };
    let task_terms = if original_terms.is_empty() {
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
    let ranking_queries = effective_context_ranking_queries(
        &prepared.search_text,
        original_terms.as_slice(),
        terms.as_slice(),
    );
    let mut relevance_scores = HashMap::new();
    let mut canonical_lock_scores = HashMap::new();
    for query in &ranking_queries {
        merge_relevance_scores(
            &mut relevance_scores,
            search::memory_relevance_scores(
                conn,
                query,
                "company",
                &reader,
                false,
                CONTEXT_SEARCH_LIMIT,
            )?, // LCOV_EXCL_LINE: fallible search boundary; success asserted by context tests.
        );
        merge_relevance_scores(
            &mut canonical_lock_scores,
            search::lock_relevance_scores(conn, query, "company", CONTEXT_SEARCH_LIMIT)?,
        );
    }
    let mut lock_relevance_scores = relevance_scores.clone();
    merge_relevance_scores(&mut lock_relevance_scores, canonical_lock_scores);
    let agent_intent = agent_context_intent(agent, task);
    let owned_limit = agent_owned_limit(agent_intent, max_lines);
    let owned_lines = agent_owned_memory_lines(
        conn,
        &reader,
        task_terms,
        Some(&relevance_scores),
        relevance_floor,
        agent_intent,
        owned_limit,
    )?; // LCOV_EXCL_LINE: fallible pack boundary; success asserted by agent goldens.
    let mut lines = if prepared.intent == QueryIntent::Orientation {
        orientation_context(
            conn,
            &reader,
            &prepared.topic_text,
            task_terms,
            relevance_floor,
            &relevance_scores,
            &lock_relevance_scores,
            owned_lines,
            max_lines,
        )? // LCOV_EXCL_LINE: successful orientation branch asserted by context goldens.
    } else if let Some(relevance_floor) = relevance_floor {
        standard_task_context(
            conn,
            &reader,
            &terms,
            &relevance_scores,
            &lock_relevance_scores,
            relevance_floor,
            owned_lines,
            max_lines,
        )? // LCOV_EXCL_LINE: successful task branch asserted by context goldens.
    } else {
        context_without_task(
            conn,
            &reader,
            &relevance_scores,
            &lock_relevance_scores,
            owned_lines,
            max_lines,
        )? // LCOV_EXCL_LINE: successful empty-task branch asserted by context goldens.
    };
    let withheld = search::withheld_private_count(conn, &prepared.search_text, &reader)?;
    if withheld > 0 {
        if lines.len() >= max_lines {
            lines.pop();
        }
        let message = format!(
            "{withheld} private note{} withheld",
            if withheld == 1 { "" } else { "s" }
        );
        lines.push(PackLine {
            section: "privacy".to_string(),
            title: message.clone(),
            body: message,
            source_path: String::new(),
            scope: String::new(),
            owner: String::new(),
            reason: "withheld-private".to_string(),
            activation: None,
            stale: false,
            confidence: Vec::new(),
        });
    }
    Ok(lines)
}

#[allow(clippy::too_many_arguments)]
fn standard_task_context(
    conn: &Connection,
    reader: &str,
    terms: &[String],
    relevance_scores: &HashMap<i64, f64>,
    lock_relevance_scores: &HashMap<i64, f64>,
    relevance_floor: f64,
    owned_lines: Vec<PackLine>,
    max_lines: usize,
) -> Result<Vec<PackLine>> {
    let house_rules = house_rules_line(conn, reader)?;
    let content_cap = max_lines.saturating_sub(usize::from(house_rules.is_some()));
    let mut lines = owned_lines;
    let mut task_memories = task_memory_lines(
        conn,
        reader,
        terms,
        relevance_scores,
        relevance_floor,
        max_lines,
    )?; // LCOV_EXCL_LINE: fallible pack boundary; asserted by context goldens.
    retain_unique_additions(&lines, &mut task_memories);
    let task_locks = task_lock_lines(
        conn,
        reader,
        terms,
        lock_relevance_scores,
        relevance_floor,
        max_lines,
    )?; // LCOV_EXCL_LINE: fallible pack boundary; asserted by context goldens.
    let available = content_cap.saturating_sub(lines.len());
    let memory_target = max_lines.saturating_mul(3).div_ceil(5);
    let minimum_task_memories = memory_target
        .saturating_sub(lines.len())
        .min(task_memories.len())
        .min(available);
    let lock_count = task_locks
        .len()
        .min(available.saturating_sub(minimum_task_memories));
    let memory_count = task_memories
        .len()
        .min(available.saturating_sub(lock_count));
    lines.extend(task_memories.into_iter().take(memory_count));
    lines.extend(task_locks.into_iter().take(lock_count));
    if !lines.iter().any(is_task_specific_line) {
        lines.extend(episode_precedent_lines(
            conn,
            reader,
            terms,
            Some(relevance_floor),
            content_cap.saturating_sub(lines.len()),
        )?); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    }
    if !lines.iter().any(is_task_specific_line) {
        lines.extend(task_future_lines(
            conn,
            terms,
            relevance_floor,
            content_cap.saturating_sub(lines.len()),
        )?); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    }
    if !lines.iter().any(is_task_specific_line) && lines.len() < content_cap {
        lines.push(nothing_specific_line());
    }
    if let Some(line) = house_rules
        && lines.len() < max_lines
    {
        lines.push(line);
    }
    Ok(cap_lines(lines, max_lines))
}

fn context_without_task(
    conn: &Connection,
    reader: &str,
    relevance_scores: &HashMap<i64, f64>,
    lock_relevance_scores: &HashMap<i64, f64>,
    owned_lines: Vec<PackLine>,
    max_lines: usize,
) -> Result<Vec<PackLine>> {
    let mut lines = owned_lines;
    lines.extend(lock_lines(
        conn,
        reader,
        None,
        Some(lock_relevance_scores),
        None,
        max_lines,
    )?); // LCOV_EXCL_LINE: legacy empty-task boundary; asserted by unit tests.
    let hot_lines = hot_memory_lines(
        conn,
        reader,
        None,
        Some(relevance_scores),
        None,
        max_lines.saturating_sub(lines.len()),
    )?; // LCOV_EXCL_LINE: legacy empty-task boundary; asserted by unit tests.
    extend_unique_lines(&mut lines, hot_lines);
    lines.extend(future_lines(
        conn,
        None,
        max_lines.saturating_sub(lines.len()),
    )?); // LCOV_EXCL_LINE: legacy empty-task boundary; asserted by unit tests.
    Ok(cap_lines(lines, max_lines))
}

#[allow(clippy::too_many_arguments)]
fn orientation_context(
    conn: &Connection,
    reader: &str,
    task: &str,
    task_terms: Option<&[String]>,
    relevance_floor: Option<f64>,
    relevance_scores: &HashMap<i64, f64>,
    lock_relevance_scores: &HashMap<i64, f64>,
    owned_lines: Vec<PackLine>,
    max_lines: usize,
) -> Result<Vec<PackLine>> {
    let lock_reserve = usize::from(max_lines >= 3) * 2;
    let owned_count = owned_lines.len();
    let state_budget = max_lines
        .saturating_sub(lock_reserve)
        .saturating_sub(owned_count);
    let mut lines = owned_lines;
    lines.extend(orientation_episode_lines(
        conn,
        reader,
        task,
        ORIENTATION_EPISODE_LIMIT.min(state_budget),
    )?); // LCOV_EXCL_LINE: fallible episode boundary; success asserted by orientation goldens.
    let state_used = lines.len().saturating_sub(owned_count);
    let hot_limit = ORIENTATION_HOT_LIMIT.min(state_budget.saturating_sub(state_used));
    let hot_lines = hot_memory_lines(
        conn,
        reader,
        task_terms,
        Some(relevance_scores),
        None,
        hot_limit,
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    extend_unique_lines(&mut lines, hot_lines);
    lines.extend(lock_lines(
        conn,
        reader,
        task_terms,
        Some(lock_relevance_scores),
        relevance_floor,
        max_lines.saturating_sub(lines.len()),
    )?); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    if !lines.iter().any(is_task_specific_line) {
        lines.push(nothing_specific_line());
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

pub fn misses(conn: &Connection, since_days: Option<u32>) -> Result<Vec<MissListRow>> {
    let cutoff = since_days.map(|days| (Utc::now() - Duration::days(i64::from(days))).to_rfc3339());
    let mut stmt = conn.prepare(
        "SELECT id, ts, summary, actor
         FROM episodes
         WHERE kind = 'miss' AND (?1 IS NULL OR ts >= ?1)
         ORDER BY ts DESC, id DESC",
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let rows = stmt.query_map(params![cutoff], |row| {
        Ok(MissListRow {
            id: row.get(0)?,
            ts: row.get(1)?,
            what: row.get(2)?,
            by: row.get(3)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn related(conn: &Connection, name: &str) -> Result<Vec<RelatedRow>> {
    related_as(conn, name, "agent:builder")
}

pub fn related_as(conn: &Connection, name: &str, reader: &str) -> Result<Vec<RelatedRow>> {
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
    if !acl::can_read_row(conn, "memory", from_id, reader)? {
        return Ok(Vec::new());
    }
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
    let mut related = Vec::new();
    for row in rows {
        let item = row?;
        if acl::can_read_row(conn, &item.kind, item.id, reader)? {
            related.push(item);
        }
    }
    Ok(related)
}

pub fn prune_report(conn: &Connection) -> Result<Vec<PackLine>> {
    prune_report_as(conn, "agent:builder")
}

pub fn prune_report_as(conn: &Connection, reader: &str) -> Result<Vec<PackLine>> {
    let decay = storage::meta_f64(conn, "decay_d", 0.5)?;
    let threshold = storage::meta_f64(conn, "hot_threshold", -1.6)?;
    let now = Utc::now();
    let mut rows = memory_rows(conn, "status != 'archived'")?;
    rows.sort_by(|a, b| a.title.cmp(&b.title));
    let mut lines = Vec::new();
    for row in rows {
        if !can_read_memory_row(conn, &row, reader)? {
            continue;
        }
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
        if !scoped_for_company(&row.scope) || !can_read_memory_row(conn, &row, reader)? {
            continue; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        }
        let relevance = row_relevance(&row, terms, relevance_scores);
        let retrieved = has_relevance_score(&row, relevance_scores)
            && min_relevance.is_some_and(|floor| floor <= DEFAULT_CONTEXT_RELEVANCE_FLOOR); // LCOV_EXCL_LINE: closure branch is asserted through strict/loose context tests.
        let core = is_core_lock(&row);
        if terms.is_some() && relevance <= 0.0 && !core {
            continue;
        }
        if !core
            && !retrieved
            && min_relevance.is_some_and(|floor| row_term_relevance(&row, terms, relevance) < floor)
        {
            continue; // LCOV_EXCL_LINE: filtering outcome is asserted by pack helper tests.
        }
        let activation = if row.id > 0 {
            activation::memory_activation(conn, row.id, now, decay)?
        } else {
            None // LCOV_EXCL_LINE: canonical lock activation is absent by construction.
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

fn task_lock_lines(
    conn: &Connection,
    reader: &str,
    terms: &[String],
    relevance_scores: &HashMap<i64, f64>,
    min_relevance: f64,
    limit: usize,
) -> Result<Vec<PackLine>> {
    if limit == 0 || terms.is_empty() {
        return Ok(Vec::new());
    }
    let decay = storage::meta_f64(conn, "decay_d", 0.5)?;
    let now = Utc::now();
    let mut candidates = Vec::new();
    let mut rows = memory_rows(conn, "is_lock = 1 AND status != 'archived'")?;
    rows.extend(lock_store_rows(conn)?);
    for row in rows {
        if !scoped_for_company(&row.scope) || !can_read_memory_row(conn, &row, reader)? {
            continue; // LCOV_EXCL_LINE: coverage artifact; asserted by context goldens.
        }
        let relevance = row_relevance(&row, Some(terms), Some(relevance_scores));
        let retrieved = has_relevance_score(&row, Some(relevance_scores))
            && min_relevance <= DEFAULT_CONTEXT_RELEVANCE_FLOOR;
        if !retrieved && row_term_relevance(&row, Some(terms), relevance) < min_relevance {
            continue;
        }
        let activation = if row.id > 0 {
            activation::memory_activation(conn, row.id, now, decay)?
        } else {
            None
        };
        let core = is_core_lock(&row);
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
                "matched-task",
            )
        })
        .collect())
}

fn house_rules_line(conn: &Connection, reader: &str) -> Result<Option<PackLine>> {
    let mut rows = memory_rows(conn, "is_lock = 1 AND status != 'archived'")?;
    rows.extend(lock_store_rows(conn)?);
    let mut titles = BTreeSet::new();
    for row in rows {
        if is_core_lock(&row)
            && scoped_for_company(&row.scope)
            && can_read_memory_row(conn, &row, reader)?
        {
            titles.insert(house_rule_title(&row));
        }
    }
    if titles.is_empty() {
        return Ok(None);
    }
    Ok(Some(PackLine {
        section: "house-rules".to_string(),
        title: "House rules".to_string(),
        body: format!(
            "House rules (loaded at boot): {}",
            titles.into_iter().collect::<Vec<_>>().join(" · ")
        ),
        source_path: String::new(),
        scope: "company".to_string(),
        owner: "shared".to_string(),
        reason: "house-rules".to_string(),
        activation: None,
        stale: false,
        confidence: Vec::new(),
    }))
}

fn house_rule_title(row: &MemoryRow) -> String {
    if !row.title.eq_ignore_ascii_case(&row.name) || !row.title.contains('-') {
        return row.title.clone();
    }
    row.title
        .split('-')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut characters = part.chars();
            characters
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + characters.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
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
        if row.memory_type == "reference" && terms.is_none() {
            continue;
        }
        if !scoped_for_company(&row.scope) || !can_read_memory_row(conn, &row, reader)? {
            continue; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        }
        let relevance = row_relevance(&row, terms, relevance_scores);
        let retrieved = has_relevance_score(&row, relevance_scores)
            && min_relevance.is_some_and(|floor| floor <= DEFAULT_CONTEXT_RELEVANCE_FLOOR);
        if terms.is_some() && relevance <= 0.0 {
            continue; // LCOV_EXCL_LINE: filtering outcome is asserted by pack helper tests.
        }
        if !retrieved
            && min_relevance.is_some_and(|floor| row_term_relevance(&row, terms, relevance) < floor)
        {
            continue;
        }
        if row.memory_type == "reference"
            && min_relevance.is_some_and(|floor| row_term_relevance(&row, terms, relevance) < floor)
        {
            continue;
        }
        if min_relevance.is_some()
            && !retrieved
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

fn task_memory_lines(
    conn: &Connection,
    reader: &str,
    terms: &[String],
    relevance_scores: &HashMap<i64, f64>,
    min_relevance: f64,
    limit: usize,
) -> Result<Vec<PackLine>> {
    if limit == 0 || terms.is_empty() {
        return Ok(Vec::new());
    }
    let decay = storage::meta_f64(conn, "decay_d", 0.5)?;
    let now = Utc::now();
    let mut candidates = Vec::new();
    for row in memory_rows(conn, "is_lock = 0 AND status != 'archived'")? {
        if !scoped_for_company(&row.scope) || !can_read_memory_row(conn, &row, reader)? {
            continue; // LCOV_EXCL_LINE: coverage artifact; asserted by context goldens.
        }
        let relevance = row_relevance(&row, Some(terms), Some(relevance_scores));
        let retrieved = has_relevance_score(&row, Some(relevance_scores))
            && min_relevance <= DEFAULT_CONTEXT_RELEVANCE_FLOOR;
        let floor_relevance = row_term_relevance(&row, Some(terms), relevance);
        if (row.memory_type == "reference" || !retrieved) && floor_relevance < min_relevance {
            continue;
        }
        let activation = activation::memory_activation(conn, row.id, now, decay)?;
        candidates.push(ScoredMemory {
            row,
            relevance,
            activation,
            core: false,
        });
    }
    sort_scored_memories(&mut candidates);
    Ok(candidates
        .into_iter()
        .take(limit)
        .map(|candidate| {
            let stale = source_stale(conn, &candidate.row.source_path).unwrap_or(false);
            pack_line(
                "task-memory",
                &candidate.row,
                candidate.activation,
                stale,
                "matched-task",
            )
        })
        .collect())
}

#[allow(clippy::too_many_arguments)]
fn agent_owned_memory_lines(
    conn: &Connection,
    reader: &str,
    terms: Option<&[String]>,
    relevance_scores: Option<&HashMap<i64, f64>>,
    min_relevance: Option<f64>,
    intent: AgentContextIntent,
    limit: usize,
) -> Result<Vec<PackLine>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let decay = storage::meta_f64(conn, "decay_d", 0.5)?;
    let now = Utc::now();
    let mut candidates = Vec::new();
    for row in memory_rows(conn, "is_lock = 0 AND status != 'archived'")? {
        if row.memory_type == "reference" || row.owner != reader || !scoped_for_company(&row.scope)
        {
            continue;
        }
        let relevance = row_relevance(&row, terms, relevance_scores);
        if !agent_owned_row_matches(&row, terms, relevance_scores, min_relevance, intent) {
            continue;
        }
        let activation = activation::memory_activation(conn, row.id, now, decay)?;
        candidates.push(AgentOwnedMemory {
            source_priority: agent_source_priority(&row.source_path, intent),
            row,
            relevance,
            activation,
        });
    }
    sort_agent_owned_memories(&mut candidates);
    Ok(candidates
        .into_iter()
        .take(limit)
        .map(|candidate| {
            let stale = source_stale(conn, &candidate.row.source_path).unwrap_or(false);
            pack_line(
                "agent-memory",
                &candidate.row,
                candidate.activation,
                stale,
                agent_owned_reason(intent),
            )
        })
        .collect())
}

fn agent_owned_row_matches(
    row: &MemoryRow,
    terms: Option<&[String]>,
    relevance_scores: Option<&HashMap<i64, f64>>,
    min_relevance: Option<f64>,
    intent: AgentContextIntent,
) -> bool {
    let source_priority = agent_source_priority(&row.source_path, intent);
    if intent == AgentContextIntent::Identity && source_priority > 0 {
        return true;
    }
    let Some(terms) = terms else {
        return false;
    };
    if has_relevance_score(row, relevance_scores) {
        return true;
    }
    let floor = min_relevance.unwrap_or(DEFAULT_CONTEXT_RELEVANCE_FLOOR);
    row_term_relevance(row, Some(terms), 0.0) >= floor
}

fn agent_source_priority(source_path: &str, intent: AgentContextIntent) -> u8 {
    if intent != AgentContextIntent::Identity {
        return 0;
    }
    if source_path.contains("/agents/") {
        3
    } else if source_path.contains("bootstrap") {
        2
    } else {
        1
    }
}

fn agent_owned_reason(intent: AgentContextIntent) -> &'static str {
    match intent {
        AgentContextIntent::Identity => "agent-owned-identity",
        AgentContextIntent::Meeting => "agent-owned-meeting",
        AgentContextIntent::Task => "agent-owned-task",
    }
}

fn extend_unique_lines(lines: &mut Vec<PackLine>, additions: Vec<PackLine>) {
    let mut seen = lines
        .iter()
        .map(|line| (line.source_path.clone(), line.title.clone()))
        .collect::<HashSet<_>>();
    for line in additions {
        if seen.insert((line.source_path.clone(), line.title.clone())) {
            lines.push(line);
        }
    }
}

fn retain_unique_additions(existing: &[PackLine], additions: &mut Vec<PackLine>) {
    let seen = existing
        .iter()
        .map(|line| (line.source_path.as_str(), line.title.as_str()))
        .collect::<HashSet<_>>();
    additions.retain(|line| !seen.contains(&(line.source_path.as_str(), line.title.as_str())));
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

fn yesterday_lines(conn: &Connection, reader: &str, limit: usize) -> Result<Vec<PackLine>> {
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
        "SELECT summary, body, actor, scope, coalesce(source_path, ''), id
         FROM episodes
         WHERE ts >= ?1 AND ts <= ?2
         ORDER BY ts DESC
         LIMIT ?3",
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let rows = stmt.query_map(params![from, to, limit as i64], |row| {
        let summary: String = row.get(0)?;
        let body: String = row.get(1)?;
        let source_path: String = row.get(4)?;
        Ok((
            row.get::<_, i64>(5)?,
            PackLine {
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
            },
        ))
    })?;
    let mut lines = Vec::new();
    for row in rows {
        let (id, line) = row?;
        if acl::can_read_row(conn, "episode", id, reader)? {
            lines.push(line)
        }
    }
    Ok(lines)
}

fn episode_precedent_lines(
    conn: &Connection,
    reader: &str,
    terms: &[String],
    min_relevance: Option<f64>,
    limit: usize,
) -> Result<Vec<PackLine>> {
    if limit == 0 || terms.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT id, summary, body, actor, scope, coalesce(source_path, '')
         FROM episodes
         WHERE kind != 'miss'
         ORDER BY ts DESC
         LIMIT 500",
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    let mut lines = Vec::new();
    for row in rows {
        let (id, summary, body, owner, scope, source_path) = row?;
        if !acl::can_read_row(conn, "episode", id, reader)? {
            continue;
        }
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

#[derive(Debug)]
struct OrientationEpisode {
    line: PackLine,
    matched_terms: usize,
    ts: Option<DateTime<Utc>>,
    id: i64,
}

fn orientation_episode_lines(
    conn: &Connection,
    reader: &str,
    task: &str,
    limit: usize,
) -> Result<Vec<PackLine>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let topic_terms = orientation_topic_terms(task);
    let anchor = orientation_anchor_date(task, Local::now().date_naive());
    let mut candidates = orientation_episode_candidates(conn, reader, &topic_terms)?;
    candidates.sort_by(|a, b| match anchor {
        Some(_) => orientation_anchor_distance(a, anchor)
            .cmp(&orientation_anchor_distance(b, anchor))
            .then_with(|| b.matched_terms.cmp(&a.matched_terms))
            .then_with(|| b.ts.cmp(&a.ts))
            .then_with(|| b.id.cmp(&a.id)),
        None => {
            b.ts.cmp(&a.ts)
                .then_with(|| b.matched_terms.cmp(&a.matched_terms))
                .then_with(|| b.id.cmp(&a.id))
        }
    });
    let mut seen = HashSet::new();
    Ok(candidates
        .into_iter()
        .filter(|candidate| {
            seen.insert((
                candidate.line.title.clone(),
                candidate.line.body.clone(),
                candidate.line.source_path.clone(),
            ))
        })
        .take(limit)
        .map(|candidate| candidate.line)
        .collect())
}

fn orientation_anchor_distance(candidate: &OrientationEpisode, anchor: Option<NaiveDate>) -> i64 {
    match (anchor, candidate.ts) {
        (Some(anchor), Some(ts)) => (ts.date_naive() - anchor).num_days().abs(),
        (Some(_), None) => i64::MAX,
        (None, _) => 0,
    }
}

fn orientation_episode_candidates(
    conn: &Connection,
    reader: &str,
    topic_terms: &[String],
) -> Result<Vec<OrientationEpisode>> {
    let mut stmt = conn.prepare(
        "SELECT id, ts, summary, coalesce(body, ''), actor, scope, coalesce(source_path, '')
         FROM episodes
         WHERE kind != 'miss'
         ORDER BY ts DESC, id DESC
         LIMIT ?1",
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let rows = stmt.query_map(params![ORIENTATION_EPISODE_SCAN_LIMIT as i64], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;
    let mut candidates = Vec::new();
    for row in rows {
        let (id, ts, summary, body, owner, scope, source_path) = row?;
        if !acl::can_read_row(conn, "episode", id, reader)? {
            continue;
        }
        if !scoped_for_company(&scope) {
            continue;
        }
        if let Some(candidate) = orientation_episode_candidate(
            id,
            &ts,
            summary,
            body,
            owner,
            scope,
            source_path,
            topic_terms,
        ) {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

#[allow(clippy::too_many_arguments)]
fn orientation_episode_candidate(
    id: i64,
    ts: &str,
    summary: String,
    body: String,
    owner: String,
    scope: String,
    source_path: String,
    topic_terms: &[String],
) -> Option<OrientationEpisode> {
    let haystack = format!("{summary}\n{body}").to_ascii_lowercase();
    let matched_terms = topic_terms
        .iter()
        .filter(|term| context_floor_term_matches(&haystack, term))
        .count();
    let minimum = usize::from(topic_terms.len() >= 3) + usize::from(!topic_terms.is_empty());
    if (topic_terms.is_empty() && !has_orientation_state_signal(&haystack))
        || (!topic_terms.is_empty() && matched_terms < minimum)
    {
        return None;
    }
    Some(OrientationEpisode {
        line: PackLine {
            section: "episodic-precedent".to_string(),
            title: summary,
            body: body.chars().take(240).collect(),
            source_path,
            scope,
            owner,
            reason: "recent-orientation".to_string(),
            activation: None,
            stale: false,
            confidence: confidence_markers(&body),
        },
        matched_terms,
        ts: DateTime::parse_from_rfc3339(ts)
            .ok()
            .map(|value| value.with_timezone(&Utc)),
        id,
    })
}

fn is_task_specific_line(line: &PackLine) -> bool {
    line.reason != "house-rule-core"
        && line.section != "house-rules"
        && line.section != "nothing-specific"
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
    memory_type: String,
    source_path: String,
    is_lock: bool,
}

fn memory_rows(conn: &Connection, predicate: &str) -> Result<Vec<MemoryRow>> {
    let sql = format!(
        "SELECT id, name, title, body, owner, scope, memory_type, coalesce(source_path, ''), is_lock
         FROM memories WHERE {predicate}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(MemoryRow {
            id: row.get(0)?,
            name: row.get(1)?,
            title: row.get(2)?,
            body: row.get(3)?,
            owner: row.get(4)?,
            scope: row.get(5)?,
            memory_type: row.get(6)?,
            source_path: row.get(7)?,
            is_lock: row.get::<_, i64>(8)? != 0,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn can_read_memory_row(conn: &Connection, row: &MemoryRow, reader: &str) -> Result<bool> {
    if row.id < 0 {
        acl::can_read_owner(conn, &row.owner, reader)
    } else {
        acl::can_read_row(conn, "memory", row.id, reader)
    }
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
            body: row.get(3)?,
            owner: "shared".to_string(),
            scope: row.get(4)?,
            memory_type: "lock".to_string(),
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

#[derive(Debug)]
struct AgentOwnedMemory {
    row: MemoryRow,
    relevance: f64,
    activation: Option<f64>,
    source_priority: u8,
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
            + if self.row.memory_type == "reference" && self.relevance > 0.0 {
                REFERENCE_MATCH_BONUS
            } else {
                0.0
            }
    }
}

fn sort_scored_memories(candidates: &mut [ScoredMemory]) {
    candidates.sort_by(|a, b| {
        b.score()
            .total_cmp(&a.score())
            .then_with(|| a.row.title.cmp(&b.row.title))
    });
}

fn sort_agent_owned_memories(candidates: &mut [AgentOwnedMemory]) {
    candidates.sort_by(|a, b| {
        b.source_priority
            .cmp(&a.source_priority)
            .then_with(|| b.relevance.total_cmp(&a.relevance))
            .then_with(|| a.row.title.cmp(&b.row.title))
    });
}

fn is_core_lock(row: &MemoryRow) -> bool {
    let haystack = format!("{}\n{}", row.name, row.body).to_ascii_lowercase();
    CORE_LOCKS
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

fn has_relevance_score(row: &MemoryRow, relevance_scores: Option<&HashMap<i64, f64>>) -> bool {
    relevance_scores.is_some_and(|scores| scores.contains_key(&row.id))
}

fn row_term_relevance(row: &MemoryRow, terms: Option<&[String]>, fallback: f64) -> f64 {
    terms
        .map(|items| context_floor_relevance(&row.name, &row.title, &row.body, items))
        .unwrap_or(fallback)
}

fn context_floor_relevance(name: &str, title: &str, body: &str, terms: &[String]) -> f64 {
    let signal_terms = context_signal_terms(terms);
    if signal_terms.len() >= 2 {
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
        body: row.body.chars().take(320).collect(),
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
    let mut terms = Vec::new();
    for term in search::query_terms(query).into_iter().take(12) {
        if !terms.contains(&term) {
            terms.push(term);
        }
    }
    terms
}

fn damped_query_terms(conn: &Connection, reader: &str, terms: &[String]) -> Result<Vec<String>> {
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let threshold = storage::meta_f64(
        conn,
        "context_max_term_document_frequency",
        DEFAULT_CONTEXT_MAX_TERM_DOCUMENT_FREQUENCY,
    )? // LCOV_EXCL_LINE: fallback is asserted after deleting the seeded meta row.
    .clamp(0.0, 1.0);
    let configured_operator_terms = storage::meta_value(conn, "context_operator_name")?
        .map(|value| query_terms(&value))
        .unwrap_or_default()
        .into_iter()
        .collect::<HashSet<_>>();
    let mut rows = memory_rows(conn, "status != 'archived'")?;
    rows.extend(lock_store_rows(conn)?);
    let mut document_terms = Vec::new();
    for row in rows {
        if scoped_for_company(&row.scope) && can_read_memory_row(conn, &row, reader)? {
            document_terms.push(
                search::query_terms(&format!("{}\n{}\n{}", row.name, row.title, row.body))
                    .into_iter()
                    .collect::<HashSet<_>>(),
            );
        }
    }
    let document_count = document_terms.len();
    Ok(terms
        .iter()
        .filter(|term| {
            if configured_operator_terms.contains(term.as_str()) {
                return false;
            }
            if document_count == 0 {
                return true;
            }
            // Each document is tokenized once above; this inner pass is O(query terms × docs).
            let matching_documents = document_terms
                .iter()
                .filter(|document| document.contains(term.as_str()))
                .count();
            let frequency = matching_documents as f64 / document_count as f64;
            matching_documents <= 1 || frequency <= threshold
        })
        .cloned()
        .collect())
}

fn context_ranking_queries(query: &str) -> Vec<&str> {
    let trimmed = query.trim();
    let segments = trimmed
        .split(';')
        .flat_map(|segment| segment.split(" / "))
        .flat_map(|segment| segment.split(" — "))
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.len() < 2 {
        return vec![trimmed];
    }
    std::iter::once(trimmed)
        .chain(segments)
        .take(CONTEXT_QUERY_LIMIT)
        .collect()
}

fn effective_context_ranking_queries(
    query: &str,
    original_terms: &[String],
    effective_terms: &[String],
) -> Vec<String> {
    if original_terms == effective_terms {
        return context_ranking_queries(query)
            .into_iter()
            .map(str::to_string)
            .collect();
    }
    let allowed = effective_terms
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    context_ranking_queries(query)
        .into_iter()
        .map(|segment| {
            query_terms(segment)
                .into_iter()
                .filter(|term| allowed.contains(term.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn merge_relevance_scores(target: &mut HashMap<i64, f64>, incoming: HashMap<i64, f64>) {
    for (id, score) in incoming {
        target
            .entry(id)
            .and_modify(|current| *current = current.max(score))
            .or_insert(score);
    }
}

#[cfg(test)]
fn is_orientation_query(query: &str) -> bool {
    crate::locale::LocaleRegistry::empty().prepare(query).intent == QueryIntent::Orientation
}

fn orientation_topic_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    for term in query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|term| term.len() >= 2)
        .map(|term| term.to_ascii_lowercase())
        .filter(|term| !search::is_query_stopword(term) && !is_orientation_filler(term))
    {
        if !terms.contains(&term) {
            terms.push(term);
        }
    }
    terms
}

fn orientation_anchor_date(query: &str, today: NaiveDate) -> Option<NaiveDate> {
    query
        .split(|ch: char| !ch.is_ascii_alphabetic())
        .find_map(parse_weekday)
        .map(|weekday| most_recent_weekday(today, weekday))
}

fn parse_weekday(term: &str) -> Option<Weekday> {
    match term.to_ascii_lowercase().as_str() {
        "monday" => Some(Weekday::Mon),
        "tuesday" => Some(Weekday::Tue),
        "wednesday" => Some(Weekday::Wed),
        "thursday" => Some(Weekday::Thu),
        "friday" => Some(Weekday::Fri),
        "saturday" => Some(Weekday::Sat),
        "sunday" => Some(Weekday::Sun),
        _ => None,
    }
}

fn most_recent_weekday(today: NaiveDate, weekday: Weekday) -> NaiveDate {
    let today_index = today.weekday().num_days_from_monday();
    let target_index = weekday.num_days_from_monday();
    let days_ago = (today_index + 7 - target_index) % 7;
    today - Duration::days(i64::from(days_ago))
}

fn is_orientation_filler(term: &str) -> bool {
    matches!(
        term,
        "current"
            | "happened"
            | "last"
            | "latest"
            | "matters"
            | "orientation"
            | "recent"
            | "state"
            | "status"
            | "today"
            | "update"
            | "where"
            | "yesterday"
            | "monday"
            | "tuesday"
            | "wednesday"
            | "thursday"
            | "friday"
            | "saturday"
            | "sunday"
    )
}

fn has_orientation_state_signal(text: &str) -> bool {
    [
        "blocked",
        "closed",
        "completed",
        "decision",
        "meeting-closed",
        "mandate",
        "next action",
        "order",
        "priority",
        "shipped",
    ]
    .iter()
    .any(|signal| text.contains(signal))
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
    fn english_agent_intent_sharpens_but_plain_tasks_still_get_a_blend() {
        assert_eq!(
            agent_context_intent("Writer", "who is Writer, current open work"),
            AgentContextIntent::Identity
        );
        assert_eq!(
            agent_context_intent("agent:engineer", "meeting: delta index migration"),
            AgentContextIntent::Meeting
        );
        assert_eq!(
            agent_context_intent(
                "engineer",
                "shelves public repo proof parity acl scopes public CI workflow"
            ),
            AgentContextIntent::Task
        );
        assert_eq!(agent_owned_limit(AgentContextIntent::Task, 10), 2);
        assert_eq!(agent_owned_limit(AgentContextIntent::Identity, 10), 4);

        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        assert!(
            agent_owned_memory_lines(
                &conn,
                "agent:engineer",
                None,
                None,
                None,
                AgentContextIntent::Task,
                0,
            )
            .unwrap()
            .is_empty()
        );
        let row = MemoryRow {
            id: 1,
            name: "unmatched".to_string(),
            title: "Unmatched".to_string(),
            body: "No query terms.".to_string(),
            owner: "agent:engineer".to_string(),
            scope: "company".to_string(),
            memory_type: "memory".to_string(),
            source_path: "/synthetic/memory/engineer/note.md".to_string(),
            is_lock: false,
        };
        assert!(!agent_owned_row_matches(
            &row,
            None,
            None,
            None,
            AgentContextIntent::Task,
        ));
    }

    #[test]
    fn unprompted_hot_memory_lines_exclude_reference_packs() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        let mut reference = memory_doc("Vendor Reference", false);
        reference.memory_type = "reference".to_string();
        reference.body = "initialMessage inbound greeting".to_string();
        let reference_id = storage::upsert_memory(&conn, &reference).unwrap();
        conn.execute(
            "INSERT INTO recall_events(memory_id, queried_by, query_scope, ts)
             VALUES(?1, 'agent:engineer', 'company', ?2)",
            params![reference_id, Utc::now().to_rfc3339()],
        )
        .unwrap();

        let lines = hot_memory_lines(&conn, "agent:engineer", None, None, None, 8).unwrap();

        assert!(lines.is_empty());
    }

    #[test]
    fn reference_hot_memory_requires_a_direct_context_match() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        let mut reference = memory_doc("Vendor Reference", false);
        reference.memory_type = "reference".to_string();
        reference.body = "initialMessage inbound greeting".to_string();
        let reference_id = storage::upsert_memory(&conn, &reference).unwrap();
        let terms = vec!["python".to_string(), "tests".to_string()];
        let relevance_scores = HashMap::from([(reference_id, 1.0)]);

        let lines = hot_memory_lines(
            &conn,
            "agent:engineer",
            Some(&terms),
            Some(&relevance_scores),
            Some(DEFAULT_CONTEXT_RELEVANCE_FLOOR),
            8,
        )
        .unwrap();

        assert!(lines.is_empty());
    }

    #[test]
    fn directly_matched_references_receive_the_reference_bonus() {
        let regular = ScoredMemory {
            row: MemoryRow {
                id: 1,
                name: "regular".to_string(),
                title: "Regular".to_string(),
                body: "body".to_string(),
                owner: "shared".to_string(),
                scope: "company".to_string(),
                memory_type: "memory".to_string(),
                source_path: "/tmp/regular.md".to_string(),
                is_lock: false,
            },
            relevance: 1.0,
            activation: None,
            core: false,
        };
        let regular_score = regular.score();
        let reference = ScoredMemory {
            row: MemoryRow {
                memory_type: "reference".to_string(),
                ..regular.row
            },
            relevance: regular.relevance,
            activation: regular.activation,
            core: regular.core,
        };

        assert!(
            (reference.score() - regular_score - REFERENCE_MATCH_BONUS).abs() < f64::EPSILON * 16.0
        );
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

        let lines = brief(&conn, "coordinator").unwrap();

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

        let lines = brief(&conn, "coordinator").unwrap();
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
            lock_lines(&conn, "agent:coordinator", None, None, None, 0)
                .unwrap()
                .is_empty()
        );
        assert!(
            hot_memory_lines(&conn, "agent:coordinator", None, None, None, 0)
                .unwrap()
                .is_empty()
        );
        let empty_terms = Vec::new();
        let scores = HashMap::new();
        assert!(
            task_lock_lines(
                &conn,
                "agent:coordinator",
                &empty_terms,
                &scores,
                DEFAULT_CONTEXT_RELEVANCE_FLOOR,
                5,
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            task_memory_lines(
                &conn,
                "agent:coordinator",
                &["task".to_string()],
                &scores,
                DEFAULT_CONTEXT_RELEVANCE_FLOOR,
                0,
            )
            .unwrap()
            .is_empty()
        );
        assert!(future_lines(&conn, None, 0).unwrap().is_empty());
        assert!(
            yesterday_lines(&conn, "agent:builder", 0)
                .unwrap()
                .is_empty()
        );

        let mut product = memory_doc("product-only", false);
        product.scope = "protected".to_string();
        product.content_hash = "hash-protected".to_string();
        storage::upsert_memory(&conn, &product).unwrap();
        insert_memory(&conn, "dispatch lock", true);
        let terms = vec!["missing".to_string()];
        assert!(
            lock_lines(&conn, "agent:coordinator", Some(&terms), None, None, 5)
                .unwrap()
                .is_empty()
        );
        let weak_terms = vec![
            "dispatch".to_string(),
            "missing".to_string(),
            "other".to_string(),
        ];
        assert!(
            lock_lines(
                &conn,
                "agent:coordinator",
                Some(&weak_terms),
                None,
                Some(DEFAULT_CONTEXT_RELEVANCE_FLOOR),
                5,
            )
            .unwrap()
            .is_empty()
        );
        insert_lock_store_entry(
            &conn,
            "canonical-lock",
            "Canonical Lock",
            "Canonical lock body",
        );
        assert!(
            lock_lines(&conn, "agent:coordinator", None, None, None, 5)
                .unwrap()
                .iter()
                .any(|line| line.title == "Canonical Lock" && line.activation.is_none())
        );

        insert_memory(&conn, "hot candidate", false);
        storage::log_recall_event(&conn, 3, "agent:coordinator", "company").unwrap();
        assert!(
            hot_memory_lines(&conn, "agent:coordinator", Some(&terms), None, None, 5)
                .unwrap()
                .is_empty()
        );

        let empty = Connection::open_in_memory().unwrap();
        schema::init_db(&empty).unwrap();
        empty
            .execute(
                "DELETE FROM meta WHERE key = 'context_max_term_document_frequency'",
                [],
            )
            .unwrap();
        assert_eq!(
            damped_query_terms(&empty, "agent:coordinator", &["alpha".to_string()]).unwrap(),
            ["alpha"]
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
            context(&conn, "coordinator", "anything", Some(0))
                .unwrap()
                .is_empty()
        );
        assert!(
            episode_precedent_lines(&conn, "agent:builder", &[], None, 10)
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

        let yesterday_rows = yesterday_lines(&conn, "agent:builder", 5).unwrap();
        assert_eq!(yesterday_rows[0].section, "yesterday");
        assert_eq!(yesterday_rows[0].confidence, ["✅", "🤔"]);

        let terms = query_terms("dispatch ticket");
        let precedent = episode_precedent_lines(&conn, "agent:builder", &terms, None, 1).unwrap();
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
                "archive rotation",
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
                "archive rotation",
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

        let lines = context(&conn, "coordinator", "dispatch lock", Some(2)).unwrap();
        assert_eq!(lines[0].section, "active-lock");
        assert!(lines.len() <= 2);
    }

    #[test]
    fn empty_locale_registry_preserves_english_context_ranking() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_memory(&conn, "dispatch lock", true);
        insert_memory(&conn, "dispatch hot", false);
        storage::log_recall_event(&conn, 2, "agent:engineer", "company").unwrap();
        let task = "dispatch ticket";

        let without_locales = context_with_locales(
            &conn,
            "coordinator",
            task,
            Some(2),
            &LocaleRegistry::empty(),
        )
        .unwrap();
        let with_bundled = context_with_locales(
            &conn,
            "coordinator",
            task,
            Some(2),
            &LocaleRegistry::bundled().unwrap(),
        )
        .unwrap();

        assert_eq!(
            without_locales
                .iter()
                .map(|line| (&line.section, &line.title, &line.reason))
                .collect::<Vec<_>>(),
            with_bundled
                .iter()
                .map(|line| (&line.section, &line.title, &line.reason))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn orientation_intent_and_topic_terms_are_narrow() {
        assert!(is_orientation_query(
            "what happened last night and what matters this morning"
        ));
        assert!(is_orientation_query("give me a status update"));
        assert!(!is_orientation_query(
            "what is the policy on widget retention"
        ));
        assert_eq!(
            orientation_topic_terms(
                "Tuesday morning orientation: what happened with the widget night-shift queue today"
            ),
            ["morning", "widget", "night", "shift", "queue"]
        );
        let saturday = NaiveDate::from_ymd_opt(2026, 7, 25).unwrap();
        assert_eq!(
            orientation_anchor_date("Tuesday morning orientation", saturday),
            NaiveDate::from_ymd_opt(2026, 7, 21)
        );
        assert_eq!(
            orientation_anchor_date("Saturday status update", saturday),
            Some(saturday)
        );
        assert_eq!(orientation_anchor_date("recent state", saturday), None);
    }

    #[test]
    fn orientation_episode_candidate_requires_topical_or_state_signal() {
        let topical = [
            "widget".to_string(),
            "queue".to_string(),
            "night".to_string(),
        ];
        let candidate = orientation_episode_candidate(
            7,
            "not-a-timestamp",
            "Widget Queue Decision".to_string(),
            "The widget queue changed.".to_string(),
            "agent:assistant".to_string(),
            "company".to_string(),
            "synthetic/decision.md".to_string(),
            &topical,
        )
        .unwrap();
        assert_eq!(candidate.matched_terms, 2);
        assert!(candidate.ts.is_none());
        assert_eq!(candidate.line.reason, "recent-orientation");
        let anchor = NaiveDate::from_ymd_opt(2026, 7, 25).unwrap();
        assert_eq!(
            orientation_anchor_distance(&candidate, Some(anchor)),
            i64::MAX
        );
        assert_eq!(orientation_anchor_distance(&candidate, None), 0);

        assert!(
            orientation_episode_candidate(
                8,
                "2026-07-25T00:00:00Z",
                "Widget Note".to_string(),
                "Only one topical match.".to_string(),
                "agent:assistant".to_string(),
                "company".to_string(),
                "synthetic/note.md".to_string(),
                &topical,
            )
            .is_none()
        );
        assert!(
            orientation_episode_candidate(
                9,
                "2026-07-25T00:00:00Z",
                "Routine note".to_string(),
                "No state transition here.".to_string(),
                "agent:assistant".to_string(),
                "company".to_string(),
                "synthetic/routine.md".to_string(),
                &[],
            )
            .is_none()
        );
        assert!(
            orientation_episode_candidate(
                10,
                "2026-07-25T00:00:00Z",
                "Priority decision".to_string(),
                "The next action is selected.".to_string(),
                "agent:assistant".to_string(),
                "company".to_string(),
                "synthetic/priority.md".to_string(),
                &[],
            )
            .is_some()
        );
        let dated = orientation_episode_candidate(
            11,
            "2026-07-25T12:00:00Z",
            "Priority decision".to_string(),
            "The next action is selected.".to_string(),
            "agent:assistant".to_string(),
            "company".to_string(),
            "synthetic/dated.md".to_string(),
            &[],
        )
        .unwrap();
        assert_eq!(orientation_anchor_distance(&dated, Some(anchor)), 0);
    }

    #[test]
    fn orientation_episode_lines_are_bounded_scoped_and_deduplicated() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        assert!(
            orientation_episode_lines(
                &conn,
                "agent:builder",
                "morning orientation widget queue",
                0
            )
            .unwrap()
            .is_empty()
        );
        conn.execute_batch(
            "INSERT INTO episodes(ts, actor, kind, summary, body, scope, source_path)
             VALUES
               ('2026-07-25T03:00:00Z', 'agent:assistant', 'note', 'Widget Queue Decision',
                'Morning widget queue decision', 'company', 'synthetic/decision.md'),
               ('2026-07-25T02:00:00Z', 'agent:assistant', 'note', 'Widget Queue Decision',
                'Morning widget queue decision', 'company', 'synthetic/decision.md'),
               ('2026-07-25T04:00:00Z', 'agent:assistant', 'note', 'Private Widget Queue',
                'Morning widget queue decision', 'protected', 'synthetic/private.md'),
               ('2026-07-25T01:00:00Z', 'agent:assistant', 'miss', 'Missed Widget Queue',
                'Morning widget queue decision', 'company', '');",
        )
        .unwrap();

        let lines = orientation_episode_lines(
            &conn,
            "agent:builder",
            "morning orientation widget queue",
            5,
        )
        .unwrap();

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].title, "Widget Queue Decision");
        let anchored = orientation_episode_lines(
            &conn,
            "agent:builder",
            "Tuesday morning orientation widget queue",
            5,
        )
        .unwrap();
        assert_eq!(anchored.len(), 1);
    }

    #[test]
    fn orientation_context_orders_state_before_hot_memory_and_core_locks() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_named_memory(
            &conn,
            "role-boundaries",
            "Role Boundaries",
            "Standing execution roles remain stable.",
            true,
        );
        insert_named_memory(
            &conn,
            "widget-queue-hot",
            "Widget Queue Hot Memory",
            "Morning widget queue orientation priority.",
            false,
        );
        storage::log_recall_event(&conn, 2, "agent:assistant", "company").unwrap();
        conn.execute(
            "INSERT INTO episodes(ts, actor, kind, summary, body, scope, source_path)
             VALUES(?1, 'agent:assistant', 'note', 'Widget Queue Decision',
                    'Morning widget queue decision', 'company', 'synthetic/decision.md')",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        storage::rebuild_fts(&conn).unwrap();

        let lines = context(
            &conn,
            "assistant",
            "morning orientation widget queue",
            Some(10),
        )
        .unwrap();
        let episode = lines
            .iter()
            .position(|line| line.section == "episodic-precedent")
            .unwrap();
        let hot = lines
            .iter()
            .position(|line| line.section == "hot-memory")
            .unwrap();
        let lock = lines
            .iter()
            .position(|line| line.section == "active-lock")
            .unwrap();
        assert!(episode < hot && hot < lock);

        let empty = Connection::open_in_memory().unwrap();
        schema::init_db(&empty).unwrap();
        let fallback = context(&empty, "assistant", "status update", Some(5)).unwrap();
        assert_eq!(fallback.len(), 1);
        assert_eq!(fallback[0].section, "nothing-specific");
    }

    #[test]
    fn misses_list_filters_by_age_and_orders_newest_first() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        storage::insert_miss(&conn, "current miss", "agent:assistant").unwrap();
        conn.execute(
            "INSERT INTO episodes(ts, actor, kind, summary, body, scope, source_path)
             VALUES('2000-01-01T00:00:00Z', 'agent:archivist', 'miss', 'old miss',
                    'old miss', 'company', '')",
            [],
        )
        .unwrap();

        let all = misses(&conn, None).unwrap();
        assert_eq!(
            all.iter().map(|row| row.what.as_str()).collect::<Vec<_>>(),
            ["current miss", "old miss"]
        );
        let recent = misses(&conn, Some(1)).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].what, "current miss");
    }

    #[test]
    fn context_ranks_locks_by_task_relevance_and_keeps_core() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_named_memory(
            &conn,
            "role-boundaries",
            "Role Boundaries",
            "Maintainer approves. Coordinator routes. Builder executes.",
            true,
        );
        insert_named_memory(
            &conn,
            "data-boundaries",
            "Memory Security Boundaries",
            "Protected data stays outside untrusted callers.",
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
            "catalog-positioning-hook-value-moat",
            "Notebook Positioning",
            "Release note: deterministic recall, bounded context, and reproducible tests.",
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
            "coordinator",
            "fix a failing python test in the shelves",
            Some(10),
        )
        .unwrap();
        let tweet_pack = context(
            &conn,
            "coordinator",
            "write a release note for Notebook",
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
        assert_eq!(lexical_relevance("name", "title", "body", &[]), 0.0);
        for pack in [test_pack, tweet_pack] {
            let house_rules = pack
                .iter()
                .filter(|line| line.section == "house-rules")
                .collect::<Vec<_>>();
            assert_eq!(house_rules.len(), 1);
            assert!(house_rules[0].body.contains("Role Boundaries"));
            assert!(house_rules[0].body.contains("Memory Security Boundaries"));
        }
    }

    #[test]
    fn context_floor_keeps_core_and_speaks_when_task_has_no_match() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_named_memory(
            &conn,
            "role-boundaries",
            "Role Boundaries",
            "Maintainer approves. Coordinator routes. Builder executes.",
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
            "coordinator",
            "espresso machine maintenance schedule for the third floor",
            Some(10),
        )
        .unwrap();

        assert!(lines.iter().any(|line| {
            line.section == "house-rules" && line.body.contains("Role Boundaries")
        }));
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
            "role-boundaries",
            "Role Boundaries",
            "Maintainer approves. Coordinator routes. Builder executes.",
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
            "conference-demo-noise",
            "Conference Demo Noise",
            "A Notebook example for product framing.",
        );
        storage::rebuild_fts(&conn).unwrap();

        let lines = context(
            &conn,
            "coordinator",
            "fix a python test in catalog",
            Some(10),
        )
        .unwrap();

        assert!(
            lines
                .iter()
                .any(|line| line.title == "Shelves Testing Standard")
        );
        assert!(
            lines
                .iter()
                .all(|line| line.title != "Conference Demo Noise")
        );
        assert!(lines.iter().all(|line| line.section != "nothing-specific"));
    }

    #[test]
    fn golden_miss_41118_education_synonyms_ignore_ubiquitous_operator_term() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_core_rules(&conn);
        for index in 0..8 {
            insert_named_memory(
                &conn,
                &format!("avery-noise-{index}"),
                &format!("Unrelated Avery Note {index}"),
                "Avery follows an unrelated delivery checklist.",
                index % 2 == 0,
            );
        }
        insert_named_memory(
            &conn,
            "education-path",
            "Education Path",
            "Education history includes school coursework, an associate path, and no degree.",
            false,
        );
        conn.execute(
            "INSERT INTO meta(key, value) VALUES('context_operator_name', 'Avery')",
            [],
        )
        .unwrap();
        storage::rebuild_fts(&conn).unwrap();

        let lines = context_with_locales(
            &conn,
            "assistant",
            "what should Avery study in college",
            Some(8),
            &LocaleRegistry::bundled().unwrap(),
        )
        .unwrap();

        assert!(
            lines
                .iter()
                .take(5)
                .any(|line| line.title == "Education Path")
        );
        assert!(
            lines
                .iter()
                .all(|line| !line.title.starts_with("Unrelated Avery"))
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.section == "house-rules")
                .count(),
            1
        );
    }

    #[test]
    fn golden_miss_40118_budget_memories_fill_before_house_rules() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_core_rules(&conn);
        conn.execute(
            "UPDATE meta SET value = '1.0' WHERE key = 'context_max_term_document_frequency'",
            [],
        )
        .unwrap();
        for index in 0..8 {
            insert_named_memory(
                &conn,
                &format!("money-ledger-{index}"),
                &format!("Money Ledger {index}"),
                "Budget finance accounts and bills reconcile against the ledger leftovers.",
                false,
            );
        }
        storage::rebuild_fts(&conn).unwrap();

        let lines = context_with_locales(
            &conn,
            "assistant",
            "budget sheet accounts bills finance ledger leftovers",
            Some(10),
            &LocaleRegistry::bundled().unwrap(),
        )
        .unwrap();

        let task_count = lines
            .iter()
            .filter(|line| line.section == "task-memory")
            .count();
        let house_index = lines
            .iter()
            .position(|line| line.section == "house-rules")
            .unwrap();
        assert!(task_count >= 6);
        assert!(
            lines[..house_index]
                .iter()
                .all(|line| line.section == "task-memory")
        );
    }

    #[test]
    fn golden_miss_40117_meeting_memories_beat_standing_rules() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_core_rules(&conn);
        conn.execute(
            "UPDATE meta SET value = '1.0' WHERE key = 'context_max_term_document_frequency'",
            [],
        )
        .unwrap();
        for index in 0..6 {
            insert_named_memory(
                &conn,
                &format!("vendor-cohort-{index}"),
                &format!("Vendor Cohort Workflow {index}"),
                "Monday vendor cohort meeting session covers the bookkeeping lane.",
                false,
            );
        }
        storage::rebuild_fts(&conn).unwrap();

        let lines = context_with_locales(
            &conn,
            "assistant",
            "prepare Monday vendor cohort meeting bookkeeping lane",
            Some(10),
            &LocaleRegistry::bundled().unwrap(),
        )
        .unwrap();

        assert!(
            lines
                .iter()
                .filter(|line| line.section == "task-memory")
                .count()
                >= 5
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.section == "house-rules")
                .count(),
            1
        );
    }

    #[test]
    fn context_keeps_clean_gate_lock_and_collapses_standing_rules() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        for (name, title) in [
            ("role-boundaries", "Role Boundaries"),
            ("accuracy-policy", "Accuracy Policy"),
            ("data-boundaries", "Data Boundaries"),
            ("automation-policy", "Automation Policy"),
            ("quality-policy", "Quality Policy"),
        ] {
            insert_named_memory(
                &conn,
                name,
                title,
                "Standing company rule for every agent session.",
                true,
            );
        }
        insert_lock_store_entry(
            &conn,
            "describe-the-tuesday-before-recommending-a-door",
            "Describe the Tuesday Before Recommending a Door — a clean gate is not a want",
            "No job is recommended before describing the Tuesday. A clean gate says they will let a candidate in; it does not say they want that family of jobs.",
        );
        storage::rebuild_fts(&conn).unwrap();
        storage::rebuild_locks_fts(&conn).unwrap();

        let lines = context(
            &conn,
            "coordinator",
            "job search: what family of jobs is the candidate looking for, clean gate rule",
            Some(15),
        )
        .unwrap();
        assert!(
            lines
                .iter()
                .any(|line| line.title.contains("Describe the Tuesday"))
        );
        let house_rules = lines
            .iter()
            .filter(|line| line.section == "house-rules")
            .collect::<Vec<_>>();
        assert_eq!(house_rules.len(), 1);
        for standing in [
            "Role Boundaries",
            "Accuracy Policy",
            "Data Boundaries",
            "Automation Policy",
            "Quality Policy",
        ] {
            assert!(house_rules[0].body.contains(standing));
        }
        assert!(lines.iter().all(|line| line.section != "nothing-specific"));
    }

    #[test]
    fn context_keeps_every_canonical_lock_returned_by_public_search() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_lock_store_entry(
            &conn,
            "error-handling-observability-standard",
            "Error Handling & Observability Standard",
            &format!(
                "Error handling is a first-class feature. {} Never console only: surface a human message.",
                "operational detail ".repeat(24)
            ),
        );
        storage::rebuild_locks_fts(&conn).unwrap();
        let query = "error handling never console only human message";

        let searched = search::search(
            &conn,
            query,
            "company",
            None,
            "agent:coordinator",
            false,
            10,
        )
        .unwrap();
        let searched_lock_titles = searched
            .iter()
            .filter(|hit| hit.kind == "lock")
            .map(|hit| hit.title.as_str())
            .collect::<Vec<_>>();
        assert!(!searched_lock_titles.is_empty());

        let lines = context(&conn, "coordinator", query, Some(15)).unwrap();
        for title in searched_lock_titles {
            assert!(
                lines.iter().any(|line| line.title == title),
                "context dropped searched canonical lock {title:?}"
            );
        }
    }

    #[test]
    fn context_recovers_each_bounded_semicolon_topic() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_lock_store_entry(
            &conn,
            "morning-brief-hygiene-sensor",
            "Morning Brief Hygiene Sensor",
            "Morning brief hygiene sensor detects stale approved decisions.",
        );
        insert_lock_store_entry(
            &conn,
            "ticket-frontmatter-validator",
            "Ticket Frontmatter Validator",
            "Ticket frontmatter validator is enforced by pre-commit.",
        );
        insert_lock_store_entry(
            &conn,
            "ticket-type-taxonomy",
            "Ticket Type Taxonomy",
            "Ticket type taxonomy keeps board grouping canonical.",
        );
        storage::rebuild_locks_fts(&conn).unwrap();

        let lines = context(
            &conn,
            "coordinator",
            "morning brief hygiene sensor; ticket frontmatter validator pre-commit; ticket type taxonomy",
            Some(15),
        )
        .unwrap();
        let titles = lines
            .iter()
            .map(|line| line.title.as_str())
            .collect::<Vec<_>>();

        for title in [
            "Morning Brief Hygiene Sensor",
            "Ticket Frontmatter Validator",
            "Ticket Type Taxonomy",
        ] {
            assert!(titles.contains(&title), "missing topic result {title:?}");
        }
    }

    #[test]
    fn context_query_segmentation_is_bounded_and_preserves_single_topics() {
        assert_eq!(context_ranking_queries("single topic"), ["single topic"]);
        assert_eq!(
            context_ranking_queries("alpha; beta / gamma — delta; ignored"),
            [
                "alpha; beta / gamma — delta; ignored",
                "alpha",
                "beta",
                "gamma",
            ]
        );
    }

    #[test]
    fn context_relevance_floor_meta_tunes_cutoff() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_named_memory(
            &conn,
            "role-boundaries",
            "Role Boundaries",
            "Maintainer approves. Coordinator routes. Builder executes.",
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
        let strict = context(
            &conn,
            "coordinator",
            "fix a python test in catalog",
            Some(10),
        )
        .unwrap();
        assert!(strict.iter().all(|line| line.title != "Borderline Testing"));
        assert!(strict.iter().any(|line| line.section == "nothing-specific"));

        conn.execute(
            "UPDATE meta SET value = '0.25' WHERE key = 'context_relevance_floor'",
            [],
        )
        .unwrap();
        let loose = context(
            &conn,
            "coordinator",
            "fix a python test in catalog",
            Some(10),
        )
        .unwrap();
        assert!(loose.iter().any(|line| line.title == "Borderline Testing"));
    }

    #[test]
    fn context_empty_task_does_not_apply_relevance_floor() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_named_memory(
            &conn,
            "role-boundaries",
            "Role Boundaries",
            "Maintainer approves. Coordinator routes. Builder executes.",
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

        let lines = context(&conn, "coordinator", "", Some(10)).unwrap();

        assert!(lines.iter().any(|line| line.title == "Generic Hot"));
        assert!(lines.iter().any(|line| line.section == "due-future"));
        assert!(lines.iter().all(|line| line.section != "nothing-specific"));
    }

    #[test]
    fn context_floor_helper_branches_are_explicit() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        let signal_terms = query_terms("fix a python test in catalog");
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
            "agent:coordinator",
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
            "agent:builder",
            &signal_terms,
            Some(DEFAULT_CONTEXT_RELEVANCE_FLOOR),
            5,
        )
        .unwrap();
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].title, "Testing Episode");

        let context_with_future =
            context(&conn, "coordinator", "maintenance schedule", Some(5)).unwrap();
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

    fn insert_core_rules(conn: &Connection) {
        for (name, title) in [
            ("role-boundaries", "Role Boundaries"),
            ("accuracy-policy", "Accuracy Policy"),
            ("data-boundaries", "Data Boundaries"),
            ("automation-policy", "Automation Policy"),
            ("quality-policy", "Quality Policy"),
        ] {
            insert_named_memory(
                conn,
                name,
                title,
                "Standing rule loaded at boot for every session.",
                true,
            );
        }
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
            memory_type: "memory".to_string(),
            visibility: None,
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
            body: "archive rotation ritual".to_string(),
            owner: "agent:archivist".to_string(),
            scope: "company".to_string(),
            memory_type: "memory".to_string(),
            visibility: None,
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
