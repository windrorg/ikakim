pub mod clock;
pub mod media;
pub mod ring_buffer;
pub mod state;
pub mod traits;

use std::fmt;

/// コア全体で共有する汎用エラー型
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreError {
    /// リングバッファの空き容量不足（ドロップ発生）
    BufferFull,
    /// リングバッファが空
    BufferEmpty,
    /// 無効な状態遷移
    InvalidStateTransition {
        from: state::PipelineState,
        to: state::PipelineState,
    },
    /// サポートされていないフォーマットやパラメータ
    UnsupportedFormat(String),
    /// プラットフォーム / ハードウェア I/O エラー
    PlatformIo(String),
    /// コーデック初期化・エンコード・デコードエラー
    Codec(String),
    /// ネットワーク / プロトコルエラー
    Network(String),
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BufferFull => write!(f, "ring buffer is full"),
            Self::BufferEmpty => write!(f, "ring buffer is empty"),
            Self::InvalidStateTransition { from, to } => {
                write!(f, "invalid pipeline state transition: {:?} -> {:?}", from, to)
            }
            Self::UnsupportedFormat(msg) => write!(f, "unsupported format: {}", msg),
            Self::PlatformIo(msg) => write!(f, "platform I/O error: {}", msg),
            Self::Codec(msg) => write!(f, "codec error: {}", msg),
            Self::Network(msg) => write!(f, "network error: {}", msg),
        }
    }
}

impl std::error::Error for CoreError {}

pub type CoreResult<T> = Result<T, CoreError>;
