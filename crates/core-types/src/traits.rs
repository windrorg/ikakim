use crate::clock::PtsUs;
use crate::media::{EncodedPacket, VideoFrame};
use crate::CoreResult;

/// OS/SoC 固有のネイティブフレームハンドルを GPU テクスチャへゼロコピー取り込みする抽象トレイト
pub trait ExternalFrameImporter: Send {
    /// 実装側（core-gpu）で定義される GPU テクスチャラッパー型
    type GpuTexture;

    /// `VideoFrame` 内の `NativeBufferHandle` を GPU テクスチャとしてバインドする
    fn import_frame(&mut self, frame: &VideoFrame) -> CoreResult<Self::GpuTexture>;
}

/// ハードウェア / ソフトウェア映像デコーダ抽象トレイト（MJPEG入力・オーバーレイ動画用）
pub trait VideoDecoder: Send {
    fn push_packet(&mut self, packet: &EncodedPacket) -> CoreResult<()>;
    fn pull_frame(&mut self) -> CoreResult<Option<VideoFrame>>;
}

/// ハードウェア / ソフトウェア映像エンコーダ抽象トレイト（本配信・プレビュー共通）
pub trait VideoEncoder: Send {
    /// 動的ビットレート制御（ABR）からのリアルタイムビットレート変更要求
    fn set_target_bitrate(&mut self, bitrate_bps: u32) -> CoreResult<()>;
    /// 次のフレームを即座に IDR（キーフレーム）としてエンコードするよう要求する
    fn request_keyframe(&mut self);
    /// エンコード完了パケットを取り出す
    fn pull_packet(&mut self) -> CoreResult<Option<EncodedPacket>>;
}

/// AAC-LC 音声エンコーダ抽象トレイト
pub trait AudioEncoder: Send {
    /// 48kHz / 2ch インターリーブ f32 PCM フレームを投入する
    fn push_pcm_f32(&mut self, interleaved_pcm: &[f32], pts: PtsUs) -> CoreResult<()>;
    /// エンコード済み AAC パケットを取り出す
    fn pull_packet(&mut self) -> CoreResult<Option<EncodedPacket>>;
}

/// 合成済み映像フレームの出力先抽象トレイト
/// - 本配信エンコーダ入力ターゲット
/// - プレビューエンコーダ入力ターゲット
/// - 将来の Linux 実機 HDMI / LCD ステータス表示（DRM/KMS + GBM スワップチェーン）
pub trait RenderSink: Send {
    type RenderTarget;

    fn target_size(&self) -> (u32, u32);
    fn acquire_target(&mut self) -> CoreResult<Self::RenderTarget>;
    fn present_target(&mut self, target: Self::RenderTarget, pts: PtsUs) -> CoreResult<()>;
}
