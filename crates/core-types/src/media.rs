use crate::clock::PtsUs;

/// カメラ・デコーダ・コンポジタ間で扱うピクセルフォーマット
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 2プレーン YUV 4:2:0 (Y: R8, UV: RG8) - HWコーデック・V4L2標準
    Nv12,
    /// パックド YUV 4:2:2 - UVC HDMIキャプチャ頻出（シェーダーで直接変換）
    Yuyv,
    /// 圧縮フレーム（HWデコーダへ投入）
    Mjpeg,
    /// 32bit RGBA
    Rgba8,
    /// 32bit BGRA（Apple Metal / 一部DRM環境）
    Bgra8,
}

/// プラットフォーム固有のゼロコピーネイティブメモリハンドル
#[derive(Debug)]
pub enum NativeBufferHandle {
    /// Linux DMA-BUF ファイルディスクリプタ（所有権はフレームドロップ時に解放）
    #[cfg(target_os = "linux")]
    DmaBuf {
        fd: std::os::fd::OwnedFd,
        offset: u32,
        stride: u32,
        modifier: u64,
    },
    /// Apple CVPixelBufferRef / Android AHardwareBuffer 等の不透明ポインタ
    OpaqueHandle {
        ptr: usize,
        stride: u32,
    },
    /// VM テストや CPU フォールバック用のメモリバッファ
    CpuFallback {
        data: Vec<u8>,
        stride: u32,
    },
}

/// 映像フレームメタデータ＋ゼロコピーハンドル
#[derive(Debug)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub pts: PtsUs,
    pub handle: NativeBufferHandle,
}

/// 圧縮ストリーム種別
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecKind {
    H264,
    Hevc,
    AacLc,
}

/// エンコード済みパケット（本配信 RTMP および プレビュー fMP4 共通）
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    pub codec: CodecKind,
    pub pts: PtsUs,
    pub dts: PtsUs,
    pub is_keyframe: bool,
    /// コーデック設定データ（H.264 SPS/PPS や AAC AudioSpecificConfig）か否か
    pub is_sequence_header: bool,
    pub data: Vec<u8>,
}

/// 2D オーバーレイレイヤーの動的トランスフォームパラメータ
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerTransform {
    /// 正規化座標 X (-1.0 .. 1.0 または 0.0 .. 1.0 基準)
    pub x: f32,
    /// 正規化座標 Y
    pub y: f32,
    /// スケール倍率 X
    pub scale_x: f32,
    /// スケール倍率 Y
    pub scale_y: f32,
    /// 回転角（ラジアン）
    pub rotation_rad: f32,
    /// 不透明度 (0.0 .. 1.0)
    pub opacity: f32,
    /// 描画順（大きいほど前面）
    pub z_order: i32,
    /// 表示有効フラグ
    pub visible: bool,
}

impl Default for LayerTransform {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
            rotation_rad: 0.0,
            opacity: 1.0,
            z_order: 0,
            visible: true,
        }
    }
}
