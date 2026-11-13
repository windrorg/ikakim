use std::sync::atomic::{AtomicU32, Ordering};

/// デジタル無音時のフロア値（dBFS）
pub const DBFS_FLOOR: f32 = -96.0;

/// リニア振幅（>= 0.0）を dBFS に変換する
#[inline]
pub fn amplitude_to_dbfs(amp: f32) -> f32 {
    if amp <= 1e-5 {
        DBFS_FLOOR
    } else {
        (20.0 * amp.log10()).max(DBFS_FLOOR)
    }
}

/// dB 値をリニアゲインに変換する（-96dB 以下は完全ミュート 0.0）
#[inline]
pub fn db_to_linear(db: f32) -> f32 {
    if db <= DBFS_FLOOR {
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
    pub fn new() -> Self {
        Self {
            left_bits: AtomicU32::new(0.0f32.to_bits()),
            right_bits: AtomicU32::new(0.0f32.to_bits()),
        }
    }

    /// オーディオスレッドから呼ばれるピーク更新（既存の未読ピークとの最大値を保持）
    #[inline]
    pub fn observe(&self, peak_l: f32, peak_r: f32) {
        Self::update_max(&self.left_bits, peak_l.abs());
        Self::update_max(&self.right_bits, peak_r.abs());
    }

    #[inline]
    fn update_max(slot: &AtomicU32, val: f32) {
        let mut cur_bits = slot.load(Ordering::Relaxed);
        loop {
            let cur_val = f32::from_bits(cur_bits);
            if val <= cur_val {
                break;
            }
            match slot.compare_exchange_weak(
                cur_bits,
                val.to_bits(),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => cur_bits = actual,
            }
        }
    }

    /// 蓄積されたピーク振幅（dBFS）を取得し、内部ピークを 0.0 にリセットする（UI 30Hz 送信向け）
    #[inline]
    pub fn take_dbfs(&self) -> (f32, f32) {
        let l = f32::from_bits(self.left_bits.swap(0.0f32.to_bits(), Ordering::AcqRel));
        let r = f32::from_bits(self.right_bits.swap(0.0f32.to_bits(), Ordering::AcqRel));
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
