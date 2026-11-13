#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

PROFILE="release"
if [[ "${1:-}" == "--debug" ]]; then
  PROFILE="debug"
elif [[ "${1:-}" == "--release" || -z "${1:-}" ]]; then
  PROFILE="release"
else
  echo "Usage: $0 [--release | --debug]" >&2
  exit 1
fi

CARGO_FLAGS=()
if [[ "$PROFILE" == "release" ]]; then
  CARGO_FLAGS+=(--release)
fi

export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-16.0}"

echo "[1/4] Building ffi-capi staticlib ($PROFILE, iOS >= $IPHONEOS_DEPLOYMENT_TARGET) for aarch64-apple-ios & aarch64-apple-ios-sim..."
cargo build -p ffi-capi --target aarch64-apple-ios ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}
cargo build -p ffi-capi --target aarch64-apple-ios-sim ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}

DEV_LIB="$ROOT_DIR/target/aarch64-apple-ios/$PROFILE/libffi_capi.a"
SIM_LIB="$ROOT_DIR/target/aarch64-apple-ios-sim/$PROFILE/libffi_capi.a"
OUT_DIR="$ROOT_DIR/build/ios"
HEADERS_STAGE="$OUT_DIR/headers"
XCFRAMEWORK_OUT="$OUT_DIR/IkakimFFI.xcframework"

echo "[2/4] Verifying static archives exist..."
ls -lh "$DEV_LIB" "$SIM_LIB"

rm -rf "$HEADERS_STAGE" "$XCFRAMEWORK_OUT"
mkdir -p "$HEADERS_STAGE"
cp "$ROOT_DIR/crates/ffi-capi/include/ikakim_ffi.h" "$HEADERS_STAGE/ikakim_ffi.h"
cat > "$HEADERS_STAGE/module.modulemap" <<'EOF'
module IkakimFFI {
    header "ikakim_ffi.h"
    export *
}
EOF

echo "[3/4] Packaging IkakimFFI.xcframework via xcodebuild..."
xcodebuild -create-xcframework \
  -library "$DEV_LIB" -headers "$HEADERS_STAGE" \
  -library "$SIM_LIB" -headers "$HEADERS_STAGE" \
  -output "$XCFRAMEWORK_OUT"

echo "[4/4] Done: $XCFRAMEWORK_OUT"
ls -la "$XCFRAMEWORK_OUT"
