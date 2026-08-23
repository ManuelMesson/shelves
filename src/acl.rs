use anyhow::Result;
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
}
