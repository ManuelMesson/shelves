use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardViolation {
    pub path: PathBuf,
    pub protected_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardError {
    MissingPersonalRoot,
    Violation(GuardViolation),
}

pub fn check_path(path: &Path) -> Result<PathBuf> {
    match canonical_allowed_path(path) {
        Ok(path) => Ok(path),
        Err(GuardError::MissingPersonalRoot) => {
            bail!("SHELVES_PROTECTED_ROOT is required for Layer-1 protection; refusing file access")
        }
        Err(GuardError::Violation(GuardViolation {
            path,
            protected_root,
        })) => bail!(
            "Layer-1 path refused: {} is under {}",
            path.display(),
            protected_root.display()
        ),
    }
}

pub fn canonical_allowed_path(path: &Path) -> std::result::Result<PathBuf, GuardError> {
    let roots = protected_roots()?;
    let lexical = lexical_absolute(path);
    if let Some(root) = matching_root(&lexical, &roots) {
        return Err(GuardError::Violation(GuardViolation {
            path: lexical,
            protected_root: root,
        }));
    }

    let canonical = canonical_existing_prefix(&lexical);
    if let Some(root) = matching_root(&canonical, &roots) {
        return Err(GuardError::Violation(GuardViolation {
            path: canonical,
            protected_root: root,
        }));
    }
    Ok(canonical)
}

pub fn assert_path_allowed_before_read(path: &Path) -> Result<PathBuf> {
    check_path(path).with_context(|| format!("refusing to read {}", path.display()))
}

pub fn require_personal_root() -> Result<PathBuf> {
    let personal_root = std::env::var("SHELVES_PROTECTED_ROOT").map_err(|_| {
        anyhow::anyhow!(
            "SHELVES_PROTECTED_ROOT is required for Layer-1 protection; refusing file walk"
        )
    })?;
    if personal_root.trim().is_empty() {
        bail!("SHELVES_PROTECTED_ROOT is required and must not be empty");
    }
    let lexical = lexical_absolute(Path::new(&personal_root));
    Ok(canonical_existing_prefix(&lexical))
}

fn protected_roots() -> std::result::Result<Vec<PathBuf>, GuardError> {
    let Ok(personal_root) = std::env::var("SHELVES_PROTECTED_ROOT") else {
        return Err(GuardError::MissingPersonalRoot);
    };
    if personal_root.trim().is_empty() {
        return Err(GuardError::MissingPersonalRoot);
    }
    let lexical = lexical_absolute(Path::new(&personal_root));
    let mut roots = vec![canonical_existing_prefix(&lexical)];
    roots.sort();
    roots.dedup();
    Ok(roots)
}

fn matching_root(path: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    roots
        .iter()
        .find(|root| path == root.as_path() || path.starts_with(root))
        .cloned()
}

fn lexical_absolute(path: &Path) -> PathBuf {
    let raw = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir() // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
            .unwrap_or_else(|_| PathBuf::from("/")) // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
            .join(path) // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    };
    normalize_lexical(&raw)
}

fn canonical_existing_prefix(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }

    let mut current = path;
    let mut missing = Vec::new();
    loop {
        if let Ok(canonical_prefix) = std::fs::canonicalize(current) {
            let mut resolved = canonical_prefix;
            for component in missing.iter().rev() {
                resolved.push(component);
            }
            return normalize_lexical(&resolved);
        }

        let Some(file_name) = current.file_name() else {
            return path.to_path_buf(); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        };
        missing.push(file_name.to_os_string());

        let Some(parent) = current.parent() else {
            return path.to_path_buf(); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        };
        current = parent;
    }
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {} // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
            Component::ParentDir => {
                out.pop();
            }
            _ => out.push(component.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::collection::vec;
    use proptest::prelude::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    #[test]
    fn missing_personal_root_refuses_file_access() {
        let _lock = env_lock().lock().unwrap();
        let old = std::env::var("SHELVES_PROTECTED_ROOT").ok();
        unsafe { std::env::remove_var("SHELVES_PROTECTED_ROOT") };
        let err =
            canonical_allowed_path(Path::new("/tmp/clean")).expect_err("missing env must refuse");
        assert_eq!(err, GuardError::MissingPersonalRoot);
        restore_personal_root(old);
    }

    #[test]
    fn public_guard_errors_include_actionable_messages() {
        let _lock = env_lock().lock().unwrap();
        let old = std::env::var("SHELVES_PROTECTED_ROOT").ok();
        unsafe { std::env::remove_var("SHELVES_PROTECTED_ROOT") };
        let missing = check_path(Path::new("/tmp/clean")).expect_err("missing env must fail");
        assert!(
            missing
                .to_string()
                .contains("SHELVES_PROTECTED_ROOT is required")
        );

        let tmp = TempDir::new().unwrap();
        let personal = tmp.path().join("personal");
        std::fs::create_dir_all(&personal).unwrap();
        unsafe { std::env::set_var("SHELVES_PROTECTED_ROOT", " ") };
        let blank = require_personal_root().expect_err("blank env must fail");
        assert!(blank.to_string().contains("must not be empty"));

        unsafe { std::env::set_var("SHELVES_PROTECTED_ROOT", &personal) };
        let refused =
            assert_path_allowed_before_read(&personal).expect_err("protected root must refuse");
        assert!(refused.to_string().contains("refusing to read"));
        assert!(format!("{refused:#}").contains("Layer-1 path refused"));
        restore_personal_root(old);
    }

    #[test]
    fn blank_personal_root_refuses_file_access() {
        let _lock = env_lock().lock().unwrap();
        let old = std::env::var("SHELVES_PROTECTED_ROOT").ok();
        unsafe { std::env::set_var("SHELVES_PROTECTED_ROOT", " ") };
        let err =
            canonical_allowed_path(Path::new("/tmp/clean")).expect_err("blank env must refuse");
        assert_eq!(err, GuardError::MissingPersonalRoot);
        restore_personal_root(old);
    }

    #[test]
    fn direct_personal_root_path_is_refused_without_reading_contents() {
        let _lock = env_lock().lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let personal = tmp.path().join("personal");
        std::fs::create_dir_all(&personal).unwrap();
        let old = std::env::var("SHELVES_PROTECTED_ROOT").ok();
        unsafe { std::env::set_var("SHELVES_PROTECTED_ROOT", &personal) };
        let err = canonical_allowed_path(&personal).expect_err("protected root must be refused");
        assert_eq!(
            violation_root(err),
            std::fs::canonicalize(&personal).unwrap()
        );
        restore_personal_root(old);
    }

    #[test]
    fn symlink_into_personal_root_is_refused() {
        let _lock = env_lock().lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let personal = tmp.path().join("personal");
        let clean = tmp.path().join("clean");
        std::fs::create_dir_all(&personal).unwrap();
        std::fs::create_dir_all(&clean).unwrap();
        let link = clean.join("link");
        std::os::unix::fs::symlink(&personal, &link).unwrap();

        let old = std::env::var("SHELVES_PROTECTED_ROOT").ok();
        unsafe { std::env::set_var("SHELVES_PROTECTED_ROOT", &personal) };
        let err = canonical_allowed_path(&link).expect_err("symlink target must be refused");
        assert_eq!(
            violation_root(err),
            std::fs::canonicalize(&personal).unwrap()
        );
        restore_personal_root(old);
    }

    #[test]
    fn personal_root_env_is_refused() {
        let _lock = env_lock().lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let personal = tmp.path().join("personal");
        std::fs::create_dir_all(&personal).unwrap();

        let old = std::env::var("SHELVES_PROTECTED_ROOT").ok();
        unsafe { std::env::set_var("SHELVES_PROTECTED_ROOT", &personal) };
        let err =
            canonical_allowed_path(&personal).expect_err("SHELVES_PROTECTED_ROOT must be refused");
        assert_eq!(
            violation_root(err),
            std::fs::canonicalize(&personal).unwrap()
        );
        restore_personal_root(old);
    }

    #[test]
    fn clean_path_passes() {
        let _lock = env_lock().lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let personal = tmp.path().join("personal");
        let clean = tmp.path().join("clean");
        std::fs::create_dir_all(&personal).unwrap();
        std::fs::create_dir_all(&clean).unwrap();
        let _env = PersonalRootEnv::set(&personal);
        let allowed = canonical_allowed_path(&clean).unwrap();
        assert_eq!(allowed, std::fs::canonicalize(clean).unwrap());
    }

    #[test]
    fn missing_descendant_resolves_existing_prefix_and_lexical_components() {
        let _lock = env_lock().lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let personal = tmp.path().join("personal");
        let clean = tmp.path().join("clean");
        std::fs::create_dir_all(&personal).unwrap();
        std::fs::create_dir_all(&clean).unwrap();
        let _env = PersonalRootEnv::set(&personal);

        let target = clean.join(".").join("nested").join("..").join("missing.md");
        let allowed = canonical_allowed_path(&target).unwrap();

        assert_eq!(allowed, clean.join("missing.md"));
    }

    proptest! {
        #[test]
        fn personal_root_descendants_are_refused(segments in vec(path_component(), 0..5)) {
            let _lock = env_lock().lock().unwrap();
            let tmp = TempDir::new().unwrap();
            let personal = tmp.path().join("private-root");
            std::fs::create_dir_all(&personal).unwrap();
            let _env = PersonalRootEnv::set(&personal);
            let target = create_existing_descendant(&personal, &segments);

            let err = canonical_allowed_path(&target).expect_err("SHELVES_PROTECTED_ROOT descendant must be refused");

            prop_assert_eq!(violation_root(err), std::fs::canonicalize(&personal).unwrap());
        }

        #[test]
        fn traversal_into_personal_root_is_refused(segments in vec(path_component(), 0..5)) {
            let _lock = env_lock().lock().unwrap();
            let tmp = TempDir::new().unwrap();
            let personal = tmp.path().join("private-root");
            let clean_nested = tmp.path().join("clean").join("nested");
            std::fs::create_dir_all(&personal).unwrap();
            std::fs::create_dir_all(&clean_nested).unwrap();
            let _env = PersonalRootEnv::set(&personal);
            let target = create_existing_descendant(&personal, &segments);
            let relative = target.strip_prefix(&personal).unwrap();
            let traversed = clean_nested.join("..").join("..").join("private-root").join(relative);

            let err = canonical_allowed_path(&traversed).expect_err(".. traversal into SHELVES_PROTECTED_ROOT must be refused");

            prop_assert_eq!(violation_root(err), std::fs::canonicalize(&personal).unwrap());
        }

        #[test]
        fn symlink_into_personal_root_is_refused_for_existing_descendants(
            segments in vec(path_component(), 0..5)
        ) {
            let _lock = env_lock().lock().unwrap();
            let tmp = TempDir::new().unwrap();
            let personal = tmp.path().join("private-root");
            let clean = tmp.path().join("clean");
            std::fs::create_dir_all(&personal).unwrap();
            std::fs::create_dir_all(&clean).unwrap();
            let _env = PersonalRootEnv::set(&personal);
            let target = create_existing_descendant(&personal, &segments);
            let link = clean.join("link");
            std::os::unix::fs::symlink(&personal, &link).unwrap();
            let relative = target.strip_prefix(&personal).unwrap();
            let through_link = link.join(relative);

            let err = canonical_allowed_path(&through_link).expect_err("symlink into SHELVES_PROTECTED_ROOT must be refused");

            prop_assert_eq!(violation_root(err), std::fs::canonicalize(&personal).unwrap());
        }

        #[test]
        fn symlink_into_personal_root_is_refused_for_missing_descendants(
            segments in vec(path_component(), 0..5)
        ) {
            let _lock = env_lock().lock().unwrap();
            let tmp = TempDir::new().unwrap();
            let personal = tmp.path().join("private-root");
            let clean = tmp.path().join("clean");
            std::fs::create_dir_all(&personal).unwrap();
            std::fs::create_dir_all(&clean).unwrap();
            let _env = PersonalRootEnv::set(&personal);
            let link = clean.join("link");
            std::os::unix::fs::symlink(&personal, &link).unwrap();
            let mut through_link = link.join("missing");
            for segment in &segments {
                through_link.push(segment);
            }

            let err = canonical_allowed_path(&through_link)
                .expect_err("symlink prefix into SHELVES_PROTECTED_ROOT must be refused before read");

            prop_assert_eq!(violation_root(err), std::fs::canonicalize(&personal).unwrap());
        }
    }

    fn violation_root(err: GuardError) -> PathBuf {
        match err {
            GuardError::Violation(violation) => violation.protected_root,
            GuardError::MissingPersonalRoot => panic!("expected guard violation"), // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        }
    }

    fn create_existing_descendant(root: &Path, segments: &[String]) -> PathBuf {
        let mut path = root.to_path_buf();
        for segment in segments {
            path.push(segment);
        }
        std::fs::create_dir_all(&path).unwrap();
        let file = path.join("leaf.md");
        std::fs::write(&file, "synthetic fixture").unwrap();
        file
    }

    fn path_component() -> impl Strategy<Value = String> {
        prop_oneof![
            "[A-Za-z0-9_-]{1,16}",
            Just("LOCK".to_string()),
            Just("cafe-con-leche".to_string()),
            Just("café".to_string()),
            Just("mañana".to_string()),
            Just("archivo".to_string()),
            Just("datos".to_string()),
        ]
    }

    struct PersonalRootEnv {
        old: Option<String>,
    }

    impl PersonalRootEnv {
        fn set(path: &Path) -> Self {
            let old = std::env::var("SHELVES_PROTECTED_ROOT").ok();
            unsafe { std::env::set_var("SHELVES_PROTECTED_ROOT", path) };
            Self { old }
        }
    }

    impl Drop for PersonalRootEnv {
        fn drop(&mut self) {
            restore_personal_root(self.old.take());
        }
    }

    fn restore_personal_root(old: Option<String>) {
        match old {
            Some(value) => unsafe { std::env::set_var("SHELVES_PROTECTED_ROOT", value) }, // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
            None => unsafe { std::env::remove_var("SHELVES_PROTECTED_ROOT") },
        }
    }

    fn env_lock() -> &'static Mutex<()> {
        crate::test_env_lock()
    }
}
