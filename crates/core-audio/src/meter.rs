use std::sync::atomic::{AtomicU32, Ordering};

/// デジタル無音時のフロア値（dBFS）
pub const DBFS_FLOOR: f32 = -96.0;

/// 無音判定の最小リニア振幅閾値（`-100 dBFS` = `1e-5`）
const SILENCE_AMP_THRESHOLD: f32 = 1e-5;

/// リニア振幅（>= 0.0）を dBFS に変換する
#[inline]
pub fn amplitude_to_dbfs(amp: f32) -> f32 {
    if !amp.is_finite() || amp <= SILENCE_AMP_THRESHOLD {
        DBFS_FLOOR
    } else {
        (20.0 * amp.log10()).max(DBFS_FLOOR)
    }
}

/// リニアゲイン倍率を dB 値に変換する（`amplitude_to_dbfs` と同等の対数変換）
#[inline]
pub fn linear_to_db(gain: f32) -> f32 {
    amplitude_to_dbfs(gain)
}

/// dB 値をリニアゲインに変換する（-96dB 以下は完全ミュート 0.0）
#[inline]
pub fn db_to_linear(db: f32) -> f32 {
    if !db.is_finite() || db <= DBFS_FLOOR {
        0.0
    } else {
        10.0f32.powf(db / 20.0)
    }
}

/// オーディオスレッドからロックフリーでピーク振幅を蓄積し、
/// UI 通知スレッド（30Hz）から安全に読み出し・リセットできるステレオピークメーター
pub struct AtomicPeakStereo {
    left_bits: AtomicU32,
    right_bits: AtomicU32,
}

impl AtomicPeakStereo {
    pub const fn new() -> Self {
        Self {
            left_bits: AtomicU32::new(0),
            right_bits: AtomicU32::new(0),
        }
    }

    /// オーディオスレッドから呼ばれるピーク更新（既存の未読ピークとの最大値を保持）
    #[inline]
    pub fn observe(&self, peak_l: f32, peak_r: f32) {
        Self::update_max(&self.left_bits, peak_l.abs());
        Self::update_max(&self.right_bits, peak_r.abs());
    }

    /// 非負の有限 IEEE-754 `f32` はビット表現 `u32` の大小関係が浮動小数点の大小関係と完全に一致する性質を利用し、
    /// CAS リトライループなしの単一アトミック `fetch_max`（ARMv8.1 `ldumax` 等）で最大値を更新する。
    #[inline]
    fn update_max(slot: &AtomicU32, val: f32) {
        let val_bits = if val.is_finite() && val > 0.0 {
            val.to_bits()
        } else {
            0
        };
        if val_bits > slot.load(Ordering::Relaxed) {
            slot.fetch_max(val_bits, Ordering::Release);
        }
    }

    /// 蓄積されたピーク振幅（dBFS）を取得し、内部ピークを 0.0 にリセットする（UI 30Hz 送信向け）
    #[inline]
    pub fn take_dbfs(&self) -> (f32, f32) {
        let l = f32::from_bits(self.left_bits.swap(0, Ordering::AcqRel));
        let r = f32::from_bits(self.right_bits.swap(0, Ordering::AcqRel));
        (amplitude_to_dbfs(l), amplitude_to_dbfs(r))
    }

    /// 内部ピークをリセットせずに現在のピーク（dBFS）を参照する
    #[inline]
    pub fn peek_dbfs(&self) -> (f32, f32) {
        let l = f32::from_bits(self.left_bits.load(Ordering::Acquire));
        let r = f32::from_bits(self.right_bits.load(Ordering::Acquire));
        (amplitude_to_dbfs(l), amplitude_to_dbfs(r))
    }
}

impl Default for AtomicPeakStereo {
    fn default() -> Self {
        Self::new()
    }
}
