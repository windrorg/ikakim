# Multi-Platform Live Streaming Engine (`ikakim`) Architecture

## 1. 設計方針

- **Hybrid Host & VM Development (macOS Native + Linux VM):**
  - 共通 Rust コア（`core-*`）および Apple ネイティブ層（`apple-common` / `macos`）は Apple Silicon Mac ホスト上で直接ビルド・検証し、実機の Metal GPU・AVFoundation・VideoToolbox による 60fps ゼロコピーパイプラインを高速にイテレーションする。
  - Linux 固有 I/O 層（V4L2 `DMA-BUF`, ALSA `libasound` FFI, Mesa Surfaceless EGL / Vulkan）は、Lima 上の Ubuntu 24.04 arm64 VM（`vivid` 仮想 V4L2 ドライバ・`snd-aloop` 仮想 ALSA・Mesa `lavapipe`/`llvmpipe`）で並行検証し、実機 SoC（Rock 5B `RK3588` / Amlogic `S905X2`）への移行リスクを最小化する。
- **Zero-Copy First:**
  - 映像入力・オーバーレイ動画デコード・GPU 合成・エンコードの全区間をネイティブメモリハンドル（`CVPixelBuffer`, `DMA-BUF`, `AHardwareBuffer`）で直結し、CPU メモリへのリードバックを排除する（Linux VM 開発時のみコーデックアダプタ層でソフトエンコードへフォールバック可能とする）。
- **Minimal-Dependency Rust Core:**
  - コアエンジンは Rust で実装し、外部クレートは `wgpu` / `wgpu-hal` と `rustls`（RTMPS 用）および OS/SoC 固有の薄い FFI バインディングのみに限定する。
  - RTMP / AMF0 / FLV / HTTP / WebSocket / JSON-RPC / fMP4 / 音声ミキサー / ロックフリーリングバッファはゼロ依存で自前実装する。
- **Decoupled Control & Preview Plane:**
  - UI は内蔵 HTTP/WebSocket サーバー経由で制御（JSON-RPC）および低遅延プレビュー（fMP4 over WebSocket + MSE）を行う疎結合構成とし、Linux ヘッドレス環境のリモートブラウザとモバイル/デスクトップのローカル WebView を同一 UI で駆動する。

---

## 2. システム全体アーキテクチャ

```text
+-----------------------------------------------------------------------------------+
| Layer 3: Decoupled Control & Preview Plane (Web UI: Browser / WKWebView / WebView)|
|  - HTTP/1.1 Static Assets (HTML/JS/CSS)                                           |
|  - WebSocket JSON-RPC (Control / Mixer / Transform / Peak Meter 30Hz)             |
|  - WebSocket Binary Stream (fMP4 moof+mdat for MSE Low-Latency Preview)           |
+-----------------------------------------^-----------------------------------------+
                                          | Loopback or LAN (Port 8080)
+-----------------------------------------v-----------------------------------------+
| Layer 2: Zero-Dependency Rust Core (`stream-core`)                                |
|  +-----------------------+  +-----------------------+  +-----------------------+  |
|  | GPU Compositor        |  | Lock-Free Audio Mixer |  | Network & Muxer       |  |
|  | - wgpu + wgpu::hal    |  | - 48kHz / 2ch f32     |  | - Pure-Rust RTMP      |  |
|  | - NV12/YUYV -> RGB    |  | - Per-ch Delay (ms)   |  | - TLS (RTMPS)         |  |
|  | - Top-Bottom Alpha    |  | - Gain / Mute / Peak  |  | - AIMD ABR Controller |  |
|  | - Dual Render Target  |  | - Master Clock Sync   |  | - fMP4 / FLV Muxer    |  |
|  +-----------^-----------+  +-----------^-----------+  +-----------^-----------+  |
+--------------|--------------------------|--------------------------|--------------+
               | Zero-Copy Handle         | PCM RingBuffer           | Encoded NAL/AAC
+--------------|--------------------------|--------------------------|--------------+
| Layer 1: Platform Native I/O (`apple-common` / `linux-*` / `android`)             |
|  - Video Capture: AVFoundation (CVPixelBuffer) / V4L2 (DMA-BUF) / Camera2 NDK     |
|  - Overlay Decode: VideoToolbox / RK MPP & Amlogic V4L2 M2M / AMediaCodec         |
|  - HW Encode (Main + Preview): VTCompression / MPP & V4L2 M2M / MediaCodec        |
|  - Audio Capture & AAC: CoreAudio+AudioToolbox / ALSA+libfdk-aac / AAudio         |
+-----------------------------------------------------------------------------------+
```

