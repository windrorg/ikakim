# ikakim — Multi-Platform Zero-Copy Live Streaming Engine

iOS / Android / Linux arm64 (Rockchip RK3588, Amlogic S905X2) / macOS に対応した、ゼロコピー GPU コンポジタ & ゼロ依存志向 Rust コアによるマルチプラットフォーム・ライブ配信エンジンです。

詳細な設計仕様とモジュール構成は [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) を参照してください。

## ワークスペース構成

### コアクレート (`crates/`)
- `crates/core-types`: ロックフリー SPSC リングバッファ、48kHz `MasterClock`、状態機械、共通メディア/コーデック抽象トレイト
- `crates/core-audio`: 完全ゼロ依存 48kHz/2ch ミキサー（チャンネル別 ms ディレイ・ソフトクリッパー・ピークメーター・Polyphase リサンプラー）
- `crates/core-rtmp`: 純 Rust RTMPS クライアント（`rustls` + AMF0 + FLV + Chunk + AIMD 動的ビットレート制御 ABR）
- `crates/core-gpu`: `wgpu` / WGSL ゼロコピー GPU コンポジタ（NV12 / YUYV / Top-Bottom Alpha、Z順、オーバーレイ負荷ガバナ、Main + Preview デュアルレンダーターゲット）
- `crates/core-net-ui`: ゼロ依存内蔵 HTTP/1.1 + WebSocket サーバー、JSON-RPC 制御、`web-ui/` 埋め込み、fMP4/MSE プレビュー配信
- `crates/core-engine`: A/V パイプライン統合ブリッジ (`EngineControlBridge`) & 純 Rust H.264 Constrained Baseline フォールバックエンコーダ/デコーダ (`SwH264Encoder` / `SwH264Decoder`)
- `crates/ffi-capi`: C99 ヘッダ ([include/ikakim_ffi.h](crates/ffi-capi/include/ikakim_ffi.h))、C-ABI エクスポート (`rlib` / `staticlib` / `cdylib`)、Android JNI エクスポート (`Java_com_ikakim_live_IkakimBridge_*`)

### プラットフォーム層 (`platforms/`)
- `platforms/apple-common`: `AVFoundation` / `CVMetalTextureCache` ゼロコピー / `VideoToolbox` / `AudioToolbox` / `ImageIO`
- `platforms/macos`: macOS ネイティブホストバイナリ (`platform-macos`)
- `platforms/ios`: Swift + `WKWebView` iOS ホスト（[IkakimEngine.swift](platforms/ios/IkakimEngine.swift), [AVCaptureCameraBridge.swift](platforms/ios/AVCaptureCameraBridge.swift), [IkakimWebViewApp.swift](platforms/ios/IkakimWebViewApp.swift), [module.modulemap](platforms/ios/module.modulemap)）
- `platforms/linux-common`: V4L2 キャプチャ/M2M (`v4l2_m2m`)・ALSA (`alsa`)・`libfdk-aac`・DRM/GBM (`drm_gbm`)・Surfaceless EGL (`dmabuf_gpu`)・Rockchip MPP (`rockchip_mpp`)
- `platforms/linux-vm` / `platforms/linux-arm64`: Lima Linux VM 検証ホストおよび Rock 5B (RK3588) / Amlogic S905X2 向け実機ランタイム (`platform-linux-arm64`)
- `platforms/android-native`: Android HAL ブリッジ（`AHardwareBuffer` + `VK_ANDROID_external_memory_android_hardware_buffer`、`AMediaCodec` `createInputSurface` デュアルエンコーダ、`AAudio` キャプチャ＋モック HAL 検証基盤）
- `platforms/android`: Kotlin + `Camera2` (`HardwareBuffer`) + `WebView` Android Studio アプリ骨格

---

## ビルド・検証コマンド

### 1. macOS ホスト（Apple Silicon Metal / VideoToolbox / AudioToolbox + 全ワークスペース）
```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p platform-macos -- --frames 30 127.0.0.1:0
```

### 2. iOS ターゲット（`aarch64-apple-ios` / `aarch64-apple-ios-sim` + Swift 型チェック）
```bash
cargo build -p ffi-capi --target aarch64-apple-ios
cargo build -p ffi-capi --target aarch64-apple-ios-sim
xcrun --sdk iphoneos swiftc -typecheck -target arm64-apple-ios16.0 -I platforms/ios platforms/ios/*.swift
xcrun --sdk iphonesimulator swiftc -typecheck -target arm64-apple-ios16.0-simulator -I platforms/ios platforms/ios/*.swift
```

### 3. Linux arm64 / Lima VM（V4L2 `vivid`/`vicodec` DMA-BUF / ALSA `snd-aloop` / Surfaceless EGL & Vulkan）
```bash
limactl start --name=ikakim scripts/lima-ikakim.yaml
limactl shell ikakim
./scripts/setup-linux-vm.sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p platform-linux-arm64 --release -- --soak --frames 150
```

### 4. Android 実機向けビルド手順（Android NDK / Gradle 環境が必要）
```bash
# Android NDK 上で Rust JNI 共有ライブラリ (libffi_capi.so) をビルド
cargo ndk -t arm64-v8a -o platforms/android/app/src/main/jniLibs build --release -p ffi-capi
# 続いて Android Studio または platforms/android で Gradle ビルドを実行
```
