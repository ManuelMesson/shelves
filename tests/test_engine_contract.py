from __future__ import annotations

import json
import os
import shutil
import sqlite3
import subprocess
from pathlib import Path

import pytest
import yaml

REPO_ROOT = Path(__file__).resolve().parents[1]
SHELVES = Path(os.environ.get("SHELVES_BINARY", REPO_ROOT / "target/release/shelves"))
GOLDEN = Path(__file__).with_name("golden_queries.yaml")
GOLDEN_FIXTURES = Path(__file__).with_name("fixtures") / "golden_corpus"


@pytest.fixture(scope="session", autouse=True)
def release_binary() -> None:
    subprocess.run(["cargo", "build", "--release"], cwd=REPO_ROOT, check=True)


@pytest.fixture()
def workspace(tmp_path: Path) -> dict[str, str]:
    root = tmp_path / "workspace"
    protected = tmp_path / "private-root"
    (root / "system").mkdir(parents=True)
    (root / "system/locks.yaml").write_text(
        """- slug: checkout-retry-rule
  title: Checkout Retry Rule
  scope: company
  locked_on: 2026-06-18
  supersedes: null
  body: |
    Checkout retries must be idempotent, rate-limited, and tested.
- slug: testing-standard
  title: Testing Standard
  scope: company
  locked_on: 2026-06-18
  supersedes: null
  body: |
    Release changes ship with automated tests and one command that runs the suite.
""",
        encoding="utf-8",
    )
    protected.mkdir()
    return {
        "AIOS_ROOT": str(root),
        "SHELVES_DB_PATH": str(root / "system/shelves.db"),
        "SHELVES_PROTECTED_ROOT": str(protected),
        "SHELVES_EXTRA_SOURCE_DIR": str(GOLDEN_FIXTURES),
        "SHELVES_SOURCE_LIST": "extra,lock-store",
        "SHELVES_AGENT_HINTS": "planner,engineer,archivist",
    }


def run_shelves(*args: str, env: dict[str, str]) -> subprocess.CompletedProcess[str]:
    merged = os.environ.copy()
    merged.update(env)
    return subprocess.run(
        [str(SHELVES), *args],
        cwd=REPO_ROOT,
        env=merged,
        text=True,
        capture_output=True,
        check=False,
    )


def ingest_fixture_corpus(env: dict[str, str]) -> None:
    result = run_shelves("ingest", "--reset", "--force", "--json", env=env)
    assert result.returncode == 0, result.stderr
    payload = json.loads(result.stdout)
    assert payload["memories"] >= 3
    assert payload["episodes"] >= 2
    assert payload["skipped"] == 0


def test_guard_check_blocks_configured_private_root(workspace: dict[str, str]) -> None:
    protected = Path(workspace["SHELVES_PROTECTED_ROOT"])

    allowed = run_shelves("guard-check", workspace["AIOS_ROOT"], "--json", env=workspace)
    blocked = run_shelves("guard-check", str(protected / "journal.md"), "--json", env=workspace)

    assert allowed.returncode == 0, allowed.stderr
    assert json.loads(allowed.stdout)["allowed"] is True
    assert blocked.returncode == 0, blocked.stderr
    blocked_payload = json.loads(blocked.stdout)
    assert blocked_payload["allowed"] is False
    assert blocked_payload["protected_root"] == str(protected)


def test_golden_search_queries_hit_fictional_fixture_corpus(workspace: dict[str, str]) -> None:
    ingest_fixture_corpus(workspace)
    config = yaml.safe_load(GOLDEN.read_text())

    for case in config["queries"]:
        result = run_shelves(
            "search",
            case["query"],
            "--scope",
            case["scope"],
            "--json",
            env=workspace,
        )
        assert result.returncode == 0, result.stderr
        rows = json.loads(result.stdout)
        haystack = "\n".join(
            f"{row['kind']} {row['scope']} {row['title']} {row['snippet']}" for row in rows[:3]
        )
        for expected in case["expected_top3_contains"]:
            assert expected in haystack


