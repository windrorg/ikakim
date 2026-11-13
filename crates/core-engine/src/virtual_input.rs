use core_types::clock::PtsUs;
use core_types::media::{NativeBufferHandle, PixelFormat, VideoFrame};
use std::f32::consts::TAU;

/// 仮想映像ソースが生成するテストパターン
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualVideoPattern {
    /// 6色カラーバー + 水平移動する白バー（実運用スモーク/モーション確認向け）
    ColorbarsMotion,
    /// 4象限の明確な単色領域 + 下部または中央に水平移動する高輝度バー（色再現・座標・動きの自動検証向け）
    QuadrantSignal {
        top_left: [u8; 4],
        top_right: [u8; 4],
        bottom_left: [u8; 4],
        bottom_right: [u8; 4],
        moving_bar: bool,
    },
    /// 単色ベタ塗り（オーバーレイレイヤー検証向け）
    Solid([u8; 4]),
}

impl Default for VirtualVideoPattern {
    fn default() -> Self {
        Self::QuadrantSignal {
            top_left: [230, 28, 28, 255],      // Red
            top_right: [28, 225, 28, 255],     // Green
            bottom_left: [28, 56, 235, 255],   // Blue
            bottom_right: [232, 220, 28, 255], // Yellow
            moving_bar: true,
        }
    }
}

/// 決定論的な仮想映像入力ジェネレータ（RGBA / BGRA / BT.709 NV12 / YUYV 出力対応）
#[derive(Debug, Clone)]
pub struct VirtualVideoSource {
    pub width: u32,
    pub height: u32,
    pub pattern: VirtualVideoPattern,
    pub bar_speed_px_per_frame: u32,
}

impl VirtualVideoSource {
    pub fn new(width: u32, height: u32, pattern: VirtualVideoPattern) -> Self {
        Self {
            width: width.max(2),
            height: height.max(2),
            pattern,
            bar_speed_px_per_frame: ((width / 40).max(4)).min(width.max(4)),
        }
    }

    pub fn with_bar_speed(mut self, px_per_frame: u32) -> Self {
        self.bar_speed_px_per_frame = px_per_frame.max(1);
        self
    }

    /// 指定フレームにおける移動バーの左上X座標と幅を返す
    pub fn moving_bar_rect(&self, frame_idx: u64) -> (u32, u32, u32, u32) {
        let bar_w = (self.width / 10).max(16).min(self.width);
        let travel = (self.width - bar_w).max(1);
        let x = ((frame_idx * self.bar_speed_px_per_frame as u64) % travel as u64) as u32;
        let band_h = (self.height / 8).max(8);
        let y = (self.height / 2).saturating_sub(band_h / 2);
        (x, y, bar_w, band_h)
    }

    #[inline]
    pub fn sample_pixel_rgba(&self, frame_idx: u64, x: u32, y: u32) -> [u8; 4] {
        match self.pattern {
            VirtualVideoPattern::Solid(c) => c,
            VirtualVideoPattern::ColorbarsMotion => {
                let (bx, by, bw, bh) = self.moving_bar_rect(frame_idx);
                if x >= bx && x < bx + bw && y >= by && y < by + bh {
                    return [245, 245, 245, 255];
                }
                let band = (x * 6) / self.width;
                match band {
                    0 => [235, 235, 235, 255], // White
                    1 => [235, 220, 24, 255],  // Yellow
                    2 => [24, 220, 225, 255],  // Cyan
                    3 => [24, 215, 24, 255],   // Green
                    4 => [220, 24, 220, 255],  // Magenta
                    _ => [225, 28, 28, 255],   // Red
                }
            }
            VirtualVideoPattern::QuadrantSignal {
                top_left,
                top_right,
                bottom_left,
                bottom_right,
                moving_bar,
            } => {
                if moving_bar {
                    let (bx, by, bw, bh) = self.moving_bar_rect(frame_idx);
                    if x >= bx && x < bx + bw && y >= by && y < by + bh {
                        return [245, 245, 245, 255];
                    }
                }
                let right = x >= self.width / 2;
                let bottom = y >= self.height / 2;
                match (right, bottom) {
                    (false, false) => top_left,
                    (true, false) => top_right,
                    (false, true) => bottom_left,
                    (true, true) => bottom_right,
                }
            }
        }
    }

    /// 32-bit RGBA8 フレームを生成
    pub fn render_rgba(&self, frame_idx: u64) -> Vec<u8> {
        let mut out = vec![0u8; (self.width * self.height * 4) as usize];
        for y in 0..self.height {
            for x in 0..self.width {
                let p = self.sample_pixel_rgba(frame_idx, x, y);
                let idx = ((y * self.width + x) * 4) as usize;
                out[idx..idx + 4].copy_from_slice(&p);
            }
        }
        out
    }

