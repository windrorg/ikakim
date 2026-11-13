#!/usr/bin/env bash
set -euo pipefail

echo "=== [1/4] Installing system dependencies (V4L2, ALSA, FDK-AAC, Mesa Vulkan/EGL) ==="
sudo apt-get update
# Ubuntuの場合、libfdk-aac-dev のために multiverse を有効化（Debianの場合はスキップ）
if command -v add-apt-repository >/dev/null 2>&1; then
    sudo add-apt-repository -y multiverse || true
    sudo apt-get update
fi

sudo apt-get install -y --no-install-recommends \
    build-essential \
    pkg-config \
    cmake \
    clang \
    llvm \
    curl \
    ca-certificates \
    v4l-utils \
    libv4l-dev \
    libasound2-dev \
    alsa-utils \
    libfdk-aac-dev \
    libegl1-mesa-dev \
    libgles2-mesa-dev \
    libvulkan-dev \
    mesa-vulkan-drivers \
    libgbm-dev \
    libdrm-dev \
    vulkan-tools \
    mesa-utils

echo "=== [2/4] Ensuring kernel extra modules (vivid, snd-aloop) are available ==="
KREL="$(uname -r)"
if ! modinfo vivid >/dev/null 2>&1 || ! modinfo snd-aloop >/dev/null 2>&1; then
    echo "Installing linux-modules-extra-${KREL}..."
    sudo apt-get install -y "linux-modules-extra-${KREL}" || {
        echo "WARNING: Could not install linux-modules-extra-${KREL}. Check your VM kernel package."
    }
fi

echo "=== [3/4] Loading and persisting vivid & snd-aloop kernel modules ==="
# V4L2仮想デバイス: エラー注入を無効化し、キャプチャデバイスを有効化
sudo modprobe vivid no_error_inj=1 || echo "WARNING: modprobe vivid failed"
# ALSA仮想ループバック: 2系統(index=10, 11)を作成して複数音声入力ミキシングを検証可能にする
sudo modprobe snd-aloop enable=1,1 index=10,11 || echo "WARNING: modprobe snd-aloop failed"

cat << 'EOF' | sudo tee /etc/modules-load.d/live-streamer.conf >/dev/null
vivid
snd-aloop
EOF

cat << 'EOF' | sudo tee /etc/modprobe.d/live-streamer.conf >/dev/null
options vivid no_error_inj=1
options snd-aloop enable=1,1 index=10,11
EOF

sudo usermod -aG video,audio "$USER" || true

echo "=== [4/4] Verifying virtual devices & GPU runtimes ==="
echo "--- V4L2 Devices ---"
v4l2-ctl --list-devices || true
echo "--- ALSA Capture Devices ---"
arecord -l || true
echo "--- Vulkan ICDs (lavapipe check) ---"
ls -la /usr/share/vulkan/icd.d/ || true

if ! command -v cargo >/dev/null 2>&1; then
    echo "Installing Rust toolchain inside VM..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
fi

rustc --version
cargo --version

echo "=== Setup complete! ==="
