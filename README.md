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
`agent:planner`. Read filtering is handled by `node_acl`:

- `shared` rows and a caller's own rows are readable.
- An explicit reader grant or revoke wins.
- A wildcard rule applies when no explicit reader rule exists.
- With no matching rule, access is allowed by default.

The caller supplies `--as`; Shelves does not authenticate that identity. The
ACL is therefore a configurable retrieval policy for a trusted local workspace,
not a privacy, authorization, or multi-tenant security boundary. The
protected-root guard prevents indexing one configured filesystem tree; it does
not create private agent storage.

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
and the ACL contract.

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
guard, and `cargo audit`. The same subcommands are named as separate steps in
`.github/workflows/ci.yml` so a failing check is visible.

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