    /// 32-bit BGRA8 フレームを生成（macOS CVPixelBuffer BGRA 互換）
    pub fn render_bgra(&self, frame_idx: u64) -> Vec<u8> {
        let mut out = vec![0u8; (self.width * self.height * 4) as usize];
        for y in 0..self.height {
            for x in 0..self.width {
                let p = self.sample_pixel_rgba(frame_idx, x, y);
                let idx = ((y * self.width + x) * 4) as usize;
                out[idx] = p[2];
                out[idx + 1] = p[1];
                out[idx + 2] = p[0];
                out[idx + 3] = p[3];
            }
        }
        out
    }

    /// `core_gpu::ColorSpace::Bt709Limited` と完全対称な BT.709 Studio Range NV12 (Y + UV) を生成
    pub fn render_nv12_bt709(&self, frame_idx: u64) -> (Vec<u8>, Vec<u8>) {
        let w = self.width as usize;
        let h = self.height as usize;
        let mut y_plane = vec![16u8; w * h];
        let mut uv_plane = vec![128u8; w * (h / 2)];

        for row in 0..h {
            for col in 0..w {
                let p = self.sample_pixel_rgba(frame_idx, col as u32, row as u32);
                let (y, cb, cr) = rgb_to_yuv_bt709_limited(p[0], p[1], p[2]);
                y_plane[row * w + col] = y;
                if row % 2 == 0 && col % 2 == 0 {
                    let uv_idx = (row / 2) * w + col;
                    uv_plane[uv_idx] = cb;
                    uv_plane[uv_idx + 1] = cr;
                }
            }
        }
        (y_plane, uv_plane)
    }

    /// BT.709 Studio Range パックド YUYV (4:2:2) フレームを生成
    pub fn render_yuyv_bt709(&self, frame_idx: u64) -> Vec<u8> {
        let w = (self.width & !1) as usize;
        let h = self.height as usize;
        let mut out = vec![0u8; w * h * 2];
        for row in 0..h {
            for col in (0..w).step_by(2) {
                let p0 = self.sample_pixel_rgba(frame_idx, col as u32, row as u32);
                let p1 = self.sample_pixel_rgba(frame_idx, (col + 1) as u32, row as u32);
                let (y0, cb0, cr0) = rgb_to_yuv_bt709_limited(p0[0], p0[1], p0[2]);
                let (y1, cb1, cr1) = rgb_to_yuv_bt709_limited(p1[0], p1[1], p1[2]);
                let cb = ((cb0 as u16 + cb1 as u16) / 2) as u8;
                let cr = ((cr0 as u16 + cr1 as u16) / 2) as u8;
                let idx = (row * w + col) * 2;
                out[idx] = y0;
                out[idx + 1] = cb;
                out[idx + 2] = y1;
                out[idx + 3] = cr;
            }
        }
        out
    }

    /// CPUフォールバックバッファを包んだ汎用 `VideoFrame` を生成
    pub fn render_cpu_video_frame(
        &self,
        frame_idx: u64,
        format: PixelFormat,
        pts: PtsUs,
    ) -> VideoFrame {
        let (data, stride) = match format {
            PixelFormat::Rgba8 => (self.render_rgba(frame_idx), self.width * 4),
            PixelFormat::Bgra8 => (self.render_bgra(frame_idx), self.width * 4),
            PixelFormat::Yuyv => (self.render_yuyv_bt709(frame_idx), self.width * 2),
            _ => {
                let (mut y, uv) = self.render_nv12_bt709(frame_idx);
                y.extend_from_slice(&uv);
                (y, self.width)
            }
        };
        VideoFrame {
            width: self.width,
            height: self.height,
            format,
            pts,
            handle: NativeBufferHandle::CpuFallback { data, stride },
        }
    }
}

#[inline]
fn rgb_to_yuv_bt709_limited(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let rf = r as f32 / 255.0;
    let gf = g as f32 / 255.0;
    let bf = b as f32 / 255.0;
    let ey = 0.2126 * rf + 0.7152 * gf + 0.0722 * bf;
    let cb = (bf - ey) / 1.8556;
    let cr = (rf - ey) / 1.5748;
    let y = (16.0 + 219.0 * ey).round().clamp(16.0, 235.0) as u8;
    let u = (128.0 + 224.0 * cb).round().clamp(16.0, 240.0) as u8;
    let v = (128.0 + 224.0 * cr).round().clamp(16.0, 240.0) as u8;
    (y, u, v)
}

/// 仮想音声ソースが生成するテスト信号種別
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VirtualAudioSignal {
    Silence,
    /// L/R 共通のモノラル正弦波（位相連続）
    SineTone { freq_hz: f32, amplitude: f32 },
    /// L/R 独立周波数のステレオ正弦波（左右チャネル分離検証用）
    DualChannelTone {
        left_freq_hz: f32,
        right_freq_hz: f32,
        amplitude: f32,
    },
}

