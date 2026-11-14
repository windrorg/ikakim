#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

export ANDROID_HOME="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
if [[ -z "${ANDROID_NDK_HOME:-}" && -d "$ANDROID_HOME/ndk" ]]; then
  ANDROID_NDK_HOME="$(ls -d "$ANDROID_HOME/ndk"/* 2>/dev/null | sort -V | tail -n1)"
  export ANDROID_NDK_HOME
fi
export JAVA_HOME="${JAVA_HOME:-/opt/homebrew/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home}"

ADB="${ADB:-$(command -v adb || echo /opt/homebrew/bin/adb)}"
GRADLE="${GRADLE:-$(command -v gradle || echo /opt/homebrew/opt/gradle@8/bin/gradle)}"

SERIAL=""
SKIP_RUST=0
ENABLE_SRS=1
UI_PORT=18095
OUT_DIR="${ROOT_DIR}/build/android-e2e"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --serial) SERIAL="$2"; shift 2 ;;
    --skip-rust) SKIP_RUST=1; shift ;;
    --no-srs) ENABLE_SRS=0; shift ;;
    *) echo "Usage: $0 [--serial SERIAL] [--skip-rust] [--no-srs]" >&2; exit 1 ;;
  esac
done

if [[ -z "$SERIAL" ]]; then
  SERIAL="$("$ADB" devices | awk '$2=="device"{print $1; exit}')"
fi
if [[ -z "$SERIAL" ]]; then
  echo "ERROR: No authorized Android device found via adb." >&2
  exit 1
fi
echo "Using Android device: $SERIAL"

mkdir -p "$OUT_DIR"

if [[ "$SKIP_RUST" -eq 0 ]]; then
  echo "[1/6] Building 16KB-aligned libffi_capi.so (release)..."
  ./scripts/build-android-jni.sh --release
else
  echo "[1/6] Skipping Rust JNI build (--skip-rust)."
fi

echo "[2/6] Building app-debug.apk..."
(cd "$ROOT_DIR/platforms/android" && "$GRADLE" assembleDebug --no-daemon)

APK_PATH="$ROOT_DIR/platforms/android/app/build/outputs/apk/debug/app-debug.apk"
"$ADB" -s "$SERIAL" install -r "$APK_PATH"

SRS_PID=""
cleanup() {
  "$ADB" -s "$SERIAL" reverse --remove tcp:1935 >/dev/null 2>&1 || true
  "$ADB" -s "$SERIAL" forward --remove "tcp:${UI_PORT}" >/dev/null 2>&1 || true
  if [[ -n "$SRS_PID" ]] && kill -0 "$SRS_PID" 2>/dev/null; then
    kill "$SRS_PID" 2>/dev/null || true
    wait "$SRS_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

if [[ "$ENABLE_SRS" -eq 1 ]] && [[ -x "$ROOT_DIR/scripts/run-srs-local.sh" ]]; then
  if ! lsof -iTCP:1935 -sTCP:LISTEN >/dev/null 2>&1; then
    echo "[3/6] Starting local SRS RTMP/HTTP-FLV server..."
    "$ROOT_DIR/scripts/run-srs-local.sh" >/tmp/ikakim-android-srs.log 2>&1 &
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
  "$ADB" -s "$SERIAL" reverse tcp:1935 tcp:1935
else
  echo "[3/6] Local SRS disabled (--no-srs)."
fi

echo "[4/6] Staging E2E suite & launching IkakimHostActivity with Virtual I/O..."
EXT_DIR="/sdcard/Android/data/com.ikakim.live/files"
JS_STAGE="$OUT_DIR/android_suite.js"
RES_LOCAL="$OUT_DIR/android_e2e_result.json"
SCREEN_LOCAL="$OUT_DIR/android_screen.png"
rm -f "$RES_LOCAL" "$SCREEN_LOCAL"

if [[ "$ENABLE_SRS" -eq 1 ]] && lsof -iTCP:1935 -sTCP:LISTEN >/dev/null 2>&1; then
  {
    echo "window.__IKAKIM_ENABLE_LIVE_STREAM_E2E = true;"
    echo "window.__IKAKIM_LIVE_STREAM_URL = 'rtmp://127.0.0.1:1935/live/ikakim_android';"
    cat "$ROOT_DIR/scripts/ios-sim-e2e-suite.js"
  } >"$JS_STAGE"
else
  cp "$ROOT_DIR/scripts/ios-sim-e2e-suite.js" "$JS_STAGE"
fi

"$ADB" -s "$SERIAL" shell "am force-stop com.ikakim.live && mkdir -p '$EXT_DIR' && rm -f '$EXT_DIR/e2e_result.json'"
"$ADB" -s "$SERIAL" push "$JS_STAGE" "$EXT_DIR/e2e_suite.js" >/dev/null
"$ADB" -s "$SERIAL" logcat -c
"$ADB" -s "$SERIAL" shell "am start -W -n com.ikakim.live/.IkakimHostActivity --ez virtual_io true --ei http_port $UI_PORT --es e2e_js_name e2e_suite.js --es e2e_result_name e2e_result.json"

echo "[5/6] Waiting for Android WebView E2E result..."
OK=0
for _ in $(seq 1 60); do
  if "$ADB" -s "$SERIAL" shell "test -f '$EXT_DIR/e2e_result.json'" >/dev/null 2>&1; then
    "$ADB" -s "$SERIAL" pull "$EXT_DIR/e2e_result.json" "$RES_LOCAL" >/dev/null
    OK=1
    break
  fi
  sleep 0.5
done
if [[ "$OK" -ne 1 ]]; then
  echo "ERROR: Timed out waiting for Android WebView E2E result" >&2
  "$ADB" -s "$SERIAL" logcat -d -s ikakim:I ikakim:E AndroidRuntime:E | tail -n 30
  exit 1
fi

cat "$RES_LOCAL"
python3 - "$RES_LOCAL" <<'PY'
import json, sys
data = json.load(open(sys.argv[1]))
if not data.get("ok"):
    raise SystemExit(f"Android WebView E2E failed: {data}")
print(f"Verified {data.get('passed')} Android WebView checks cleanly.")
PY

if "$ADB" -s "$SERIAL" logcat -d -s ikakim:E | grep -q "WebView console.error"; then
  echo "ERROR: WebView console.error detected in logcat:" >&2
  "$ADB" -s "$SERIAL" logcat -d -s ikakim:E
  exit 1
fi

echo "[6/6] Verifying host /api/status via adb forward & capturing screenshot..."
"$ADB" -s "$SERIAL" forward "tcp:${UI_PORT}" "tcp:${UI_PORT}"
curl -sf "http://127.0.0.1:${UI_PORT}/api/status" | python3 -c 'import sys,json; s=json.load(sys.stdin); assert "stream" in s and "master" in s and "channels" in s and "overlays" in s, s; print("Host /api/status OK: fps=", s["stream"].get("fps"), "overlays=", len(s["overlays"]))'

"$ADB" -s "$SERIAL" shell "screencap -p /data/local/tmp/ikakim_android_screen.png" >/dev/null 2>&1
"$ADB" -s "$SERIAL" pull /data/local/tmp/ikakim_android_screen.png "$SCREEN_LOCAL" >/dev/null
"$ADB" -s "$SERIAL" shell "rm -f /data/local/tmp/ikakim_android_screen.png" >/dev/null
echo "Screenshot saved: $SCREEN_LOCAL"
echo "ALL ANDROID REAL-DEVICE E2E SUITES PASSED."
