#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

SKIP_RUST=0
ENABLE_SRS=1
PORT_IPHONE=18091
PORT_IPAD=18092
OUT_DIR="${ROOT_DIR}/build/ios-sim-e2e"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-rust) SKIP_RUST=1; shift ;;
    --no-srs) ENABLE_SRS=0; shift ;;
    --out-dir) OUT_DIR="$2"; shift 2 ;;
    *) echo "Usage: $0 [--skip-rust] [--no-srs] [--out-dir DIR]" >&2; exit 1 ;;
  esac
done

mkdir -p "$OUT_DIR"

if [[ "$SKIP_RUST" -eq 0 ]]; then
  echo "[1/6] Building IkakimFFI.xcframework..."
  ./scripts/build-ios-xcframework.sh --release
else
  echo "[1/6] Skipping Rust build (--skip-rust)."
fi

echo "[2/6] Generating Xcode project & building IkakimIosHost.app (arm64 iphonesimulator)..."
(cd "$ROOT_DIR/platforms/ios" && xcodegen generate)
xcodebuild \
  -project "$ROOT_DIR/platforms/ios/IkakimIosHost.xcodeproj" \
  -scheme IkakimIosHost \
  -configuration Debug \
  -sdk iphonesimulator \
  -destination 'generic/platform=iOS Simulator' \
  -derivedDataPath "$ROOT_DIR/build/ios-sim-dd" \
  ARCHS=arm64 ONLY_ACTIVE_ARCH=YES CODE_SIGNING_ALLOWED=NO \
  build >/tmp/ikakim-ios-xcodebuild.log

APP_PATH="$ROOT_DIR/build/ios-sim-dd/Build/Products/Debug-iphonesimulator/IkakimIosHost.app"
if [[ ! -d "$APP_PATH" ]]; then
  echo "ERROR: Built app not found at $APP_PATH" >&2
  exit 1
fi

