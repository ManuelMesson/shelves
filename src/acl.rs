use anyhow::{Result, bail};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};

pub fn can_read_owner(conn: &Connection, owner_node: &str, reader: &str) -> Result<bool> {
    if owner_node == "shared" || owner_node == reader {
        return Ok(true);
    }
    let explicit: Option<i64> = conn
        .query_row(
            "SELECT granted FROM node_acl WHERE owner_node = ?1 AND reader = ?2",
            params![owner_node, reader],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(granted) = explicit {
        return Ok(granted != 0);
    }

    let wildcard: Option<i64> = conn
        .query_row(
            "SELECT granted FROM node_acl WHERE owner_node = ?1 AND reader = '*'",
            params![owner_node],
            |row| row.get(0),
        )
        .optional()?;
    Ok(wildcard.map(|granted| granted != 0).unwrap_or(true))
}

/// The only access decision for indexed memory and episode rows. Callers must
/// pass a row identity, never infer access from a returned title or owner.
pub fn can_read_row(conn: &Connection, kind: &str, id: i64, reader: &str) -> Result<bool> {
    let table = match kind {
        "memory" => "memories",
        "episode" => "episodes",
        _ => bail!("unsupported ACL row kind {kind:?}"),
    };
    let owner_column = if kind == "memory" { "owner" } else { "actor" };
    let sql = format!("SELECT {owner_column}, visibility FROM {table} WHERE id=?1");
    let row: Option<(String, Option<String>)> = conn
        .query_row(&sql, params![id], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    let Some((owner, visibility)) = row else {
        return Ok(false);
    };
    let is_private = match visibility.as_deref() {
        Some("shared") => false,
        Some(_) => true,
        None => default_visibility_private(conn)?,
    };
    if owner == reader {
        return Ok(true);
    }
    let row_grant: Option<i64> = conn.query_row(
        "SELECT include_private FROM row_acl WHERE row_kind=?1 AND row_id=?2 AND reader=?3 AND owner_node=?4",
        params![kind, id, reader, owner], |row| row.get(0),
    ).optional()?;
    if !is_private {
        return Ok(row_grant.is_some() || can_read_owner(conn, &owner, reader)?);
    }
    if row_grant == Some(1) {
        return Ok(true);
    }
    let owner_grant: Option<(i64, i64)> = conn
        .query_row(
            "SELECT granted, include_private FROM node_acl WHERE owner_node=?1 AND reader=?2",
            params![owner, reader],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(owner_grant == Some((1, 1)))
}

pub fn private_row_withheld(conn: &Connection, kind: &str, id: i64, reader: &str) -> Result<bool> {
    Ok(row_is_private(conn, kind, id)? && !can_read_row(conn, kind, id, reader)?)
}

pub fn row_is_private(conn: &Connection, kind: &str, id: i64) -> Result<bool> {
    let table = match kind {
        "memory" => "memories",
        "episode" => "episodes",
        _ => bail!("unsupported ACL row kind {kind:?}"),
    };
    let visibility: Option<Option<String>> = conn
        .query_row(
            &format!("SELECT visibility FROM {table} WHERE id=?1"),
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    let is_private = match visibility {
        None => false,
        Some(Some(ref value)) if value == "shared" => false,
        Some(Some(_)) => true,
        Some(None) => default_visibility_private(conn)?,
    };
    Ok(is_private)
}

fn default_visibility_private(conn: &Connection) -> Result<bool> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key='default_visibility'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match value.as_deref() {
        None | Some("shared") => Ok(false),
        Some("private") => Ok(true),
        Some(other) => {
            bail!("invalid default_visibility meta value {other:?}; expected shared or private")
        }
    }
}

// The CLI's complete grant intent crosses this boundary as one audited transaction.
#[allow(clippy::too_many_arguments)]
pub fn change_grant(
    conn: &Connection,
    owner: &str,
    reader: &str,
    row: Option<i64>,
    kind: &str,
    include_private: bool,
    revoke: bool,
    actor: &str,
) -> Result<()> {
    if owner == reader {
        bail!("owner already has access")
    }
    conn.execute_batch("SAVEPOINT grant_change")?;
    let result = (|| -> Result<()> {
        if let Some(id) = row {
            if kind != "memory" && kind != "episode" {
                bail!("row kind must be memory or episode")
            }
            let table = if kind == "memory" {
                "memories"
            } else {
                "episodes"
            };
            let owner_column = if kind == "memory" { "owner" } else { "actor" };
            let actual: Option<String> = conn
                .query_row(
                    &format!("SELECT {owner_column} FROM {table} WHERE id=?1"),
                    params![id],
                    |r| r.get(0),
                )
                .optional()?;
            if actual.as_deref() != Some(owner) {
                bail!("row not found for owner")
            }
            if revoke {
                conn.execute("DELETE FROM row_acl WHERE row_kind=?1 AND row_id=?2 AND reader=?3 AND owner_node=?4",
                params![kind, id, reader, owner])?;
            } else {
                conn.execute("INSERT INTO row_acl(row_kind,row_id,owner_node,reader,include_private)
                VALUES(?1,?2,?3,?4,?5)
                ON CONFLICT(row_kind,row_id,reader) DO UPDATE SET owner_node=excluded.owner_node, include_private=excluded.include_private",
                params![kind,id,owner,reader,include_private])?;
            }
        } else {
            conn.execute("INSERT INTO node_acl(owner_node,reader,granted,include_private)
            VALUES(?1,?2,?3,?4)
            ON CONFLICT(owner_node,reader) DO UPDATE SET granted=excluded.granted, include_private=excluded.include_private",
            params![owner, reader, !revoke, include_private && !revoke])?;
        }
        conn.execute(
        "INSERT INTO grant_audit(ts,actor,action,owner_node,reader,row_kind,row_id,include_private)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            Utc::now().to_rfc3339(),
            actor,
            if revoke { "revoke" } else { "grant" },
            owner,
            reader,
            row.map(|_| kind),
            row,
            include_private
        ],
    )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            conn.execute_batch("RELEASE grant_change")?;
            Ok(())
        }
        Err(error) => {
            conn.execute_batch("ROLLBACK TO grant_change; RELEASE grant_change")?;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;

    #[test]
    fn acl_is_default_open_and_explicit_revoke_wins() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        assert!(can_read_owner(&conn, "agent:archivist", "agent:engineer").unwrap());

        conn.execute(
            "INSERT INTO node_acl(owner_node, reader, granted) VALUES('agent:archivist', 'agent:engineer', 0)",
            [],
        )
        .unwrap();
        assert!(!can_read_owner(&conn, "agent:archivist", "agent:engineer").unwrap());
    }

    #[test]
    fn wildcard_acl_grant_is_used_when_reader_has_no_explicit_row() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        conn.execute(
            "INSERT INTO node_acl(owner_node, reader, granted) VALUES('agent:archivist', '*', 0)",
            [],
        )
        .unwrap();

        assert!(!can_read_owner(&conn, "agent:archivist", "agent:engineer").unwrap());
        assert!(can_read_owner(&conn, "shared", "agent:engineer").unwrap());
        assert!(can_read_owner(&conn, "agent:engineer", "agent:engineer").unwrap());
    }

    #[test]
    fn explicit_reader_rows_override_wildcard_for_grants_and_revokes() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        conn.execute(
            "INSERT INTO node_acl(owner_node, reader, granted) VALUES('agent:archivist', '*', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO node_acl(owner_node, reader, granted) VALUES('agent:archivist', 'agent:reviewer', 1)",
            [],
        )
        .unwrap();

        assert!(can_read_owner(&conn, "agent:archivist", "agent:reviewer").unwrap());
        assert!(!can_read_owner(&conn, "agent:archivist", "agent:visitor").unwrap());

        conn.execute(
            "UPDATE node_acl SET granted = 1 WHERE owner_node = 'agent:archivist' AND reader = '*'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE node_acl SET granted = 0 WHERE owner_node = 'agent:archivist' AND reader = 'agent:reviewer'",
            [],
        )
        .unwrap();

        assert!(!can_read_owner(&conn, "agent:archivist", "agent:reviewer").unwrap());
        assert!(can_read_owner(&conn, "agent:archivist", "agent:visitor").unwrap());
    }

    #[test]
    fn caller_identity_is_an_unverified_string_and_default_open() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();

        assert!(
            can_read_owner(
                &conn,
                "agent:archivist",
                "agent:caller-supplied-without-authentication"
            )
            .unwrap()
        );
    }

    #[test]
    fn private_memory_requires_explicit_private_grant_and_audit_is_append_only() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        conn.execute(
            "INSERT INTO memories(name,title,body,owner,scope,visibility,content_hash,created_at,updated_at)
             VALUES('m','M','secret','agent:a','company','private','h','2026-01-01','2026-01-01')",
            [],
        ).unwrap();
        let id = conn.last_insert_rowid();
        assert!(can_read_row(&conn, "memory", id, "agent:a").unwrap());
        assert!(!can_read_row(&conn, "memory", id, "agent:b").unwrap());
        assert!(private_row_withheld(&conn, "memory", id, "agent:b").unwrap());
        change_grant(
            &conn, "agent:a", "agent:b", None, "memory", false, false, "agent:a",
        )
        .unwrap();
        assert!(!can_read_row(&conn, "memory", id, "agent:b").unwrap());
        change_grant(
            &conn,
            "agent:a",
            "agent:b",
            Some(id),
            "memory",
            false,
            false,
            "agent:a",
        )
        .unwrap();
        assert!(!can_read_row(&conn, "memory", id, "agent:b").unwrap());
        change_grant(
            &conn,
            "agent:a",
            "agent:b",
            Some(id),
            "memory",
            true,
            false,
            "agent:a",
        )
        .unwrap();
        assert!(can_read_row(&conn, "memory", id, "agent:b").unwrap());
        change_grant(
            &conn,
            "agent:a",
            "agent:b",
            Some(id),
            "memory",
            false,
            true,
            "agent:a",
        )
        .unwrap();
        assert!(!can_read_row(&conn, "memory", id, "agent:b").unwrap());
        change_grant(
            &conn, "agent:a", "agent:b", None, "memory", true, false, "agent:a",
        )
        .unwrap();
        assert!(can_read_row(&conn, "memory", id, "agent:b").unwrap());
        change_grant(
            &conn, "agent:a", "agent:b", None, "memory", false, true, "agent:a",
        )
        .unwrap();
        assert!(!can_read_row(&conn, "memory", id, "agent:b").unwrap());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM grant_audit", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 6);
        assert!(
            change_grant(
                &conn,
                "agent:c",
                "agent:b",
                Some(id),
                "memory",
                true,
                false,
                "agent:c"
            )
            .is_err()
        );
        assert!(!can_read_row(&conn, "memory", 999, "agent:b").unwrap());
        assert!(can_read_row(&conn, "invalid", id, "agent:b").is_err());
    }

    #[test]
    fn unmarked_rows_follow_configured_default_without_resetting_configuration() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        conn.execute(
            "INSERT INTO episodes(ts,actor,kind,summary,body,scope) VALUES('2026-01-01','agent:a','note','note','body','company')",
            [],
        ).unwrap();
        let id = conn.last_insert_rowid();
        assert!(can_read_row(&conn, "episode", id, "agent:b").unwrap());
        conn.execute(
            "UPDATE meta SET value='private' WHERE key='default_visibility'",
            [],
        )
        .unwrap();
        schema::init_db(&conn).unwrap();
        assert!(!can_read_row(&conn, "episode", id, "agent:b").unwrap());
        assert!(private_row_withheld(&conn, "episode", id, "agent:b").unwrap());
        assert!(can_read_row(&conn, "episode", id, "agent:a").unwrap());
    }

    #[test]
    fn failed_audit_rolls_back_grant_change() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        conn.execute("DROP TABLE grant_audit", []).unwrap();
        assert!(
            change_grant(
                &conn, "agent:a", "agent:b", None, "memory", true, false, "agent:a"
            )
            .is_err()
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM node_acl", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn invalid_grants_and_visibility_fail_closed() {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_db(&conn).unwrap();
        conn.execute(
            "INSERT INTO episodes(ts,actor,kind,summary,body,scope,visibility)
            VALUES('2026-01-01','agent:a','note','note','body','company','private')",
            [],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        assert!(!private_row_withheld(&conn, "episode", 999, "agent:b").unwrap());
        assert!(private_row_withheld(&conn, "other", id, "agent:b").is_err());
        assert!(
            change_grant(
                &conn, "agent:a", "agent:a", None, "episode", true, false, "agent:a"
            )
            .is_err()
        );
        assert!(
            change_grant(
                &conn,
                "agent:a",
                "agent:b",
                Some(id),
                "other",
                true,
                false,
                "agent:a"
            )
            .is_err()
        );
        change_grant(
            &conn,
            "agent:a",
            "agent:b",
            Some(id),
            "episode",
            true,
            false,
            "agent:a",
        )
        .unwrap();
        assert!(can_read_row(&conn, "episode", id, "agent:b").unwrap());
        conn.execute(
            "UPDATE episodes SET visibility=NULL WHERE id=?1",
            params![id],
        )
        .unwrap();
        conn.execute(
            "UPDATE meta SET value='invalid' WHERE key='default_visibility'",
            [],
        )
        .unwrap();
        assert!(can_read_row(&conn, "episode", id, "agent:b").is_err());
    }
}
