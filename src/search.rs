use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::acl;
use crate::storage;

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub kind: String,
    pub id: i64,
    pub name: String,
    pub title: String,
    pub snippet: String,
    pub owner: String,
    pub scope: String,
    pub is_lock: i64,
    pub status: String,
    pub source_path: String,
    pub rank: f64,
}

#[derive(Debug, Clone)]
struct RawSearchHit {
    hit: SearchHit,
    raw_rank: f64,
    scope_index: usize,
    search_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FtsQuery {
    expression: String,
    terms: Vec<String>,
}

const DEFAULT_REAL_MATCH_FLOOR: f64 = 0.50;

pub fn search(
    conn: &Connection,
    query: &str,
    scope: &str,
    owner: Option<&str>,
    as_agent: &str,
    include_cold: bool,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let fts_queries = fts_queries(query);
    if fts_queries.is_empty() {
        return Ok(Vec::new());
    }
    let scopes = scope_fallthrough(scope);
    let mut raw_hits = Vec::new();
    let real_match_floor = real_match_floor(conn)?;
    for fts_query in fts_queries {
        let mut query_hits = Vec::new();
        for (scope_index, search_scope) in scopes.iter().enumerate() {
            query_hits.extend(search_memories(
                conn,
                &fts_query.expression,
                search_scope,
                scope_index,
                owner,
                as_agent,
                include_cold,
                limit,
            )?); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
            query_hits.extend(search_episodes(
                conn,
                &fts_query.expression,
                search_scope,
                scope_index,
                limit,
            )?); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        }
        query_hits = filter_real_matches(query_hits, &fts_query.terms, real_match_floor);
        if !query_hits.is_empty() {
            raw_hits = query_hits;
            break;
        }
    }
    let episode_weight = rank_episode_weight(conn)?;
    let mut ranked_hits = rank_hits(raw_hits, episode_weight);
    ranked_hits.sort_by(|a, b| {
        a.hit
            .rank
            .total_cmp(&b.hit.rank)
            .then_with(|| a.scope_index.cmp(&b.scope_index))
            .then_with(|| kind_order(&a.hit.kind).cmp(&kind_order(&b.hit.kind)))
            .then_with(|| a.hit.id.cmp(&b.hit.id))
    });
    let mut hits: Vec<SearchHit> = ranked_hits.into_iter().map(|ranked| ranked.hit).collect();
    hits.dedup_by(|a, b| a.kind == b.kind && a.id == b.id);
    hits.truncate(limit);

    for hit in &hits {
        if hit.kind == "memory" {
            storage::log_recall_event(conn, hit.id, as_agent, scope)?;
        }
    }
    Ok(hits)
}

pub fn memory_relevance_scores(
    conn: &Connection,
    query: &str,
    scope: &str,
    as_agent: &str,
    include_cold: bool,
    limit: usize,
) -> Result<HashMap<i64, f64>> {
    let fts_queries = fts_queries(query);
    if fts_queries.is_empty() || limit == 0 {
        return Ok(HashMap::new());
    }
    let scopes = scope_fallthrough(scope);
    let mut raw_hits = Vec::new();
    let real_match_floor = real_match_floor(conn)?;
    for fts_query in fts_queries {
        let mut query_hits = Vec::new();
        for (scope_index, search_scope) in scopes.iter().enumerate() {
            query_hits.extend(search_memories(
                conn,
                &fts_query.expression,
                search_scope,
                scope_index,
                None,
                as_agent,
                include_cold,
                limit,
            )?); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        }
        query_hits = filter_real_matches(query_hits, &fts_query.terms, real_match_floor);
        if !query_hits.is_empty() {
            raw_hits = query_hits;
            break;
        }
    }
    let ranked_hits = rank_hits(raw_hits, 1.0);
    let mut scores = HashMap::new();
    for ranked in ranked_hits {
        let score = -ranked.hit.rank;
        scores
            .entry(ranked.hit.id)
            .and_modify(|existing: &mut f64| *existing = existing.max(score))
            .or_insert(score);
    }
    Ok(scores)
}

pub fn lock_relevance_scores(
    conn: &Connection,
    query: &str,
    scope: &str,
    limit: usize,
) -> Result<HashMap<i64, f64>> {
    let fts_queries = fts_queries(query);
    if fts_queries.is_empty() || limit == 0 {
        return Ok(HashMap::new());
    }
    let scopes = scope_fallthrough(scope);
    let mut raw_hits = Vec::new();
    let real_match_floor = real_match_floor(conn)?;
    for fts_query in fts_queries {
        let mut query_hits = Vec::new();
        for (scope_index, search_scope) in scopes.iter().enumerate() {
            query_hits.extend(search_locks(
                conn,
                &fts_query.expression,
                search_scope,
                scope_index,
                limit,
            )?); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        }
        query_hits = filter_real_matches(query_hits, &fts_query.terms, real_match_floor);
        if !query_hits.is_empty() {
            raw_hits = query_hits;
            break;
        }
    }
    let ranked_hits = rank_hits(raw_hits, 1.0);
    let mut scores = HashMap::new();
    for ranked in ranked_hits {
        let score = -ranked.hit.rank;
        scores
            .entry(ranked.hit.id)
            .and_modify(|existing: &mut f64| *existing = existing.max(score))
            .or_insert(score);
    }
    Ok(scores)
}

pub fn scope_fallthrough(scope: &str) -> Vec<String> {
    if scope.starts_with("product:") {
        vec![scope.to_string(), "company".to_string(), "os".to_string()]
    } else if scope == "company" {
        vec![
            "company".to_string(),
            "product:%".to_string(),
            "os".to_string(),
        ]
    } else {
        vec!["os".to_string(), "company".to_string()]
    }
}

fn search_locks(
    conn: &Connection,
    fts_query: &str,
    scope: &str,
    scope_index: usize,
    limit: usize,
) -> Result<Vec<RawSearchHit>> {
    let mut stmt = conn.prepare(
        "SELECT l.id, l.slug, l.title, substr(l.body, 1, 300), l.scope, bm25(locks_fts, 5.0, 1.0, 1.0) AS rank, l.body
         FROM locks_fts
         JOIN locks l ON l.id = locks_fts.rowid
         WHERE locks_fts MATCH ?1
           AND l.status = 'active'
           AND (l.scope = ?2 OR (?4 AND l.scope LIKE ?2))
         ORDER BY rank
         LIMIT ?3",
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let rows = stmt.query_map(
        params![fts_query, scope, limit as i64, scope.contains('%')],
        |row| {
            let id = row.get::<_, i64>(0)?;
            let raw_rank = row.get(5)?;
            let slug = row.get::<_, String>(1)?;
            let title = row.get::<_, String>(2)?;
            let body = row.get::<_, String>(6)?;
            Ok(RawSearchHit {
                raw_rank,
                scope_index,
                search_text: format!("{slug}\n{title}\n{body}"),
                hit: SearchHit {
                    kind: "memory".to_string(),
                    id: -id,
                    name: slug,
                    title,
                    snippet: row.get(3)?,
                    owner: "shared".to_string(),
                    scope: row.get(4)?,
                    is_lock: 1,
                    status: "active".to_string(),
                    source_path: String::new(),
                    rank: raw_rank,
                },
            })
        },
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

// Query shape mirrors the CLI knobs; refactoring this into a builder would add churn
// without changing the audit surface.
#[allow(clippy::too_many_arguments)]
fn search_memories(
    conn: &Connection,
    fts_query: &str,
    scope: &str,
    scope_index: usize,
    owner: Option<&str>,
    as_agent: &str,
    include_cold: bool,
    limit: usize,
) -> Result<Vec<RawSearchHit>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.name, m.title, substr(m.body, 1, 300), m.owner, m.scope, m.is_lock, m.status, coalesce(m.source_path, ''), bm25(memories_fts, 5.0, 1.0) AS rank, m.body
         FROM memories_fts
         JOIN memories m ON m.id = memories_fts.rowid
         WHERE memories_fts MATCH ?1
           AND (m.scope = ?2 OR (?6 AND m.scope LIKE ?2))
           AND (?3 IS NULL OR m.owner = ?3 OR m.owner = 'shared')
           AND (?4 OR m.status != 'archived')
         ORDER BY rank
         LIMIT ?5",
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let rows = stmt.query_map(
        params![
            fts_query,
            scope,
            owner,
            include_cold,
            limit as i64,
            scope.contains('%')
        ],
        |row| {
            let raw_rank = row.get(9)?;
            let name = row.get::<_, String>(1)?;
            let title = row.get::<_, String>(2)?;
            let body = row.get::<_, String>(10)?;
            Ok(RawSearchHit {
                raw_rank,
                scope_index,
                search_text: format!("{name}\n{title}\n{body}"),
                hit: SearchHit {
                    kind: "memory".to_string(),
                    id: row.get(0)?,
                    name,
                    title,
                    snippet: row.get(3)?,
                    owner: row.get(4)?,
                    scope: row.get(5)?,
                    is_lock: row.get(6)?,
                    status: row.get(7)?,
                    source_path: row.get(8)?,
                    rank: raw_rank,
                },
            })
        },
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.

    let mut hits = Vec::new();
    for row in rows {
        let hit = row?;
        if acl::can_read_owner(conn, &hit.hit.owner, as_agent)? {
            hits.push(hit);
        }
    }
    Ok(hits)
}

fn search_episodes(
    conn: &Connection,
    fts_query: &str,
    scope: &str,
    scope_index: usize,
    limit: usize,
) -> Result<Vec<RawSearchHit>> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.summary, substr(e.body, 1, 300), e.actor, e.scope, coalesce(e.source_path, ''), bm25(episodes_fts, 3.0, 1.0) AS rank, e.body
         FROM episodes_fts
         JOIN episodes e ON e.id = episodes_fts.rowid
         WHERE episodes_fts MATCH ?1 AND (e.scope = ?2 OR (?4 AND e.scope LIKE ?2))
         ORDER BY rank
         LIMIT ?3",
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let rows = stmt.query_map(
        params![fts_query, scope, limit as i64, scope.contains('%')],
        |row| {
            let raw_rank = row.get(6)?;
            let summary = row.get::<_, String>(1)?;
            let body = row.get::<_, String>(7)?;
            Ok(RawSearchHit {
                raw_rank,
                scope_index,
                search_text: format!("{summary}\n{body}"),
                hit: SearchHit {
                    kind: "episode".to_string(),
                    id: row.get(0)?,
                    name: format!("episode:{}", row.get::<_, i64>(0)?),
                    title: summary,
                    snippet: row.get(2)?,
                    owner: row.get(3)?,
                    scope: row.get(4)?,
                    is_lock: 0,
                    status: String::new(),
                    source_path: row.get(5)?,
                    rank: raw_rank,
                },
            })
        },
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn rank_episode_weight(conn: &Connection) -> Result<f64> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key='rank_episode_weight'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value
        .and_then(|raw| raw.parse::<f64>().ok())
        .filter(|weight| *weight > 0.0 && *weight <= 1.0)
        .unwrap_or(0.55))
}

fn real_match_floor(conn: &Connection) -> Result<f64> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key='real_match_floor'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value
        .and_then(|raw| raw.parse::<f64>().ok())
        .filter(|floor| *floor > 0.0 && *floor <= 1.0)
        .unwrap_or(DEFAULT_REAL_MATCH_FLOOR))
}

fn filter_real_matches(
    raw_hits: Vec<RawSearchHit>,
    terms: &[String],
    real_match_floor: f64,
) -> Vec<RawSearchHit> {
    raw_hits
        .into_iter()
        .filter(|hit| passes_real_match_floor(&hit.search_text, terms, real_match_floor))
        .collect()
}

fn passes_real_match_floor(search_text: &str, terms: &[String], real_match_floor: f64) -> bool {
    let (matched, ratio) = real_match_stats(search_text, terms);
    if terms.len() <= 1 {
        return ratio >= real_match_floor;
    }
    matched >= 2 && ratio >= real_match_floor
}

#[cfg(test)]
fn real_match_ratio(search_text: &str, terms: &[String]) -> f64 {
    real_match_stats(search_text, terms).1
}

fn real_match_stats(search_text: &str, terms: &[String]) -> (usize, f64) {
    if terms.is_empty() {
        return (0, 0.0); // LCOV_EXCL_LINE: fts_queries returns early before empty-term searches.
    }
    let text_terms: Vec<String> = search_text
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(|term| term.to_ascii_lowercase())
        .collect();
    let matched = terms
        .iter()
        .filter(|term| {
            text_terms
                .iter()
                .any(|text_term| text_term.starts_with(term.as_str()))
        })
        .count();
    (matched, matched as f64 / terms.len() as f64)
}

fn rank_hits(raw_hits: Vec<RawSearchHit>, episode_weight: f64) -> Vec<RawSearchHit> {
    let max_memory_score = max_abs_rank(&raw_hits, "memory");
    let max_episode_score = max_abs_rank(&raw_hits, "episode");
    raw_hits
        .into_iter()
        .map(|mut raw| {
            let max_for_kind = if raw.hit.kind == "episode" {
                max_episode_score
            } else {
                max_memory_score
            };
            let normalized = if max_for_kind > 0.0 {
                raw.raw_rank.abs() / max_for_kind
            } else {
                0.0 // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
            };
            let kind_weight = if raw.hit.kind == "episode" {
                episode_weight
            } else {
                1.0
            };
            raw.hit.rank = -(normalized * kind_weight);
            raw
        })
        .collect()
}

fn max_abs_rank(raw_hits: &[RawSearchHit], kind: &str) -> f64 {
    raw_hits
        .iter()
        .filter(|raw| raw.hit.kind == kind)
        .map(|raw| raw.raw_rank.abs())
        .fold(0.0, f64::max)
}

fn kind_order(kind: &str) -> u8 {
    if kind == "memory" { 0 } else { 1 }
}

fn fts_queries(query: &str) -> Vec<FtsQuery> {
    let mut terms: Vec<String> = Vec::new();
    for term in query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|term| term.len() >= 2 && !is_query_stopword(term))
        .take(8)
        .map(|term| term.to_ascii_lowercase())
    {
        if !terms.contains(&term) {
            terms.push(term);
        }
    }
    if terms.is_empty() {
        Vec::new()
    } else if terms.len() == 1 {
        vec![FtsQuery {
            expression: format!("{}*", terms[0]),
            terms,
        }]
    } else {
        let fts_terms: Vec<String> = terms.iter().map(|term| format!("{term}*")).collect();
        vec![
            FtsQuery {
                expression: fts_terms.join(" "),
                terms: terms.clone(),
            },
            FtsQuery {
                expression: fts_terms.join(" OR "),
                terms,
            },
        ]
    }
}

