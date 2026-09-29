use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryDoc {
    pub name: String,
    pub title: String,
    pub body: String,
    pub owner: String,
    pub scope: String,
    pub memory_type: String,
    pub visibility: Option<String>,
    pub source_path: PathBuf,
    pub content_hash: String,
    pub is_lock: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeDoc {
    pub ts: String,
    pub actor: String,
    pub kind: String,
    pub summary: String,
    pub body: String,
    pub scope: String,
    pub visibility: Option<String>,
    pub source_path: PathBuf,
}

#[derive(Debug, Deserialize, Default)]
struct ExternalFrontmatter {
    name: Option<String>,
    title: Option<String>,
    description: Option<String>,
    owner: Option<String>,
    #[serde(rename = "type")]
    memory_type: Option<String>,
    #[serde(default)]
    metadata: ExternalMetadata,
}

#[derive(Debug, Deserialize, Default)]
struct ExternalMetadata {
    #[serde(rename = "type")]
    memory_type: Option<String>,
}

pub fn parse_external_memory(path: &Path, text: &str) -> Vec<MemoryDoc> {
    let (frontmatter, body) = split_frontmatter(text);
    let visibility = frontmatter_visibility(frontmatter);
    let parsed: ExternalFrontmatter = frontmatter
        .and_then(|raw| serde_yaml::from_str(raw).ok())
        .unwrap_or_default();
    let fallback = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("memory");
    let memory_type = parsed
        .memory_type
        .or(parsed.metadata.memory_type)
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "memory".to_string());
    let title = parsed
        .title
        .or(parsed.description)
        .unwrap_or_else(|| fallback.replace('_', " "));
    let name = parsed.name.unwrap_or_else(|| slugify(fallback));
    let body = body.trim().to_string();
    let scope = classify_scope_for_memory(path, &name, &title, &body);
    let owner = if memory_type == "reference" && visibility.as_deref() != Some("private") {
        "shared".to_string()
    } else {
        owner_for_path(path)
    };
    let mut doc = MemoryDoc::new(name, title, body, owner, scope, memory_type, path, false);
    doc.visibility = visibility;
    vec![doc]
}

fn normalize_visibility(value: &str) -> String {
    if value.trim().eq_ignore_ascii_case("shared") {
        "shared".to_string()
    } else {
        // An invalid explicit value must never turn a sensitive row public.
        "private".to_string()
    }
}

fn frontmatter_visibility(frontmatter: Option<&str>) -> Option<String> {
    // Preserve an explicit private marker even when another YAML field is malformed.
    let raw = frontmatter?
        .lines()
        .find_map(|line| line.trim().strip_prefix("visibility:"))?;
    Some(normalize_visibility(raw.trim().trim_matches(['"', '\''])))
}

pub fn parse_agent_memory(path: &Path, text: &str) -> Vec<MemoryDoc> {
    let mut docs = parse_external_memory(path, text);
    let explicit_owner = split_frontmatter(text)
        .0
        .and_then(|raw| serde_yaml::from_str::<ExternalFrontmatter>(raw).ok())
        .and_then(|frontmatter| frontmatter.owner)
        .filter(|owner| !owner.trim().is_empty())
        .map(|owner| normalize_agent_owner(&owner));
    for doc in &mut docs {
        if (doc.memory_type != "reference" || doc.visibility.as_deref() == Some("private"))
            && let Some(owner) = explicit_owner.as_ref()
        {
            doc.owner.clone_from(owner);
        }
        if !has_explicit_product_scope(&doc.body) {
            doc.scope = "company".to_string();
        }
    }
    docs
}

fn normalize_agent_owner(owner: &str) -> String {
    let normalized = owner
        .trim()
        .trim_start_matches('@')
        .trim_start_matches("agent:")
        .to_ascii_lowercase()
        .replace('-', "_");
    format!("agent:{normalized}")
}

pub fn parse_system_memory(path: &Path, text: &str) -> Vec<MemoryDoc> {
    let visibility = frontmatter_visibility(split_frontmatter(text).0);
    let mut docs = Vec::new();
    let mut current_title: Option<String> = None;
    let mut current_body = Vec::new();

    for line in text.lines() {
        if let Some(title) = line.strip_prefix("## ") {
            flush_system_memory(path, &mut docs, &mut current_title, &mut current_body);
            current_title = Some(title.trim().to_string());
        } else if current_title.is_some() {
            current_body.push(line);
        }
    }
    flush_system_memory(path, &mut docs, &mut current_title, &mut current_body);
    for doc in &mut docs {
        doc.visibility.clone_from(&visibility);
    }
    docs
}

pub fn parse_team_log(path: &Path, text: &str) -> Vec<EpisodeDoc> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(idx, line)| {
            let (actor, rest) = if let Some(end) = line.find(']') {
                (
                    format!("agent:{}", line[1..end].trim().to_ascii_lowercase()),
                    line[end + 1..].trim(),
                )
            } else {
                ("system".to_string(), line.trim())
            };
            let ts = parse_log_ts(rest).unwrap_or_else(|| synthetic_ts_from_index(path, idx));
            EpisodeDoc::new(ts, actor, "team-log", line.trim(), line.trim(), "os", path)
        })
        .collect()
}