/// 48kHz インターリーブステレオ f32 PCM を位相連続で生成する仮想音声ソース
#[derive(Debug, Clone)]
pub struct VirtualAudioSource {
    pub sample_rate: u32,
    pub signal: VirtualAudioSignal,
    phase_l: f32,
    phase_r: f32,
}

impl VirtualAudioSource {
    pub fn new(sample_rate: u32, signal: VirtualAudioSignal) -> Self {
        Self {
            sample_rate: sample_rate.max(8_000),
            signal,
            phase_l: 0.0,
            phase_r: 0.0,
        }
    }

    /// 指定フレームレート（30fps: 4,3,3 / 60fps: 2,2,1）の1フレームあたりに供給すべき
    /// 10ms ミキサーブロック数（`MIX_BLOCK_FRAMES=480` で厳密に 48kHz 同期）
    #[inline]
    pub fn mix_blocks_for_fps_frame(fps: u32, frame_idx: u64) -> usize {
        if fps >= 60 {
            if frame_idx % 3 == 2 {
                1
            } else {
                2
            }
        } else if frame_idx % 3 == 0 {
            4
        } else {
            3
        }
    }

    /// 30fps（`33.33ms`）映像フレーム1枚あたりに供給すべき 10ms ミキサーブロック数（4, 3, 3）
    #[inline]
    pub fn mix_blocks_for_30fps_frame(frame_idx: u64) -> usize {
        Self::mix_blocks_for_fps_frame(30, frame_idx)
    }

    /// 60fps（`16.67ms`）映像フレーム1枚あたりに供給すべき 10ms ミキサーブロック数（2, 2, 1）
    #[inline]
    pub fn mix_blocks_for_60fps_frame(frame_idx: u64) -> usize {
        Self::mix_blocks_for_fps_frame(60, frame_idx)
    }

    /// `frames` サンプル分のインターリーブ・ステレオ f32 PCM（要素数 = `frames * 2`）を位相連続で生成
    pub fn next_stereo_f32(&mut self, frames: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames * 2);
        let sr = self.sample_rate as f32;
        match self.signal {
            VirtualAudioSignal::Silence => {
                out.resize(frames * 2, 0.0);
            }
            VirtualAudioSignal::SineTone { freq_hz, amplitude } => {
                let inc = TAU * freq_hz / sr;
                for _ in 0..frames {
                    let s = self.phase_l.sin() * amplitude;
                    self.phase_l = (self.phase_l + inc) % TAU;
                    self.phase_r = self.phase_l;
                    out.push(s);
                    out.push(s);
                }
            }
            VirtualAudioSignal::DualChannelTone {
                left_freq_hz,
                right_freq_hz,
                amplitude,
            } => {
                let inc_l = TAU * left_freq_hz / sr;
                let inc_r = TAU * right_freq_hz / sr;
                for _ in 0..frames {
                    out.push(self.phase_l.sin() * amplitude);
                    out.push(self.phase_r.sin() * amplitude);
                    self.phase_l = (self.phase_l + inc_l) % TAU;
                    self.phase_r = (self.phase_r + inc_r) % TAU;
                }
            }
        }
        out
    }
}

/// インターリーブ信号から単一チャネルを指定周波数で Goertzel 解析した正規化振幅を返す
pub fn goertzel_magnitude(
    interleaved: &[f32],
    channels: usize,
    channel_idx: usize,
    sample_rate: f32,
    target_hz: f32,
) -> f32 {
    if channels == 0 || channel_idx >= channels {
        return 0.0;
    }
    let n = interleaved.len() / channels;
    if n < 32 {
        return 0.0;
    }
    let omega = TAU * target_hz / sample_rate;
    let coeff = 2.0 * omega.cos();
    let mut q1 = 0.0f64;
    let mut q2 = 0.0f64;
    let c = coeff as f64;
    for frame in 0..n {
        let x = interleaved[frame * channels + channel_idx] as f64;
        let q0 = c * q1 - q2 + x;
        q2 = q1;
        q1 = q0;
    }
    let mag_sq = q1 * q1 + q2 * q2 - q1 * q2 * c;
    (mag_sq.max(0.0).sqrt() * 2.0 / n as f64) as f32
}

/// インターリーブ信号の単一チャネル RMS を dBFS で返す
pub fn channel_rms_dbfs(interleaved: &[f32], channels: usize, channel_idx: usize) -> f32 {
    if channels == 0 || channel_idx >= channels {
        return -96.0;
    }
    let n = interleaved.len() / channels;
    if n == 0 {
        return -96.0;
    }
    let mut sum_sq = 0.0f64;
    for frame in 0..n {
        let s = interleaved[frame * channels + channel_idx] as f64;
        sum_sq += s * s;
    }
    let rms = (sum_sq / n as f64).sqrt();
    if rms <= 1e-5 {
        -96.0
    } else {
        (20.0 * rms.log10()) as f32
    }
}
