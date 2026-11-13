#!/usr/bin/env bash
set -euo pipefail

SRS_BIN="${IKAKIM_SRS_BIN:-$HOME/.local/srs/objs/srs}"
if [ ! -x "$SRS_BIN" ]; then
  if command -v srs >/dev/null 2>&1; then
    SRS_BIN="$(command -v srs)"
  else
    echo "SRS binary not found at $SRS_BIN. Run ./scripts/setup-srs-macos.sh first." >&2
    exit 1
  fi
fi

RTMP_PORT="${SRS_RTMP_PORT:-1935}"
API_PORT="${SRS_API_PORT:-1985}"
HTTP_PORT="${SRS_HTTP_PORT:-8080}"
WORK_DIR="${IKAKIM_SRS_WORKDIR:-/tmp/ikakim-srs-local}"

mkdir -p "$WORK_DIR/html"
CONF_PATH="$WORK_DIR/srs.local.conf"

cat > "$CONF_PATH" <<EOF
listen              $RTMP_PORT;
max_connections     1000;
daemon              off;
srs_log_tank        console;
pid                 $WORK_DIR/srs.pid;

rtc_server {
    enabled         off;
    ecdsa           off;
}

http_api {
    enabled         on;
    listen          $API_PORT;
    raw_api {
        enabled             on;
        allow_reload        on;
        allow_query         on;
        allow_update        on;
    }
}

http_server {
    enabled         on;
    listen          $HTTP_PORT;
    dir             $WORK_DIR/html;
}

vhost __defaultVhost__ {
    tcp_nodelay     on;
    min_latency     on;
    gop_cache       on;
    gop_cache_max_frames 2500;
    http_remux {
        enabled     on;
        mount       [vhost]/[app]/[stream].flv;
    }
    hls {
        enabled     on;
        hls_path    $WORK_DIR/html;
    }
}
EOF

echo "Starting local SRS ($SRS_BIN)"
echo "  RTMP ingest : rtmp://127.0.0.1:$RTMP_PORT/live/<stream_key>"
echo "  HTTP-FLV    : http://127.0.0.1:$HTTP_PORT/live/<stream_key>.flv"
echo "  SRS HTTP API: http://127.0.0.1:$API_PORT/api/v1/streams/"
export MallocNanoZone=0
exec "$SRS_BIN" -c "$CONF_PATH"
