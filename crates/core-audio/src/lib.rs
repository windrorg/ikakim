pub mod delay;
pub mod meter;
pub mod mixer;
pub mod resampler;

pub use meter::{amplitude_to_dbfs, db_to_linear, linear_to_db, AtomicPeakStereo, DBFS_FLOOR};
pub use mixer::{
    AudioInputConfig, AudioInputProducer, AudioMixer, ChannelHandle, ChannelId, MasterHandle,
    MixerSnapshot, ChannelSnapshot,
};

/// システム共通のオーディオチャンネル数（ステレオ 2ch）
pub const AUDIO_CHANNELS: usize = 2;

/// 1ミキシングブロックあたりの標準フレーム数（48kHz * 10ms = 480 frames）
pub const MIX_BLOCK_FRAMES: usize = 480;

/// チャンネル別ディレイラインの最大遅延（ミリ秒）
pub const MAX_DELAY_MS: u32 = 1000;
