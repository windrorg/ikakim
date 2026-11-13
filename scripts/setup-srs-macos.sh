#!/usr/bin/env bash
set -euo pipefail

SRS_TAG="${SRS_TAG:-v6.0.48}"
SRS_PREFIX="${IKAKIM_SRS_DIR:-$HOME/.local/srs}"
SRS_SRC="${IKAKIM_SRS_SRC:-$HOME/.local/srs-src}"

echo "=== [1/4] Checking Homebrew build tools & ffmpeg ==="
if ! command -v brew >/dev/null 2>&1; then
  echo "error: Homebrew is required on macOS to install build dependencies" >&2
  exit 1
fi

for pkg in automake autoconf pkgconf libtool ffmpeg; do
  if ! brew list "$pkg" >/dev/null 2>&1; then
    echo "Installing $pkg..."
    NONINTERACTIVE=1 brew install "$pkg"
  fi
done

if [ -x "$SRS_PREFIX/objs/srs" ]; then
  echo "SRS already installed at $SRS_PREFIX/objs/srs ($("$SRS_PREFIX/objs/srs" -v 2>/dev/null | tail -n 1 || true))"
  exit 0
fi

echo "=== [2/4] Cloning ossrs/srs ($SRS_TAG) ==="
rm -rf "$SRS_SRC"
git clone --depth 1 --branch "$SRS_TAG" https://github.com/ossrs/srs.git "$SRS_SRC"

echo "=== [3/4] Configuring & Building SRS for macOS ==="
cd "$SRS_SRC/trunk"
JOBS="$(sysctl -n hw.ncpu 2>/dev/null || echo 8)"
./configure --prefix="$SRS_PREFIX" --srt=off --sanitizer=off --https=off --jobs="$JOBS"
make -j"$JOBS"

echo "=== [4/4] Installing SRS binary & configs into $SRS_PREFIX ==="
# make install runs Linux init.d sed near the end on macOS BSD sed; binary and conf copy finish first
make install || true
mkdir -p "$SRS_PREFIX/objs" "$SRS_PREFIX/conf"
cp -f "$SRS_SRC/trunk/objs/srs" "$SRS_PREFIX/objs/srs"
cp -Rf "$SRS_SRC/trunk/conf/"* "$SRS_PREFIX/conf/"

MallocNanoZone=0 "$SRS_PREFIX/objs/srs" -v
echo "SRS setup complete: $SRS_PREFIX/objs/srs"
