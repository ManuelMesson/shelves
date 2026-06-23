use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

pub fn base_level_activation(ages_days: &[f64], decay: f64) -> Option<f64> {
    if ages_days.is_empty() {
        return None;
    }
    let total: f64 = ages_days
        .iter()
        .copied()
        .filter(|age| *age > 0.0)
        .map(|age| age.powf(-decay))
        .sum();
    (total > 0.0).then(|| total.ln())
}

pub fn memory_activation(
    conn: &Connection,
    memory_id: i64,
    now: DateTime<Utc>,
    decay: f64,
) -> Result<Option<f64>> {
    let mut stmt = conn.prepare("SELECT ts FROM recall_events WHERE memory_id = ?1")?;
    let rows = stmt.query_map(params![memory_id], |row| row.get::<_, String>(0))?;
    let mut ages = Vec::new();
    for row in rows {
        if let Ok(ts) = DateTime::parse_from_rfc3339(&row?) {
            let age_seconds = now
                .signed_duration_since(ts.with_timezone(&Utc))
                .num_seconds()
                .max(1) as f64;
            ages.push(age_seconds / 86_400.0);
        }
    }
    Ok(base_level_activation(&ages, decay))
}

pub fn is_hot(is_lock: bool, activation: Option<f64>, threshold: f64) -> bool {
    is_lock || activation.map(|value| value >= threshold).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;
    use rusqlite::Connection;

    #[test]
    fn act_r_decay_matches_hand_calculated_values() {
        // d = 0.5.
        // [1] day: ln(1^-0.5) = ln(1) = 0.
        assert!((base_level_activation(&[1.0], 0.5).unwrap() - 0.0).abs() < 1e-12);

        // [4] days: ln(4^-0.5) = ln(1/2) = -0.6931471805599453.
        let four_days = base_level_activation(&[4.0], 0.5).unwrap();
        assert!((four_days + std::f64::consts::LN_2).abs() < 1e-12);

        // [1, 4] days: ln(1^-0.5 + 4^-0.5) = ln(1 + 0.5) = ln(1.5).
        let mixed = base_level_activation(&[1.0, 4.0], 0.5).unwrap();
        assert!((mixed - 1.5_f64.ln()).abs() < 1e-12);
    }

    #[test]
    fn hot_threshold_treats_locks_as_exempt() {
        assert!(is_hot(true, Some(-10.0), -1.6));
        assert!(is_hot(false, Some(-1.5), -1.6));
        assert!(!is_hot(false, Some(-1.7), -1.6));
        assert!(!is_hot(false, None, -1.6));
    }

    #[test]
    fn activation_reads_recall_events() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        conn.execute(
            "INSERT INTO memories(name, title, body, owner, scope, source_path, content_hash, created_at, updated_at)
             VALUES('m', 'Memory', 'body', 'shared', 'company', '/tmp/m.md', 'h', '2026-06-01T00:00:00Z', '2026-06-01T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO recall_events(memory_id, queried_by, query_scope, ts) VALUES(1, 'system:ingest', 'company', '2026-06-10T00:00:00Z')",
            [],
        )
        .unwrap();

        let now = DateTime::parse_from_rfc3339("2026-06-11T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let activation = memory_activation(&conn, 1, now, 0.5).unwrap().unwrap();
        assert!((activation - 0.0).abs() < 1e-12);
    }
}
