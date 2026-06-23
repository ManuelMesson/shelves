# Using Shelves

Shelves is a derived memory index. Your Markdown files stay canonical; the SQLite
database can be rebuilt at any time.

## Required Environment

```bash
export AIOS_ROOT=/path/to/workspace
export SHELVES_DB_PATH="$AIOS_ROOT/system/shelves.db"
export SHELVES_PROTECTED_ROOT="$AIOS_ROOT/private"
```

`SHELVES_PROTECTED_ROOT` is required. Shelves refuses that path and its descendants
before reading files.

## Common Commands

```bash
target/release/shelves ingest --reset --force
target/release/shelves search "checkout retry" --scope company
target/release/shelves context planner "write checkout retry tests"
target/release/shelves missed "checkout retry owner did not appear" --by engineer
target/release/shelves lock show checkout-retry-rule
```

## Source Configuration

Default sources are neutral workspace conventions:

- `system-memory`: `$AIOS_ROOT/system/memory.md`
- `team-log`: `$AIOS_ROOT/system/TEAM_LOG.md`
- `handoffs`: `$AIOS_ROOT/system/handoffs/*.md`
- `agent-to-agent`: `$AIOS_ROOT/system/inbox/agent-to-agent/**/*.md`
- `processed-tickets`: `$AIOS_ROOT/system/inbox/builder-code/processed/**/*.md`
- `agent-memory`: `$AIOS_ROOT/memory/*/*.md`
- `lock-store`: `$AIOS_ROOT/system/locks.yaml`

Useful overrides:

```bash
export SHELVES_SOURCE_LIST=system-memory,agent-memory,extra
export SHELVES_EXTRA_SOURCE_DIR=/path/to/markdown-corpus
export SHELVES_CURATOR_MEMORY_DIR=/path/to/external/memory
export SHELVES_AGENT_HINTS=planner,engineer,reviewer
export SHELVES_COMPANY_TOKENS=company,organization,team
export SHELVES_PRODUCT_SCOPES=notebook,console,voice
export SHELVES_COMPANY_SLUG_PREFIXES=feedback-
```

## Recall Loop

1. Write durable memories and locks in Markdown.
2. Run `shelves ingest --reset --force`.
3. Ask for context with the task in plain English.
4. When recall misses, record the miss with `shelves missed`.
5. Promote durable decisions into locks so future context packs keep them near the top.
