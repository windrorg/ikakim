# ikakim — Multi-Platform Zero-Copy Live Streaming Engine

iOS / Android / Linux arm64 (Rockchip RK3588, Amlogic S905X2) / macOS に対応した、ゼロコピー GPU コンポジタ & ゼロ依存志向 Rust コアによるライブ配信エンジンです。

詳細な設計仕様とロードマップは [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) を参照してください。

## 開発環境

### 1. macOS ホスト環境（共通 Rust コア & Apple Metal ゼロコピー検証）
```bash
cargo test --workspace
```

### 2. Linux VM 環境（V4L2 `vivid` DMA-BUF / ALSA `snd-aloop` / Mesa Surfaceless EGL & Vulkan 検証）
Apple Silicon Mac 上で Lima を用いて Ubuntu 24.04 arm64 VM を起動・プロビジョニングできます：
```bash
brew install lima
limactl start --name=ikakim scripts/lima-ikakim.yaml
limactl shell ikakim
# VM 内で追加確認を行う場合:
./scripts/setup-linux-vm.sh
cargo test --workspace
```
