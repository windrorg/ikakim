use crate::MAX_DELAY_MS;
use core_types::clock::MASTER_SAMPLE_RATE;

/// 48kHz / 2ch ステレオ用のリングバッファ型ディレイライン
/// 最大 1000ms (48,000 frames) を 1ms (48 frames) 単位で動的に調整可能。
/// 遅延変更時には 5ms (240 frames) の線形クロスフェードを行いクリックノイズを防止する。
pub struct StereoDelayLine {
    /// インターリーブステレオ [L0, R0, L1, R1, ...]
    buffer: Vec<f32>,
    capacity_frames: usize,
    mask: usize,
    write_pos: usize,
    current_delay_frames: usize,
    target_delay_frames: usize,
    prev_delay_frames: usize,
    crossfade_pos: usize,
    crossfade_len: usize,
}

const FRAMES_PER_MS: usize = (MASTER_SAMPLE_RATE / 1000) as usize; // 48
const CROSSFADE_FRAMES: usize = 240; // 5ms @ 48kHz

impl StereoDelayLine {
    pub fn new(initial_delay_ms: u32) -> Self {
        // 48,000 frames + 余白より大きい最小の2のべき乗 = 65,536 frames
        let capacity_frames = 65_536;
        let mask = capacity_frames - 1;
        let clamped_frames = Self::ms_to_frames(initial_delay_ms);

        Self {
            buffer: vec![0.0f32; capacity_frames * 2],
            capacity_frames,
            mask,
            write_pos: 0,
            current_delay_frames: clamped_frames,
            target_delay_frames: clamped_frames,
            prev_delay_frames: clamped_frames,
            crossfade_pos: CROSSFADE_FRAMES,
            crossfade_len: CROSSFADE_FRAMES,
        }
    }

    #[inline]
    fn ms_to_frames(delay_ms: u32) -> usize {
        (delay_ms.min(MAX_DELAY_MS) as usize) * FRAMES_PER_MS
    }

    /// 遅延量をミリ秒単位（0..=1000ms）で更新する
    pub fn set_delay_ms(&mut self, delay_ms: u32) {
        let next_frames = Self::ms_to_frames(delay_ms);
        if next_frames != self.target_delay_frames {
            self.prev_delay_frames = self.current_delay_frames;
            self.target_delay_frames = next_frames;
            self.crossfade_pos = 0;
        }
    }

    #[inline]
    pub fn target_delay_ms(&self) -> u32 {
        (self.target_delay_frames / FRAMES_PER_MS) as u32
    }

    #[inline]
    fn read_delayed(&self, delay_frames: usize) -> (f32, f32) {
        let read_idx = self.write_pos.wrapping_sub(delay_frames) & self.mask;
        let base = read_idx * 2;
        (self.buffer[base], self.buffer[base + 1])
    }

    /// インターリーブステレオPCMスライス `[L, R, L, R, ...]` をインプレースで遅延処理する
    pub fn process_in_place(&mut self, interleaved_pcm: &mut [f32]) {
        debug_assert_eq!(interleaved_pcm.len() % 2, 0);

        for frame in interleaved_pcm.chunks_exact_mut(2) {
            let w_base = self.write_pos * 2;
            self.buffer[w_base] = frame[0];
            self.buffer[w_base + 1] = frame[1];

            let (out_l, out_r) = if self.crossfade_pos < self.crossfade_len {
                let (old_l, old_r) = self.read_delayed(self.prev_delay_frames);
                let (new_l, new_r) = self.read_delayed(self.target_delay_frames);
                let alpha = (self.crossfade_pos + 1) as f32 / self.crossfade_len as f32;
                let inv = 1.0 - alpha;
                self.crossfade_pos += 1;
                if self.crossfade_pos >= self.crossfade_len {
                    self.current_delay_frames = self.target_delay_frames;
                }
                (old_l * inv + new_l * alpha, old_r * inv + new_r * alpha)
            } else {
                self.read_delayed(self.current_delay_frames)
            };

            frame[0] = out_l;
            frame[1] = out_r;
            self.write_pos = (self.write_pos + 1) & self.mask;
        }
    }

    pub fn clear(&mut self) {
        self.buffer.fill(0.0);
        self.write_pos = 0;
        let _ = self.capacity_frames;
    }
}
