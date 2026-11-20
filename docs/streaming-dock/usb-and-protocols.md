# USB・IP・MIDIの接続仕様

[仕様と進捗の概要](README.md)

基準：構想仕様 v0.6、2026年11月20日。C＝確定要求、P＝設計提案、O＝未決定、V＝実機検証待ち。確定要求は実装済みを意味しません。

## 4  USB接続と端末ごとの動作

USBデータのHostは端末、DeviceはDock。同一configurationに複数の機能を公開し、アプリが利用経路を選ぶ案を維持する。同時公開と同時ストリーム転送は別であり、未使用の転送とエンコーダは止める。

| 対象 | 映像と音声の初期方針 | 確認事項 |
| --- | --- | --- |
| iPhone | USB EthernetでH.264と4つの論理音声バスを転送する案。 | 標準UVC入力を前提にできない。NCM、音声リターン、MIDI、Wi-Fi／携帯回線併用を確認。[S01–S03,S20](references.md#s01) |
| USB-C iPad | UVC＋UAC、またはUSB Ethernet。 | iPadOS 17以降のUVC対応あり。対応機種・USB速度・形式・経路ごとの動作保証を決める。[S01–S02](references.md#s01) |
| Android | UVC＋UAC、またはUSB Ethernet。 | Camera2外部カメラは端末実装に依存。独自USB Host入力、NCM、権限を機種別に評価。[S05](references.md#s05) |

### 複合デバイスと必要なエンドポイント数

| 機能 | 主用途 | IN / OUTの概算 |
| --- | --- | --- |
| UVC | HDMI映像 | 1〜2 / 0 |
| UAC2 | 端末視点4ch capture＋4ch playback案 | 2〜3 / 1 |
| USB MIDI 1.0 | 設定と状態通知 | 1 / 1 |
| NCM | IP映像・音声・セッション管理 | 2 / 1 |
| 合計 | 双方向UACと明示feedbackを想定 | 6〜8 / 3 ＋ 共通EP0 |

上表はLinux functionによる設計例であり、通知・同期方式によって変わる。未使用でもbind時に資源を確保するため、UDCのIN／OUT数、isochronous、FIFO、BSPで同時公開できるかを最初に確認する。1つのUAC functionを各方向2chから4chへ増やす案では通常EP数は増えないが、パケット長・FIFO・帯域を再評価する。通常の経路選択では再列挙を避ける。[S06–S07,S21–S24](references.md#s06)

### アプリとのメディア転送仕様

UVCとIPは同じHDMI入力IDに対応づけ、UACとIPでは[第5章](audio.md)の音声バスを共通化する。PROGRAM／LOCAL_MEDIA／MEDIA_RETURN／GUEST_RETURNの4バスを識別し、同じバスを二重に取り込まない。映像・音声の取得時刻、形式変更、不連続、再開位置を共通の入力管理へ渡す。

iPad UVCにはJPEG／H.264の取り込みを復号済み形式へ変換する経路もある。圧縮UVCを候補にできるが、SoCの符号化対応とLinux UVC Gadgetの実装、ホスト互換性は別々に確認する。[S01](references.md#s01)

Apple／Androidには多チャネルを扱うAPIがあるが、全対象端末のUAC 4in／4outを保証しない。4ch同時入出力を実測し、IPで同等のバスを運べるようにする。[S35–S37](references.md#s35) IP映像は端末側の復号・合成・配信用再符号化を必要とする。

## 10  IP通信とMIDI制御

### 専用リンクとインターネット回線を併用する

USB EthernetはNCMを第一候補とする。Apple DTSにはiPhoneとNCM機器の直接接続に関する回答があるが、自作複合機器と対象Androidの認識を保証するものではない。全USB機能を公開した状態で対象端末ごとに検証する。[S03,S20](references.md#s03)

DockのDHCPは、インターネットを中継しない場合にデフォルトゲートウェイとDNSを配布しない案とする。IPv6のデフォルトルート広告も行わず、配信は端末のWi-Fi／携帯回線を使う。専用リンクのアドレス衝突を避け、サービス探索と機器IDで接続先を識別する。

Dock用ソケットのみをUSBネットワークへ向ける。Androidのプロセス全体を専用リンクに固定せず、入力受信と外部配信を同時に成立させる。iOSのローカルネットワーク権限、Android 17・target SDK 37以降のACCESS_LOCAL_NETWORK等も扱う。[S17–S18](references.md#s17)

### 映像と音声をタイムスタンプ付きで転送する

初期候補はH.264映像と4つのPCM音声バスをRTP／RTCP系で転送する方式。MPEG-TS等と遅延・実装量を比較して確定する。各方向4ch相当の48kHz・packed 24bit PCMは片方向4.608Mbps、双方向9.216Mbps。32bitの格納形式なら双方向12.288Mbps。通信オーバーヘッドと映像は別に加算する。

4バスを各方向2本のステレオ、または識別可能な4ch形式で転送する。バスID、PTS、クロックID、サンプル数、不連続を定義する。RTPではメディア別のクロックをRTCP等で共通時刻へ対応づける。[S34](references.md#s34) USBリターンは対応するfeedback等、IPリターンはバッファ監視と再サンプリング等でクロック差に追従する。再接続時に古い映像・音声を出力しない。H.264のGOP、Bフレーム、バッファ上限、キーフレーム要求を調整項目とする。

### MIDIで操作と状態を同期する

USB MIDI 1.0 SysExを初期案とする。AppleのUAC標準ドライバはMixer UnitやEffect Unit等を操作しないため、Dock独自のミックス／FXをMIDIで操作する方針は整合する。[S04](references.md#s04)

| 分類 | 定義する内容 |
| --- | --- |
| 対応機能と接続 | 機器ID、プロトコル版、FW版、音声入力・バスID・採用FXと同時使用数、UACチャネルとIPストリームの対応、映像形式と転送経路。 |
| 音声操作 | 入力ゲイン、48V、フェーダー、pan、mute、FX、配信／返送の遅延、monitor音量。単位と有効範囲を定義。 |
| 応答と整合性 | 要求ID、ACK／エラー、実際の適用値。物理操作とアプリ操作の結果を双方へ反映。 |
| 表示と状態 | 音量メーター、クリップ、入力形式、給電・温度、配信状態とheartbeat。 |
| 復帰と保存 | 接続時の全状態取得、再接続、プリセット保存、破損／旧版設定の扱い。 |

OLEDは文字・メーター・状態をDockで描画する。メーター通知には頻度上限を設け、制御応答を優先する。ON AIRはUSB接続状態ではなくアプリの配信状態から表示し、heartbeatが途切れたら状態不明に変更する。