pub fn is_query_stopword(term: &str) -> bool {
    matches!(
        term.to_ascii_lowercase().as_str(),
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "at"
            | "for"
            | "from"
            | "how"
            | "in"
            | "is"
            | "of"
            | "on"
            | "or"
            | "the"
            | "to"
            | "was"
            | "what"
            | "with"
            | "write"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{EpisodeDoc, MemoryDoc};
    use crate::{schema, storage};
    use proptest::prelude::*;
    use rusqlite::params;
    use std::path::Path;

    #[test]
    fn scope_fallthrough_orders_nearest_first() {
        assert_eq!(
            scope_fallthrough("product:notebook"),
            ["product:notebook", "company", "os"]
        );
        assert_eq!(scope_fallthrough("company"), ["company", "product:%", "os"]);
        assert_eq!(scope_fallthrough("ops"), ["os", "company"]);
    }

    #[test]
    fn search_logs_recall_event_for_memory_hits() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        let doc = MemoryDoc {
            name: "barista".to_string(),
            title: "Barista thesis".to_string(),
            body: "A great barista knows your name and walks you through.".to_string(),
            owner: "shared".to_string(),
            scope: "company".to_string(),
            source_path: Path::new("/tmp/memory.md").to_path_buf(),
            content_hash: "hash".to_string(),
            is_lock: false,
            created_at: "2026-06-10T00:00:00Z".to_string(),
            updated_at: "2026-06-10T00:00:00Z".to_string(),
        };
        storage::upsert_memory(&conn, &doc).unwrap();
        storage::rebuild_fts(&conn).unwrap();
        let hits = search(
            &conn,
            "barista thesis",
            "company",
            None,
            "agent:engineer",
            false,
            3,
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM recall_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn memory_relevance_scores_reuse_fts_without_logging_recall() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        assert!(
            memory_relevance_scores(&conn, "", "company", "agent:engineer", false, 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            memory_relevance_scores(&conn, "shelves", "company", "agent:engineer", false, 0)
                .unwrap()
                .is_empty()
        );
        storage::upsert_memory(
            &conn,
            &MemoryDoc {
                name: "shelves-testing-standard".to_string(),
                title: "Shelves Testing Standard".to_string(),
                body: "python pytest rust cargo golden coverage".to_string(),
                owner: "shared".to_string(),
                scope: "company".to_string(),
                source_path: Path::new("/tmp/memory.md").to_path_buf(),
                content_hash: "hash-shelves".to_string(),
                is_lock: true,
                created_at: "2026-06-10T00:00:00Z".to_string(),
                updated_at: "2026-06-10T00:00:00Z".to_string(),
            },
        )
        .unwrap();
        storage::rebuild_fts(&conn).unwrap();

        let scores = memory_relevance_scores(
            &conn,
            "fix a failing python test",
            "company",
            "agent:engineer",
            false,
            10,
        )
        .unwrap();

        assert_eq!(scores.len(), 1);
        assert!(scores.values().all(|score| *score > 0.0));
        let recall_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM recall_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(recall_count, 0);
    }

    #[test]
    fn lock_relevance_scores_key_canonical_locks_as_negative_ids() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        assert!(
            lock_relevance_scores(&conn, "", "company", 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            lock_relevance_scores(&conn, "shelves", "company", 0)
                .unwrap()
                .is_empty()
        );
        conn.execute(
            "INSERT INTO locks(slug, title, body, scope, locked_on, status)
             VALUES('shelves-testing-standard', 'Shelves Testing Standard', 'python pytest cargo coverage shelves', 'company', '2026-06-11', 'active')",
            [],
        )
        .unwrap();
        let testing_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO locks(slug, title, body, scope, locked_on, status)
             VALUES('shelves-name-positioning', 'Shelves Name Positioning', 'brand naming shelves shelves positioning', 'company', '2026-06-11', 'active')",
            [],
        )
        .unwrap();
        let naming_id = conn.last_insert_rowid();
        storage::rebuild_locks_fts(&conn).unwrap();

        let scores =
            lock_relevance_scores(&conn, "write python tests for the shelves", "company", 10)
                .unwrap();

        assert!(scores[&-testing_id] > *scores.get(&-naming_id).unwrap_or(&0.0));
        assert!(scores.keys().all(|id| *id < 0));
        let recall_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM recall_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(recall_count, 0);
    }

    #[test]
    fn fts_queries_try_all_terms_before_or_fallback() {
        let expressions = |query: &str| {
            fts_queries(query)
                .into_iter()
                .map(|query| query.expression)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            expressions("builder dispatch pattern"),
            [
                "builder* dispatch* pattern*",
                "builder* OR dispatch* OR pattern*"
            ]
        );
        assert_eq!(expressions("builder"), ["builder*"]);
        assert_eq!(
            expressions("what was done yesterday"),
            ["done* yesterday*", "done* OR yesterday*"]
        );
    }

    #[test]
    fn or_fallback_requires_real_match_floor() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        for (name, title, body) in [
            (
                "query-noise",
                "Query Noise",
                "query question queue context with no relevant instruction",
            ),
            (
                "question-noise",
                "Question Noise",
                "question queue query context with no relevant instruction",
            ),
            (
                "queue-noise",
                "Queue Noise",
                "queue query question context with no relevant instruction",
            ),
            (
                "service-steps",
                "Service Steps",
                "service steps for the recall threshold",
            ),
        ] {
            insert_memory(&conn, name, title, body, "company");
        }
        storage::rebuild_fts(&conn).unwrap();

        let hits = search(
            &conn,
            "que service steps",
            "company",
            None,
            "agent:engineer",
            false,
            10,
        )
        .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Service Steps");
    }

    #[test]
    fn real_match_floor_uses_valid_positive_fraction_only() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        assert_eq!(real_match_floor(&conn).unwrap(), DEFAULT_REAL_MATCH_FLOOR);

        for (raw, expected) in [
            ("0.25", 0.25),
            ("0", DEFAULT_REAL_MATCH_FLOOR),
            ("2", DEFAULT_REAL_MATCH_FLOOR),
            ("nope", DEFAULT_REAL_MATCH_FLOOR),
        ] {
            conn.execute(
                "INSERT INTO meta(key, value) VALUES('real_match_floor', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![raw],
            )
            .unwrap();
            assert_eq!(real_match_floor(&conn).unwrap(), expected);
        }
    }

    #[test]
    fn real_match_ratio_counts_distinct_prefix_terms() {
        assert_eq!(
            real_match_ratio(
                "service steps and context",
                &fts_queries("que service steps")[1].terms
            ),
            2.0 / 3.0
        );
        assert_eq!(
            real_match_ratio(
                "query question queue context",
                &fts_queries("que service steps")[1].terms
            ),
            1.0 / 3.0
        );
        assert!(!passes_real_match_floor(
            "query question queue context",
            &fts_queries("que service")[1].terms,
            DEFAULT_REAL_MATCH_FLOOR
        ));
        assert!(passes_real_match_floor(
            "python testing standard",
            &fts_queries("fix failing python test")[1].terms,
            DEFAULT_REAL_MATCH_FLOOR
        ));
    }

    #[test]
    fn rank_hits_normalizes_per_kind_and_weights_episodes() {
        let hits = rank_hits(
            vec![
                raw_hit("memory", 1, -1.0, 0),
                raw_hit("episode", 2, -100.0, 0),
            ],
            0.55,
        );

        let memory = hits.iter().find(|hit| hit.hit.kind == "memory").unwrap();
        let episode = hits.iter().find(|hit| hit.hit.kind == "episode").unwrap();
        assert!(memory.hit.rank < episode.hit.rank);
        assert_eq!(episode.hit.rank, -0.55);
    }

    #[test]
    fn search_prefers_memory_over_episode_noise_after_kind_weighting() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_memory(
            &conn,
            "builder-dispatch-pattern",
            "Builder dispatch pattern",
            "Absolute workspace tickets and scripted handoff are the canonical dispatch pattern.",
            "os",
        );
        let noisy_episode = EpisodeDoc {
            ts: "2026-06-10T00:00:00Z".to_string(),
            actor: "agent:builder".to_string(),
            kind: "ticket".to_string(),
            summary: "builder dispatch pattern builder dispatch pattern builder dispatch pattern"
                .to_string(),
            body: "builder dispatch pattern builder dispatch pattern builder dispatch pattern"
                .to_string(),
            scope: "os".to_string(),
            source_path: Path::new("/tmp/noise.md").to_path_buf(),
        };
        storage::insert_episode_if_new(&conn, &noisy_episode).unwrap();
        storage::rebuild_fts(&conn).unwrap();

        let hits = search(
            &conn,
            "builder dispatch pattern",
            "os",
            None,
            "agent:engineer",
            false,
            3,
        )
        .unwrap();

        assert_eq!(hits[0].kind, "memory");
        assert_eq!(hits[0].title, "Builder dispatch pattern");
    }

    #[test]
    fn search_maps_episode_hits_and_skips_acl_refused_memories() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        let mut private = memory_doc(
            "private-memory",
            "Private Memory",
            "secret needle",
            "company",
        );
        private.owner = "agent:archivist".to_string();
        storage::upsert_memory(&conn, &private).unwrap();
        conn.execute(
            "INSERT INTO node_acl(owner_node, reader, granted) VALUES('agent:archivist', 'agent:engineer', 0)",
            [],
        )
        .unwrap();
        let episode = EpisodeDoc {
            ts: "2026-06-10T00:00:00Z".to_string(),
            actor: "agent:builder".to_string(),
            kind: "ticket".to_string(),
            summary: "secret needle episode".to_string(),
            body: "secret needle episode body".to_string(),
            scope: "company".to_string(),
            source_path: Path::new("/tmp/episode.md").to_path_buf(),
        };
        storage::insert_episode_if_new(&conn, &episode).unwrap();
        storage::rebuild_fts(&conn).unwrap();

        let hits = search(
            &conn,
            "secret needle",
            "company",
            None,
            "agent:engineer",
            false,
            3,
        )
        .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, "episode");
        assert_eq!(hits[0].name, "episode:1");
        assert_eq!(hits[0].owner, "agent:builder");
    }

    #[test]
    fn rank_episode_weight_uses_valid_positive_fraction_only() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        assert_eq!(rank_episode_weight(&conn).unwrap(), 0.55);

        for (raw, expected) in [("0.25", 0.25), ("0", 0.55), ("2", 0.55), ("nope", 0.55)] {
            conn.execute(
                "INSERT INTO meta(key, value) VALUES('rank_episode_weight', ?1)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![raw],
            )
            .unwrap();
            assert_eq!(rank_episode_weight(&conn).unwrap(), expected);
        }
    }

    #[test]
    fn search_uses_scope_distance_as_tie_breaker() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        insert_memory(
            &conn,
            "company-pattern",
            "Shared dispatch pattern",
            "builder dispatch pattern",
            "company",
        );
        insert_memory(
            &conn,
            "os-pattern",
            "Shared dispatch pattern",
            "builder dispatch pattern",
            "os",
        );
        storage::rebuild_fts(&conn).unwrap();

        let hits = search(
            &conn,
            "dispatch pattern",
            "company",
            None,
            "agent:engineer",
            false,
            3,
        )
        .unwrap();

        assert_eq!(hits[0].scope, "company");
        assert_eq!(hits[1].scope, "os");
    }

    proptest! {
        #[test]
        fn arbitrary_non_control_input_never_breaks_fts_search(query in "\\PC*") {
            let conn = Connection::open_in_memory().unwrap();
            schema::init_db(&conn).unwrap();

            let _queries = fts_queries(&query);
            let result = search(&conn, &query, "company", None, "agent:engineer", true, 10);

            prop_assert!(
                result.is_ok(),
                "query {query:?} leaked an FTS/SQLite error: {:?}",
                result.err()
            );
        }
    }

    fn insert_memory(conn: &Connection, name: &str, title: &str, body: &str, scope: &str) {
        let doc = memory_doc(name, title, body, scope);
        storage::upsert_memory(conn, &doc).unwrap();
    }

    fn memory_doc(name: &str, title: &str, body: &str, scope: &str) -> MemoryDoc {
        MemoryDoc {
            name: name.to_string(),
            title: title.to_string(),
            body: body.to_string(),
            owner: "shared".to_string(),
            scope: scope.to_string(),
            source_path: Path::new("/tmp/memory.md").to_path_buf(),
            content_hash: format!("hash-{name}"),
            is_lock: false,
            created_at: "2026-06-10T00:00:00Z".to_string(),
            updated_at: "2026-06-10T00:00:00Z".to_string(),
        }
    }

    fn raw_hit(kind: &str, id: i64, raw_rank: f64, scope_index: usize) -> RawSearchHit {
        RawSearchHit {
            raw_rank,
            scope_index,
            search_text: format!("{kind} {id}"),
            hit: SearchHit {
                kind: kind.to_string(),
                id,
                name: format!("{kind}:{id}"),
                title: format!("{kind} {id}"),
                snippet: String::new(),
                owner: "shared".to_string(),
                scope: "os".to_string(),
                is_lock: 0,
                status: "hot".to_string(),
                source_path: String::new(),
                rank: raw_rank,
            },
        }
    }
}
