"""Private rows stay closed across every public read verb."""

from __future__ import annotations

import json
import os
import re
import sqlite3
import subprocess
from datetime import datetime, timedelta
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("SHELVES_BINARY", ROOT / "target/release/shelves"))


@pytest.fixture(scope="module", autouse=True)
def release_binary() -> None:
    subprocess.run(
        ["cargo", "build", "--release"],
        cwd=ROOT,
        check=True,
    )


def call(root: Path, *args: str) -> str:
    env = os.environ.copy()
    env.update(
        AIOS_ROOT=str(root),
        SHELVES_PROTECTED_ROOT=str(root / "excluded-root"),
        SHELVES_DB_PATH=str(root / "shelves.db"),
    )
    env.pop("SHELVES_LOCALE_DIR", None)
    result = subprocess.run(
        [str(BINARY), *args],
        cwd=root,
        env=env,
        text=True,
        capture_output=True,
        check=False,
    )
    assert result.returncode == 0, f"{args}: {result.stderr}"
    return result.stdout


def seed(root: Path) -> None:
    call(root, "search", "mosaic", "--json")
    now = datetime.now().astimezone()
    today = now.date().isoformat()
    yesterday = (now.date() - timedelta(days=1)).isoformat()
    with sqlite3.connect(root / "shelves.db") as conn:
        conn.executemany(
            """INSERT INTO memories
            (id,name,title,body,owner,scope,visibility,source_path,
             content_hash,created_at,updated_at)
            VALUES(?,?,?,?,?,?,?,?,?,?,?)""",
            [
                (
                    1,
                    "private-mosaic",
                    "Private mosaic",
                    "mosaic secret pattern",
                    "agent:a",
                    "company",
                    "private",
                    "/fixture/private.md",
                    "hash1",
                    now.isoformat(),
                    now.isoformat(),
                ),
                (
                    2,
                    "public-mosaic",
                    "Public mosaic",
                    "mosaic common pattern",
                    "agent:a",
                    "company",
                    None,
                    "/fixture/public.md",
                    "hash2",
                    now.isoformat(),
                    now.isoformat(),
                ),
            ],
        )
        conn.executemany(
            """INSERT INTO episodes
            (id,ts,actor,kind,summary,body,scope,visibility,source_path)
            VALUES(?,?,?,?,?,?,?,?,?)""",
            [
                (
                    1,
                    f"{today}T12:00:00+00:00",
                    "agent:a",
                    "ticket",
                    "Private today mosaic",
                    "mosaic hidden today",
                    "company",
                    "private",
                    "/fixture/today.md",
                ),
                (
                    2,
                    f"{yesterday}T12:00:00+00:00",
                    "agent:a",
                    "ticket",
                    "Private yesterday mosaic",
                    "mosaic hidden yesterday",
                    "company",
                    "private",
                    "/fixture/yesterday.md",
                ),
            ],
        )
        conn.executemany(
            "INSERT INTO links(from_kind,from_id,to_kind,to_id,kind) "
            "VALUES('memory',2,?,?, 'related')",
            [("memory", 1), ("episode", 1)],
        )
        conn.execute("INSERT INTO memories_fts(memories_fts) VALUES('rebuild')")
        conn.execute("INSERT INTO episodes_fts(episodes_fts) VALUES('rebuild')")


def read_surfaces(root: Path, reader: str) -> dict[str, str]:
    return {
        "search": call(root, "search", "mosaic", "--scope", "company", "--as", reader, "--json"),
        "ask": call(root, "ask", "a", "mosaic", "--as", reader, "--json"),
        "context": call(root, "context", reader, "mosaic secret pattern", "--json"),
        "brief": call(root, "brief", reader, "--json"),
        "related": call(root, "related", "public-mosaic", "--as", reader, "--json"),
        "prune": call(root, "prune-report", "--as", reader, "--json"),
        "timeline": call(root, "timeline", "--as", reader, "--json"),
        "today": call(root, "today", "--as", reader, "--json"),
        "yesterday": call(root, "yesterday", "--as", reader, "--json"),
    }


