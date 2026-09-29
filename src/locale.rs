use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

const LOCALE_VERSION: u32 = 1;
const BUNDLED_PACKS: &[(&str, &str)] = &[
    ("generic.toml", include_str!("../locales/generic.toml")),
    ("es.toml", include_str!("../locales/es.toml")),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryIntent {
    Standard,
    Orientation,
    Rule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedQuery {
    pub search_text: String,
    pub topic_text: String,
    pub intent: QueryIntent,
}

impl PreparedQuery {
    fn identity(query: &str) -> Self {
        Self {
            search_text: query.to_string(),
            topic_text: query.to_string(),
            intent: legacy_intent(query),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LocaleRegistry {
    packs: Vec<LocalePack>,
}

#[derive(Debug, Clone)]
struct LocalePack {
    locale: String,
    stopwords: Vec<String>,
    orientation_patterns: Vec<String>,
    rule_patterns: Vec<String>,
    terms: BTreeMap<String, Vec<String>>,
    synonyms: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocaleFile {
    version: u32,
    locale: String,
    #[serde(default)]
    stopwords: Vec<String>,
    #[serde(default)]
    intents: IntentFile,
    #[serde(default)]
    terms: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    synonyms: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct IntentFile {
    #[serde(default)]
    orientation: Vec<String>,
    #[serde(default)]
    rule: Vec<String>,
}

impl LocaleRegistry {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn installed() -> Result<Self> {
        let mut registry = Self::bundled()?;
        if let Some(path) = std::env::var_os("SHELVES_LOCALE_DIR") {
            if path.is_empty() {
                bail!("SHELVES_LOCALE_DIR is set but empty");
            }
            registry.extend_dir(Path::new(&path))?;
        }
        Ok(registry)
    }

    pub fn bundled() -> Result<Self> {
        static REGISTRY: OnceLock<Result<LocaleRegistry, String>> = OnceLock::new();
        REGISTRY
            .get_or_init(|| {
                let mut registry = Self::empty();
                for (name, contents) in BUNDLED_PACKS {
                    registry
                        .add_toml(name, contents)
                        .map_err(|error| format!("{error:#}"))?;
                }
                Ok(registry)
            })
            .clone()
            .map_err(anyhow::Error::msg)
    }

    pub fn from_dir(path: &Path) -> Result<Self> {
        let mut registry = Self::empty();
        registry.extend_dir(path)?;
        Ok(registry)
    }

    pub fn prepare(&self, query: &str) -> PreparedQuery {
        if self.packs.is_empty() {
            return PreparedQuery::identity(query);
        }
        let normalized_query = normalize(query);
        let intent = self.intent(&normalized_query);
        let mut expansions = Vec::new();
        let mut primary_expansions = Vec::new();
        let mut seen = HashSet::new();
        let mut primary_seen = HashSet::new();
        let query_tokens: Vec<&str> = normalized_query.split_whitespace().collect();
        let mut covered_tokens = vec![false; query_tokens.len()];
        for pack in &self.packs {
            for (source, targets) in &pack.terms {
                if contains_phrase(&normalized_query, source) {
                    mark_covered_tokens(&query_tokens, source, &mut covered_tokens);
                    if let Some(primary) = targets.first()
                        && primary_seen.insert(primary.as_str())
                    {
                        primary_expansions.push(primary.as_str());
                    }
                    for target in targets {
                        if seen.insert(target.as_str()) {
                            expansions.push(target.as_str());
                        }
                    }
                }
            }
            for (concept, members) in &pack.synonyms {
                let matched = members
                    .iter()
                    .filter(|member| contains_phrase(&normalized_query, member))
                    .collect::<Vec<_>>();
                if matched.is_empty() {
                    continue;
                }
                for member in matched {
                    mark_covered_tokens(&query_tokens, member, &mut covered_tokens);
                }
                if primary_seen.insert(concept.as_str()) {
                    primary_expansions.push(concept.as_str());
                }
                for member in members {
                    if seen.insert(member.as_str()) {
                        expansions.push(member.as_str());
                    }
                }
            }
        }
        if expansions.is_empty() {
            PreparedQuery {
                search_text: query.to_string(),
                topic_text: query.to_string(),
                intent,
            }
        } else {
            let uncovered = query_tokens
                .iter()
                .zip(covered_tokens)
                .filter_map(|(token, covered)| {
                    (!covered && !self.is_stopword(token)).then_some(*token)
                })
                .collect::<Vec<_>>();
            PreparedQuery {
                search_text: expansions
                    .iter()
                    .copied()
                    .chain(uncovered.iter().copied())
                    .collect::<Vec<_>>()
                    .join(" "),
                topic_text: primary_expansions
                    .into_iter()
                    .chain(uncovered.iter().copied())
                    .collect::<Vec<_>>()
                    .join(" "),
                intent,
            }
        }
    }

    fn intent(&self, query: &str) -> QueryIntent {
        if self
            .packs
            .iter()
            .any(|pack| patterns_match(query, &pack.rule_patterns))
        {
            return QueryIntent::Rule;
        }
        if legacy_intent(query) == QueryIntent::Orientation
            || self
                .packs
                .iter()
                .any(|pack| patterns_match(query, &pack.orientation_patterns))
        {
            QueryIntent::Orientation
        } else {
            QueryIntent::Standard
        }
    }

    fn is_stopword(&self, token: &str) -> bool {
        self.packs
            .iter()
            .any(|pack| pack.stopwords.iter().any(|stopword| stopword == token))
    }

    fn extend_dir(&mut self, path: &Path) -> Result<()> {
        let mut files = std::fs::read_dir(path)
            .with_context(|| format!("reading locale directory {}", path.display()))?
            .collect::<std::io::Result<Vec<_>>>()
            .with_context(|| format!("listing locale directory {}", path.display()))?;
        files.sort_by_key(std::fs::DirEntry::file_name);
        for entry in files {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("toml") {
                continue;
            }
            let contents = std::fs::read_to_string(&path)
                .with_context(|| format!("reading locale pack {}", path.display()))?;
            self.add_toml(&path.display().to_string(), &contents)?;
        }
        Ok(())
    }

    fn add_toml(&mut self, name: &str, contents: &str) -> Result<()> {
        let parsed: LocaleFile =
            toml::from_str(contents).with_context(|| format!("parsing locale pack {name}"))?;
        if parsed.version != LOCALE_VERSION {
            bail!(
                "locale pack {name} uses version {}; expected {LOCALE_VERSION}",
                parsed.version
            );
        }
        let locale = parsed.locale.trim();
        if locale.is_empty() {
            bail!("locale pack {name} has an empty locale");
        }
        let pack = LocalePack {
            locale: locale.to_string(),
            stopwords: normalize_values(name, "stopword", parsed.stopwords)?,
            orientation_patterns: normalize_values(
                name,
                "orientation pattern",
                parsed.intents.orientation,
            )?,
            rule_patterns: normalize_values(name, "rule pattern", parsed.intents.rule)?,
            terms: normalize_terms(name, parsed.terms)?,
            synonyms: normalize_synonyms(name, parsed.synonyms)?,
        };
        if let Some(existing) = self
            .packs
            .iter_mut()
            .find(|existing| existing.locale == pack.locale)
        {
            merge_pack(name, existing, pack)?;
            return Ok(());
        }
        self.packs.push(pack);
        self.packs
            .sort_by(|left, right| left.locale.cmp(&right.locale));
        Ok(())
    }
}

fn merge_pack(name: &str, existing: &mut LocalePack, extension: LocalePack) -> Result<()> {
    for stopword in extension.stopwords {
        if !existing.stopwords.contains(&stopword) {
            existing.stopwords.push(stopword);
        }
    }
    for pattern in extension.orientation_patterns {
        if !existing.orientation_patterns.contains(&pattern) {
            existing.orientation_patterns.push(pattern);
        }
    }
    for pattern in extension.rule_patterns {
        if !existing.rule_patterns.contains(&pattern) {
            existing.rule_patterns.push(pattern);
        }
    }
    for (source, targets) in extension.terms {
        if existing.terms.insert(source.clone(), targets).is_some() {
            bail!(
                "locale pack {name} redefines term {source:?} for locale {}",
                existing.locale
            );
        }
    }
    for (concept, members) in extension.synonyms {
        if existing.synonyms.insert(concept.clone(), members).is_some() {
            bail!(
                "locale pack {name} redefines synonym group {concept:?} for locale {}",
                existing.locale
            ); // LCOV_EXCL_LINE: error text is asserted by the merge-conflict test.
        } // LCOV_EXCL_LINE: merge-conflict branch is asserted by the adjacent test.
    }
    Ok(())
}

pub fn prepare(query: &str) -> Result<PreparedQuery> {
    LocaleRegistry::installed().map(|registry| registry.prepare(query))
}

fn normalize_values(name: &str, kind: &str, values: Vec<String>) -> Result<Vec<String>> {
    let mut normalized = Vec::new();
    for value in values {
        let value = normalize(&value);
        if value.is_empty() {
            bail!("locale pack {name} has an empty {kind}");
        }
        if !normalized.contains(&value) {
            normalized.push(value);
        }
    }
    Ok(normalized)
}

fn normalize_terms(
    name: &str,
    terms: BTreeMap<String, Vec<String>>,
) -> Result<BTreeMap<String, Vec<String>>> {
    let mut normalized = BTreeMap::new();
    for (source, targets) in terms {
        let source = normalize(&source);
        if source.is_empty() {
            bail!("locale pack {name} has an empty expansion source");
        }
        let targets = normalize_values(name, "expansion target", targets)?;
        if targets.is_empty() {
            bail!("locale pack {name} term {source:?} has no expansion targets");
        }
        if normalized.insert(source.clone(), targets).is_some() {
            bail!("locale pack {name} repeats normalized term {source:?}");
        }
    }
    Ok(normalized)
}

fn normalize_synonyms(
    name: &str,
    groups: BTreeMap<String, Vec<String>>,
) -> Result<BTreeMap<String, Vec<String>>> {
    let mut normalized = BTreeMap::new();
    for (concept, members) in groups {
        let concept = normalize(&concept);
        if concept.is_empty() {
            bail!("locale pack {name} has an empty synonym concept");
        }
        let members = normalize_values(name, "synonym", members)?;
        if members.is_empty() {
            bail!("locale pack {name} synonym group {concept:?} has no members");
        }
        let mut complete = vec![concept.clone()];
        for member in members {
            if !complete.contains(&member) {
                complete.push(member);
            }
        }
        if normalized.insert(concept.clone(), complete).is_some() {
            bail!("locale pack {name} repeats synonym group {concept:?}");
        }
    }
    Ok(normalized)
}

fn patterns_match(query: &str, patterns: &[String]) -> bool {
    patterns
        .iter()
        .any(|pattern| contains_phrase(query, pattern))
}

fn contains_phrase(haystack: &str, needle: &str) -> bool {
    let padded_haystack = format!(" {haystack} ");
    let padded_needle = format!(" {needle} ");
    padded_haystack.contains(&padded_needle)
}

fn mark_covered_tokens(query: &[&str], source: &str, covered: &mut [bool]) {
    let source: Vec<&str> = source.split_whitespace().collect();
    for start in 0..query.len() {
        let end = start + source.len();
        if end <= query.len() && query[start..end] == source {
            covered[start..end].fill(true);
        }
    }
}

fn normalize(value: &str) -> String {
    let folded: String = value
        .nfkd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase)
        .map(|character| {
            if character.is_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                ' '
            }
        })
        .collect();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn legacy_intent(query: &str) -> QueryIntent {
    let lowered = query.to_ascii_lowercase();
    if [
        "orientation",
        "what happened",
        "what matters",
        "last night",
        "this morning",
        "where are we",
        "latest state",
        "recent state",
        "status update",
    ]
    .iter()
    .any(|cue| lowered.contains(cue))
    {
        QueryIntent::Orientation
    } else {
        QueryIntent::Standard
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use tempfile::tempdir;

    #[test]
    fn bundled_spanish_pack_expands_accents_and_detects_intents() {
        let registry = LocaleRegistry::bundled().unwrap();
        let orientation = registry.prepare("¿Qué pasó anoche, qué importa hoy?");
        assert_eq!(orientation.intent, QueryIntent::Orientation);
        assert!(orientation.search_text.contains("what happened"));
        assert!(orientation.search_text.contains("last night"));
        assert!(orientation.search_text.contains("what matters"));
        assert!(orientation.search_text.contains("today"));

        let rule = registry.prepare("¿Cuál es la política de retención?");
        assert_eq!(rule.intent, QueryIntent::Rule);
        assert!(rule.search_text.contains("policy"));
        assert!(rule.search_text.contains("retention"));

        let compactor = registry.prepare("compactador corta el summary primero");
        assert!(compactor.search_text.contains("compactor"));
        assert!(compactor.search_text.contains("attacks"));
        assert!(compactor.search_text.contains("summary"));
        assert!(compactor.search_text.contains("first"));
    }

    #[test]
    fn bundled_synonyms_bridge_generic_english_and_spanish_at_query_time() {
        let registry = LocaleRegistry::bundled().unwrap();

        let education = registry.prepare("compare college paths");
        for term in ["education", "university", "degree", "associate", "school"] {
            assert!(education.search_text.contains(term), "missing {term:?}");
        }
        assert_eq!(education.topic_text, "education compare paths");

        let money = registry.prepare("revisar presupuesto");
        for term in ["money", "budget", "finance", "accounts", "bills"] {
            assert!(money.search_text.contains(term), "missing {term:?}");
        }

        let meeting = registry.prepare("preparar reunión");
        assert!(meeting.search_text.contains("meeting"));
        assert!(meeting.search_text.contains("session"));
    }

    #[test]
    fn rule_intent_wins_when_query_contains_both_intent_shapes() {
        let registry = LocaleRegistry::bundled().unwrap();
        let query = registry.prepare("que pasó anoche y cuál es la política");
        assert_eq!(query.intent, QueryIntent::Rule);
    }

    #[test]
    fn empty_registry_preserves_the_legacy_query_path_byte_for_byte() {
        let registry = LocaleRegistry::empty();
        let english = "morning orientation: what happened last night";
        let plain = "que pasó anoche";
        assert_eq!(
            registry.prepare(english),
            PreparedQuery {
                search_text: english.to_string(),
                topic_text: english.to_string(),
                intent: QueryIntent::Orientation,
            }
        );
        assert_eq!(
            registry.prepare(plain),
            PreparedQuery {
                search_text: plain.to_string(),
                topic_text: plain.to_string(),
                intent: QueryIntent::Standard,
            }
        );
    }

    #[test]
    fn directory_loading_is_sorted_and_rejects_bad_packs() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("aa.toml"),
            "version = 1\nlocale = \"zz\"\n[terms]\nhello = [\"world\"]\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ab.toml"),
            "version = 1\nlocale = \"zz\"\n[terms]\nfriend = [\"ally\"]\n",
        )
        .unwrap();
        let registry = LocaleRegistry::from_dir(dir.path()).unwrap();
        assert_eq!(registry.prepare("hello friend").search_text, "ally world");

        std::fs::write(
            dir.path().join("bad.toml"),
            "version = 2\nlocale = \"bad\"\n",
        )
        .unwrap();
        let error = LocaleRegistry::from_dir(dir.path()).unwrap_err();
        assert!(error.to_string().contains("uses version 2"));
    }

    #[test]
    fn installed_registry_loads_extensions_and_rejects_an_empty_path() {
        let _lock = crate::test_env_lock().lock().unwrap();
        let previous = std::env::var_os("SHELVES_LOCALE_DIR");
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("extension.toml"),
            "version = 1\nlocale = \"zz\"\n[terms]\nhello = [\"world\"]\n",
        )
        .unwrap();

        unsafe { std::env::set_var("SHELVES_LOCALE_DIR", dir.path()) };
        assert_eq!(
            LocaleRegistry::installed()
                .unwrap()
                .prepare("hello")
                .search_text,
            "world"
        );

        unsafe { std::env::set_var("SHELVES_LOCALE_DIR", OsString::new()) };
        assert!(
            LocaleRegistry::installed()
                .unwrap_err()
                .to_string()
                .contains("set but empty")
        );

        match previous {
            Some(value) => unsafe { std::env::set_var("SHELVES_LOCALE_DIR", value) }, // LCOV_EXCL_LINE: cleanup depends on caller's ambient environment.
            None => unsafe { std::env::remove_var("SHELVES_LOCALE_DIR") },
        }
    }

    #[test]
    fn directory_extensions_merge_unique_values_and_ignore_other_files() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("README.txt"), "ignored").unwrap();
        std::fs::write(
            dir.path().join("aa.toml"),
            "version = 1\nlocale = \"zz\"\nstopwords = [\"the\"]\n[intents]\norientation = [\"where now\"]\nrule = [\"must do\"]\n[terms]\nhello = [\"world\"]\n[synonyms]\neducation = [\"college\"]\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("ab.toml"),
            "version = 1\nlocale = \"zz\"\nstopwords = [\"and\"]\n[intents]\norientation = [\"status now\"]\nrule = [\"required now\"]\n[terms]\nfriend = [\"ally\"]\n",
        )
        .unwrap();

        let registry = LocaleRegistry::from_dir(dir.path()).unwrap();
        assert_eq!(
            registry.prepare("the hello and friend").search_text,
            "ally world"
        );
        assert_eq!(
            registry.prepare("status now").intent,
            QueryIntent::Orientation
        );
        assert_eq!(registry.prepare("required now").intent, QueryIntent::Rule);
        assert_eq!(registry.prepare("college").search_text, "education college");
    }

    #[test]
    fn locale_validation_reports_each_invalid_shape() {
        let cases = [
            ("version = 1\nlocale = \" \"\n", "empty locale"),
            (
                "version = 1\nlocale = \"zz\"\n[intents]\norientation = [\" \" ]\n",
                "empty orientation pattern",
            ),
            (
                "version = 1\nlocale = \"zz\"\nstopwords = [\" \" ]\n",
                "empty stopword",
            ),
            (
                "version = 1\nlocale = \"zz\"\n[terms]\n\" \" = [\"target\"]\n",
                "empty expansion source",
            ),
            (
                "version = 1\nlocale = \"zz\"\n[terms]\nsource = []\n",
                "has no expansion targets",
            ),
            (
                "version = 1\nlocale = \"zz\"\n[terms]\na = [\"one\"]\n\"á\" = [\"two\"]\n",
                "repeats normalized term",
            ),
            (
                "version = 1\nlocale = \"zz\"\n[synonyms]\neducation = []\n",
                "has no members",
            ),
            (
                "version = 1\nlocale = \"zz\"\n[synonyms]\n\" \" = [\"college\"]\n",
                "empty synonym concept",
            ),
            (
                "version = 1\nlocale = \"zz\"\n[synonyms]\na = [\"one\"]\n\"á\" = [\"two\"]\n",
                "repeats synonym group",
            ),
        ];
        for (contents, expected) in cases {
            let mut registry = LocaleRegistry::empty();
            let error = registry.add_toml("invalid.toml", contents).unwrap_err();
            assert!(
                error.to_string().contains(expected),
                "expected {expected:?}, got {error:#}"
            );
        }

        let mut registry = LocaleRegistry::empty();
        registry
            .add_toml(
                "first.toml",
                "version = 1\nlocale = \"zz\"\n[terms]\nhello = [\"world\"]\n",
            )
            .unwrap();
        let error = registry
            .add_toml(
                "second.toml",
                "version = 1\nlocale = \"zz\"\n[terms]\nhello = [\"again\"]\n",
            )
            .unwrap_err();
        assert!(error.to_string().contains("redefines term"));

        let mut registry = LocaleRegistry::empty();
        registry
            .add_toml(
                "first.toml",
                "version = 1\nlocale = \"zz\"\n[synonyms]\neducation = [\"college\"]\n",
            )
            .unwrap();
        let error = registry
            .add_toml(
                "second.toml",
                "version = 1\nlocale = \"zz\"\n[synonyms]\neducation = [\"school\"]\n",
            )
            .unwrap_err();
        assert!(error.to_string().contains("redefines synonym group"));
    }
}
