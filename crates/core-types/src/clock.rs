use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// システム共通のマスターサンプリングレート（48kHz）
pub const MASTER_SAMPLE_RATE: u64 = 48_000;

/// 48kHz 基準の累積サンプル時間
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct SampleTime(pub u64);

/// プレゼンテーションタイムスタンプ（マイクロ秒）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct PtsUs(pub u64);

/// RTMP / FLV タイムスタンプ（ミリ秒）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct TimestampMs(pub u32);

impl SampleTime {
    #[inline]
    pub const fn from_samples(samples: u64) -> Self {
        Self(samples)
    }

    #[inline]
    pub const fn as_samples(self) -> u64 {
        self.0
    }

    /// マイクロ秒へ変換（u128 中間演算によりオーバーフローと誤差蓄積を防止）
    #[inline]
    pub const fn to_pts_us(self) -> PtsUs {
        let us = (self.0 as u128 * 1_000_000) / (MASTER_SAMPLE_RATE as u128);
        PtsUs(us as u64)
    }

    /// RTMP / FLV パケット用のミリ秒タイムスタンプへ変換
    #[inline]
    pub const fn to_timestamp_ms(self) -> TimestampMs {
        let ms = (self.0 as u128 * 1_000) / (MASTER_SAMPLE_RATE as u128);
        TimestampMs(ms as u32)
    }
}

impl PtsUs {
    #[inline]
    pub const fn to_timestamp_ms(self) -> TimestampMs {
        TimestampMs((self.0 / 1_000) as u32)
    }
}

/// 音声クロック（48kHz）を基準とするスレッドセーフなマスタークロック
#[derive(Clone)]
pub struct MasterClock {
    inner: Arc<MasterClockInner>,
}

struct MasterClockInner {
    total_samples: AtomicU64,
    epoch: Instant,
}

impl MasterClock {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(MasterClockInner {
                total_samples: AtomicU64::new(0),
                epoch: Instant::now(),
            }),
        }
    }

    /// 音声ミキサーが 1 ブロック（例: 480 samples = 10ms）処理するごとに時間を進める
    #[inline]
    pub fn advance_samples(&self, frames: u64) -> SampleTime {
        let prev = self
            .inner
            .total_samples
            .fetch_add(frames, Ordering::AcqRel);
        SampleTime(prev + frames)
    }

    /// 現在のマスターサンプル時間を取得する
    #[inline]
    pub fn now_samples(&self) -> SampleTime {
        SampleTime(self.inner.total_samples.load(Ordering::Acquire))
    }

    /// 映像フレーム到着時などに現在の A/V 同期用 PTS（マイクロ秒）を取得する
    #[inline]
    pub fn now_pts_us(&self) -> PtsUs {
        self.now_samples().to_pts_us()
    }

    /// システム起動（クロック生成）からのウォールクロック経過時間（マイクロ秒）
    #[inline]
    pub fn wall_elapsed_us(&self) -> u64 {
        self.inner.epoch.elapsed().as_micros() as u64
    }

    /// 音声クロックとウォールクロックのドリフト量（符号付きマイクロ秒）を算出する
    /// 正の値: 音声クロックがウォールクロックより進んでいる
    #[inline]
    pub fn clock_drift_us(&self) -> i64 {
        let audio_us = self.now_pts_us().0 as i128;
        let wall_us = self.wall_elapsed_us() as i128;
        (audio_us - wall_us) as i64
    }
}

impl Default for MasterClock {
    fn default() -> Self {
        Self::new()
    }
}
