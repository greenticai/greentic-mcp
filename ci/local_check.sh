#!/usr/bin/env bash

# Usage:
#   LOCAL_CHECK_ONLINE=1 LOCAL_CHECK_STRICT=1 LOCAL_CHECK_VERBOSE=1 ci/local_check.sh
# Defaults: offline, non-strict, quiet.

set -euo pipefail

if [[ "${LOCAL_CHECK_VERBOSE:-0}" == "1" ]]; then
  set -x
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

STRICT="${LOCAL_CHECK_STRICT:-0}"
ONLINE="${LOCAL_CHECK_ONLINE:-0}"

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "[miss] $1"
    return 1
  }
}

step() {
  printf "\n▶ %s\n" "$*"
}

run_cmd() {
  local desc="$1"
  shift
  step "$desc"
  local log_file
  log_file="$(mktemp)"
  set +e
  "$@" 2>&1 | tee "$log_file"
  local status=$?
  set -e
  if [[ $status -ne 0 ]]; then
    rm -f "$log_file"
    exit $status
  fi
  rm -f "$log_file"
}

run_or_skip() {
  local desc="$1"
  shift
  if "$@"; then
    return 0
  fi
  if [[ "$STRICT" == "1" ]]; then
    echo "[fail] $desc (strict mode requires this step)"
    exit 1
  fi
  echo "[skip] $desc"
  return 0
}

ensure_tool() {
  local tool="$1"
  local desc="${2:-$1}"
  if need "$tool"; then
    return 0
  fi
  if [[ "$STRICT" == "1" ]]; then
    echo "[fatal] Missing required tool '$tool' for: $desc"
    exit 1
  fi
  echo "[skip] $desc (missing $tool)"
  return 1
}

step "Toolchain"
if need cargo >/dev/null 2>&1; then
  cargo --version
else
  if [[ "$STRICT" == "1" ]]; then
    echo "[fatal] cargo is required for local checks"
    exit 1
  fi
  echo "[skip] cargo not found; skipping local checks (set LOCAL_CHECK_STRICT=1 to require)"
  exit 0
fi
if need rustc >/dev/null 2>&1; then
  rustc --version
else
  if [[ "$STRICT" == "1" ]]; then
    echo "[fatal] rustc is required for local checks"
    exit 1
  fi
  echo "[skip] rustc not found; skipping local checks (set LOCAL_CHECK_STRICT=1 to require)"
  exit 0
fi
if need wasm-tools >/dev/null 2>&1; then
  wasm-tools --version
else
  echo "[info] wasm-tools not found; WIT/wasm checks skipped"
fi
if need jq >/dev/null 2>&1; then
  jq --version >/dev/null
  jq --version
else
  echo "[info] jq not found; auto-tag sanity check may be skipped"
fi

if [[ -f scripts/version-tools.sh ]]; then
  if ensure_tool jq "Auto-tag helper sanity"; then
    run_cmd "Auto-tag helper: list workspace crates" bash -c 'set -euo pipefail; source scripts/version-tools.sh; list_crates >/dev/null'
  else
    run_or_skip "Auto-tag helper (needs jq)" false
  fi
fi

run_cmd "Block tracked build artifacts (target/dist/build/cache)" bash -c '
  set -euo pipefail
  if ! command -v rg >/dev/null 2>&1; then
    echo "[skip] rg not found; cannot enforce tracked artifact guard"
    exit 0
  fi
  matches="$(git ls-files | rg -n "(^|/)(target|dist|build|node_modules|coverage|\\.pytest_cache|\\.cache)/" || true)"
  if [[ -n "${matches}" ]]; then
    echo "ERROR: tracked build artifacts detected. Remove from index with:"
    echo "  git rm -r --cached <path>"
    echo "${matches}"
    exit 1
  fi
'

if ensure_tool rg "Canonical greentic-interfaces import guard"; then
  run_cmd "Enforce canonical greentic-interfaces imports" bash -c 'set -euo pipefail; if rg -n "greentic_interfaces::bindings::|\\bbindings::greentic::" --glob "!**/target/**" --glob "!.codex/**" --glob "!ci/local_check.sh" .; then echo "ERROR: use greentic_interfaces::canonical instead of bindings::*"; exit 1; fi'
fi

run_cmd "cargo fmt --all -- --check" cargo fmt --all -- --check
run_cmd "cargo clippy --workspace --all-targets -- -D warnings" cargo clippy --workspace --all-targets -- -D warnings
run_cmd "cargo build --workspace --locked" cargo build --workspace --locked
run_cmd "cargo test -- --nocapture" cargo test -- --nocapture
run_cmd "cargo build --workspace --all-features" cargo build --workspace --all-features
run_cmd "cargo test --workspace --all-features -- --nocapture" cargo test --workspace --all-features -- --nocapture
run_cmd "cargo build --locked --features describe-v1,runner-host-v1" cargo build --locked --features "describe-v1,runner-host-v1"
run_cmd "cargo test --locked --features describe-v1,runner-host-v1" cargo test --locked --features "describe-v1,runner-host-v1"

if [[ "$ONLINE" == "1" ]]; then
  run_cmd "Online tests (greentic-mcp-exec::online_weather)" env RUN_ONLINE_TESTS=1 cargo test -p greentic-mcp-exec --test online_weather -- --nocapture
else
  run_or_skip "Online tests (set LOCAL_CHECK_ONLINE=1 to enable)" false
fi

echo ""
echo "✅ ci/local_check.sh completed successfully"
