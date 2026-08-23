#!/usr/bin/env python3
"""Fail when a public artifact contains private paths or deployment-specific data."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class Rule:
    label: str
    pattern: re.Pattern[str]


@dataclass(frozen=True)
class Finding:
    path: Path
    line: int
    label: str


RULES = (
    Rule("absolute home path", re.compile(r"/(?:home|Users)/[^/\s]+/")),
    Rule("private OS path", re.compile(r"personal" + r"-os", re.IGNORECASE)),
    Rule("internal engine name", re.compile(r"\bbrain\b", re.IGNORECASE)),
    Rule("internal engine environment", re.compile(r"\bBRAIN_[A-Z0-9_]+\b")),
    Rule("internal engine database", re.compile(r"(?:system/|system\\)brain\.db")),
    Rule("company workspace name", re.compile(r"popo" + r"soft", re.IGNORECASE)),
    Rule("credential-like token", re.compile(r"\b(?:ghp|github_pat|sk)-[A-Za-z0-9_-]{16,}\b")),
    Rule(
        "email address",
        re.compile(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b"),
    ),
)

SCANNED_NAMES = {
    "Cargo.lock",
    "Cargo.toml",
    "LICENSE",
    "README.md",
    "pyproject.toml",
    "requirements-dev.txt",
    "rust-toolchain.toml",
}
SCANNED_SUFFIXES = {".md", ".py", ".rs", ".sh", ".toml", ".yaml", ".yml"}
EXCLUDED = {Path("scripts/check_public_content.py")}
IGNORED_PARTS = {".git", ".pytest_cache", ".ruff_cache", ".venv", "__pycache__", "target"}


def tracked_files(root: Path) -> list[Path]:
    """Return tracked and new files while excluding generated directories."""
    files = {
        path
        for path in root.rglob("*")
        if path.is_file() and not IGNORED_PARTS.intersection(path.relative_to(root).parts)
    }
    result = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-z"],
        check=False,
        capture_output=True,
    )
    if result.returncode == 0:
        files.update(root / Path(raw.decode()) for raw in result.stdout.split(b"\0") if raw)
    return sorted(files)


def should_scan(root: Path, path: Path) -> bool:
    """Limit scanning to public text artifacts and exclude the rule definitions."""
    relative = path.relative_to(root)
    return relative not in EXCLUDED and (
        path.name in SCANNED_NAMES or path.suffix.lower() in SCANNED_SUFFIXES
    )


def scan(root: Path) -> list[Finding]:
    """Scan text files without returning or printing their contents."""
    findings: list[Finding] = []
    for path in tracked_files(root):
        if not should_scan(root, path):
            continue
        try:
            lines = path.read_text(encoding="utf-8").splitlines()
        except (OSError, UnicodeDecodeError) as exc:
            findings.append(Finding(path.relative_to(root), 0, f"unreadable text: {exc}"))
            continue
        for line_number, line in enumerate(lines, start=1):
            for rule in RULES:
                if rule.pattern.search(line):
                    findings.append(Finding(path.relative_to(root), line_number, rule.label))
    return findings


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--simulate-red", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    root = args.root.resolve()
    findings = scan(root)
    if args.simulate_red:
        findings.append(Finding(Path("<simulation>"), 1, "simulated public-content violation"))
    if findings:
        for finding in findings:
            print(f"{finding.path}:{finding.line}: {finding.label}", file=sys.stderr)
        return 1
    print(f"public content clean: {len(tracked_files(root))} files considered")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
