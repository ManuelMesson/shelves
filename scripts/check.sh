#!/usr/bin/env bash
set -euo pipefail

run_format() {
  cargo fmt --all --check
  python3 -m ruff format --check .
  bash -n scripts/check.sh
}

run_clippy() {
  cargo clippy --all-targets --all-features -- -D warnings
}

run_rust_tests() {
  cargo test --all-features --quiet
}

run_python() {
  python3 -m ruff check .
  python3 -m pytest -q
}

run_audit() {
  cargo audit --deny warnings
}

run_public_content() {
  python3 scripts/check_public_content.py "$@"
}

run_all() {
  local simulate_red="${1:-false}"
  run_format
  run_clippy
  run_rust_tests
  run_python
  run_audit
  if [[ "$simulate_red" == "true" ]]; then
    run_public_content --simulate-red
  else
    run_public_content
  fi
}

case "${1:-all}" in
  all) run_all false ;;
  --simulate-red) run_all true ;;
  format) run_format ;;
  clippy) run_clippy ;;
  rust-tests) run_rust_tests ;;
  python) run_python ;;
  audit) run_audit ;;
  public-content)
    shift
    run_public_content "$@"
    ;;
  *)
    echo "usage: $0 [all|--simulate-red|format|clippy|rust-tests|python|audit|public-content]" >&2
    exit 2
    ;;
esac
