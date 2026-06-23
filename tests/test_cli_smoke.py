from __future__ import annotations

import os
import subprocess
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
SHELVES = Path(os.environ.get("SHELVES_BINARY", REPO_ROOT / "target/release/shelves"))


def test_release_binary_version() -> None:
    subprocess.run(["cargo", "build", "--release"], cwd=REPO_ROOT, check=True)

    result = subprocess.run(
        [str(SHELVES), "--version"],
        cwd=REPO_ROOT,
        text=True,
        capture_output=True,
        check=False,
    )

    assert result.returncode == 0
    assert result.stdout.strip().startswith("shelves ")
    assert result.stderr == ""