---

## 3. 環境別・ゼロコピーおよびメディア I/O マッピング

| レイヤー | macOS (M1-M2 ホスト検証) | Linux VM (M1-M2 VM 検証) | Linux arm64 実機 (M3: RK3588 / S905X2) | iOS (M4) | Android (M4) |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **映像キャプチャ** | AVFoundation (`CVPixelBuffer`) | V4L2 `vivid` (`VIDIOC_EXPBUF` -> `DMA-BUF`) | V4L2 UVC/MIPI-CSI (`DMA-BUF`) | AVFoundation (`CVPixelBuffer`) | Camera2 NDK (`AHardwareBuffer`) |
| **GPU バックエンド (`wgpu`)** | Metal (`CVMetalTextureCache`) | Mesa `lavapipe` (Vulkan) / `llvmpipe` (Surfaceless EGL GLES 3.2) | RK3588: Vulkan (`DMA-BUF`) / S905X2: Surfaceless EGL GLES 3.2 | Metal (`CVMetalTextureCache`) | Vulkan (`AHardwareBuffer` ext) |
| **動画デコード (オーバーレイ)** | VideoToolbox (`CVPixelBuffer`) | `VideoDecoder` トレイト (VM 用ソフトデコードアダプタ) | Rockchip MPP / Amlogic V4L2 M2M (`DMA-BUF`) | VideoToolbox (`CVPixelBuffer`) | `AMediaCodec` (`AHardwareBuffer`) |
| **動画エンコード (Main + Preview)** | `VTCompressionSession` (Dual) | `VideoEncoder` トレイト (VM 用ソフトエンコードアダプタ) | Rockchip MPP / Amlogic V4L2 M2M (`DMA-BUF` 直結) | `VTCompressionSession` (Dual) | `AMediaCodec` Surface (Dual) |
| **音声入力 / AAC** | CoreAudio / AudioToolbox | ALSA `snd-aloop` (`libasound` FFI) / `libfdk-aac` FFI | ALSA 実機 PCM (`libasound` FFI) / `libfdk-aac` FFI | AVAudioEngine / AudioToolbox | AAudio / `AMediaCodec` |
| **UI ホスト** | `WKWebView` / ローカルブラウザ | ヘッドレス内蔵 HTTP/WS (ホストブラウザ接続) | ヘッドレス内蔵 HTTP/WS (リモート端末接続) | `WKWebView` | `Android WebView` |

---

## 4. リポジトリ・クレート構成

