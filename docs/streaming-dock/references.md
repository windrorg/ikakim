# 技術資料と改訂記録

[仕様と進捗の概要](README.md)

基準：構想仕様 v0.6、2026年11月20日。C＝確定要求、P＝設計提案、O＝未決定、V＝実機検証待ち。確定要求は実装済みを意味しません。

## 19  技術資料 OSとUSBと候補ボード

参照日 2026年11月20日。メーカー公表値、OSの説明、ソース上の実装は実機保証と区別する。提案と計算は本文の条件に基づく。各資料名から原文を開ける。

<a id="s01"></a>

S01  [Apple WWDC23  Support external cameras in your iPadOS app](https://developer.apple.com/videos/play/wwdc2023/10106/)

<a id="s02"></a>

S02  [Apple Developer Forums  iPhoneでのUVC対応に関する回答](https://developer.apple.com/forums/thread/776912)

<a id="s03"></a>

S03  [Apple Developer Forums  iPhoneのUSB Ethernetと公開API](https://developer.apple.com/forums/thread/802640)

<a id="s04"></a>

S04  [Apple TN3190  USB audio device design considerations](https://developer.apple.com/documentation/technotes/tn3190-usb-audio-device-design-considerations)

<a id="s05"></a>

S05  [Android Open Source Project  External USB cameras](https://source.android.com/docs/core/camera/external-usb-cameras)

<a id="s06"></a>

S06  [Linux Kernel  USB Gadget configured through configfs](https://www.kernel.org/doc/html/latest/usb/gadget_configfs.html)

<a id="s07"></a>

S07  [Linux Kernel  USB Gadget API  機能とエンドポイント割当](https://www.kernel.org/doc/html/latest/driver-api/usb/gadget.html)

<a id="s08"></a>

S08  [Radxa  Cubie A7S仕様](https://docs.radxa.com/en/cubie/a7s)

<a id="s09"></a>

S09  [Radxa  Cubie A7A仕様](https://docs.radxa.com/en/cubie/a7a)

<a id="s10"></a>

S10  [Radxa  ZERO 3Wおよび3E仕様](https://docs.radxa.com/en/zero/zero3)

<a id="s11"></a>

S11  [Radxa  A7S CSIコネクタと4Kカメラ接続](https://docs.radxa.com/en/cubie/a7s/accessories-use/camera-4k)

<a id="s12"></a>

S12  [Rockchip  RK3566公式仕様](https://www.rock-chips.com/a/en/products/RK35_Series/2021/0113/1274.html)

<a id="s13"></a>

S13  [Geekworm  C790仕様](https://wiki.geekworm.com/C790)

<a id="s14"></a>

S14  [Linux Kernel  TC358743 Device Tree binding](https://www.kernel.org/doc/Documentation/devicetree/bindings/media/i2c/toshiba,tc358743.yaml)

<a id="s15"></a>

S15  [Radxa  Allwinner向けVideo Codec利用手順](https://docs.radxa.com/en/cubie/a7s/app-dev/video-codec)

<a id="s16"></a>

S16  [Toshiba  TC358743XBG datasheet Rev 2.20  2026年5月11日](https://toshiba.semicon-storage.com/info/TC358743XBG_datasheet_en_20260511.pdf?did=35655&prodName=TC358743XBG)

<a id="s17"></a>

S17  [Apple TN3179  Understanding local network privacy](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy)

<a id="s18"></a>

S18  [Android Developers  Local network permission](https://developer.android.com/privacy-and-security/local-network-permission)

<a id="s19"></a>

S19  [USB-IF  USB Type-C Cable and Connector Specification R2.0](https://www.usb.org/sites/default/files/USB%20Type-C%20Spec%20R2.0%20-%20August%202019.pdf)

<a id="s20"></a>

S20  [Apple DTS  NCM機器とiPhoneの直接接続  2026年の回答](https://developer.apple.com/forums/thread/772812)

<a id="s21"></a>

S21  [Linux  UVC functionの実装](https://github.com/torvalds/linux/blob/master/drivers/usb/gadget/function/f_uvc.c)

<a id="s22"></a>

S22  [Linux  UAC2 functionの実装](https://github.com/torvalds/linux/blob/master/drivers/usb/gadget/function/f_uac2.c)

<a id="s23"></a>

S23  [Linux  NCM functionの実装](https://github.com/torvalds/linux/blob/master/drivers/usb/gadget/function/f_ncm.c)

<a id="s24"></a>

S24  [Linux  MIDI functionの実装](https://github.com/torvalds/linux/blob/master/drivers/usb/gadget/function/f_midi.c)

## 20  技術資料 競合と回路とアプリ

HOMRA 4Kは開発中。C792は公式ページ内の入力上限表記に差があり、対象基板版と条件を確認する。ikakimは以下のコミットを基準にしたソース確認で、実機動作を保証しない。

<a id="s25"></a>

S25  [Roland AeroCaster 製品ページと映像入力仕様](https://www.roland.com/jp/products/aerocaster/)

<a id="s26"></a>

S26  [Roland VRC-01 主な仕様](https://static.roland.com/manuals/vrc-01/jpn/24281703.html)

<a id="s27"></a>

S27  [Roland AeroCaster LIVE 有線HDMI入力について](https://support.roland.com/hc/en-us/articles/4586917331483-AeroCaster-Can-I-use-this-app-to-livestream-content-from-a-connected-gaming-device)

<a id="s28"></a>

S28  [Shiftall HOMRA 4K 製品ページ 開発中仕様](https://ja.shiftall.net/products/homra4k)

<a id="s29"></a>

S29  [Shiftall HOMRA 4K 発表 2026年11月5日](https://ja.shiftall.net/news/20261105)

<a id="s30"></a>

S30  [Geekworm C792 公式Wiki 入力とループと縮小](https://wiki.geekworm.com/C792)

<a id="s31"></a>

S31  [STMicroelectronics STUSB4500 PD Sink制御の例](https://www.st.com/en/interfaces-and-transceivers/stusb4500.html)

<a id="s32"></a>

S32  [Texas Instruments TIDA-01243 給電対応Dock参照設計](https://www.ti.com/tool/TIDA-01243)

<a id="s33"></a>

S33  [Linux Kernel UAC2 Gadget設定と同期方式](https://docs.kernel.org/usb/gadget-testing.html)

<a id="s34"></a>

S34  [RFC 3550 RTPとRTCPのクロック対応](https://www.rfc-editor.org/rfc/rfc3550.html)

### ikakimの参照ソース

<a id="r01"></a>

R01  [READMEとモジュール構成](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/README.md)

<a id="r02"></a>

R02  [iOSカメラ入力と外部バッファの入力インターフェース](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/ios/AVCaptureCameraBridge.swift)

<a id="r03"></a>

R03  [共通FFIとエンコーダ選択](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/crates/ffi-capi/src/lib.rs)

<a id="r04"></a>

R04  [AndroidホストActivity](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/android/app/src/main/java/com/ikakim/live/IkakimHostActivity.kt)

<a id="r05"></a>

R05  [iOSホストと映像投入コールバック](https://github.com/windrorg/ikakim/blob/37823d312d07ca8b3e39b34468b77ce80e1aa81c/platforms/ios/IkakimWebViewApp.swift)

参照コミット  37823d312d07ca8b3e39b34468b77ce80e1aa81c

### 参照情報の扱い

公式資料の対応機能は設計の候補を絞る根拠として用い、特定SBC・OSイメージ・端末・複合Gadgetの同時動作は[第14章](validation.md)の検証で確認する。量産価格、性能、対応端末、最大ゲスト数はいずれも未保証。

## 21  追加資料と改訂記録

<a id="s35"></a>

S35  [Apple AVAudioSession 入力チャネル数の指定](https://developer.apple.com/documentation/avfaudio/avaudiosession/setpreferredinputnumberofchannels(_:))

<a id="s36"></a>

S36  [Android AudioFormat 多チャネルUSBとchannel index mask](https://developer.android.com/reference/android/media/AudioFormat)

<a id="s37"></a>

S37  [AOSP USB digital audio 公表されている基本対応範囲](https://source.android.com/docs/core/audio/usb)

<a id="s38"></a>

S38  [Apple AVAudioSession 他アプリの通話中のsetActive制約](https://developer.apple.com/documentation/avfaudio/avaudiosession/setactive(_:options:))

<a id="s39"></a>

S39  [Android Sharing audio input 同時録音の優先度](https://developer.android.com/media/platform/sharing-audio-input)

<a id="s40"></a>

S40  [Android 再生音キャプチャのusageと許可条件](https://developer.android.com/media/platform/av-capture)

<a id="s41"></a>

S41  [WebRTC Peer connections シグナリングとICE](https://webrtc.org/getting-started/peer-connections)

<a id="s42"></a>

S42  [WebRTC RTCAudioDevice ネイティブ音声の供給と受領](https://webrtc.googlesource.com/src/+/refs/heads/main/sdk/objc/components/audio/RTCAudioDevice.h)

<a id="s43"></a>

S43  [Analog Devices ADAU1452 前版の比較資料・初期構成には含めない](https://www.analog.com/en/products/adau1452.html)

<a id="s44"></a>

S44  [Analog Devices ADAU1372 4入力／2出力codec](https://www.analog.com/en/products/adau1372.html)

<a id="s45"></a>

S45  [Analog Devices ADAU1467 前版の比較資料・初期構成には含めない](https://www.analog.com/en/products/adau1467.html)

<a id="s46"></a>

S46  [国税庁 No.6303 消費税と地方消費税の税率](https://www.nta.go.jp/taxes/shiraberu/taxanswer/shohi/6303.htm)

### 改訂記録

| 版 | 日付 | 主な変更 |
| --- | --- | --- |
| 0.1 | 2026年11月20日 | 構想、OS／USB、候補ボード、アプリの現状と初回質問を整理。 |
| 0.2 | 2026年11月20日 | Q1〜Q6を反映。1080p60、Dock音声mix、直接monitor、48V、PD給電、短ケーブルを要件化。 |
| 0.3 | 2026年11月20日 | Q7〜Q11を反映。1080p60パススルー希望、6種FX、BGM／ゲスト、独立LINE希望、5万円以下・3,000台。4音声バス、個別返送、回路候補、採算条件とQ12〜Q14を追加。 |
| 0.4 | 2026年11月20日 | Q12〜Q14を反映。Linux音声を基本化、機能縮小方針、3.5mm LINE／HP1人、Cloudflare WebRTC、税込直販・付属品なしの条件を追加。 |
| 0.5 | 2026年11月20日 | Q15を反映。ゲスト映像を初期想定へ追加し、Cubie A7Sのバラック接続案、USB音声IFとCSIの検証条件を具体化。 |
| 0.6 | 2026年11月20日 | Q16を反映。VRC-01を暫定音声IFに採用する案、stereo／2マイク分離の範囲、別電源、内蔵monitor／FX解除と実機確認を追加。 |

本書は構想仕様。ハードウェア製作、アプリへのコード変更、量産見積の取得、実機試験は本版の作成には含まれない。

## 22  Linux音声とCloudflareと試作の資料

参照日2026年11月20日。料金は確認時点の公開条件。ALSA／LinuxのAPIや設定は、採用基板での性能保証を意味しない。JACKの資料は音声処理中の操作原則の参考であり、JACK採用を決めたものではない。

<a id="s47"></a>

S47  [Cloudflare Realtime TURN 役割と接続先](https://developers.cloudflare.com/realtime/turn/)

<a id="s48"></a>

S48  [Cloudflare TURN 期限付き資格情報の生成](https://developers.cloudflare.com/realtime/turn/generate-credentials/)

<a id="s49"></a>

S49  [Cloudflare Realtime 料金と月間共有無料枠](https://developers.cloudflare.com/realtime/sfu/platform/pricing/)

<a id="s50"></a>

S50  [Cloudflare TURN FAQ 課金方向・TTL・再接続](https://developers.cloudflare.com/realtime/turn/faq/)

<a id="s51"></a>

S51  [Cloudflare Durable Objects WebSocketとHibernation](https://developers.cloudflare.com/durable-objects/best-practices/websockets/)

<a id="s52"></a>

S52  [ALSA PCM API バッファ・period・xrun](https://www.alsa-project.org/alsa-doc/alsa-lib/pcm.html)

<a id="s53"></a>

S53  [JACK 処理コールバック内で避ける操作](https://jackaudio.org/api/group__ClientCallbacks.html)

<a id="s54"></a>

S54  [libsamplerate 連続ストリームと変換比変更API](https://libsndfile.github.io/libsamplerate/api_full.html)

<a id="s55"></a>

S55  [Linux Kernel PREEMPT_RTの動作上の差](https://docs.kernel.org/core-api/real-time/differences.html)

<a id="s56"></a>

S56  [Linux man-pages スケジューリング方針](https://man7.org/linux/man-pages/man7/sched.7.html)

<a id="s57"></a>

S57  [Linux man-pages mlockとメモリの事前確保](https://man7.org/linux/man-pages/man2/mlock.2.html)

<a id="s58"></a>

S58  [Radxa Cubie A7S ハードウェア端子とpin表](https://docs.radxa.com/en/cubie/a7s/hardware-use/hardware-info)

<a id="s59"></a>

S59  [Radxa Cubie A7S Quick start 5V電源と起動](https://docs.radxa.com/en/cubie/a7s/getting-started/quickly-started)

<a id="s60"></a>

S60  [Radxa Cubie A7S V1.10 回路図 10 CAMERA・12 TYPE C](https://dl.radxa.com/cubie/a7s/hw/radxa_cubie_a7s_schematic_v1.10.pdf)

<a id="s61"></a>

S61  [Roland VRC-01 AUDIOブロック図 p.2 USB L/Rとリターン経路](https://static.roland.com/products/vrc-01/images/blockdiagram.pdf)

<a id="s62"></a>

S62  [Roland VRC-01 v1.05 USB音声IF・電源・MIC 2の48V](https://static.roland.com/manuals/vrc-01_v105/eng/38964200.html)

<a id="s63"></a>

S63  [Roland VRC-01 v1.05 パネル・FX・monitor・stereo設定](https://static.roland.com/manuals/vrc-01_v105/eng/38964264.html)

<a id="s64"></a>

S64  [Roland VRC-01 v1.05 ノイズゲート・起動時ON](https://static.roland.com/manuals/vrc-01_v105/eng/38964111.html)

<a id="s65"></a>

S65  [Roland VRC-01 MIC 1＝L／MIC 2＝Rのstereo link](https://support.roland.com/hc/en-us/articles/4587779657243-AeroCaster-Stereo-input-to-the-VRC-01-MIC-1-2-connectors)

### 今回の設計判断と資料の関係

4音声バス、Linuxサービス、FX縮小順、Cloudflare構成、ゲスト720p30、VRC-01の暫定利用、価格試算は設計提案。メーカーの完成済み製品仕様や、実機で達成した測定値とは区別する。

S43／S45の専用DSP資料は前版との比較履歴として残した。現行の初期音声構成はLinux上のmix／FXであり、専用音声DSPを含めていない。