def test_private_rows_across_verbs_grants_and_default(tmp_path: Path) -> None:
    seed(tmp_path)
    hidden = read_surfaces(tmp_path, "b")
    for verb, output in hidden.items():
        assert "Private mosaic" not in output, verb
        assert "Private today mosaic" not in output, verb
        assert "Private yesterday mosaic" not in output, verb
        assert "mosaic secret pattern" not in output, verb
    assert re.search(r"[1-9] private notes withheld", hidden["context"])
    small_pack = json.loads(
        call(tmp_path, "context", "b", "mosaic secret pattern", "--budget", "1", "--json")
    )
    assert len(small_pack) == 1
    assert "private note" in small_pack[0]["title"]
    assert "Private today mosaic" not in call(
        tmp_path, "context", "b", "what happened today mosaic", "--json"
    )
    assert "Public mosaic" in hidden["search"]

    # The CLI accepts the caller identity verbatim; this is a local read policy,
    # not authentication for an untrusted process.
    forged_owner = call(tmp_path, "search", "mosaic", "--as", "a", "--json")
    assert "Private mosaic" in forged_owner

    owner = read_surfaces(tmp_path, "a")
    for verb in ("search", "ask", "context", "brief", "related"):
        assert "Private mosaic" in owner[verb], verb
    for verb in ("search", "timeline", "today"):
        assert "Private today mosaic" in owner[verb], verb
    assert "Private yesterday mosaic" in owner["yesterday"]

    call(tmp_path, "grant", "a", "b", "--include-private")
    granted = read_surfaces(tmp_path, "b")
    for verb in ("search", "ask", "context", "brief", "related"):
        assert "Private mosaic" in granted[verb], verb
    for verb in ("search", "timeline", "today"):
        assert "Private today mosaic" in granted[verb], verb
    call(tmp_path, "revoke", "a", "b")
    revoked = read_surfaces(tmp_path, "b")
    assert all("Private mosaic" not in text for text in revoked.values())

    call(tmp_path, "grant", "a", "b", "--row", "1", "--include-private")
    assert "Private mosaic" in read_surfaces(tmp_path, "b")["search"]
    assert "Private today mosaic" not in read_surfaces(tmp_path, "b")["search"]
    call(tmp_path, "revoke", "a", "b", "--row", "1")
    assert "Private mosaic" not in read_surfaces(tmp_path, "b")["search"]

    audit = json.loads(call(tmp_path, "grants", "--json"))
    assert "grant by agent:a" in call(tmp_path, "grants")
    assert [row["action"] for row in audit] == ["grant", "revoke", "grant", "revoke"]
    assert all(row["actor"] == "agent:a" for row in audit)
    assert audit[0]["include_private"] is True

    with sqlite3.connect(tmp_path / "shelves.db") as conn:
        conn.execute("DELETE FROM node_acl WHERE owner_node='agent:a' AND reader='agent:b'")
        conn.execute("UPDATE meta SET value='private' WHERE key='default_visibility'")
    assert "Public mosaic" not in read_surfaces(tmp_path, "b")["search"]
    assert "Public mosaic" in read_surfaces(tmp_path, "a")["search"]
    with sqlite3.connect(tmp_path / "shelves.db") as conn:
        conn.execute("UPDATE meta SET value='shared' WHERE key='default_visibility'")
    assert "Public mosaic" in read_surfaces(tmp_path, "b")["search"]

    with sqlite3.connect(tmp_path / "shelves.db") as conn:
        conn.execute(
            """INSERT INTO memories(id,name,title,body,owner,scope,visibility,
            source_path,content_hash,created_at,updated_at)
            VALUES(3,'private-cold','Private cold','old private body','agent:a',
            'company','private','/fixture/cold.md','hash3','2000-01-01','2000-01-01')"""
        )
        conn.execute(
            """INSERT INTO memories(id,name,title,body,owner,scope,visibility,
            source_path,content_hash,created_at,updated_at)
            VALUES(4,'public-cold','Public cold','old shared body','agent:a',
            'company','shared','/fixture/public-cold.md','hash4','2000-01-01','2000-01-01')"""
        )
        conn.execute("INSERT INTO memories_fts(memories_fts) VALUES('rebuild')")
    assert "Private cold" not in call(tmp_path, "prune-report", "--as", "b", "--json")
    assert "Private cold" in call(tmp_path, "prune-report", "--as", "a", "--json")
    assert "Private cold" not in call(tmp_path, "consolidate", "--as", "b", "--json")
    owner_report = json.loads(call(tmp_path, "consolidate", "--as", "a", "--json"))
    assert "Private cold" in json.dumps(owner_report)
    persisted = Path(owner_report["report_path"]).read_text()
    assert "Private cold" not in persisted
    assert "Public cold" in persisted

    call(tmp_path, "grant", "a", "operator", "--row", "1", "--include-private")
    call(tmp_path, "promote", "1", "--to", "os", "--by", "operator", "--json")
    with sqlite3.connect(tmp_path / "shelves.db") as conn:
        summary, body, visibility = conn.execute(
            "SELECT summary,body,visibility FROM episodes WHERE kind='promotion'"
        ).fetchone()
    assert visibility == "private"
    assert "Private mosaic" not in summary + body
    assert "Private mosaic" not in call(tmp_path, "timeline", "--as", "b", "--json")
    assert "private memory promoted" in call(tmp_path, "timeline", "--as", "a", "--json")