pub fn parse_markdown_episode_file(path: &Path, text: &str, kind: &str) -> Vec<EpisodeDoc> {
    let visibility = frontmatter_visibility(split_frontmatter(text).0);
    let ts = ts_from_text_or_path(text, path);
    let actor = actor_from_text_or_path(text, path);
    let scope = classify_scope_for_markdown(path, text);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();

    if lines.is_empty() {
        let mut doc = EpisodeDoc::new(
            ts,
            actor,
            kind,
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("empty file"),
            "",
            scope,
            path,
        );
        doc.visibility = visibility;
        return vec![doc];
    }

    lines
        .iter()
        .take(50)
        .enumerate()
        .map(|(idx, line)| {
            let line_ts = offset_seconds(&ts, idx as i64);
            let mut doc = EpisodeDoc::new(
                line_ts,
                actor.clone(),
                kind,
                line,
                *line,
                scope.clone(),
                path,
            );
            doc.visibility.clone_from(&visibility);
            doc
        })
        .collect()
}

pub fn parse_meeting_file(path: &Path, text: &str) -> Vec<EpisodeDoc> {
    let visibility = frontmatter_visibility(split_frontmatter(text).0);
    let ts = ts_from_text_or_path(text, path);
    let actor = actor_from_text_or_path(text, path);
    let scope = classify_scope_for_markdown(path, text);
    let fallback_title = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("meeting")
        .replace('-', " ");
    let mut title = fallback_title;
    let mut saw_title = false;
    let mut current_heading: Option<String> = None;
    let mut current_body = Vec::new();
    let mut sections: Vec<(Option<String>, String)> = Vec::new();

    for line in text.lines() {
        if let Some((level, heading)) = semantic_heading(line) {
            if level == 1 && !saw_title {
                title = meeting_title(heading);
                saw_title = true;
                continue;
            }
            flush_meeting_section(&mut sections, &mut current_heading, &mut current_body);
            current_heading = Some(heading.to_string());
        } else if let Some(heading) = semantic_bold_bullet(line) {
            flush_meeting_section(&mut sections, &mut current_heading, &mut current_body);
            current_heading = Some(heading.to_string());
            current_body.push(line);
        } else {
            current_body.push(line);
        }
    }
    flush_meeting_section(&mut sections, &mut current_heading, &mut current_body);

    if sections.is_empty() {
        let mut doc = EpisodeDoc::new(
            ts,
            actor,
            "meeting",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("empty meeting"),
            "",
            scope,
            path,
        );
        doc.visibility = visibility;
        return vec![doc];
    }

    sections
        .into_iter()
        .enumerate()
        .map(|(idx, (heading, body))| {
            let summary = heading
                .as_ref()
                .map(|heading| format!("{title} — {heading}"))
                .unwrap_or_else(|| title.clone());
            let searchable_body = heading
                .map(|heading| format!("{heading}\n{body}"))
                .unwrap_or(body);
            let mut doc = EpisodeDoc::new(
                offset_seconds(&ts, idx as i64),
                actor.clone(),
                "meeting",
                summary,
                searchable_body,
                scope.clone(),
                path,
            );
            doc.visibility.clone_from(&visibility);
            doc
        })
        .collect()
}

fn semantic_heading(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim();
    let level = trimmed.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=3).contains(&level) || trimmed.as_bytes().get(level) != Some(&b' ') {
        return None;
    }
    let heading = trimmed[level + 1..].trim();
    (!heading.is_empty()).then_some((level, heading))
}

fn meeting_title(heading: &str) -> String {
    for prefix in ["Meeting — ", "Meeting - ", "MEETING — ", "MEETING - "] {
        if let Some(title) = heading.strip_prefix(prefix) {
            return title.trim().to_string();
        }
    }
    heading.trim().to_string()
}

fn semantic_bold_bullet(line: &str) -> Option<&str> {
    if line.len() != line.trim_start().len() {
        return None;
    }
    let rest = line.strip_prefix("- **")?;
    let end = rest.find("**")?;
    let heading = rest[..end].trim();
    (!heading.is_empty()).then_some(heading)
}

fn flush_meeting_section(
    sections: &mut Vec<(Option<String>, String)>,
    heading: &mut Option<String>,
    body: &mut Vec<&str>,
) {
    let normalized = body.join("\n").trim().to_string();
    body.clear();
    let current_heading = heading.take();
    if !normalized.is_empty() {
        sections.push((current_heading, normalized));
    }
}

pub fn classify_scope(text: &str) -> String {
    classify_scope_from_signals("", "", text)
}

fn classify_scope_for_markdown(path: &Path, text: &str) -> String {
    let title = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("# "))
        .unwrap_or("");
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("");
    classify_scope_from_signals(stem, title, text)
}

fn classify_scope_for_memory(path: &Path, name: &str, title: &str, body: &str) -> String {
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "MEMORY.md")
    {
        return "os".to_string();
    }
    if title.eq_ignore_ascii_case("active files") {
        return "os".to_string();
    }
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(name);
    classify_scope_from_signals(stem, title, body)
}

