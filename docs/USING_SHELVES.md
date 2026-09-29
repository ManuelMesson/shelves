# Using Shelves

Shelves is a local derived index. Markdown stays canonical; the SQLite database
can be rebuilt at any time.

## Required environment

```bash
export AIOS_ROOT=/path/to/workspace
export SHELVES_DB_PATH="$AIOS_ROOT/system/shelves.db"
export SHELVES_PROTECTED_ROOT=/path/to/a/refused/tree
```

`SHELVES_PROTECTED_ROOT` is mandatory. Shelves resolves lexical paths, existing
prefixes, and symlinks before reading. It refuses that root and descendants.
This is a filesystem ingestion guard, not an authenticated privacy boundary.

## Common commands

```bash
target/release/shelves ingest --reset --force
target/release/shelves search "checkout retry" --scope company
target/release/shelves context planner "write checkout retry tests"
target/release/shelves ask archivist "release checklist" --as agent:planner
target/release/shelves grant planner reviewer --include-private
target/release/shelves grants --json
target/release/shelves revoke planner reviewer
target/release/shelves missed "retry owner did not appear" --by engineer
target/release/shelves misses list --since 7
target/release/shelves lock show checkout-retry-rule
```

Most read commands accept `--json`.

## Retrieval contract

Scopes fall through from product to company to OS. Company searches can also
consider product rows; OS searches can consider company rows. Scope controls
retrieval distance only.

Owners, visibility, and ACL rows are independent of scope. Mark a Markdown
memory or episode file with YAML frontmatter to close its derived rows:

```yaml
---
owner: planner
visibility: private
---
# Planning note
```

The owner can read a private row. Other readers need an explicit owner or row
grant with `--include-private`; an ordinary grant or wildcard does not expose
it. `grant planner reviewer --row 3 --kind memory --include-private` limits a
grant to one indexed memory. `revoke` removes a grant, and `grants --json`
lists the append-only audit. A reset refuses while row grants exist because
recreated IDs might identify different rows.

Unmarked rows use `default_visibility` in the SQLite `meta` table, initially
`shared`; an invalid explicit marker closes the row. Shared rows use the older
owner ACL: self reads pass, explicit reader rows override wildcard rows, and
no matching ACL row means allowed. `--as` and grant actors are unverified local
CLI inputs. The source files and database need normal filesystem protection;
the CLI read policy is not an authentication or multi-tenant boundary.

## Source configuration

Default sources use neutral workspace conventions, including system memory,
team logs, handoffs, agent-to-agent notes, processed/open builder tickets,
agent memory, agent identity files, meetings, and the lock store. Limit them
with `SHELVES_SOURCE_LIST`, or add a standalone recursive corpus with:

```bash
export SHELVES_SOURCE_LIST=system-memory,agent-memory,extra,lock-store
export SHELVES_EXTRA_SOURCE_DIR=/path/to/markdown-corpus
export SHELVES_EXTERNAL_MEMORY_DIR=/path/to/external/memory
export SHELVES_AGENT_HINTS=planner,engineer,reviewer
export SHELVES_PRODUCT_SCOPES=notebook,console:dashboard|cockpit,voice
export SHELVES_COMPANY_TOKENS=company,organization,team
export SHELVES_COMPANY_SLUG_PREFIXES=feedback-,policy-
```

## Locale packs

Spanish query normalization ships in `locales/es.toml`. Additional TOML packs
can be loaded from `SHELVES_LOCALE_DIR`. Locale expansion happens at query time;
stored content and the existing FTS index remain unchanged.

## Miss ratchet

`missed` records a retrieval failure, and `misses list` provides a review queue.
Improvement is deliberate rather than automatic: reproduce the retrieval shape
with generic fixture text, add a failing golden query, tune the engine, and keep
the regression. Never copy live workspace content into a fixture.

## Ingest identity

Unchanged memory upserts and repeated episode identities are idempotent. Episode
identity includes body content, so a later source record with the same timestamp
and summary but a changed body remains observable instead of being discarded.
