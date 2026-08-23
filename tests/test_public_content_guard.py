from __future__ import annotations

import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
GUARD = REPO_ROOT / "scripts" / "check_public_content.py"


def run_guard(root: Path, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(GUARD), "--root", str(root), *args],
        text=True,
        capture_output=True,
        check=False,
    )


def test_clean_generic_tree_passes(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("Generic memory engine fixture.\n", encoding="utf-8")

    result = run_guard(tmp_path)

    assert result.returncode == 0, result.stderr
    assert "public content clean" in result.stdout


def test_absolute_home_path_fails_without_echoing_content(tmp_path: Path) -> None:
    private_path = "/".join(["", "home", "sample-user", "workspace", "note.md"])
    (tmp_path / "fixture.md").write_text(private_path, encoding="utf-8")

    result = run_guard(tmp_path)

    assert result.returncode == 1
    assert "absolute home path" in result.stderr
    assert private_path not in result.stderr


def test_simulated_red_gate_fails(tmp_path: Path) -> None:
    (tmp_path / "README.md").write_text("clean\n", encoding="utf-8")

    result = run_guard(tmp_path, "--simulate-red")

    assert result.returncode == 1
    assert "simulated public-content violation" in result.stderr
