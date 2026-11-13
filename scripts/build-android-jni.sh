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

API_LEVEL="${ANDROID_API_LEVEL:-29}"
NDK_DIR="${ANDROID_NDK_HOME:-${ANDROID_NDK_LATEST_HOME:-}}"
if [[ -z "$NDK_DIR" && -n "${ANDROID_HOME:-}" && -d "$ANDROID_HOME/ndk" ]]; then
  NDK_DIR="$(ls -d "$ANDROID_HOME"/ndk/* 2>/dev/null | sort -V | tail -n 1 || true)"
fi

if [[ -z "$NDK_DIR" || ! -d "$NDK_DIR/toolchains/llvm/prebuilt" ]]; then
  echo "ERROR: Android NDK not found. Set ANDROID_NDK_HOME or install NDK under \$ANDROID_HOME/ndk." >&2
  exit 1
fi

HOST_TAG="$(ls "$NDK_DIR/toolchains/llvm/prebuilt" | head -n 1)"
LLVM_BIN="$NDK_DIR/toolchains/llvm/prebuilt/$HOST_TAG/bin"
CLANG_BIN="$LLVM_BIN/aarch64-linux-android${API_LEVEL}-clang"
AR_BIN="$LLVM_BIN/llvm-ar"

if [[ ! -x "$CLANG_BIN" ]]; then
  echo "ERROR: Expected NDK compiler not executable: $CLANG_BIN" >&2
  exit 1
fi

export CC_aarch64_linux_android="$CLANG_BIN"
export AR_aarch64_linux_android="$AR_BIN"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CLANG_BIN"
export PATH="$LLVM_BIN:$PATH"

CARGO_FLAGS=()
if [[ "$PROFILE" == "release" ]]; then
  CARGO_FLAGS+=(--release)
fi

echo "[1/2] Building ffi-capi cdylib ($PROFILE) for aarch64-linux-android (API $API_LEVEL)..."
cargo build -p ffi-capi --target aarch64-linux-android ${CARGO_FLAGS[@]+"${CARGO_FLAGS[@]}"}

SO_SRC="$ROOT_DIR/target/aarch64-linux-android/$PROFILE/libffi_capi.so"
JNILIBS_DIR="$ROOT_DIR/platforms/android/app/src/main/jniLibs/arm64-v8a"
mkdir -p "$JNILIBS_DIR"
cp "$SO_SRC" "$JNILIBS_DIR/libffi_capi.so"

echo "[2/2] Staged JNI library:"
ls -lh "$JNILIBS_DIR/libffi_capi.so"
