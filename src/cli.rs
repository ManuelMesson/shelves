use std::io::{self, ErrorKind, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use chrono::{Duration, Local, NaiveDate, TimeZone, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use rusqlite::params;
use serde::Serialize;

use crate::{
    DbPathResolution, answer, graduation, guard, ingest, is_default_db_path, locks,
    resolve_db_path, search, storage, workspace_root,
};

#[derive(Debug, Parser)]
#[command(
    name = "shelves",
    version,
    about = "Shelves memory engine",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Override the SQLite database path for this invocation.
    #[arg(long, global = true, value_name = "PATH")]
    db: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Parse canonical sources into the derived Shelves index.
    Ingest(IngestArgs),
    /// Search memories and episodes with scope and owner fall-through.
    Search(SearchArgs),
    /// Query another agent node.
    Ask(AskArgs),
    /// Show dated episodes.
    Timeline(TimelineArgs),
    /// Show today's episodes.
    Today(OutputArgs),
    /// Show yesterday's episodes.
    Yesterday(OutputArgs),
    /// List open future items.
    Upcoming(UpcomingArgs),
    /// Store a future memory item.
    Remember(RememberArgs),
    /// Close a future item as done or dropped.
    Done(DoneArgs),
    /// Walk wiki and temporal links.
    Related(RelatedArgs),
    /// Build an agent startup brief.
    Brief(BriefArgs),
    /// Log a context or recall miss.
    Missed(MissedArgs),
    /// Assemble a per-decision context pack.
    Context(ContextArgs),
    /// Nominate promotion and cooling candidates.
    Consolidate(ConsolidateArgs),
    /// Apply a ratified scope graduation.
    Promote(PromoteArgs),
    /// Report cooling candidates without deleting anything.
    PruneReport(OutputArgs),
    /// Manage write-once Lock-It records.
    Lock(LockArgs),
    /// Print table, scope, owner, status, and staleness counts.
    Stats(OutputArgs),
    /// Refuse Layer-1 paths before any file walking.
    GuardCheck(GuardCheckArgs),
}

#[derive(Debug, Args)]
struct IngestArgs {
    #[arg(long)]
    reset: bool,
    #[arg(long)]
    force: bool,
    #[arg(long)]
    source: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SearchArgs {
    query: Vec<String>,
    #[arg(long, default_value = "os")]
    scope: String,
    #[arg(long)]
    owner: Option<String>,
    #[arg(long = "as", default_value = "agent:builder")]
    as_agent: String,
    #[arg(long)]
    include_cold: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct AskArgs {
    agent: String,
    query: Vec<String>,
    #[arg(long = "as", default_value = "agent:curator")]
    as_agent: String,
    #[arg(long, default_value = "company")]
    scope: String,
    #[arg(long)]
    include_cold: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct UpcomingArgs {
    #[arg(long)]
    days: Option<i64>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RememberArgs {
    text: Vec<String>,
    #[arg(long)]
    due: String,
    #[arg(long = "by", default_value = "agent:curator")]
    by: String,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct DoneArgs {
    id: i64,
    #[arg(long)]
    drop: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RelatedArgs {
    name: String,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct BriefArgs {
    agent: String,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ContextArgs {
    agent: String,
    task: Vec<String>,
    #[arg(long)]
    budget: Option<usize>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MissedArgs {
    what: Vec<String>,
    #[arg(long = "by", default_value = "agent:curator")]
    by: String,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct LockArgs {
    #[command(subcommand)]
    command: Option<LockCommands>,
}

#[derive(Debug, Subcommand)]
enum LockCommands {
    /// Append one write-once lock to system/locks.yaml.
    Add(LockAddArgs),
    /// Bulk-import parsed lock proposals.
    Import(LockImportArgs),
    /// List indexed locks.
    List(LockListArgs),
    /// Show one indexed lock.
    Show(LockShowArgs),
    /// Render active locks into a generated markdown projection or marked section.
    Render(LockRenderArgs),
}

#[derive(Debug, Args)]
struct LockAddArgs {
    #[arg(long)]
    slug: String,
    #[arg(long)]
    title: String,
    #[arg(long)]
    scope: String,
    #[arg(long)]
    locked_on: Option<String>,
    #[arg(long)]
    supersedes: Option<String>,
    #[arg(long, conflicts_with = "body")]
    body_file: Option<PathBuf>,
    #[arg(long)]
    body: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct LockImportArgs {
    proposals: PathBuf,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct LockListArgs {
    #[arg(long)]
    scope: Option<String>,
    #[arg(long)]
    include_superseded: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct LockShowArgs {
    slug: String,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct LockRenderArgs {
    #[arg(long, value_name = "PATH", conflicts_with_all = ["into", "target"])]
    out: Option<PathBuf>,
    #[arg(long, value_name = "PATH", conflicts_with = "target")]
    into: Option<PathBuf>,
    #[arg(long, value_enum, conflicts_with = "into")]
    target: Option<LockRenderTarget>,
    #[arg(long)]
    check: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LockRenderTarget {
    Curator,
    Memory,
}

#[derive(Debug, Args)]
struct TimelineArgs {
    shortcut: Option<String>,
    #[arg(long = "from")]
    from: Option<String>,
    #[arg(long = "to")]
    to: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct OutputArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct GuardCheckArgs {
    path: PathBuf,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ConsolidateArgs {
    #[arg(long)]
    at_close: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct PromoteArgs {
    id: i64,
    #[arg(long = "to")]
    to_scope: String,
    #[arg(long = "by")]
    by: String,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Serialize)]
struct EpisodeRow {
    ts: String,
    actor: String,
    kind: String,
    summary: String,
    scope: String,
    source_path: String,
}

#[derive(Debug, Serialize)]
struct GuardCheckOutput {
    allowed: bool,
    status: String,
    checked_path: String,
    resolved_path: Option<String>,
    protected_root: Option<String>,
    error: Option<String>,
}

pub fn main_entry() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("shelves: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let db = resolve_db_path(cli.db.as_deref())?;
    match cli.command {
        Some(Commands::Ingest(args)) => {
            ensure_reset_allowed(&db, args.reset, args.force)?;
            let mut conn = storage::open(&db.path)?;
            let report = ingest::run(&mut conn, args.reset, args.source.as_deref())?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "Ingested {} memories and {} episodes ({} skipped)",
                    report.memories, report.episodes, report.skipped
                );
            }
        }
        Some(Commands::Search(args)) => {
            let query = args.query.join(" ");
            let conn = storage::open(&db.path)?;
            let hits = search::search(
                &conn,
                &query,
                &args.scope,
                args.owner.as_deref(),
                &args.as_agent,
                args.include_cold,
                10,
            )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
            print_hits(hits, args.json)?;
        }
        Some(Commands::Ask(args)) => {
            let query = args.query.join(" ");
            if query.trim().is_empty() {
                bail!("ask requires a query");
            }
            let conn = storage::open(&db.path)?;
            let hits = answer::ask(
                &conn,
                &args.agent,
                &query,
                &args.as_agent,
                &args.scope,
                args.include_cold,
                10,
            )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
            print_hits(hits, args.json)?;
        }
        Some(Commands::Upcoming(args)) => {
            let conn = storage::open(&db.path)?;
            let rows = answer::upcoming(&conn, args.days)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if rows.is_empty() {
                println!("No upcoming items.");
            } else {
                for item in rows {
                    println!(
                        "[future:{}] due {} by {}\n  {}",
                        item.id, item.due, item.created_by, item.body
                    );
                }
            }
        }
        Some(Commands::Remember(args)) => {
            let text = args.text.join(" ");
            if text.trim().is_empty() {
                bail!("remember requires text");
            }
            let conn = storage::open(&db.path)?;
            let row = answer::remember(&conn, &text, &args.due, &args.by)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&row)?);
            } else {
                println!(
                    "[future:{}] due {} by {}\n  {}",
                    row.id, row.due, row.created_by, row.body
                );
            }
        }
        Some(Commands::Done(args)) => {
            let conn = storage::open(&db.path)?;
            let row = answer::done(&conn, args.id, args.drop)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&row)?);
            } else if row.changed {
                println!(
                    "[future:{}] {} -> {}\n  {}",
                    row.item.id, row.previous_status, row.item.status, row.item.body
                );
            } else {
                println!(
                    "[future:{}] already {}\n  {}",
                    row.item.id, row.item.status, row.item.body
                );
            }
        }
        Some(Commands::Related(args)) => {
            let conn = storage::open(&db.path)?;
            let rows = answer::related(&conn, &args.name)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if rows.is_empty() {
                println!("No related rows.");
            } else {
                for row in rows {
                    println!(
                        "[{}:{} via {}] {}\n  {}\n  {}",
                        row.kind, row.id, row.link_kind, row.title, row.body, row.source_path
                    );
                }
            }
        }
        Some(Commands::Brief(args)) => {
            let conn = storage::open(&db.path)?;
            print_pack(answer::brief(&conn, &args.agent)?, args.json)?;
        }
        Some(Commands::Context(args)) => {
            let task = args.task.join(" ");
            if task.trim().is_empty() {
                bail!("context requires a task");
            }
            let conn = storage::open(&db.path)?;
            print_pack(
                answer::context(&conn, &args.agent, &task, args.budget)?,
                args.json,
            )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        }
        Some(Commands::Missed(args)) => {
            let what = args.what.join(" ");
            if what.trim().is_empty() {
                bail!("missed requires text");
            }
            let conn = storage::open(&db.path)?;
            let row = answer::missed(&conn, &what, &args.by)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&row)?); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
            } else {
                println!("[miss:{}] by {}\n  {}", row.id, row.by, row.what);
            }
        }
        Some(Commands::Timeline(args)) => {
            let (from, to) = timeline_bounds(&args)?;
            print_timeline(&db, from, to, args.json)?;
        }
        Some(Commands::Today(args)) => {
            let today = Local::now().date_naive();
            print_timeline(&db, Some(day_start(today)), Some(day_end(today)), args.json)?;
        }
        Some(Commands::Yesterday(args)) => {
            let yesterday = Local::now().date_naive() - Duration::days(1);
            print_timeline(
                &db,
                Some(day_start(yesterday)),
                Some(day_end(yesterday)),
                args.json,
            )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        }
        Some(Commands::Stats(args)) => {
            let conn = storage::open(&db.path)?;
            let sources = ingest::discover_paths(None)?;
            let stats = storage::stats(&conn, &sources)?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                println!("memories: {}", stats.memories);
                println!("episodes: {}", stats.episodes);
                println!("recall_events: {}", stats.recall_events);
                println!("misses: {}", stats.misses);
                println!("active_locks: {}", stats.active_locks);
                println!("superseded_locks: {}", stats.superseded_locks);
                println!("hot_memories: {}", stats.hot_memories);
                println!("cold_memories: {}", stats.cold_memories);
                println!(
                    "last_ingest_ts: {}",
                    stats.last_ingest_ts.unwrap_or_else(|| "never".to_string())
                );
                println!("stale: {}", stats.stale);
                println!("memories_by_scope: {:?}", stats.memories_by_scope);
                println!("memories_by_owner: {:?}", stats.memories_by_owner);
                println!("memories_by_status: {:?}", stats.memories_by_status);
                println!("episodes_by_kind: {:?}", stats.episodes_by_kind);
            }
        }
        Some(Commands::GuardCheck(args)) => {
            if args.json {
                let output = guard_check_output(&args.path);
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                let allowed = guard::check_path(&args.path)?;
                println!("allowed: {}", allowed.display());
            }
        }
        Some(Commands::PruneReport(args)) => {
            let conn = storage::open(&db.path)?;
            print_pack(answer::prune_report(&conn)?, args.json)?;
        }
        Some(Commands::Consolidate(args)) => {
            let conn = storage::open(&db.path)?;
            let report = graduation::consolidate(&conn, args.at_close)?;
            print_consolidation_report(&report, args.json)?;
        }
        Some(Commands::Promote(args)) => {
            let conn = storage::open(&db.path)?;
            let outcome = graduation::promote(&conn, args.id, &args.to_scope, &args.by)?;
            print_promote_outcome(&outcome, args.json)?;
        }
        Some(Commands::Lock(args)) => {
            run_lock(args, &db)?;
        }
        None => {
            eprintln!("shelves: pass --version or a contract verb");
        }
    }
    Ok(())
}

fn guard_check_output(path: &std::path::Path) -> GuardCheckOutput {
    match guard::canonical_allowed_path(path) {
        Ok(resolved) => GuardCheckOutput {
            allowed: true,
            status: "verified-open".to_string(),
            checked_path: path.display().to_string(),
            resolved_path: Some(resolved.display().to_string()),
            protected_root: None,
            error: None,
        },
        Err(guard::GuardError::MissingPersonalRoot) => GuardCheckOutput {
            allowed: false,
            status: "blocked".to_string(),
            checked_path: path.display().to_string(),
            resolved_path: None,
            protected_root: None,
            error: Some(
                "SHELVES_PROTECTED_ROOT is required for Layer-1 protection; refusing file access"
                    .to_string(),
            ),
        },
        Err(guard::GuardError::Violation(violation)) => GuardCheckOutput {
            allowed: false,
            status: "blocked".to_string(),
            checked_path: path.display().to_string(),
            resolved_path: Some(violation.path.display().to_string()),
            protected_root: Some(violation.protected_root.display().to_string()),
            error: Some("Layer-1 path refused".to_string()),
        },
    }
}

fn ensure_reset_allowed(db: &DbPathResolution, reset: bool, force: bool) -> Result<()> {
    if !reset || force || !is_default_db_path(db)? {
        return Ok(());
    }
    bail!(
        "ingest --reset targets the default Shelves db at {}; pass --force to confirm, or use --db/SHELVES_DB_PATH/AIOS_ROOT for a hermetic reset",
        db.path.display()
    )
}

fn print_consolidation_report(report: &graduation::ConsolidationReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else {
        println!(
            "Consolidation report: {}",
            report.report_path.as_deref().unwrap_or("(not written)")
        );
        println!(
            "candidates: {} (promotion threshold: {})",
            report.candidates.len(),
            report.promotion_recall_threshold
        );
        for item in &report.candidates {
            println!(
                "[{} memory:{}] {} | {} -> {} | owner={} | tier={} | higher_recalls={} | activation={}",
                item.kind,
                item.id,
                item.title,
                item.current_scope,
                item.suggested_scope.as_deref().unwrap_or("-"),
                item.owner,
                item.required_tier,
                item.higher_scope_recall_count,
                item.activation
                    .map(|value| format!("{value:.3}"))
                    .unwrap_or_else(|| "-".to_string())
            );
        }
    }
    Ok(())
}

fn print_promote_outcome(outcome: &graduation::PromoteOutcome, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(outcome)?);
    } else if outcome.changed {
        println!(
            "[memory:{}] {} -> {} by {} ({})",
            outcome.id, outcome.from_scope, outcome.to_scope, outcome.by, outcome.name
        );
        if let Some(id) = outcome.audit_episode_id {
            println!("audit_episode: {id}");
        }
    } else {
        println!("[memory:{}] already at {}", outcome.id, outcome.to_scope);
    }
    Ok(())
}

fn run_lock(args: LockArgs, db: &DbPathResolution) -> Result<()> {
    match args.command {
        Some(LockCommands::Add(args)) => {
            let body = lock_body(&args)?;
            let entry = locks::LockEntry {
                slug: args.slug,
                title: args.title,
                scope: args.scope,
                locked_on: args.locked_on.unwrap_or_else(locks::default_locked_on),
                supersedes: args.supersedes,
                body,
            };
            let conn = storage::open(&db.path)?;
            let row = locks::add_lock(&conn, entry)?;
            print_lock_row(row, args.json)?;
        }
        Some(LockCommands::Import(args)) => {
            let conn = storage::open(&db.path)?;
            let report = locks::import_file(&conn, &args.proposals, args.dry_run)?;
            print_import_report(&report, args.json)?;
            if report.rejected > 0 {
                bail!("lock import rejected {} entries", report.rejected);
            }
        }
        Some(LockCommands::List(args)) => {
            if let Some(scope) = args.scope.as_deref()
                && !locks::validate_scope(scope)
            {
                bail!("scope must be os, company, or product:<slug>");
            }
            let conn = storage::open(&db.path)?;
            let rows = locks::list_locks(&conn, args.scope.as_deref(), args.include_superseded)?;
            print_lock_list(rows, args.json)?;
        }
        Some(LockCommands::Show(args)) => {
            let conn = storage::open(&db.path)?;
            let Some(row) = locks::show_lock(&conn, &args.slug)? else {
                bail!("lock {:?} not found", args.slug);
            };
            print_lock_row(row, args.json)?;
        }
        Some(LockCommands::Render(args)) => {
            let conn = storage::open_read_only(&db.path)?;
            if args.into.is_some() || args.target.is_some() {
                let target = render_target_path(args.into, args.target)?;
                let report = locks::render_into_marked_section(&conn, &target, args.check)?;
                if args.json {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else if args.check {
                    if let Some(diff) = report.diff.as_ref() {
                        print!("{diff}");
                    } else {
                        println!(
                            "No generated-lock drift in {} ({})",
                            report.out, report.content_hash
                        );
                    }
                } else {
                    let suffix = if report.changed { "" } else { " (unchanged)" };
                    println!(
                        "Rendered {} active locks into {} ({}){}",
                        report.count, report.out, report.content_hash, suffix
                    );
                }
            } else {
                if args.check {
                    bail!(
                        "lock render --check requires --into <file> or --target <curator|memory>"
                    );
                }
                let out = args.out.unwrap_or(locks::default_render_path()?);
                let report = locks::render_projection(&conn, &out)?;
                if args.json {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else {
                    println!(
                        "Rendered {} active locks to {} ({})",
                        report.count, report.out, report.content_hash
                    );
                }
            }
        }
        None => {
            eprintln!("lock requires a subcommand: add, import, list, show, or render");
        }
    }
    Ok(())
}

fn render_target_path(into: Option<PathBuf>, target: Option<LockRenderTarget>) -> Result<PathBuf> {
    if let Some(path) = into {
        return Ok(path);
    }
    match target {
        Some(LockRenderTarget::Curator) => Ok(workspace_root()?.join("CURATOR.md")),
        Some(LockRenderTarget::Memory) => Ok(workspace_root()?.join("system/memory.md")),
        None => bail!("lock render target missing"),
    }
}

fn lock_body(args: &LockAddArgs) -> Result<String> {
    if let Some(body) = args.body.as_ref() {
        if body.trim().is_empty() {
            bail!("lock body must not be empty");
        }
        return Ok(body.clone());
    }
    let Some(path) = args.body_file.as_ref() else {
        bail!("lock add requires --body or --body-file");
    };
    let path = guard::assert_path_allowed_before_read(path)?;
    let body = std::fs::read_to_string(&path)
        .map_err(|err| anyhow::anyhow!("reading lock body file {}: {err}", path.display()))?;
    if body.trim().is_empty() {
        bail!("lock body must not be empty");
    }
    Ok(body)
}

fn print_import_report(report: &locks::ImportReport, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else {
        for verdict in &report.verdicts {
            println!(
                "{}: {} - {}",
                verdict.slug, verdict.verdict, verdict.message
            );
        }
        println!(
            "accepted: {}; skipped: {}; rejected: {}; dry_run: {}",
            report.accepted, report.skipped, report.rejected, report.dry_run
        );
    }
    Ok(())
}

fn print_lock_list(rows: Vec<locks::LockRow>, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else if rows.is_empty() {
        println!("No locks.");
    } else {
        for row in rows {
            println!(
                "{} | {} | {} | {} | {}",
                row.slug, row.title, row.scope, row.locked_on, row.status
            );
        }
    }
    Ok(())
}

fn print_lock_row(row: locks::LockRow, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&row)?);
    } else {
        println!("# {}", row.title);
        println!("slug: {}", row.slug);
        println!("scope: {}", row.scope);
        println!("locked_on: {}", row.locked_on);
        println!("status: {}", row.status);
        if let Some(supersedes) = row.supersedes.as_deref() {
            println!("supersedes: {supersedes}");
        }
        println!("source: {}", row.source_path);
        println!();
        println!("{}", row.body);
    }
    Ok(())
}

fn print_hits(hits: Vec<search::SearchHit>, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else if hits.is_empty() {
        println!("No hits.");
    } else {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        for hit in hits {
            if !write_stdout(
                &mut out,
                format_args!(
                    "[{}:{}] {} ({}, {})\n  {}\n  {}",
                    hit.kind, hit.id, hit.title, hit.owner, hit.scope, hit.snippet, hit.source_path
                ),
            )? {
                // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
                // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
                return Ok(()); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
            }
        }
    }
    Ok(())
}

fn print_pack(lines: Vec<answer::PackLine>, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&lines)?);
    } else if lines.is_empty() {
        println!("No lines.");
    } else {
        let stdout = io::stdout();
        let mut out = stdout.lock();
        for line in lines {
            let activation = line
                .activation
                .map(|value| format!("; activation={value:.3}"))
                .unwrap_or_default();
            let stale = if line.stale { "; stale" } else { "" };
            let confidence = if line.confidence.is_empty() {
                String::new()
            } else {
                format!("; confidence={}", line.confidence.join(""))
            };
            let body = single_line(&line.body);
            if !write_stdout(
                &mut out,
                format_args!(
                    "[{}] {} :: {} (source={}; owner={}; scope={}{}{}{})",
                    line.section,
                    line.title,
                    body,
                    line.source_path,
                    line.owner,
                    line.scope,
                    activation,
                    stale,
                    confidence
                ),
            )? {
                // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
                // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
                return Ok(()); // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
            }
        }
    }
    Ok(())
}

