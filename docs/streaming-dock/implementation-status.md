# 実装状況と検証の記録

[仕様と進捗の概要](README.md)

最終参照日：2026年11月20日。仕様基準：[構想仕様 v0.6](requirements.md)。  
コード基準：[コミット `37823d312d07ca8b3e39b34468b77ce80e1aa81c`](https://github.com/windrorg/ikakim/commit/37823d312d07ca8b3e39b34468b77ce80e1aa81c)（2026年11月14日）。

**ikakim には、音声ミキサー、GPU 合成、RTMPS、Web UI の基盤がある。Streaming Dock とモバイル端末を組み合わせた製品としての実装・実証はこれから進める。** コード上の基盤は [core-audio](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-audio/src/mixer.rs#L412)、[core-gpu](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-gpu/src/compositor.rs#L751)、[core-rtmp](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-rtmp/src/publisher.rs)、[core-net-ui](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-net-ui/src/server.rs) に確認できる。

## 実装と検証を分けて読む

| 表記 | 意味 |
| --- | --- |
| 実装済み | 指定コミットに、その機能を担うコードがある。対応端末や製品性能の保証とは別。 |
| 一部実装 | 部品や試験用経路はあるが、本番の入力・出力・処理の接続が不足する、または経路が限定される。 |
| 過去の試験記録 | 既存 Issue のコメント等に記録された結果。記録されたコミット・環境に対する証拠。 |
| 未実証・未確認 | 必要な実機、接続、負荷条件を満たす記録がまだ揃っていない。 |

仕様書の C / P / O / V は要求と設計判断の区分。C は実装完了や試験合格を表さない。テストコードがあること、仮想入力や mock で動くこと、実機の映像・音声が連続配信できることも区別する。

## 既存 Issue #1〜#8 の位置づけ

2026年11月20日時点で、以下はすべて **closed / completed**。既存の開発履歴を維持し、新しい Dock 要件と追加の実機確認は新規 Issue で追跡する。

| 既存 Issue | コードの到達点と根拠 | Dock に向けて追加すること |
| --- | --- | --- |
| [#1 開発環境・コア](https://github.com/windrorg/ikakim/issues/1) | 実装済み。[SPSC](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-types/src/ring_buffer.rs#L8)、[48 kHz クロック](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-types/src/clock.rs)、[状態機械](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-types/src/state.rs)・[トレイト](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-types/src/traits.rs)、[ワークスペース](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/Cargo.toml) | A7S の BSP と再現環境、外部クロックとの対応 |
| [#2 音声ミキサー](https://github.com/windrorg/ikakim/issues/2) | 実装済み。[ステレオ mix / gain / mute / delay](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-audio/src/mixer.rs#L412)、[ALSA capture](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/linux-common/src/alsa.rs#L143)、[Apple AAC](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/apple-common/src/audio_toolbox.rs#L213) | 4 論理バス、独立モニター、指定 FX、クロック調整 |
| [#3 RTMPS・ABR](https://github.com/windrorg/ikakim/issues/3) | 実装済み。[TLS](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-rtmp/src/transport.rs)、[再接続](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-rtmp/src/publisher.rs)、[ABR](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-rtmp/src/abr.rs) | Dock・ゲスト通話・配信の同時負荷、端末回線変更 |
| [#4 制御 UI・プレビュー](https://github.com/windrorg/ikakim/issues/4) | 実装済み。[HTTP / WS](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-net-ui/src/server.rs)、[JSON-RPC](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-net-ui/src/rpc.rs#L250)、[MSE](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/web-ui/app.js#L390) | Dock 制御と状態同期、実測遅延、端末別の表示 |
| [#5 GPU 合成](https://github.com/windrorg/ikakim/issues/5) | 実装済み。[NV12 / YUYV / alpha 合成・変形・main / preview](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-gpu/src/compositor.rs) | HDMI・ゲスト映像・返送映像・配信の同時処理 |
| [#6 Apple / Linux 統合](https://github.com/windrorg/ikakim/issues/6) | 一部実装・環境依存。[Apple 直接経路](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/ffi-capi/src/lib.rs#L485)、[Linux DMA-BUF / mmap 経路](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/linux-common/src/dmabuf_gpu.rs#L474) | 形式ごとのコピー有無、実入力から配信までの性能確認 |
| [#7 RK3588 / S905X2](https://github.com/windrorg/ikakim/issues/7) | 一部実装、対象実機は未実証。[MPP binding](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/linux-common/src/rockchip_mpp.rs#L166)、[V4L2 M2M](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/linux-common/src/v4l2_m2m.rs) | A7S / A733・C790 は別の実装・実証として扱う |
| [#8 iOS / Android](https://github.com/windrorg/ikakim/issues/8) | 一部実装。[iOS 入力接続](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/ios/IkakimWebViewApp.swift#L245)、[Android Activity](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/android/app/src/main/java/com/ikakim/live/IkakimHostActivity.kt#L28)、[共通 FFI encoder 選択](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/ffi-capi/src/lib.rs#L126) | Android の実 I/O・codec 接続、Dock 入力、元 PTS、両 OS 実機統合 |

## 過去の試験記録

以下は Issue に残る報告と、基準コミットに存在する試験コードを分けて記したもの。

| 対象 | 過去の記録 | 基準コミットの試験コード |
| --- | --- | --- |
| #1 | [e36a519 で workspace テスト通過](https://github.com/windrorg/ikakim/issues/1#issuecomment-6847966024) | [core_types_test.rs](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-types/tests/core_types_test.rs) |
| #2 | [Lima arm64・snd-aloop・ALSA・FDK の検証](https://github.com/windrorg/ikakim/issues/2#issuecomment-6848175150) | [Linux 音声試験](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/linux-common/tests/linux_audio_test.rs) |
| #3 | [macOS / Lima でのテスト報告](https://github.com/windrorg/ikakim/issues/3#issuecomment-6848290145) | [ローカル TLS の RTMPS E2E](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-rtmp/tests/core_rtmp_test.rs#L260)、[Apple / SRS の帯域制限試験](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/apple-common/tests/apple_srs_e2e_test.rs#L1184) |
| #4 | [HTTP / WS・fMP4・UI のテスト報告](https://github.com/windrorg/ikakim/issues/4#issuecomment-6848937479) | [サーバー・RPC・メーター・fMP4 試験](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-net-ui/tests/core_net_ui_test.rs#L203) |
| #5 | [Apple Metal と Lima llvmpipe / Vulkan での報告](https://github.com/windrorg/ikakim/issues/5#issuecomment-6849430139) | [GPU 色変換・合成・外部 target 試験](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-gpu/tests/core_gpu_test.rs) |
| #7 | [VM・vicodec・vivid・mock・soak の報告。RK3588 / S905X2 実機未接続と明記](https://github.com/windrorg/ikakim/issues/7#issuecomment-6850394653) | [MockMppSys を含む M3 試験](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/linux-common/tests/linux_m3_hw_test.rs#L126) |

#6 と #8 には、参照日時点で試験結果を記した Issue コメントがない。実装済みの部品やテストコードの存在を、各 Issue の全実機条件を満たした記録としては扱わない。

Apple / SRS の試験には [1080p60 の試験コード](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/apple-common/tests/apple_srs_e2e_test.rs#L1775) がある。ただし [必要なツールがない場合は試験を終了する条件](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/apple-common/tests/apple_srs_e2e_test.rs#L64) があるため、通過の表示だけで実行範囲を判断しない。実行環境、選ばれた入力と codec、スキップの有無を記録する。

## 製品計画で注意する実装の境界

### 音声：ステレオ mix と Dock の 4 バスは別

既存 [AudioMixer](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-audio/src/mixer.rs#L412) は複数入力をステレオ master へミックスする基盤。Program と Local/Media の送出、BGM/SE と Guest の独立 return、直接モニター、物理マイク2本＋ステレオLINE の独立取得を完成させたものではない。

[既存 resampler](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/core-audio/src/resampler.rs#L29) は設定レート間の変換であり、独立した HDMI・ADC・USB・WebRTC のクロック偏差に追従する制御ループは追加対象。既存 soft clip も、製品が求める limiter の仕様適合とは分けて評価する。新しいバス構造と FX の範囲は [音声設計](audio.md) に定める。

### Linux：ゼロコピーの範囲と A7S 対応を限定して記す

[Linux import](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/linux-common/src/dmabuf_gpu.rs#L474) は直接 Vulkan import を RGBA8 と対応 extension に限定し、他の経路には mmap と GPU upload がある。[Linux VM の出力](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/linux-vm/src/main.rs#L131) には GPU readback もある。

[arm64 harness](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/linux-arm64/src/main.rs#L138) は capture を最大 640×360 で開き、[encode 時の DMA-BUF 引数](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/linux-arm64/src/main.rs#L186) に `None` を渡している。[MPP backend](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/linux-common/src/codec_factory.rs#L110) はその入力ではエラーを返し、他の backend は readback を使う。したがって、この harness を「1080p60 の完全ゼロコピー実機配信が完成した」根拠にはしない。

A7S / A733 の CSI・符号化・USB Gadget は、新しい [試作計画](prototype.md) と [検証条件](validation.md) で成立を確認する。

### iOS：入力インターフェースはあるが、Dock 受信と同期は追加対象

[iOS の AVFoundation 入力](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/ios/AVCaptureCameraBridge.swift#L49) は内蔵カメラと既定音声入力を構成する。[外部 CVPixelBuffer の入力インターフェース](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/ios/AVCaptureCameraBridge.swift#L43) はあるが、USB / IP デバイスの探索、受信、復号、切断復帰を実装したことにはならない。

[映像 callback](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/ios/IkakimWebViewApp.swift#L245) では CMTime を引き継いでいない。Dock や WebRTC の元 PTS と共通クロックの対応、追加された Dock 音声往復の補償は [アプリ統合](app-integration.md) の課題とする。

### Android：実カメラと実音声の配信経路は未完成

| 項目 | 基準コミットの状態 | 必要な接続 |
| --- | --- | --- |
| 実カメラ | [Activity](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/android/app/src/main/java/com/ikakim/live/IkakimHostActivity.kt#L88) は仮想 RGBA / PCM の入力処理があるが、実 Camera2 capture を開始しない。[Camera2 wrapper](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/android/app/src/main/java/com/ikakim/live/Camera2AhbCapture.kt) は別に存在 | 起動、権限、実フレームの取得、停止・再開を engine へ接続 |
| AHardwareBuffer | [FFI](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/ffi-capi/src/lib.rs#L761) は descriptor の登録。[importer](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/android-native/src/ahb_vulkan.rs#L339) は native texture 未指定時に placeholder を作る | バッファの寿命・同期を含む実画素の GPU import |
| MediaCodec / AAudio | [MediaCodec](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/android-native/src/mediacodec.rs#L194) は NDK symbol probe と抽象化、[AAudio](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/android-native/src/aaudio.rs#L32) は trait / mock / wrapper。[M4 試験](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/android-native/tests/android_m4_bridge_test.rs) も mock を使用 | 実 backend を実装し、Android の稼働経路へ接続 |
| 配信用音声 | [共通 FFI](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/ffi-capi/src/lib.rs#L126) は非 Apple で fallback audio を選ぶ。[その出力](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/ffi-capi/src/lib.rs#L108) は固定の無音 AAC placeholder | 入力 PCM を実際に AAC へ符号化し、音が配信されることを確認 |

**現状の Android UI 起動や仮想入力の結果を、マイク・Dock 音声を使う配信製品の完成と表現しない。** 実映像・実音声の接続後、各 OS の実機で外部入力、ゲスト通話、RTMPS、再接続、連続負荷を検証する。

## 新しい Dock 開発の完了根拠

[試作](prototype.md) → [アプリ統合](app-integration.md) → [全負荷の検証](validation.md) を、新規 Issue の完了条件に結び付ける。完了時は少なくとも次を残す。

- 実装 PR と commit。
- 基板版、OS / kernel / BSP、端末と OS、音声・映像形式、USB 接続条件。
- 使用した信号源と codec、実行した試験、スキップと制約。
- fps / 欠落、遅延、A/V ずれ、xrun / FIFO、温度・電力など、その課題に必要な測定ログ。
- 達成した要求と残る制限。仕様変更があれば [仕様書](README.md) も更新する。

初回 VRC-01 の stereo / 2 マイク分離実証、物理 4 入力の回路実証、端末との論理 4 in / 4 out は別の結果として記録する。ゲスト 1 名は初期 PoC、720p30 は仮の評価条件であり、製品最大人数と保証画質を決めたものではない。


