# Shelves

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Shelves is a local-first, LLM-free memory engine for multi-agent workspaces. It
derives a rebuildable SQLite/FTS index from canonical Markdown, then returns
bounded context packs ranked by keyword relevance and activation.

It does not call a model, require embeddings, or replace canonical source
files. The CLI and schema operate on paths, strings, and SQLite, so callers are
not tied to a model provider.

## What the engine implements

- Deterministic keyword recall with activation and decay.
- Write-once decision locks. Corrections supersede prior locks rather than
  editing their bodies.
- Product, company, and OS retrieval scopes with documented fall-through.
- Agent-owned memory rows and configurable read filtering.
- Per-row `visibility: private` with explicit private grants and an append-only
  grant audit.
- A required protected-root guard that refuses a configured path before file
  walking.
- Miss logging, miss review, and hermetic golden regressions.
- Query-time locale packs; Spanish is bundled without changing stored content.

## Scope, owner, and ACL are separate

`scope` is a retrieval bucket, not an agent identity:

```text
product:<name> -> company -> os
company        -> product:* -> os
os             -> company
```

Agent association lives in the separate `owner` field, such as
`agent:planner`. `visibility` is a separate property on indexed memories and
episodes:

- `visibility: private` rows are readable by the owner. Another reader needs
  an explicit `grant OWNER READER --include-private`, optionally limited to a
  row with `--row ID --kind memory|episode`. Ordinary owner grants and wildcard
  rules do not expose private rows.
- Unmarked rows use the stored `default_visibility` setting, initially
  `shared`. An invalid explicit visibility marker is treated as `private`.
- Shared rows use the owner ACL: an explicit reader rule wins over a wildcard;
  with no matching rule, the owner ACL is open by default. An explicit revoke
  can close that path.
- `grant`, `revoke`, and `grants` record changes in an append-only audit.
  A reset refuses to rebind existing row grants to new numeric IDs.

The caller supplies `--as`; Shelves does not authenticate that identity or
protect the Markdown source files or SQLite database from local readers. These
read rules are for a trusted local workspace, not a multi-tenant security
boundary. The protected-root guard prevents indexing one configured filesystem
tree; it does not create private filesystem storage.

## Install and quickstart

Rust 1.96.0 is pinned in `rust-toolchain.toml`.

```bash
cargo build --release

mkdir -p demo/system demo/memory/planner demo/protected
cat > demo/system/memory.md <<'EOF'
## Checkout Retry Rule
LOCKED: checkout retries must be idempotent and tested.
EOF

AIOS_ROOT="$PWD/demo" \
SHELVES_DB_PATH="$PWD/demo/system/shelves.db" \
SHELVES_PROTECTED_ROOT="$PWD/demo/protected" \
target/release/shelves ingest --reset --force

AIOS_ROOT="$PWD/demo" \
SHELVES_DB_PATH="$PWD/demo/system/shelves.db" \
SHELVES_PROTECTED_ROOT="$PWD/demo/protected" \
target/release/shelves context planner "write checkout retry tests"
```

See [Using Shelves](docs/USING_SHELVES.md) for sources, commands, locale packs,
and the visibility/ACL contract.

## Verify a clone

Required development tools are Rust/Cargo, Python 3.12+, and `cargo-audit`
0.22.2. Python packages are pinned in `requirements-dev.txt`.

```bash
python3 -m venv .venv
.venv/bin/python -m pip install -r requirements-dev.txt
cargo install cargo-audit --locked --version 0.22.2
PATH="$PWD/.venv/bin:$PATH" ./scripts/check.sh
```

The last line is the single repository gate. It runs format, Clippy with
warnings denied, all Rust tests, Ruff, Python tests, the public-content/path
guard, and `cargo audit`. The workflow in `.github/workflows/ci.yml` invokes
the same checks on GitHub-hosted runners. Run status depends on GitHub Actions
availability for the repository.

## Configuration

| Variable | Purpose |
|---|---|
| `AIOS_ROOT` | Workspace root to index. |
| `SHELVES_DB_PATH` | SQLite index path; defaults to `$AIOS_ROOT/system/shelves.db`. |
| `SHELVES_PROTECTED_ROOT` | Required path that Shelves refuses before reading. |
| `SHELVES_SOURCE_LIST` | Comma-separated enabled source names. |
| `SHELVES_EXTRA_SOURCE_DIR` | Extra recursive Markdown source. |
| `SHELVES_EXTERNAL_MEMORY_DIR` | Optional external memory directory. |
| `SHELVES_AGENT_HINTS` | Agent names used for owner/actor detection. |
| `SHELVES_PRODUCT_SCOPES` | Product names and optional aliases used for scope classification. |
| `SHELVES_COMPANY_TOKENS` | Body tokens used for company-scope classification. |
| `SHELVES_COMPANY_SLUG_PREFIXES` | Slug prefixes used for company-scope classification. |
| `SHELVES_LOCALE_DIR` | Optional directory of additional locale packs. |

## License

MIT © 2026 Manuel Messon-Roque
