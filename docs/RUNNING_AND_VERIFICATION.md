# macOS / iOS / Android 動作・検証ガイド

別の Mac で作業を再開する際や、iOS / Android 実機・シミュレーターで `ikakim` を起動・検証するための手順まとめです。

---

## 1. 環境セットアップ（新しい Mac での初回準備）

### 共通（Rust）
```bash
rustup target add aarch64-apple-ios aarch64-apple-ios-sim aarch64-linux-android
```

### iOS / iPadOS 用
- Xcode（iOS Simulator ランタイムを含む。未導入なら `xcodebuild -downloadPlatform iOS`）
- XcodeGen:
  ```bash
  brew install xcodegen
  ```

### Android 用
- Android SDK & NDK（Android Studio または SDK Manager で `ndk` をインストール）
- `adb` / `gradle@8` / `openjdk@17`:
  ```bash
  brew install android-platform-tools gradle@8 openjdk@17
  ```
- 環境変数例（`~/.zshrc` 等）:
  ```bash
  export ANDROID_HOME="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
  export ANDROID_NDK_HOME="$(ls -d "$ANDROID_HOME"/ndk/* 2>/dev/null | sort -V | tail -n 1)"
  export JAVA_HOME="/opt/homebrew/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home"
  ```

### ローカル RTMP / HTTP-FLV サーバー（SRS: 実配信 E2E 用・任意）
```bash
./scripts/setup-srs-macos.sh
# 起動する場合:
./scripts/run-srs-local.sh
```

---

## 2. macOS での動作・検証

```bash
# Clippy + 全ワークスペーステスト + Metal/VideoToolbox/AudioToolbox + SRS + Web UI E2E
./scripts/run-macos-e2e.sh
```

単体でビルド・テストする場合:
```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p platform-macos -- --frames 30 127.0.0.1:0
```

---

## 3. iOS / iPadOS での動作・検証

### A. 自動 E2E スイート（iPhone + iPad Simulator）
Rust XCFramework ビルド、`xcodegen`、`xcodebuild`、ローカル SRS 起動、iPhone / iPad Simulator でのアプリ起動、`WKWebView` 内 UI & MSE プレビュー & RTMP 配信の自動検証を一括実行します。

```bash
./scripts/run-ios-sim-e2e.sh
# Rust ビルドを省略する場合:
./scripts/run-ios-sim-e2e.sh --skip-rust
# ローカル SRS を使わない場合:
./scripts/run-ios-sim-e2e.sh --no-srs
```

### B. Xcode から実機 iPhone / iPad または Simulator で手動起動する
```bash
# 1. Rust staticlib (aarch64-apple-ios / aarch64-apple-ios-sim) -> XCFramework 生成
./scripts/build-ios-xcframework.sh --release

# 2. Xcode プロジェクト生成
cd platforms/ios && xcodegen generate

# 3. Xcode で開く
open platforms/ios/IkakimIosHost.xcodeproj
```
- 実機 iPhone / iPad で動かす場合は、Xcode の **Signing & Capabilities** で自身の Apple Developer Team を選択してビルド・実行してください。
- アプリ上部の **Camera** ボタンで実機カメラ入力、**Virtual I/O** ボタンでテスト映像・音声入力を切り替えられます。

### C. Simulator で起動したまま Mac のブラウザ / Safari で確認する
```bash
# Simulator 上で Virtual I/O 有効・ポート 18091 固定で起動
SIMCTL_CHILD_IKAKIM_VIRTUAL_IO=1 \
SIMCTL_CHILD_IKAKIM_HTTP_PORT=18091 \
xcrun simctl launch booted com.ikakim.live.ios
```
- Mac のブラウザで `http://127.0.0.1:18091/` を開くと、Simulator 内のエンジンと UI に直接アクセスできます。
- `WKWebView.isInspectable = true` が有効なため、Mac の Safari「開発」メニューから `WKWebView` の Web Inspector も利用できます。

---

## 4. Android（実機 / エミュレーター）での動作・検証

### A. Rust JNI ライブラリと APK のビルド
Pixel 等の Android 15+ 16KB ページサイズ端末向けに `-C link-arg=-Wl,-z,max-page-size=16384` 付きで `libffi_capi.so` をビルドします。

```bash
# 1. libffi_capi.so をビルドして platforms/android/app/src/main/jniLibs/arm64-v8a へ配置
./scripts/build-android-jni.sh --release

# 2. APK ビルド
cd platforms/android && gradle assembleDebug --no-daemon
```
生成先: `platforms/android/app/build/outputs/apk/debug/app-debug.apk`

### B. 自動 E2E スイート（実機 / エミュレーター）
```bash
# 接続中のデバイスを自動検出して実行（複数台ある場合は --serial を指定）
./scripts/run-android-e2e.sh
./scripts/run-android-e2e.sh --serial <DEVICE_SERIAL>
./scripts/run-android-e2e.sh --serial <DEVICE_SERIAL> --skip-rust --no-srs
```
スクリプト終了時の `trap cleanup EXIT` で `adb forward` / `adb reverse` は自動解除されます。

### C. 実機（Pixel 等）を起動したまま Mac からリモートアクセスして動作確認する

Android の内蔵サーバー (`crates/ffi-capi/src/lib.rs`) はセキュリティ上 `127.0.0.1`（ループバック）にバインドしているため、USB 接続した Mac から `adb forward` 経由でアクセスします。

1. **接続と USB デバッグ許可の確認**
   ```bash
   adb devices -l
   ```
   対象端末が `device` になっていることを確認します（`unauthorized` の場合は端末側の「USBデバッグを許可しますか？」で許可をタップ）。

2. **（未インストール／更新時）APK インストール**
   ```bash
   adb -s <DEVICE_SERIAL> install -r platforms/android/app/build/outputs/apk/debug/app-debug.apk
   ```

3. **ポートフォワード設定（必要に応じて RTMP 用 reverse も設定）**
   ```bash
   # Mac の http://127.0.0.1:18095/ -> Android 端末の 127.0.0.1:18095
   adb -s <DEVICE_SERIAL> forward tcp:18095 tcp:18095

   # （任意）Android 端末から Mac 上のローカル SRS (:1935) へ RTMP 配信する場合
   adb -s <DEVICE_SERIAL> reverse tcp:1935 tcp:1935
   ```

4. **Virtual I/O + 指定ポートでアプリを起動**
   ```bash
   adb -s <DEVICE_SERIAL> shell am start -W \
     -n com.ikakim.live/.IkakimHostActivity \
     --ez virtual_io true \
     --ei http_port 18095
   ```
   実機カメラ入力を試したい場合は `--ez virtual_io false`（または省略）で起動し、アプリ上でカメラ権限を許可してください。

5. **Mac からアクセスして確認**
   - ブラウザで `http://127.0.0.1:18095/` を開く（コントロール UI・ライブ MSE プレビュー・音声メーター・RTMP 配信操作が可能）
   - API 疎通確認例: `curl -s http://127.0.0.1:18095/api/status`
   - Chrome で `chrome://inspect/#devices` を開くと、端末内の `WebView` (`WebView.setWebContentsDebuggingEnabled(true)`) をリモートデバッグ可能

6. **後片付け（フォワード解除）**
   ```bash
   adb -s <DEVICE_SERIAL> forward --remove tcp:18095
   adb -s <DEVICE_SERIAL> reverse --remove tcp:1935
   ```
