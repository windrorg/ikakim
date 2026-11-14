#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "ERROR: scripts/run-macos-e2e.sh requires macOS." >&2
  exit 1
fi

echo "[1/4] Running workspace clippy (-D warnings)..."
cargo clippy --workspace --all-targets -- -D warnings

echo "[2/4] Running full workspace test suite..."
cargo test --workspace

SRS_PID=""
cleanup() {
  if [[ -n "$SRS_PID" ]] && kill -0 "$SRS_PID" 2>/dev/null; then
    kill "$SRS_PID" 2>/dev/null || true
    wait "$SRS_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

if [[ -x "$ROOT_DIR/scripts/run-srs-local.sh" ]]; then
  if ! lsof -iTCP:1935 -sTCP:LISTEN >/dev/null 2>&1; then
    echo "[3/4] Starting local SRS RTMP/HTTP-FLV server..."
    "$ROOT_DIR/scripts/run-srs-local.sh" >/tmp/ikakim-macos-srs.log 2>&1 &
    SRS_PID=$!
    for _ in $(seq 1 30); do
      if lsof -iTCP:1935 -sTCP:LISTEN >/dev/null 2>&1; then
        break
      fi
      sleep 0.2
    done
  else
    echo "[3/4] Reusing active RTMP server on :1935."
  fi
fi

echo "[4/4] Running macOS real-hardware Metal/VideoToolbox/AudioToolbox + SRS E2E tests..."
IKAKIM_REQUIRE_SRS=1 cargo test -p apple-common -p ffi-capi -p platform-macos

echo "[bonus] Verifying Web UI syntax, footprint & live platform-macos HTTP endpoint..."
./scripts/run-webui-e2e.sh

echo "ALL MACOS REAL-HARDWARE & WORKSPACE E2E SUITES PASSED."
