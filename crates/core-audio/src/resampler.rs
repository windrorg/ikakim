use core_types::clock::MASTER_SAMPLE_RATE;
use std::f64::consts::PI;

const TAPS: usize = 16;
const TAP_MASK: usize = TAPS - 1;
const NUM_PHASES: usize = 512;

/// ゼロ依存のポリフェーズ・ウィンドウ付き Sinc FIR リサンプラー。
/// 任意の入力サンプルレートおよびチャンネル数（1ch または 2ch）を、
/// システム共通の 48kHz / 2ch インターリーブ f32 PCM へ変換する。
pub struct PolyphaseResampler {
    src_rate: u32,
    dst_rate: u32,
    src_channels: usize,
    passthrough: bool,
    mono_upmix_only: bool,
    /// `[phase]` ごとに 16 タップ固定長配列として格納した事前計算済みフィルタ係数テーブル
    coeffs: Vec<[f32; TAPS]>,
    /// L/R 各チャンネルの履歴リング（TAPS 要素の連続スライス参照用に2倍長保持）
    hist_l: [f32; TAPS * 2],
    hist_r: [f32; TAPS * 2],
    hist_pos: usize,
    /// 入力フレーム単位の小数位相位置（整数部が入力消費数、小数部がフィルタ位相）
    phase_acc: f64,
    step: f64,
}

impl PolyphaseResampler {
    pub fn new(src_rate: u32, src_channels: usize) -> Self {
        assert!(src_rate > 0, "source sample rate must be > 0");
        assert!(
            src_channels == 1 || src_channels == 2,
            "only mono (1ch) or stereo (2ch) source is supported"
        );

        let dst_rate = MASTER_SAMPLE_RATE as u32;
        let passthrough = src_rate == dst_rate && src_channels == 2;
        let mono_upmix_only = src_rate == dst_rate && src_channels == 1;
        let step = src_rate as f64 / dst_rate as f64;
        let cutoff = if src_rate > dst_rate {
            (dst_rate as f64 / src_rate as f64) * 0.92
        } else {
            0.95
        };

        let coeffs = if passthrough || mono_upmix_only {
            Vec::new()
        } else {
            Self::build_polyphase_table(cutoff)
        };

        Self {
            src_rate,
            dst_rate,
            src_channels,
            passthrough,
            mono_upmix_only,
            coeffs,
            hist_l: [0.0; TAPS * 2],
            hist_r: [0.0; TAPS * 2],
            hist_pos: 0,
            phase_acc: 0.0,
            step,
        }
    }

    pub fn src_rate(&self) -> u32 {
        self.src_rate
    }

    pub fn dst_rate(&self) -> u32 {
        self.dst_rate
    }

    pub fn src_channels(&self) -> usize {
        self.src_channels
    }

    #[inline]
    pub fn is_passthrough(&self) -> bool {
        self.passthrough
    }

    #[inline]
    pub fn is_mono_upmix_only(&self) -> bool {
        self.mono_upmix_only
    }

    fn build_polyphase_table(cutoff: f64) -> Vec<[f32; TAPS]> {
        let mut table = vec![[0.0f32; TAPS]; NUM_PHASES];
        let half = (TAPS / 2) as f64;

        for (p, phase_row) in table.iter_mut().enumerate() {
            let frac = p as f64 / NUM_PHASES as f64;
            let mut sum = 0.0f64;
            let mut row = [0.0f64; TAPS];

            for (t, slot) in row.iter_mut().enumerate() {
                // 中心タップ (TAPS/2 - 1) からの距離
                let x = (t as f64 - (half - 1.0)) - frac;
                let sinc = if x.abs() < 1e-9 {
                    cutoff
                } else {
                    (PI * cutoff * x).sin() / (PI * x)
                };

                // Blackman-Harris 窓（サイドローブ抑制）
                let w_pos = (t as f64 + (1.0 - frac)) / TAPS as f64;
                let window = if (0.0..=1.0).contains(&w_pos) {
                    0.35875
                        - 0.48829 * (2.0 * PI * w_pos).cos()
                        + 0.14128 * (4.0 * PI * w_pos).cos()
                        - 0.01168 * (6.0 * PI * w_pos).cos()
                } else {
                    0.0
                };

                let val = sinc * window;
                *slot = val;
                sum += val;
            }

            let inv_sum = if sum.abs() > 1e-9 { 1.0 / sum } else { 1.0 };
            for (t, &val) in row.iter().enumerate() {
                phase_row[t] = (val * inv_sum) as f32;
            }
        }
        table
    }