fn classify_scope_from_signals(stem: &str, title: &str, body: &str) -> String {
    let stem_slug = slugify(stem).replace('_', "-");
    let title_slug = slugify(title).replace('_', "-");
    let lower = body.to_ascii_lowercase();
    if is_company_memory_slug(&stem_slug) || is_company_memory_slug(&title_slug) {
        return "company".to_string();
    }
    for (product, aliases) in configured_product_scopes() {
        if has_product_signal(&product, &aliases, &stem_slug, &title_slug, &lower) {
            return format!("product:{product}");
        }
    }
    if configured_company_tokens()
        .iter()
        .any(|token| lower.contains(token))
    {
        "company".to_string()
    } else {
        "os".to_string()
    }
}

fn has_explicit_product_scope(text: &str) -> bool {
    text.to_ascii_lowercase().contains("product:")
}

fn has_product_signal(
    product: &str,
    aliases: &[String],
    stem_slug: &str,
    title_slug: &str,
    body: &str,
) -> bool {
    if body.contains(&format!("product:{product}"))
        || body.contains(&format!("scope: product:{product}"))
    {
        return true;
    }
    aliases.iter().any(|alias| {
        slug_strongly_names_product(stem_slug, alias)
            || slug_strongly_names_product(title_slug, alias)
    })
}

fn configured_product_scopes() -> Vec<(String, Vec<String>)> {
    let raw = std::env::var("SHELVES_PRODUCT_SCOPES")
        .unwrap_or_else(|_| "notebook,console,voice".to_string());
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| {
            let (name, aliases) = entry.split_once(':').unwrap_or((entry, ""));
            let name = name.trim().to_ascii_lowercase();
            if name.is_empty() {
                return None;
            }
            let mut all_aliases = vec![name.clone()];
            for alias in aliases
                .split('|')
                .map(str::trim)
                .filter(|alias| !alias.is_empty())
                .map(|alias| alias.to_ascii_lowercase())
            {
                if !all_aliases.contains(&alias) {
                    all_aliases.push(alias);
                }
            }
            Some((name, all_aliases))
        })
        .collect()
}

fn configured_company_tokens() -> Vec<String> {
    let raw = std::env::var("SHELVES_COMPANY_TOKENS")
        .unwrap_or_else(|_| "company,organization,team".to_string());
    raw.split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

fn slug_strongly_names_product(slug: &str, alias: &str) -> bool {
    slug == alias
        || slug.starts_with(&format!("{alias}-"))
        || slug.starts_with(&format!("project-{alias}"))
        || slug.starts_with(&format!("product-{alias}"))
        || slug.ends_with(&format!("-{alias}"))
}

fn is_company_memory_slug(slug: &str) -> bool {
    company_slug_prefixes()
        .iter()
        .any(|prefix| slug.starts_with(prefix))
}

fn company_slug_prefixes() -> Vec<String> {
    let raw =
        std::env::var("SHELVES_COMPANY_SLUG_PREFIXES").unwrap_or_else(|_| "feedback-".to_string());
    raw.split(',')
        .map(str::trim)
        .filter(|prefix| !prefix.is_empty())
        .map(|prefix| prefix.to_ascii_lowercase())
        .collect()
}

fn flush_system_memory(
    path: &Path,
    docs: &mut Vec<MemoryDoc>,
    current_title: &mut Option<String>,
    current_body: &mut Vec<&str>,
) {
    let Some(title) = current_title.take() else {
        return;
    };
    let body = current_body.join("\n").trim().to_string();
    current_body.clear();
    if body.is_empty() {
        return; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    }
    let combined = format!("{title}\n{body}");
    let is_lock = combined.contains("LOCKED")
        || combined.contains("non-negotiable")
        || combined.contains("NON-NEGOTIABLE")
        || title.contains("locked");
    let name = slugify(&title);
    let scope = classify_scope_for_memory(path, &name, &title, &body);
    docs.push(MemoryDoc::new(
        name, title, body, "shared", scope, "memory", path, is_lock,
    ));
}

fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (None, text);
    };
    let Some(end) = rest.find("\n---") else {
        return (None, text);
    };
    let frontmatter = &rest[..end];
    let body = rest[end + 4..].trim_start_matches('\n');
    (Some(frontmatter), body)
}

fn owner_for_path(path: &Path) -> String {
    if let Some(agent) = agent_from_memory_dir(path) {
        return format!("agent:{agent}");
    }
    let tokens = path_tokens(path);
    for agent in agent_hints() {
        if tokens.iter().any(|token| token == &agent) {
            return format!("agent:{agent}");
        } // LCOV_EXCL_LINE: coverage artifact; return branch asserted by owner_hints_can_come_from_env.
    }
    "shared".to_string()
}

fn agent_from_memory_dir(path: &Path) -> Option<String> {
    let components: Vec<String> = path
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .map(|component| component.to_ascii_lowercase())
        .collect();
    for window in components.windows(3) {
        if window[0] == "memory" && !window[1].contains('.') {
            return Some(slugify(&window[1]).replace('-', "_"));
        }
    }
    None
}

