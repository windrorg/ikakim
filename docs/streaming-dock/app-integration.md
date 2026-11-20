# ikakimとの統合

[仕様と進捗の概要](README.md)

基準：構想仕様 v0.6、2026年11月20日。C＝確定要求、P＝設計提案、O＝未決定、V＝実機検証待ち。確定要求は実装済みを意味しません。

## 12  ikakimへの接続と実装の差分

前版で確認したwindrorg/ikakimのmain、コミット37823d312d07ca8b3e39b34468b77ce80e1aa81c（2026年11月14日）を基準に整理する。以下はソースの構造と統合状況であり、ビルドや実機性能を検証した結果ではない。[R01–R05](references.md#r01)

| 領域 | 確認した状態 | 今回の追加または変更 |
| --- | --- | --- |
| 共通構成 | Rustコア、Swift／WKWebView、Kotlin／WebView。 | Dock接続管理とOSごとの入力を追加。画面と共通エンジンを共用。 |
| 映像と配信 | 映像レイヤー、Main／Preview、RTMP系配信の基盤。 | HDMI 1080p60の実入力、復号、合成、配信までを通して測定。 |
| iOS入力 | 内蔵カメラ選択、外部CVPixelBufferの入力インターフェース。 | iPad UVCの探索・選択、IP受信と復号、切断復帰。 |
| 音声バス | FFIはMic / DeviceとExternalの2ステレオバス。 | PROGRAM／LOCAL_MEDIAの受信とBGM／ゲスト送信を分離。ゲスト別N−1を追加。 |
| Android入力 | 確認したActivityはVirtual I/O中心。AHB入力インターフェースでmetadataを保存。 | 実カメラ／Dock映像を合成へ、実音声を配信へ接続。 |
| Android符号化 | 非Apple FFIはソフトH.264とsilent AAC代替実装。 | MediaCodec等の実機符号化経路と、入力音声を使うAACを統合。 |
| プレビュー | fMP4／MSE経由の表示基盤。 | 表示遅延を測定。必要に応じネイティブ描画を比較。 |
| 時刻と制御 | iOSでカメラ時刻を破棄する箇所があり、現FFI入力も時刻を受け取らない。 | 取得PTSと共通クロックへの対応、音声ドリフト追従、MIDI状態同期を追加。 |

### 音声ミキサー画面の役割を変える

Mic 1／2、LINE、HDMIのフェーダー操作は、DockへMIDIコマンドを送る。アプリはDockのPROGRAMとLOCAL_MEDIAを別々に受け取る。既存アプリ内ミキサーをそのままMic 1／2の個別DSPとして扱わず、Dockの適用値とメーターを表示する。

アプリ素材の音声はDockへ送信し、BGM／SE用の出力と配信出力を別バスにする。端末の通常再生音を無条件にDockへ送信したり、Dockから受信するProgramを再度Dockへ送信したりしない。動画素材・ゲスト映像をDock音声往復分に合わせて同期する。WebRTCの映像入力・送信、招待、Cloudflare接続管理は追加開発。Linux audio serviceと4音声バスの通信仕様を共通化する。

### 端末別の動作保証範囲を定義する

最初は各OSの代表機種でIP経路と4つの音声バスを確認し、対応iPad／AndroidでUVC経路を追加する。機種・OS・USB速度・映像経路・音声経路・充電条件を組にした保証表を作る。USB-C端子だけを条件に一律の性能保証はしない。

既存Linux層の対象はRK3588／S905X2が中心で、A733／RK3566のCSI→符号化→Gadgetがそのまま動く根拠にはしない。[R01](references.md#r01)

