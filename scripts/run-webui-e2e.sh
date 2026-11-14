#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

echo "[1/3] Checking Web UI JS syntax & gzip footprint..."
node --check web-ui/app.js
HTML_GZ=$(gzip -c web-ui/index.html | wc -c | tr -d ' ')
CSS_GZ=$(gzip -c web-ui/style.css | wc -c | tr -d ' ')
JS_GZ=$(gzip -c web-ui/app.js | wc -c | tr -d ' ')
echo "      index.html=${HTML_GZ}B style.css=${CSS_GZ}B app.js=${JS_GZ}B (total gzip=$((HTML_GZ + CSS_GZ + JS_GZ))B)"

echo "[2/3] Running core-net-ui HTTP/WS/RPC/MSE + subset invariant tests..."
cargo test -p core-net-ui

if [ "$(uname -s)" != "Darwin" ]; then
  echo "[3/3] Non-macOS host detected; skipping platform-macos launch (steps 1-2 passed)."
  exit 0
fi

echo "[3/3] Starting temporary platform-macos host for browser E2E verification..."
cargo build -p platform-macos
LOG_FILE=$(mktemp)
./target/debug/platform-macos --frames 0 127.0.0.1:0 >"$LOG_FILE" 2>&1 &
HOST_PID=$!
cleanup() {
  if kill -0 "$HOST_PID" 2>/dev/null; then
    kill "$HOST_PID" 2>/dev/null || true
    wait "$HOST_PID" 2>/dev/null || true
  fi
  rm -f "$LOG_FILE"
}
trap cleanup EXIT

URL=""
for _ in $(seq 1 40); do
  if grep -q "Web UI listening at " "$LOG_FILE"; then
    URL=$(sed -n 's/.*Web UI listening at \(http:\/\/[^ ]*\).*/\1/p' "$LOG_FILE" | head -n1)
    break
  fi
  sleep 0.1
done

if [ -z "$URL" ]; then
  echo "Failed to detect live Web UI URL from platform-macos:"
  cat "$LOG_FILE"
  exit 1
fi

echo "Live Web UI active at: $URL"
echo "Suite script ready at: scripts/webui-e2e-suite.js"
if command -v node >/dev/null 2>&1 && node -e "require.resolve('playwright')" >/dev/null 2>&1; then
  node - "$URL" <<'EOF'
const fs = require('fs');
const { chromium } = require('playwright');
(async () => {
  const url = process.argv[2];
  const browser = await chromium.launch();
  try {
    const errors = [];
    for (const vp of [{ width: 1280, height: 800 }, { width: 390, height: 844 }]) {
      const page = await browser.newPage({ viewport: vp });
      page.on('pageerror', (e) => errors.push(String(e)));
      page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
      await page.goto(url, { waitUntil: 'networkidle' });
      const res = await page.evaluate(fs.readFileSync('scripts/webui-e2e-suite.js', 'utf8'));
      console.log(`Viewport ${vp.width}x${vp.height}:`, JSON.stringify(res));
      await page.close();
    }
    if (errors.length > 0) throw new Error('Console/Page errors: ' + errors.join('; '));
  } finally {
    await browser.close();
  }
})();
EOF
else
  echo "Headless playwright module not installed globally; verified HTTP readiness at $URL:"
  curl -sf "${URL}/" | grep -q "MSE Low-Latency Preview"
  curl -sf "${URL}/api/status" | grep -q "stream"
  echo "Live HTTP endpoints verified cleanly (evaluate scripts/webui-e2e-suite.js in Chromium/Safari for full interactive run)."
fi