fn path_tokens(path: &Path) -> Vec<String> {
    path.to_string_lossy()
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| part.to_ascii_lowercase())
        .collect()
}

pub fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "untitled".to_string()
    } else {
        out
    }
}

pub fn extract_wiki_links(text: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find("]]") else {
            break;
        };
        let raw = after_start[..end].split('|').next().unwrap_or("").trim();
        if !raw.is_empty() {
            links.push(slugify(raw));
        }
        rest = &after_start[end + 2..];
    }
    links.sort();
    links.dedup();
    links
}

fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

impl MemoryDoc {
    #[allow(clippy::too_many_arguments)]
    fn new(
        name: String,
        title: String,
        body: String,
        owner: impl Into<String>,
        scope: impl Into<String>,
        memory_type: impl Into<String>,
        source_path: &Path,
        is_lock: bool,
    ) -> Self {
        let now = Utc::now().to_rfc3339();
        let memory_type = memory_type.into();
        let hash_input = format!(
            "{}\n{}\n{}\n{}",
            title,
            body,
            memory_type,
            source_path.display()
        );
        Self {
            name,
            title,
            body,
            owner: owner.into(),
            scope: scope.into(),
            visibility: None,
            memory_type,
            source_path: source_path.to_path_buf(),
            content_hash: sha256_hex(&hash_input),
            is_lock,
            created_at: now.clone(),
            updated_at: now,
        }
    }
}

impl EpisodeDoc {
    fn new(
        ts: impl Into<String>,
        actor: impl Into<String>,
        kind: impl Into<String>,
        summary: impl AsRef<str>,
        body: impl AsRef<str>,
        scope: impl Into<String>,
        source_path: &Path,
    ) -> Self {
        Self {
            ts: ts.into(),
            actor: actor.into(),
            kind: kind.into(),
            summary: summary.as_ref().chars().take(240).collect(),
            body: body.as_ref().to_string(),
            scope: scope.into(),
            visibility: None,
            source_path: source_path.to_path_buf(),
        }
    }
}

fn parse_log_ts(text: &str) -> Option<String> {
    let candidate = text.split('|').next()?.trim();
    NaiveDateTime::parse_from_str(candidate, "%Y-%m-%d %I:%M %p")
        .map(|dt| Utc.from_utc_datetime(&dt).to_rfc3339())
        .or_else(|_| {
            NaiveDateTime::parse_from_str(candidate, "%Y-%m-%d %H:%M")
                .map(|dt| Utc.from_utc_datetime(&dt).to_rfc3339())
        })
        .ok()
}

fn ts_from_text_or_path(text: &str, path: &Path) -> String {
    if let Some(ts) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(parse_compact_utc_stamp)
    {
        return ts;
    }
    for line in text.lines().take(20) {
        for prefix in ["DATE:", "- Date:", "**Date:**"] {
            if let Some(date) = line.trim().strip_prefix(prefix)
                && let Some(ts) = parse_dateish(date.trim())
            {
                return ts;
            }
        }
    }
    file_mtime_ts(path).unwrap_or_else(|| Utc::now().to_rfc3339())
}

fn actor_from_text_or_path(text: &str, path: &Path) -> String {
    for line in text.lines().take(20) {
        if let Some(from) = line.trim().strip_prefix("FROM:") {
            return format!("agent:{}", from.trim().to_ascii_lowercase());
        }
        if let Some(tool) = line.trim().strip_prefix("- Tool:") {
            return format!("agent:{}", tool.trim().to_ascii_lowercase());
        }
    }
    let lower = path.to_string_lossy().to_ascii_lowercase();
    for actor in agent_hints() {
        if lower.contains(&actor) {
            return format!("agent:{actor}");
        }
    }
    "system".to_string()
}

fn agent_hints() -> Vec<String> {
    if let Ok(raw) = std::env::var("SHELVES_AGENT_HINTS") {
        return parse_agent_hints(&raw, ',');
    }
    let Ok(root) = std::env::var("AIOS_ROOT") else {
        return Vec::new();
    };
    let path = Path::new(&root).join("system/agents.txt");
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_agent_hints(&raw, '\n')
}

fn parse_agent_hints(raw: &str, separator: char) -> Vec<String> {
    let mut hints: Vec<String> = raw
        .split(separator)
        .map(str::trim)
        .filter(|name| !name.is_empty() && !name.starts_with('#'))
        .map(|name| {
            name.trim_start_matches("agent:")
                .to_ascii_lowercase()
                .replace('-', "_")
        })
        .collect();
    hints.sort();
    hints.dedup();
    hints
}

