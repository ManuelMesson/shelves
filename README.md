# Shelves

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

**A memory engine for multi-agent AI systems that doesn't store everything — it remembers what matters.**

Most "agent memory" is a pile of chat logs you hope to grep later. Shelves is the opposite: a small, local-first, **LLM-free** engine that keeps the *durable* stuff — decisions, active context, and the misses it should learn from — and hands each agent a compact, ranked context pack for the task in front of it.

English-primary. Deterministic. No vector DB, no embeddings, no model calls — just keyword recall + activation scoring + write-once records, behind a stable CLI + SQLite contract.

> **we don't store — we remember.**

## Shared memory, plus a private one for each agent

This is the part most agent memory misses. A shared log isn't personal; a private log isn't shared. A real team of agents needs both — the house knowledge everyone works from, and the context that belongs to one worker.

Shelves puts a **scope** on every memory, and recall falls through from narrow to broad:

> per-agent → product → company → OS

An agent gets its own working context first, with the shared knowledge behind it — who's being served, how things are done, what's already been decided. One system, many agents; one memory, with private shelves inside.

*(A picture, if you want one: a counter where every barista shares the regulars, the recipes, and the house rules — but each still keeps their own notebook.)*

## What's inside

- **LLM-free recall** — search by task language; results rank by keyword relevance + activation, not embeddings. Fast, deterministic, debuggable, runs anywhere.
- **Activation & decay** — every hit warms a memory; frequently-used memory stays hot, stale memory cools and ages toward archive, so context packs stay small and relevant.
- **Write-once locks** — durable decisions are recorded once. Corrections *supersede* old locks instead of overwriting them — you can always see what was true, and when.
- **Scoped recall** — per-agent → product → company → OS, with fall-through so the right context surfaces (see above).
- **A boundary** — mark private paths and Shelves refuses to read them, before any access. Memory that respects a wall.

## Install

```bash
cargo build --release
# binary: target/release/shelves
```

## Quickstart

```bash
mkdir -p demo/system demo/memory/planner demo/private
cat > demo/system/memory.md <<'EOF'
## Checkout Retry Rule
LOCKED: checkout retries must be idempotent and tested.
EOF

AIOS_ROOT="$PWD/demo" \
SHELVES_DB_PATH="$PWD/demo/system/shelves.db" \
SHELVES_PROTECTED_ROOT="$PWD/demo/private" \
target/release/shelves ingest --reset --force

AIOS_ROOT="$PWD/demo" \
SHELVES_DB_PATH="$PWD/demo/system/shelves.db" \
SHELVES_PROTECTED_ROOT="$PWD/demo/private" \
target/release/shelves context planner "write checkout retry tests"
```

You get back a ranked context pack — the locked rule, plus anything else relevant — for that exact task.

## Configuration

| Env var | Purpose |
|---|---|
| `AIOS_ROOT` | Root of the workspace to index. |
| `SHELVES_DB_PATH` | SQLite index path (default: `$AIOS_ROOT/system/shelves.db`). |
| `SHELVES_PROTECTED_ROOT` | Private path Shelves refuses to read. |
| `SHELVES_COMPANY_TOKENS` | Words that classify a memory as company scope (default: `company,organization,team`). |
| `SHELVES_PRODUCT_SCOPES` | Product scope names and aliases (default: `notebook,console,voice`). |
| `SHELVES_COMPANY_SLUG_PREFIXES` | Slug prefixes that force company scope (default: `feedback-`). |
| `SHELVES_AGENT_HINTS` | Agent names used for owner/actor detection. |
| `SHELVES_SOURCE_LIST` | Source names to enable (empty = all defaults). |
| `SHELVES_EXTRA_SOURCE_DIR` | Extra recursive Markdown source for imports. |
| `SHELVES_CURATOR_MEMORY_DIR` | Optional external memory directory override. |

## Develop

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test
ruff check && pytest
```

## Why I built it

I run a multi-agent system day to day, and the hard part was never getting the agents to talk — it was getting them to *remember the right things* without drowning in their own logs. Shelves is the memory layer I needed: small, deterministic, honest about what it keeps. It's the engine under my own work; I'm opening it because the idea — remember, don't hoard — is worth more shared than hidden.

— Manuel Messon-Roque

## License

MIT © 2026 Manuel Messon-Roque