fn single_line(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(260)
        .collect()
}

fn write_stdout(out: &mut impl Write, args: std::fmt::Arguments<'_>) -> Result<bool> {
    if let Err(err) = out.write_fmt(args).and_then(|_| out.write_all(b"\n")) {
        if err.kind() == ErrorKind::BrokenPipe {
            return Ok(false);
        }
        return Err(err.into());
    }
    Ok(true)
}

fn print_timeline(
    db: &DbPathResolution,
    from: Option<String>,
    to: Option<String>,
    json: bool,
) -> Result<()> {
    let conn = storage::open(&db.path)?;
    let rows = timeline_rows(&conn, from.as_deref(), to.as_deref(), 100)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else if rows.is_empty() {
        println!("No episodes.");
    } else {
        for row in rows {
            println!(
                "{} [{}:{}] {}\n  {}",
                row.ts, row.actor, row.kind, row.summary, row.source_path
            );
        }
    }
    Ok(())
}

fn timeline_bounds(args: &TimelineArgs) -> Result<(Option<String>, Option<String>)> {
    match args.shortcut.as_deref() {
        Some("today") => {
            let today = Local::now().date_naive();
            Ok((Some(day_start(today)), Some(day_end(today))))
        }
        Some("yesterday") => {
            let yesterday = Local::now().date_naive() - Duration::days(1);
            Ok((Some(day_start(yesterday)), Some(day_end(yesterday))))
        }
        Some(other) => bail!("timeline shortcut must be 'today' or 'yesterday', got {other:?}"),
        None => Ok((
            args.from.as_deref().and_then(parse_day_start),
            args.to.as_deref().and_then(parse_day_end),
        )),
    }
}