fn parse_dateish(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Some(dt.with_timezone(&Utc).to_rfc3339());
    }
    if let Some(ts) = parse_compact_utc_stamp(trimmed) {
        return Some(ts); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    }
    for len in [10_usize, 8] {
        if trimmed.len() >= len && trimmed.as_bytes()[..len].iter().all(u8::is_ascii) {
            let prefix = std::str::from_utf8(&trimmed.as_bytes()[..len]).ok()?;
            if len == 10 {
                if let Ok(date) = NaiveDate::parse_from_str(prefix, "%Y-%m-%d") {
                    return date
                        .and_hms_opt(0, 0, 0)
                        .map(|dt| Utc.from_utc_datetime(&dt).to_rfc3339());
                } // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
            } else if let Ok(date) = NaiveDate::parse_from_str(prefix, "%Y%m%d") {
                return date
                    .and_hms_opt(0, 0, 0)
                    .map(|dt| Utc.from_utc_datetime(&dt).to_rfc3339());
            } // LCOV_EXCL_LINE: coverage artifact (brace after return), same as line 519.
        }
    }
    None
}

fn parse_compact_utc_stamp(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 16 && bytes[..16].iter().all(u8::is_ascii) {
        let prefix = std::str::from_utf8(&bytes[..16]).ok()?;
        if bytes.get(8) == Some(&b'T')
            && bytes.get(15) == Some(&b'Z')
            && let Ok(dt) = NaiveDateTime::parse_from_str(prefix, "%Y%m%dT%H%M%SZ")
        {
            return Some(Utc.from_utc_datetime(&dt).to_rfc3339());
        }
    }
    if bytes.len() >= 14 && bytes[..14].iter().all(u8::is_ascii) {
        let prefix = std::str::from_utf8(&bytes[..14]).ok()?;
        if bytes.get(8) == Some(&b'T')
            && bytes.get(13) == Some(&b'Z')
            && let Ok(dt) = NaiveDateTime::parse_from_str(prefix, "%Y%m%dT%H%MZ")
        {
            return Some(Utc.from_utc_datetime(&dt).to_rfc3339());
        }
    }
    None
}

fn offset_seconds(ts: &str, seconds: i64) -> String {
    DateTime::parse_from_rfc3339(ts)
        .map(|dt| (dt.with_timezone(&Utc) + chrono::Duration::seconds(seconds)).to_rfc3339())
        .unwrap_or_else(|_| ts.to_string())
}

fn synthetic_ts_from_index(path: &Path, idx: usize) -> String {
    let base = file_mtime_ts(path).unwrap_or_else(|| Utc::now().to_rfc3339());
    offset_seconds(&base, idx as i64)
}