SRS_PID=""
cleanup() {
  if [[ -n "$SRS_PID" ]] && kill -0 "$SRS_PID" 2>/dev/null; then
    kill "$SRS_PID" 2>/dev/null || true
    wait "$SRS_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

if [[ "$ENABLE_SRS" -eq 1 ]] && [[ -x "$ROOT_DIR/scripts/run-srs-local.sh" ]]; then
  if ! lsof -iTCP:1935 -sTCP:LISTEN >/dev/null 2>&1; then
    echo "[3/6] Starting local SRS RTMP/HTTP-FLV server..."
    "$ROOT_DIR/scripts/run-srs-local.sh" >/tmp/ikakim-ios-srs.log 2>&1 &
    SRS_PID=$!
    for _ in $(seq 1 30); do
      if lsof -iTCP:1935 -sTCP:LISTEN >/dev/null 2>&1; then
        break
      fi
      sleep 0.2
    done
  else
    echo "[3/6] Reusing active RTMP server on :1935."
  fi
else
  echo "[3/6] Local SRS disabled (--no-srs)."
fi

RUNTIME_ID="$(xcrun simctl list runtimes -j | python3 -c 'import sys,json; r=[x["identifier"] for x in json.load(sys.stdin)["runtimes"] if "iOS" in x.get("name","") and x.get("isAvailable")]; print(r[-1] if r else "")')"
if [[ -z "$RUNTIME_ID" ]]; then
  echo "ERROR: No available iOS Simulator runtime found. Run: xcodebuild -downloadPlatform iOS" >&2
  exit 1
fi
echo "[4/6] Using Simulator runtime: $RUNTIME_ID"

ensure_sim() {
  local name="$1"
  local dev_type="$2"
  local udid
  udid="$(xcrun simctl list devices -j | python3 -c "import sys,json; d=json.load(sys.stdin)['devices'].get('$RUNTIME_ID',[]); m=[x['udid'] for x in d if x['name']=='$name' and x.get('isAvailable')]; print(m[0] if m else '')")"
  if [[ -z "$udid" ]]; then
    udid="$(xcrun simctl create "$name" "$dev_type" "$RUNTIME_ID")"
  fi
  xcrun simctl boot "$udid" >/dev/null 2>&1 || true
  xcrun simctl bootstatus "$udid" -b >/dev/null
  echo "$udid"
}

run_sim_suite() {
  local label="$1"
  local sim_name="$2"
  local dev_type="$3"
  local port="$4"

  echo "=== [$label] Preparing $sim_name ($dev_type) ==="
  local udid
  udid="$(ensure_sim "$sim_name" "$dev_type")"
  local result_json="$OUT_DIR/${label}_e2e_result.json"
  local screen_png="$OUT_DIR/${label}_screen.png"
  local js_suite="$OUT_DIR/${label}_suite.js"
  rm -f "$result_json" "$screen_png"

  if [[ "$ENABLE_SRS" -eq 1 ]] && lsof -iTCP:1935 -sTCP:LISTEN >/dev/null 2>&1; then
    {
      echo "window.__IKAKIM_ENABLE_LIVE_STREAM_E2E = true;"
      echo "window.__IKAKIM_LIVE_STREAM_URL = 'rtmp://127.0.0.1:1935/live/ikakim_${label}';"
      cat "$ROOT_DIR/scripts/ios-sim-e2e-suite.js"
    } >"$js_suite"
  else
    cp "$ROOT_DIR/scripts/ios-sim-e2e-suite.js" "$js_suite"
  fi

  xcrun simctl terminate "$udid" com.ikakim.live.ios >/dev/null 2>&1 || true
  xcrun simctl install "$udid" "$APP_PATH"

  SIMCTL_CHILD_IKAKIM_VIRTUAL_IO=1 \
  SIMCTL_CHILD_IKAKIM_HTTP_PORT="$port" \
  SIMCTL_CHILD_IKAKIM_E2E_JS_PATH="$js_suite" \
  SIMCTL_CHILD_IKAKIM_E2E_RESULT_PATH="$result_json" \
    xcrun simctl launch "$udid" com.ikakim.live.ios

  echo "[$label] Waiting for WKWebView E2E completion on port $port..."
  local ok=0
  for _ in $(seq 1 60); do
    if [[ -f "$result_json" ]]; then
      ok=1
      break
    fi
    sleep 0.5
  done
  if [[ "$ok" -ne 1 ]]; then
    echo "ERROR: [$label] Timed out waiting for $result_json" >&2
    exit 1
  fi

  cat "$result_json"
  python3 - "$result_json" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
if not data.get("ok"):
    raise SystemExit(f"E2E failed: {data}")
if data.get("consoleErrors"):
    raise SystemExit(f"Console errors detected: {data['consoleErrors']}")
print(f"Verified {data.get('passed')} WKWebView checks cleanly with 0 console errors.")
PY

  # Verify HTTP endpoints from host directly against the running Simulator app
  curl -sf "http://127.0.0.1:${port}/api/status" | python3 -c 'import sys,json; s=json.load(sys.stdin); assert "stream" in s and "master" in s and "channels" in s and "overlays" in s, s; print("Host /api/status OK: fps=", s["stream"].get("fps"), "overlays=", len(s["overlays"]))'

  xcrun simctl io "$udid" screenshot "$screen_png"
  echo "[$label] Screenshot saved: $screen_png"

  xcrun simctl terminate "$udid" com.ikakim.live.ios >/dev/null 2>&1 || true
}

echo "[5/6] Running iPhone Simulator (iOS) E2E..."
run_sim_suite "iphone" "ikakim-iPhone-E2E" "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro" "$PORT_IPHONE"

echo "[6/6] Running iPad Simulator (iPadOS) E2E..."
run_sim_suite "ipad" "ikakim-iPad-E2E" "com.apple.CoreSimulator.SimDeviceType.iPad-Pro-13-inch-M4-16GB" "$PORT_IPAD"

echo "ALL iOS & iPadOS SIMULATOR E2E SUITES PASSED."