def test_context_pack_uses_locks_and_relevance_floor(workspace: dict[str, str]) -> None:
    corpus = Path(workspace["AIOS_ROOT"]) / "synthetic-corpus"
    shutil.copytree(GOLDEN_FIXTURES, corpus)
    for index in range(10):
        (corpus / f"unrelated-{index}.md").write_text(
            f"---\nname: unrelated_{index}\ntitle: Unrelated Note {index}\n---\n"
            "An independent synthetic note about garden tools.\n"
        )
    workspace["SHELVES_EXTRA_SOURCE_DIR"] = str(corpus)
    ingest_fixture_corpus(workspace)

    focused = run_shelves(
        "context",
        "planner",
        "write checkout retry tests",
        "--json",
        env=workspace,
    )
    assert focused.returncode == 0, focused.stderr
    rows = json.loads(focused.stdout)
    titles = [row["title"] for row in rows]
    assert "Checkout Retry Rule" in titles
    assert rows[0]["section"] == "task-memory"
    assert any(row["section"] == "active-lock" for row in rows)

    unrelated = run_shelves(
        "context",
        "planner",
        "third floor espresso maintenance",
        "--json",
        env=workspace,
    )
    assert unrelated.returncode == 0, unrelated.stderr
    sections = [row["section"] for row in json.loads(unrelated.stdout)]
    assert "nothing-specific" in sections


def test_missed_records_are_queryable(workspace: dict[str, str]) -> None:
    ingest_fixture_corpus(workspace)
    missed = run_shelves(
        "missed",
        "checkout retry owner not recalled",
        "--by",
        "engineer",
        env=workspace,
    )
    assert missed.returncode == 0, missed.stderr

    conn = sqlite3.connect(workspace["SHELVES_DB_PATH"])
    try:
        row = conn.execute("SELECT actor, kind, summary FROM episodes WHERE kind='miss'").fetchone()
    finally:
        conn.close()

    assert row == ("agent:engineer", "miss", "checkout retry owner not recalled")


def test_repeated_ingest_is_idempotent_and_changed_episode_body_is_observable(
    workspace: dict[str, str], tmp_path: Path
) -> None:
    corpus = tmp_path / "corpus"
    shutil.copytree(GOLDEN_FIXTURES, corpus)
    env = {**workspace, "SHELVES_EXTRA_SOURCE_DIR": str(corpus)}
    ingest_fixture_corpus(env)

    conn = sqlite3.connect(env["SHELVES_DB_PATH"])
    try:
        first_count = conn.execute("SELECT COUNT(*) FROM episodes WHERE kind='ticket'").fetchone()[
            0
        ]
    finally:
        conn.close()

    repeated = run_shelves("ingest", "--json", env=env)
    assert repeated.returncode == 0, repeated.stderr
    conn = sqlite3.connect(env["SHELVES_DB_PATH"])
    try:
        repeated_count = conn.execute(
            "SELECT COUNT(*) FROM episodes WHERE kind='ticket'"
        ).fetchone()[0]
    finally:
        conn.close()
    assert repeated_count == first_count

    changed = corpus / "20260618-closeout.md"
    changed.write_text(
        changed.read_text(encoding="utf-8") + "Changed body remains observable.\n",
        encoding="utf-8",
    )
    changed_ingest = run_shelves("ingest", "--json", env=env)
    assert changed_ingest.returncode == 0, changed_ingest.stderr
    conn = sqlite3.connect(env["SHELVES_DB_PATH"])
    try:
        changed_count = conn.execute(
            "SELECT COUNT(*) FROM episodes WHERE kind='ticket'"
        ).fetchone()[0]
        changed_rows = conn.execute(
            "SELECT COUNT(*) FROM episodes WHERE body LIKE '%Changed body remains observable%'"
        ).fetchone()[0]
    finally:
        conn.close()
    assert changed_count == first_count + 1
    assert changed_rows == 1


def test_miss_derived_goldens_are_labeled_synthetic() -> None:
    cases = yaml.safe_load(GOLDEN.read_text(encoding="utf-8"))["queries"]
    miss_cases = [case for case in cases if "miss_id" in case]

    assert len(miss_cases) >= 2
    assert all(str(case["miss_id"]).startswith("synthetic-") for case in miss_cases)