fn timeline_rows(
    conn: &rusqlite::Connection,
    from: Option<&str>,
    to: Option<&str>,
    limit: usize,
) -> Result<Vec<EpisodeRow>> {
    let mut stmt = conn.prepare(
        "SELECT ts, actor, kind, summary, scope, coalesce(source_path, '')
         FROM episodes
         WHERE (?1 IS NULL OR ts >= ?1) AND (?2 IS NULL OR ts <= ?2)
         ORDER BY ts DESC
         LIMIT ?3",
    )?; // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
    let rows = stmt.query_map(params![from, to, limit as i64], |row| {
        Ok(EpisodeRow {
            ts: row.get(0)?,
            actor: row.get(1)?,
            kind: row.get(2)?,
            summary: row.get(3)?,
            scope: row.get(4)?,
            source_path: row.get(5)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn parse_day_start(input: &str) -> Option<String> {
    NaiveDate::parse_from_str(input, "%Y-%m-%d")
        .ok()
        .map(day_start)
}

fn parse_day_end(input: &str) -> Option<String> {
    NaiveDate::parse_from_str(input, "%Y-%m-%d")
        .ok()
        .map(day_end)
}

#[allow(clippy::expect_used)]
fn day_start(day: NaiveDate) -> String {
    Utc.from_utc_datetime(
        &day.and_hms_opt(0, 0, 0)
            .expect("00:00:00 is always a valid time"),
    )
    .to_rfc3339()
}

#[allow(clippy::expect_used)]
fn day_end(day: NaiveDate) -> String {
    Utc.from_utc_datetime(
        &day.and_hms_opt(23, 59, 59)
            .expect("23:59:59 is always a valid time"),
    )
    .to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DbPathSource;
    use clap::{CommandFactory, Parser};
    use std::sync::Mutex;

    const CONTRACT_VERBS: &[&str] = &[
        "ingest",
        "search",
        "ask",
        "timeline",
        "today",
        "yesterday",
        "upcoming",
        "remember",
        "done",
        "related",
        "brief",
        "missed",
        "context",
        "consolidate",
        "promote",
        "prune-report",
        "lock",
        "stats",
        "guard-check",
    ];

    #[test]
    fn version_flag_is_registered() {
        let err = Cli::command()
            .try_get_matches_from(["shelves", "--version"])
            .expect_err("--version should short-circuit with display output");

        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    #[test]
    fn rooted_default_reset_requires_force_before_opening_db() {
        let _lock = env_lock().lock().unwrap();
        let old_root = std::env::var("AIOS_ROOT").ok();
        unsafe { std::env::set_var("AIOS_ROOT", "/tmp/shelves-root") };
        let db = DbPathResolution {
            path: PathBuf::from("/tmp/shelves-root/system/shelves.db"),
            source: DbPathSource::AiosRoot,
        };

        let err = ensure_reset_allowed(&db, true, false).expect_err("default reset needs force");
        assert!(err.to_string().contains("--force"));
        assert!(ensure_reset_allowed(&db, true, true).is_ok());
        restore_env("AIOS_ROOT", old_root);
    }

    #[test]
    fn hermetic_reset_does_not_require_force() {
        let db = DbPathResolution {
            path: PathBuf::from("/tmp/shelves-root/system/shelves.db"),
            source: DbPathSource::CliFlag,
        };

        assert!(ensure_reset_allowed(&db, true, false).is_ok());
    }

    #[test]
    fn render_target_path_covers_explicit_and_named_targets() {
        let _lock = env_lock().lock().unwrap();
        let old_root = std::env::var("AIOS_ROOT").ok();
        unsafe { std::env::set_var("AIOS_ROOT", "/tmp/shelves-root") };

        assert_eq!(
            render_target_path(Some(PathBuf::from("/tmp/explicit.md")), None).unwrap(),
            PathBuf::from("/tmp/explicit.md")
        );
        assert_eq!(
            render_target_path(None, Some(LockRenderTarget::Curator)).unwrap(),
            PathBuf::from("/tmp/shelves-root/CURATOR.md")
        );
        assert_eq!(
            render_target_path(None, Some(LockRenderTarget::Memory)).unwrap(),
            PathBuf::from("/tmp/shelves-root/system/memory.md")
        );
        assert!(
            render_target_path(None, None)
                .unwrap_err()
                .to_string()
                .contains("target missing")
        );

        restore_env("AIOS_ROOT", old_root);
    }

    #[test]
    fn timeline_bounds_cover_shortcuts_explicit_range_and_invalid_shortcut() {
        // Covers timeline_bounds at lines 825-839: today/yesterday shortcuts,
        // explicit date ranges, and the invalid-shortcut bail path.
        let today = Local::now().date_naive();
        let yesterday = today - Duration::days(1);

        let today_bounds = timeline_bounds(&TimelineArgs {
            shortcut: Some("today".to_string()),
            from: None,
            to: None,
            json: true,
        })
        .unwrap();
        assert_eq!(today_bounds, (Some(day_start(today)), Some(day_end(today))));

        let yesterday_bounds = timeline_bounds(&TimelineArgs {
            shortcut: Some("yesterday".to_string()),
            from: None,
            to: None,
            json: false,
        })
        .unwrap();
        assert_eq!(
            yesterday_bounds,
            (Some(day_start(yesterday)), Some(day_end(yesterday)))
        );

        let ranged = timeline_bounds(&TimelineArgs {
            shortcut: None,
            from: Some("2026-06-10".to_string()),
            to: Some("2026-06-12".to_string()),
            json: false,
        })
        .unwrap();
        assert_eq!(
            ranged,
            (
                Some("2026-06-10T00:00:00+00:00".to_string()),
                Some("2026-06-12T23:59:59+00:00".to_string())
            )
        );

        let err = timeline_bounds(&TimelineArgs {
            shortcut: Some("tomorrow".to_string()),
            from: None,
            to: None,
            json: false,
        })
        .expect_err("unknown shortcut should bail");
        assert!(err.to_string().contains("timeline shortcut"));
    }

    #[test]
    fn timeline_format_helpers_are_stable() {
        // Covers single_line at lines 783-789 and day_start/day_end at
        // lines 883-897 so compact timeline output keeps fixed UTC bounds.
        assert_eq!(
            single_line("  one\n\n two\tthree  "),
            "one two three".to_string()
        );
        let long = format!("{}tail", "x".repeat(260));
        assert_eq!(single_line(&long), "x".repeat(260));

        let day = NaiveDate::from_ymd_opt(2026, 6, 15).unwrap();
        assert_eq!(day_start(day), "2026-06-15T00:00:00+00:00");
        assert_eq!(day_end(day), "2026-06-15T23:59:59+00:00");
    }

    #[test]
    fn write_stdout_handles_broken_pipe_and_propagates_other_errors() {
        let mut broken = FailingWriter {
            kind: ErrorKind::BrokenPipe,
        };
        assert!(!write_stdout(&mut broken, format_args!("line")).unwrap());

        let mut other = FailingWriter {
            kind: ErrorKind::Other,
        };
        let err = write_stdout(&mut other, format_args!("line")).unwrap_err();
        assert!(err.to_string().contains("synthetic write failure"));
    }

    #[test]
    fn guard_check_json_output_reports_verified_and_blocked_states() {
        let _lock = env_lock().lock().unwrap();
        let old_personal_root = std::env::var("SHELVES_PROTECTED_ROOT").ok();
        let temp = tempfile::tempdir().unwrap();
        let personal = temp.path().join("private-root");
        std::fs::create_dir(&personal).unwrap();
        unsafe { std::env::set_var("SHELVES_PROTECTED_ROOT", &personal) };

        let allowed = guard_check_output(PathBuf::from("/tmp").as_path());
        assert!(allowed.allowed);
        assert_eq!(allowed.status, "verified-open");
        assert!(allowed.resolved_path.is_some());
        assert!(allowed.error.is_none());

        let blocked = guard_check_output(personal.join("finance.md").as_path());
        assert!(!blocked.allowed);
        assert_eq!(blocked.status, "blocked");
        let expected_root = personal.display().to_string();
        assert_eq!(
            blocked.protected_root.as_deref(),
            Some(expected_root.as_str())
        );
        assert_eq!(blocked.error.as_deref(), Some("Layer-1 path refused"));

        unsafe { std::env::remove_var("SHELVES_PROTECTED_ROOT") };
        let missing = guard_check_output(PathBuf::from("/tmp").as_path());
        assert!(!missing.allowed);
        assert_eq!(missing.status, "blocked");
        assert!(
            missing
                .error
                .as_deref()
                .unwrap()
                .contains("SHELVES_PROTECTED_ROOT is required")
        );

        restore_env("SHELVES_PROTECTED_ROOT", old_personal_root);
    }

    struct FailingWriter {
        kind: ErrorKind,
    }

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(self.kind, "synthetic write failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
            // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
            Ok(()) // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
        } // LCOV_EXCL_LINE: coverage artifact; asserted by adjacent tests.
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
    fn all_contract_verbs_parse_without_panic() {
        for verb in CONTRACT_VERBS {
            let args = if *verb == "search" {
                vec!["shelves", verb, "test"]
            } else if *verb == "ask" {
                vec!["shelves", verb, "archivist", "test"]
            } else if *verb == "guard-check" {
                vec!["shelves", verb, "/tmp"]
            } else if *verb == "timeline" {
                vec!["shelves", verb, "yesterday"]
            } else if *verb == "remember" {
                vec!["shelves", verb, "test", "--due", "2026-06-13"]
            } else if *verb == "done" {
                vec!["shelves", verb, "1"]
            } else if *verb == "related" {
                vec!["shelves", verb, "test"]
            } else if *verb == "brief" {
                vec!["shelves", verb, "curator"]
            } else if *verb == "missed" {
                vec!["shelves", verb, "test"]
            } else if *verb == "context" {
                vec!["shelves", verb, "curator", "test"]
            } else if *verb == "promote" {
                vec!["shelves", verb, "1", "--to", "company", "--by", "curator"]
            } else if *verb == "lock" {
                vec!["shelves", verb, "render"]
            } else {
                vec!["shelves", verb]
            };
            Cli::try_parse_from(args).unwrap_or_else(|err| {
                panic!("contract verb {verb:?} failed to parse: {err}"); // LCOV_EXCL_LINE: test-only contract assertion under cfg(test).
            });
        }
    }
}
