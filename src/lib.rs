#![cfg_attr(not(test), warn(clippy::unwrap_used, clippy::expect_used))]

pub mod acl;
pub mod activation;
pub mod answer;
pub mod cli;
pub mod graduation;
pub mod guard;
pub mod ingest;
pub mod locks;
pub mod parser;
pub mod schema;
pub mod search;
pub mod storage;

pub const CONTRACT_VERSION: &str = "1.0";
pub const SCHEMA_VERSION: &str = "1";

#[cfg(test)]
pub(crate) fn test_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbPathSource {
    CliFlag,
    EnvOverride,
    AiosRoot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbPathResolution {
    pub path: std::path::PathBuf,
    pub source: DbPathSource,
}

pub fn workspace_root() -> anyhow::Result<std::path::PathBuf> {
    let root = std::env::var("AIOS_ROOT").map_err(|_| {
        anyhow::anyhow!("AIOS_ROOT is required; run through scripts/shelves or set AIOS_ROOT")
    })?;
    if root.trim().is_empty() {
        anyhow::bail!("AIOS_ROOT is required and must not be empty");
    }
    Ok(std::path::PathBuf::from(root))
}

pub fn db_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(resolve_db_path(None)?.path)
}

pub fn resolve_db_path(cli_path: Option<&std::path::Path>) -> anyhow::Result<DbPathResolution> {
    if let Some(path) = cli_path {
        return Ok(DbPathResolution {
            path: path.to_path_buf(),
            source: DbPathSource::CliFlag,
        });
    }
    if let Some(path) = std::env::var_os("SHELVES_DB_PATH") {
        if path.is_empty() {
            anyhow::bail!("SHELVES_DB_PATH is set but empty");
        }
        return Ok(DbPathResolution {
            path: std::path::PathBuf::from(path),
            source: DbPathSource::EnvOverride,
        });
    }
    if let Some(root) = std::env::var_os("AIOS_ROOT") {
        if root.is_empty() {
            anyhow::bail!("AIOS_ROOT is set but empty");
        }
        return Ok(DbPathResolution {
            path: std::path::PathBuf::from(root).join("system/shelves.db"),
            source: DbPathSource::AiosRoot,
        });
    }
    anyhow::bail!(
        "SHELVES_DB_PATH or AIOS_ROOT is required; run through scripts/shelves or set one explicitly"
    )
}

pub fn is_default_db_path(db: &DbPathResolution) -> anyhow::Result<bool> {
    if db.source == DbPathSource::CliFlag {
        return Ok(false);
    }
    Ok(db.path == workspace_root()?.join("system/shelves.db"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn db_path_precedence_is_flag_env_root_no_fallback() {
        let _lock = env_lock().lock().unwrap();
        let old_db = std::env::var("SHELVES_DB_PATH").ok();
        let old_root = std::env::var("AIOS_ROOT").ok();

        unsafe {
            std::env::remove_var("SHELVES_DB_PATH");
            std::env::remove_var("AIOS_ROOT");
        }
        let err = resolve_db_path(None).expect_err("missing env must fail loud");
        assert!(err.to_string().contains("SHELVES_DB_PATH or AIOS_ROOT"));

        unsafe { std::env::set_var("AIOS_ROOT", "/tmp/shelves-root") };
        let rooted = resolve_db_path(None).unwrap();
        assert_eq!(rooted.source, DbPathSource::AiosRoot);
        assert_eq!(
            rooted.path,
            std::path::PathBuf::from("/tmp/shelves-root/system/shelves.db")
        );
        assert!(is_default_db_path(&rooted).unwrap());

        unsafe { std::env::set_var("SHELVES_DB_PATH", "/tmp/shelves-explicit.db") };
        let env = resolve_db_path(None).unwrap();
        assert_eq!(env.source, DbPathSource::EnvOverride);
        assert_eq!(
            env.path,
            std::path::PathBuf::from("/tmp/shelves-explicit.db")
        );
        assert!(!is_default_db_path(&env).unwrap());

        let flag = resolve_db_path(Some(std::path::Path::new("/tmp/shelves-flag.db"))).unwrap();
        assert_eq!(flag.source, DbPathSource::CliFlag);
        assert_eq!(flag.path, std::path::PathBuf::from("/tmp/shelves-flag.db"));
        assert!(!is_default_db_path(&flag).unwrap());

        restore_env("SHELVES_DB_PATH", old_db);
        restore_env("AIOS_ROOT", old_root);
    }

    #[test]
    fn env_required_helpers_report_empty_and_missing_roots() {
        let _lock = env_lock().lock().unwrap();
        let old_db = std::env::var("SHELVES_DB_PATH").ok();
        let old_root = std::env::var("AIOS_ROOT").ok();

        unsafe {
            std::env::remove_var("SHELVES_DB_PATH");
            std::env::remove_var("AIOS_ROOT");
        }
        let missing_root = workspace_root().expect_err("missing AIOS_ROOT must fail");
        assert!(missing_root.to_string().contains("AIOS_ROOT is required"));

        unsafe { std::env::set_var("AIOS_ROOT", "  ") };
        let empty_root = workspace_root().expect_err("blank AIOS_ROOT must fail");
        assert!(empty_root.to_string().contains("must not be empty"));

        unsafe {
            std::env::set_var("AIOS_ROOT", "");
            std::env::remove_var("SHELVES_DB_PATH");
        }
        let empty_root_path = resolve_db_path(None).expect_err("empty AIOS_ROOT must fail");
        assert!(
            empty_root_path
                .to_string()
                .contains("AIOS_ROOT is set but empty")
        );

        unsafe {
            std::env::remove_var("AIOS_ROOT");
            std::env::set_var("SHELVES_DB_PATH", "");
        }
        let empty_db = resolve_db_path(None).expect_err("empty SHELVES_DB_PATH must fail");
        assert!(
            empty_db
                .to_string()
                .contains("SHELVES_DB_PATH is set but empty")
        );

        unsafe {
            std::env::remove_var("SHELVES_DB_PATH");
            std::env::set_var("AIOS_ROOT", "/tmp/shelves-root");
        }
        assert_eq!(
            db_path().unwrap(),
            std::path::PathBuf::from("/tmp/shelves-root/system/shelves.db")
        );

        restore_env("SHELVES_DB_PATH", old_db);
        restore_env("AIOS_ROOT", old_root);
    }

    fn env_lock() -> &'static Mutex<()> {
        crate::test_env_lock()
    }

    fn restore_env(key: &str, old: Option<String>) {
        match old {
            Some(value) => unsafe { std::env::set_var(key, value) }, // LCOV_EXCL_LINE: cleanup branch depends on caller's ambient env.
            None => unsafe { std::env::remove_var(key) },
        }
    }
}