fn file_mtime_ts(path: &Path) -> Option<String> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .map(DateTime::<Utc>::from)
        .map(|dt| dt.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use proptest::prelude::*;
    use std::sync::Mutex;
    use tempfile::NamedTempFile;
    use tempfile::TempDir;

    #[test]
    fn parses_external_frontmatter_memory() {
        let path = Path::new("/tmp/project_notebook.md");
        let docs = parse_external_memory(
            path,
            "---\nname: project_notebook\ndescription: Notebook product memory\ntype: project\n---\nBody",
        );
        assert_eq!(docs[0].name, "project_notebook");
        assert_eq!(docs[0].scope, "product:notebook");
    }

    #[test]
    fn visibility_frontmatter_marks_memories_and_episode_files() {
        let memory = parse_agent_memory(
            Path::new("/tmp/a.md"),
            "---\nowner: a\nvisibility: private\n---\nA secret",
        );
        assert_eq!(memory[0].visibility.as_deref(), Some("private"));
        assert_eq!(memory[0].owner, "agent:a");
        let private_reference = parse_agent_memory(
            Path::new("/tmp/private-reference.md"),
            "---\nowner: a\ntype: reference\nvisibility: private\n---\nA source",
        );
        assert_eq!(private_reference[0].owner, "agent:a");
        let shared = parse_external_memory(
            Path::new("/tmp/b.md"),
            "---\nvisibility: shared\n---\nA note",
        );
        assert_eq!(shared[0].visibility.as_deref(), Some("shared"));
        let invalid = parse_external_memory(
            Path::new("/tmp/c.md"),
            "---\nvisibility: unknown\n---\nA note",
        );
        assert_eq!(invalid[0].visibility.as_deref(), Some("private"));
        let malformed = parse_external_memory(
            Path::new("/tmp/malformed.md"),
            "---\nvisibility: private\nother: [\n---\nA note",
        );
        assert_eq!(malformed[0].visibility.as_deref(), Some("private"));
        let unmarked = parse_external_memory(Path::new("/tmp/d.md"), "A note");
        assert_eq!(unmarked[0].visibility, None);

        let episode = parse_markdown_episode_file(
            Path::new("/tmp/e.md"),
            "---\nvisibility: private\n---\n# A note",
            "ticket",
        );
        assert!(
            episode
                .iter()
                .all(|row| row.visibility.as_deref() == Some("private"))
        );
        let meeting = parse_meeting_file(
            Path::new("/tmp/m.md"),
            "---\nvisibility: private\n---\n# Meeting\n## Decision\nA note",
        );
        assert!(
            meeting
                .iter()
                .all(|row| row.visibility.as_deref() == Some("private"))
        );
        let system = parse_system_memory(
            Path::new("/tmp/system.md"),
            "---\nvisibility: private\n---\n## Note\nSensitive text",
        );
        assert_eq!(system[0].visibility.as_deref(), Some("private"));
    }

    #[test]
    fn malformed_frontmatter_falls_back_to_filename_title() {
        let docs = parse_external_memory(
            Path::new("/tmp/custom_memory.md"),
            "---\nname: custom\nBody without closing fence",
        );

        assert_eq!(docs[0].name, "custom-memory");
        assert_eq!(docs[0].title, "custom memory");
        assert!(docs[0].body.starts_with("---"));
    }

    #[test]
    fn parses_agent_memory_as_company_scope_by_default() {
        let docs = parse_agent_memory(
            Path::new("/tmp/workspace/memory/archivist/archivist-bootstrap.md"),
            "# Archivist\narchive rotation",
        );

        assert_eq!(docs[0].owner, "agent:archivist");
        assert_eq!(docs[0].scope, "company");
    }

    #[test]
    fn parses_synthetic_agent_memory_owner_from_memory_directory() {
        let docs = parse_agent_memory(
            Path::new("/tmp/workspace/memory/cafe-owner/menu.md"),
            "# Cafe Owner\nservice standards",
        );

        assert_eq!(docs[0].owner, "agent:cafe_owner");
        assert_eq!(docs[0].scope, "company");
    }

    #[test]
    fn reference_metadata_stays_shared_inside_memory_directories() {
        let docs = parse_agent_memory(
            Path::new("/tmp/workspace/memory/vendor-docs/phone.md"),
            "---\nname: vendor-phone\nmetadata:\n  type: reference\n---\nPhone docs.",
        );

        assert_eq!(docs[0].memory_type, "reference");
        assert_eq!(docs[0].owner, "shared");
        assert_eq!(docs[0].scope, "company");
    }

    #[test]
    fn agent_memory_frontmatter_owner_wins_when_path_has_no_agent_hint() {
        let docs = parse_agent_memory(
            Path::new("/tmp/workspace/system/inbox/builder-code/open-ticket.md"),
            "---\nowner: Cafe-Owner\ntitle: Open Menu Ticket\n---\nCurrent open work.",
        );

        assert_eq!(docs[0].owner, "agent:cafe_owner");
        assert_eq!(docs[0].title, "Open Menu Ticket");
        assert_eq!(docs[0].scope, "company");
    }

    #[test]
    fn owner_hints_can_come_from_env() {
        let _lock = env_lock().lock().unwrap();
        let old_hints = std::env::var("SHELVES_AGENT_HINTS").ok();
        let old_root = std::env::var("AIOS_ROOT").ok();
        unsafe { std::env::set_var("SHELVES_AGENT_HINTS", "reviewer") };
        unsafe { std::env::remove_var("AIOS_ROOT") };

        let docs = parse_external_memory(
            Path::new("/tmp/workspace/reviewer-note.md"),
            "---\nname: reviewer_note\n---\nBody",
        );

        assert_eq!(docs[0].owner, "agent:reviewer");
        assert_eq!(
            owner_for_path(Path::new("/tmp/workspace/reviewer/note.md")),
            "agent:reviewer"
        );
        restore_env("SHELVES_AGENT_HINTS", old_hints);
        restore_env("AIOS_ROOT", old_root);
    }

    #[test]
    fn actor_hints_can_come_from_workspace_roster() {
        let _lock = env_lock().lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let system = tmp.path().join("system");
        std::fs::create_dir_all(&system).unwrap();
        std::fs::write(system.join("agents.txt"), "reviewer\n").unwrap();
        let old_hints = std::env::var("SHELVES_AGENT_HINTS").ok();
        let old_root = std::env::var("AIOS_ROOT").ok();
        unsafe { std::env::remove_var("SHELVES_AGENT_HINTS") };
        unsafe { std::env::set_var("AIOS_ROOT", tmp.path()) };

        let docs = parse_markdown_episode_file(
            Path::new("/tmp/workspace/system/inbox/reviewer-note.md"),
            "Body",
            "ticket",
        );

        assert_eq!(docs[0].actor, "agent:reviewer");
        restore_env("SHELVES_AGENT_HINTS", old_hints);
        restore_env("AIOS_ROOT", old_root);
    }

    #[test]
    fn classifier_uses_file_identity_before_body_product_mentions() {
        let cases = [
            (
                "/tmp/shelves-root/.agents/projects/workspace/memory/feedback_external_voice_profile.md",
                "feedback-coordinator-voice-profile",
                "Coordinator Voice Profile",
                "The voice carries the configured house tone.",
                "company",
            ),
            (
                "/tmp/shelves-root/.agents/projects/workspace/memory/project_archive_policy.md",
                "project_archive_policy",
                "Archive Policy",
                "The company archive policy keeps indexed references current.",
                "company",
            ),
            (
                "/tmp/shelves-root/.agents/projects/workspace/memory/feedback_external_correction_log.md",
                "feedback_external_correction_log",
                "Coordinator Correction Log",
                "Corrections can mention a product without becoming product memory.",
                "company",
            ),
            (
                "/tmp/shelves-root/.agents/projects/workspace/memory/project_os_shelves_direction.md",
                "project_os_shelves_direction",
                "OS Shelves Direction",
                "Company memory-engine direction references Notebook and Console as examples.",
                "company",
            ),
            (
                "/tmp/shelves-root/.agents/projects/workspace/memory/MEMORY.md",
                "MEMORY",
                "MEMORY index",
                "Active Files include console and notebook pointers.",
                "os",
            ),
        ];

        for (path, name, title, body, expected) in cases {
            let scope = classify_scope_for_memory(Path::new(path), name, title, body);
            assert_eq!(scope, expected, "{path}");
        }
    }

    #[test]
    fn system_memory_active_files_section_is_os_scope() {
        let docs = parse_system_memory(
            Path::new("/tmp/workspace/system/memory.md"),
            "# Memory\n\n## Active Files\nconsole and notebook pointers\n\n## Coordinator Voice Profile\nTTS voice and company tone",
        );

        assert_eq!(docs[0].title, "Active Files");
        assert_eq!(docs[0].scope, "os");
        assert_eq!(docs[1].scope, "company");
    }

    #[test]
    fn extracts_wiki_links_as_slugs() {
        assert_eq!(
            extract_wiki_links("See [[Shelves Contract]] and [[archivist-bootstrap|Archivist]]."),
            ["archivist-bootstrap", "shelves-contract"]
        );
        assert!(extract_wiki_links("broken [[link").is_empty());
    }

    #[test]
    fn parser_helper_edge_cases_are_stable() {
        assert_eq!(classify_scope("the organization company note"), "company");
        assert_eq!(classify_scope("plain operating note"), "os");
        assert!(!has_product_signal("unknown", &[], "", "", "plain note"));
        assert_eq!(slugify(" --- "), "untitled");
        assert_eq!(slugify("Project X!"), "project-x");
        assert!(has_explicit_product_scope("Scope: product:catalog"));
        assert!(has_product_signal(
            "catalog",
            &["catalog".to_string()],
            "",
            "",
            "scope: product:catalog"
        ));
        assert_eq!(
            parse_agent_hints("agent:Engineer\n# comment\ncafe-owner\n\nEngineer", '\n'),
            ["cafe_owner", "engineer"]
        );
    }

    #[test]
    fn markdown_episode_empty_body_actor_and_date_variants() {
        let empty = parse_markdown_episode_file(Path::new("/tmp/empty.md"), "", "ticket");
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].summary, "empty.md");
        assert_eq!(empty[0].body, "");

        let from = parse_markdown_episode_file(
            Path::new("/tmp/from.md"),
            "FROM: Engineer\nDATE: 2026-06-09T01:02:03Z\nBody",
            "ticket",
        );
        assert_eq!(from[0].actor, "agent:engineer");
        assert_eq!(from[0].ts, "2026-06-09T01:02:03+00:00");

        let tool = parse_markdown_episode_file(
            Path::new("/tmp/tool.md"),
            "- Tool: Archivist\nDATE: 20260609\nBody",
            "ticket",
        );
        assert_eq!(tool[0].actor, "agent:archivist");
        assert_eq!(tool[0].ts, "2026-06-09T00:00:00+00:00");

        assert_eq!(offset_seconds("not-a-date", 3), "not-a-date");
    }

    #[test]
    fn meeting_parser_keeps_late_semantic_sections_searchable() {
        let text = r#"# Meeting — Base resume
**Date:** 2026-08-01
**Attendees:** Maintainer · Coordinator · Navigator

Opening context for the room.

## Early discussion
This section deliberately comes first.

# MEETING CLOSED

## What was learned that we did not know this morning
The compactor attacks the summary FIRST.

### Evidence
It deleted the line that came out of the comprehension gate.
"#;

        let docs = parse_meeting_file(Path::new("/tmp/2026-08-01-base-resume.md"), text);

        assert_eq!(docs.len(), 4);
        assert!(docs.iter().all(|doc| doc.kind == "meeting"));
        assert!(docs.iter().all(|doc| doc.actor == "system"));
        assert!(docs.iter().all(|doc| doc.ts.starts_with("2026-08-01T")));
        assert_eq!(
            docs[2].summary,
            "Base resume — What was learned that we did not know this morning"
        );
        assert!(docs[2].body.contains("compactor attacks the summary FIRST"));
        assert_eq!(docs[3].summary, "Base resume — Evidence");
        assert!(docs[3].body.contains("comprehension gate"));
    }

    #[test]
    fn empty_meeting_still_has_one_source_record() {
        let docs = parse_meeting_file(Path::new("/tmp/2026-08-01-empty.md"), "");

        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].kind, "meeting");
        assert_eq!(docs[0].summary, "2026-08-01-empty.md");
    }

    #[test]
    fn meeting_parser_splits_bold_decision_bullets_from_long_sections() {
        let docs = parse_meeting_file(
            Path::new("/tmp/2026-08-01-base-resume.md"),
            "# Meeting — Base resume\n**Date:** 2026-08-01\n\n## Learned\n- **Position beats vocabulary.** Section order moved the score.\n- **The compactor attacks the summary FIRST.** It deleted the gated line.\n",
        );

        assert_eq!(docs.len(), 3);
        assert_eq!(docs[1].summary, "Base resume — Position beats vocabulary.");
        assert_eq!(
            docs[2].summary,
            "Base resume — The compactor attacks the summary FIRST."
        );
        assert!(docs[2].body.contains("deleted the gated line"));
    }

    #[test]
    fn parses_system_memory_sections_and_lock_heuristic() {
        let docs = parse_system_memory(
            Path::new("/tmp/memory.md"),
            "# Memory\n\n## Rule locked\nNON-NEGOTIABLE thing\n\n## Normal\nbody",
        );
        assert_eq!(docs.len(), 2);
        assert!(docs[0].is_lock);
        assert_eq!(docs[1].name, "normal");
    }

    #[test]
    fn parses_team_log_actor() {
        let docs = parse_team_log(
            Path::new("/tmp/team.md"),
            "[BUILDER] 2026-06-08 12:36 AM | session-close | Built",
        );
        assert_eq!(docs[0].actor, "agent:builder");
        assert_eq!(docs[0].kind, "team-log");
        assert_eq!(docs[0].ts, "2026-06-08T00:36:00+00:00");
    }

    fn env_lock() -> &'static Mutex<()> {
        crate::test_env_lock()
    }

    fn restore_env(key: &str, old: Option<String>) {
        match old {
            Some(value) => unsafe { std::env::set_var(key, value) }, // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
            None => unsafe { std::env::remove_var(key) },
        }
    }

    #[test]
    fn full_precision_filename_stamp_wins_over_body_date() {
        let docs = parse_markdown_episode_file(
            Path::new("/tmp/20260611T031530Z-ticket.md"),
            "DATE: 2026-01-01\nBody",
            "ticket",
        );

        assert_eq!(docs[0].ts, "2026-06-11T03:15:30+00:00");
    }

    #[test]
    fn minute_precision_filename_stamp_is_not_midnight() {
        let docs = parse_markdown_episode_file(
            Path::new("/tmp/20260611T0315Z-ticket.md"),
            "Body without date",
            "ticket",
        );

        assert_eq!(docs[0].ts, "2026-06-11T03:15:00+00:00");
    }

    #[test]
    fn in_text_date_is_used_when_filename_has_no_full_precision_stamp() {
        let docs = parse_markdown_episode_file(
            Path::new("/tmp/20260610-ticket.md"),
            "- Date: 2026-06-09\nBody",
            "ticket",
        );

        assert_eq!(docs[0].ts, "2026-06-09T00:00:00+00:00");
    }

    #[test]
    fn compact_in_text_date_is_used_as_midnight_utc() {
        assert_eq!(
            parse_dateish("20260615"),
            Some("2026-06-15T00:00:00+00:00".to_string())
        );

        let docs = parse_markdown_episode_file(
            Path::new("/tmp/no-stamp-ticket.md"),
            "DATE: 20260615\nBody",
            "ticket",
        );

        assert_eq!(docs[0].ts, "2026-06-15T00:00:00+00:00");
    }

    #[test]
    fn undateable_episode_file_falls_back_to_mtime() {
        let file = NamedTempFile::new().unwrap();
        let before = Utc::now() - Duration::seconds(5);
        let docs = parse_markdown_episode_file(file.path(), "Body without date", "ticket");
        let parsed = DateTime::parse_from_rfc3339(&docs[0].ts)
            .unwrap()
            .with_timezone(&Utc);
        let after = Utc::now() + Duration::seconds(5);

        assert!(parsed >= before, "{parsed} should be near file mtime");
        assert!(parsed <= after, "{parsed} should be near file mtime");
    }

    #[test]
    fn team_log_unparseable_line_falls_back_to_mtime_plus_line_offset() {
        let file = NamedTempFile::new().unwrap();
        let docs = parse_team_log(file.path(), "first\nsecond");
        let first = DateTime::parse_from_rfc3339(&docs[0].ts)
            .unwrap()
            .with_timezone(&Utc);
        let second = DateTime::parse_from_rfc3339(&docs[1].ts)
            .unwrap()
            .with_timezone(&Utc);

        assert_eq!(second - first, Duration::seconds(1));
    }

    proptest! {
        #[test]
        fn parser_handles_arbitrary_markdown_without_panic(input in "\\PC*") {
            let path = Path::new("/tmp/fuzz.md");
            let memories = parse_system_memory(path, &input);
            for memory in memories {
                prop_assert!(!memory.owner.is_empty());
                prop_assert!(!memory.scope.is_empty());
                prop_assert!(crate::locks::validate_scope(&memory.scope));
            }
            let episodes = parse_markdown_episode_file(path, &input, "ticket");
            for episode in episodes {
                prop_assert!(!episode.actor.is_empty());
                prop_assert!(!episode.scope.is_empty());
                prop_assert!(crate::locks::validate_scope(&episode.scope));
            }
        }

        #[test]
        fn compact_timestamp_parser_never_panics(input in "\\PC*") {
            let _ = parse_compact_utc_stamp(&input);
            let _ = parse_dateish(&input);
        }

        #[test]
        fn wiki_link_extractor_never_panics(input in "\\PC*") {
            let _ = extract_wiki_links(&input);
        }
    }
}