    #[inline]
    fn push_hist(&mut self, l: f32, r: f32) {
        let pos = self.hist_pos;
        self.hist_l[pos] = l;
        self.hist_l[pos + TAPS] = l;
        self.hist_r[pos] = r;
        self.hist_r[pos + TAPS] = r;
        self.hist_pos = (pos + 1) & TAP_MASK;
    }

    /// 固定長 16 タップ配列参照による境界チェックなし・完全ループアンロール/SIMD FMA 畳み込み
    #[inline]
    fn convolve(&self, phase_idx: usize) -> (f32, f32) {
        let row: &[f32; TAPS] = &self.coeffs[phase_idx];
        let base = self.hist_pos;
        let hl: &[f32; TAPS] = self.hist_l[base..base + TAPS].try_into().unwrap();
        let hr: &[f32; TAPS] = self.hist_r[base..base + TAPS].try_into().unwrap();
        let mut acc_l = 0.0f32;
        let mut acc_r = 0.0f32;
        for i in 0..TAPS {
            let c = row[i];
            acc_l += hl[i] * c;
            acc_r += hr[i] * c;
        }
        (acc_l, acc_r)
    }

    /// 入力PCMを 48kHz / 2ch インターリーブPCMへ変換し、`out` へ追記する。
    /// 追記されたステレオ出力サンプル数（フレーム数 × 2）を返す。
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) -> usize {
        if self.passthrough {
            let valid = (input.len() / 2) * 2;
            out.extend_from_slice(&input[..valid]);
            return valid;
        }
        if self.mono_upmix_only {
            out.reserve(input.len() * 2);
            for &s in input {
                out.push(s);
                out.push(s);
            }
            return input.len() * 2;
        }

        let in_frames = input.len() / self.src_channels;
        if in_frames == 0 {
            return 0;
        }

        let start_len = out.len();
        let est_out_frames = ((in_frames as f64) / self.step).ceil() as usize + 4;
        out.reserve(est_out_frames * 2);

        let mut in_idx = 0usize;
        if self.src_channels == 1 {
            while in_idx < in_frames {
                while self.phase_acc >= 1.0 && in_idx < in_frames {
                    let v = input[in_idx];
                    self.push_hist(v, v);
                    self.phase_acc -= 1.0;
                    in_idx += 1;
                }
                if self.phase_acc < 1.0 {
                    let phase_idx =
                        ((self.phase_acc * NUM_PHASES as f64) as usize).min(NUM_PHASES - 1);
                    let (l, r) = self.convolve(phase_idx);
                    out.push(l);
                    out.push(r);
                    self.phase_acc += self.step;
                }
            }
        } else {
            while in_idx < in_frames {
                while self.phase_acc >= 1.0 && in_idx < in_frames {
                    let base = in_idx * 2;
                    self.push_hist(input[base], input[base + 1]);
                    self.phase_acc -= 1.0;
                    in_idx += 1;
                }
                if self.phase_acc < 1.0 {
                    let phase_idx =
                        ((self.phase_acc * NUM_PHASES as f64) as usize).min(NUM_PHASES - 1);
                    let (l, r) = self.convolve(phase_idx);
                    out.push(l);
                    out.push(r);
                    self.phase_acc += self.step;
                }
            }
        }

        out.len() - start_len
    }

    pub fn reset(&mut self) {
        self.hist_l.fill(0.0);
        self.hist_r.fill(0.0);
        self.hist_pos = 0;
        self.phase_acc = 0.0;
    }
}