```text
ikakim/
├── docs/
│   └── ARCHITECTURE.md
├── scripts/
│   ├── lima-ikakim.yaml        # macOS 上の Lima Ubuntu 24.04 arm64 VM 定義
│   └── setup-linux-vm.sh       # vivid, snd-aloop, Mesa EGL/Vulkan, libasound/libfdk-aac 開発環境構築
├── crates/
│   ├── core-types/             # [依存ゼロ] 共通型、ロックフリーSPSCリングバッファ、48kHzマスタークロック、抽象トレイト
│   ├── core-audio/             # [依存ゼロ] 48kHz/2ch f32ミキサー、msディレイライン、リサンプラー、ピークメーター
│   ├── core-rtmp/              # [依存: rustls] 純Rust RTMP/RTMPS、AMF0、FLVミキサ、AIMD ABRコントローラ
│   ├── core-net-ui/            # [依存ゼロ] 内蔵HTTP/1.1 & WSサーバー、JSON-RPC、fMP4(moof/mdat)ミキサ
│   ├── core-gpu/               # [依存: wgpu, wgpu-hal] WGSLシェーダー(NV12/YUYV/Top-Bottom Alpha)、動的変形
│   ├── core-engine/            # パイプライン統合、A/V同期スケジューラ、状態管理
│   └── ffi-capi/               # Swift / Kotlin(JNI) / Linuxバイナリ向け C-ABI エクスポート
├── platforms/
│   ├── apple-common/           # macOS / iOS 共通の Metal・VideoToolbox・AudioToolbox ブリッジ
│   ├── macos/                  # macOS 検証・実行ホスト (AVFoundation, WKWebView / CLI)
│   ├── linux-common/           # V4L2 (DMA-BUF), Surfaceless EGL/Vulkan HAL, ALSA, libfdk-aac 共通実装
│   ├── linux-vm/               # VM開発用コーデックアダプタ & ヘッドレス実行バイナリ
│   ├── linux-arm64/            # RK3588 (MPP) & Amlogic S905X2 (V4L2 M2M) ハードウェアコーデック実装
│   ├── ios/                    # iOS アプリホスト
│   └── android/                # Android アプリホスト (Camera2, Vulkan AHardwareBuffer, AAudio, WebView)
└── web-ui/                     # 共通Web UI (ビルド成果物をRustバイナリに埋め込み配信)
```

---

## 5. コア抽象化インターフェース

- **`ExternalFrameImporter`**: OS/SoC 固有のネイティブバッファハンドル（`DmaBuf`, `CVPixelBuffer`, `AHardwareBuffer`）を `wgpu::Texture` へゼロコピー変換し、GPU 描画完了までライフタイムを保持するトレイト。
- **`VideoDecoder` / `VideoEncoder` / `AudioEncoder`**: オーバーレイ動画デコード、本配信・プレビューのデュアル映像エンコード、および 48kHz AAC-LC 音声エンコードを抽象化するトレイト。
- **`RenderSink`**: 合成フレームの出力先を抽象化するトレイト（エンコーダ入力用レンダーターゲットに加え、将来 Linux 実機の HDMI/LCD へ簡易ステータスを出す際の DRM/KMS + GBM スワップチェーン拡張に対応）。

---

## 6. 開発マイルストーン

1. **Milestone 1 (Core & Streaming Foundation - macOS & Linux VM):**
   - Cargo ワークスペース構築、`core-types`（SPSC リングバッファ・48kHz マスタークロック）、`core-audio`（48kHz ミキサー・入力別 ms ディレイ・ピークメーター・ALSA/CoreAudio 連携）、`core-rtmp`（純 Rust RTMPS + AIMD ABR）、`core-net-ui`（内蔵 HTTP/WS サーバー + fMP4/MSE プレビュー UI）。
2. **Milestone 2 (Zero-Copy GPU & Capture Pipeline - macOS Metal & Linux VM V4L2):**
   - `core-gpu`（`wgpu::hal` ゼロコピーインポート、NV12/YUYV シェーダー、Top-Bottom Alpha 動画合成、動的レイヤー変形、デュアルレンダーターゲット出力）。
   - macOS (`apple-common` Metal + VideoToolbox) および Linux VM (`vivid` V4L2 `DMA-BUF` + Mesa `lavapipe`/`llvmpipe`) での一気通貫統合。
3. **Milestone 3 (Linux arm64 Hardware Integration - RK3588 & S905X2):**
   - Rock 5B（RK3588 MPP）および Amlogic S905X2（V4L2 M2M / Surfaceless EGL）へのハードウェアコーデック差し替え、UVC/MIPI-CSI 実機入力、60fps 長時間負荷試験。
4. **Milestone 4 (Mobile Native Integration - iOS & Android):**
   - `apple-common` を用いた iOS アプリ統合、および Android（`AHardwareBuffer`・`AMediaCodec`・`AAudio`）のネイティブ I/O ブリッジ統合。
