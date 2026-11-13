use crate::delay::StereoDelayLine;
use crate::meter::{db_to_linear, AtomicPeakStereo};
use crate::resampler::PolyphaseResampler;
use crate::{AUDIO_CHANNELS, MAX_DELAY_MS, MIX_BLOCK_FRAMES};
use core_types::clock::{MasterClock, SampleTime, MASTER_SAMPLE_RATE};
use core_types::ring_buffer::{spsc_channel, SpscConsumer, SpscProducer};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

/// 入力チャンネル識別子
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelId(pub u32);

/// 音声入力ソース設定
#[derive(Debug, Clone)]
pub struct AudioInputConfig {
    pub name: String,
    pub sample_rate: u32,
    pub channels: usize,
    pub initial_gain: f32,
    pub initial_muted: bool,
    pub initial_delay_ms: u32,
}

impl Default for AudioInputConfig {
    fn default() -> Self {
        Self {
            name: "Input".to_string(),
            sample_rate: MASTER_SAMPLE_RATE as u32,
            channels: AUDIO_CHANNELS,
            initial_gain: 1.0,
            initial_muted: false,
            initial_delay_ms: 0,
        }
    }
}

struct ChannelControl {
    id: ChannelId,
    name: String,
    gain_bits: AtomicU32,
    muted: AtomicBool,
    delay_ms: AtomicU32,
    peak: AtomicPeakStereo,
}

impl ChannelControl {
    fn new(id: ChannelId, config: &AudioInputConfig) -> Self {
        Self {
            id,
            name: config.name.clone(),
            gain_bits: AtomicU32::new(config.initial_gain.max(0.0).to_bits()),
            muted: AtomicBool::new(config.initial_muted),
            delay_ms: AtomicU32::new(config.initial_delay_ms.min(MAX_DELAY_MS)),
            peak: AtomicPeakStereo::new(),
        }
    }
}

/// UI / RPC スレッドからロックフリーにチャンネル設定変更・ピーク監視を行うハンドル
#[derive(Clone)]
pub struct ChannelHandle {
    ctrl: Arc<ChannelControl>,
}

impl ChannelHandle {
    pub fn id(&self) -> ChannelId {
        self.ctrl.id
    }

    pub fn name(&self) -> &str {
        &self.ctrl.name
    }

    pub fn set_gain_linear(&self, gain: f32) {
        self.ctrl
            .gain_bits
            .store(gain.clamp(0.0, 4.0).to_bits(), Ordering::Release);
    }

    pub fn set_gain_db(&self, db: f32) {
        self.set_gain_linear(db_to_linear(db));
    }

    pub fn gain_linear(&self) -> f32 {
        f32::from_bits(self.ctrl.gain_bits.load(Ordering::Acquire))
    }

    pub fn set_muted(&self, muted: bool) {
        self.ctrl.muted.store(muted, Ordering::Release);
    }

    pub fn is_muted(&self) -> bool {
        self.ctrl.muted.load(Ordering::Acquire)
    }

    pub fn set_delay_ms(&self, delay_ms: u32) {
        self.ctrl
            .delay_ms
            .store(delay_ms.min(MAX_DELAY_MS), Ordering::Release);
    }

    pub fn delay_ms(&self) -> u32 {
        self.ctrl.delay_ms.load(Ordering::Acquire)
    }

    pub fn take_peak_dbfs(&self) -> (f32, f32) {
        self.ctrl.peak.take_dbfs()
    }

    pub fn peek_peak_dbfs(&self) -> (f32, f32) {
        self.ctrl.peak.peek_dbfs()
    }
}

struct MasterControl {
    gain_bits: AtomicU32,
    muted: AtomicBool,
    peak: AtomicPeakStereo,
}

/// マスター出力の制御・ピーク取得ハンドル
#[derive(Clone)]
pub struct MasterHandle {
    ctrl: Arc<MasterControl>,
}

impl MasterHandle {
    pub fn set_gain_linear(&self, gain: f32) {
        self.ctrl
            .gain_bits
            .store(gain.clamp(0.0, 4.0).to_bits(), Ordering::Release);
    }

    pub fn set_gain_db(&self, db: f32) {
        self.set_gain_linear(db_to_linear(db));
    }

    pub fn gain_linear(&self) -> f32 {
        f32::from_bits(self.ctrl.gain_bits.load(Ordering::Acquire))
    }

    pub fn set_muted(&self, muted: bool) {
        self.ctrl.muted.store(muted, Ordering::Release);
    }

    pub fn is_muted(&self) -> bool {
        self.ctrl.muted.load(Ordering::Acquire)
    }

    pub fn take_peak_dbfs(&self) -> (f32, f32) {
        self.ctrl.peak.take_dbfs()
    }

    pub fn peek_peak_dbfs(&self) -> (f32, f32) {
        self.ctrl.peak.peek_dbfs()
    }
}

/// キャプチャスレッドが所有し、任意レートのPCMを48kHz/2chへ変換して
/// ロックフリー SPSC リングバッファ経由でミキサーへ送り込むプロデューサー
pub struct AudioInputProducer {
    id: ChannelId,
    resampler: PolyphaseResampler,
    producer: SpscProducer<f32>,
    scratch_f32: Vec<f32>,
    scratch_48k: Vec<f32>,
}

impl AudioInputProducer {
    pub fn id(&self) -> ChannelId {
        self.id
    }

    /// f32 PCM（インターリーブ）を投入し、SPSCバッファに書き込めたサンプル数を返す
    pub fn push_f32(&mut self, interleaved_pcm: &[f32]) -> usize {
        if self.resampler.is_passthrough() {
            let valid = (interleaved_pcm.len() / 2) * 2;
            return self.producer.push_slice(&interleaved_pcm[..valid]);
        }

        self.scratch_48k.clear();
        self.resampler.process(interleaved_pcm, &mut self.scratch_48k);
        self.producer.push_slice(&self.scratch_48k)
    }

    /// S16_LE 整数PCM（ALSA等）を f32 [-1.0, 1.0] に正規化して投入する
    pub fn push_i16(&mut self, interleaved_i16: &[i16]) -> usize {
        self.scratch_f32.clear();
        self.scratch_f32.reserve(interleaved_i16.len());
        const INV_I16: f32 = 1.0 / 32768.0;
        for &s in interleaved_i16 {
            self.scratch_f32.push(s as f32 * INV_I16);
        }
        if self.resampler.is_passthrough() {
            let valid = (self.scratch_f32.len() / 2) * 2;
            self.producer.push_slice(&self.scratch_f32[..valid])
        } else {
            self.scratch_48k.clear();
            self.resampler
                .process(&self.scratch_f32, &mut self.scratch_48k);
            self.producer.push_slice(&self.scratch_48k)
        }
    }

    /// S32_LE 整数PCM（ALSAプロ向け等）を f32 [-1.0, 1.0] に正規化して投入する
    pub fn push_i32(&mut self, interleaved_i32: &[i32]) -> usize {
        self.scratch_f32.clear();
        self.scratch_f32.reserve(interleaved_i32.len());
        const INV_I32: f32 = 1.0 / 2_147_483_648.0;
        for &s in interleaved_i32 {
            self.scratch_f32.push(s as f32 * INV_I32);
        }
        if self.resampler.is_passthrough() {
            let valid = (self.scratch_f32.len() / 2) * 2;
            self.producer.push_slice(&self.scratch_f32[..valid])
        } else {
            self.scratch_48k.clear();
            self.resampler
                .process(&self.scratch_f32, &mut self.scratch_48k);
            self.producer.push_slice(&self.scratch_48k)
        }
    }

    pub fn available_write_samples(&self) -> usize {
        self.producer.available_write()
    }
}

struct ChannelRuntime {
    ctrl: Arc<ChannelControl>,
    consumer: SpscConsumer<f32>,
    delay_line: StereoDelayLine,
    scratch: Vec<f32>,
}

/// UI 通知用の全チャンネル・マスタースナップショット（30Hz 送信向け）
#[derive(Debug, Clone)]
pub struct ChannelSnapshot {
    pub id: ChannelId,
    pub name: String,
    pub gain_linear: f32,
    pub muted: bool,
    pub delay_ms: u32,
    pub peak_l_dbfs: f32,
    pub peak_r_dbfs: f32,
}

#[derive(Debug, Clone)]
pub struct MixerSnapshot {
    pub master_gain_linear: f32,
    pub master_muted: bool,
    pub master_peak_l_dbfs: f32,
    pub master_peak_r_dbfs: f32,
    pub channels: Vec<ChannelSnapshot>,
}

/// 48kHz / 2ch f32 ロックフリーマルチチャンネル音声ミキサー
pub struct AudioMixer {
    clock: MasterClock,
    next_id: u32,
    channels: Vec<ChannelRuntime>,
    master: Arc<MasterControl>,
    /// ジッター・ドリフト吸収用に各入力SPSCへ許す最大滞留サンプル数（超えたら古いサンプルを間引く）
    max_input_backlog_samples: usize,
}

impl AudioMixer {
    pub fn new(clock: MasterClock) -> Self {
        Self {
            clock,
            next_id: 1,
            channels: Vec::new(),
            master: Arc::new(MasterControl {
                gain_bits: AtomicU32::new(1.0f32.to_bits()),
                muted: AtomicBool::new(false),
                peak: AtomicPeakStereo::new(),
            }),
            // 48kHz * 2ch * 120ms = 11,520 floats
            max_input_backlog_samples: (MASTER_SAMPLE_RATE as usize * AUDIO_CHANNELS * 120) / 1000,
        }
    }

    pub fn master_handle(&self) -> MasterHandle {
        MasterHandle {
            ctrl: Arc::clone(&self.master),
        }
    }

    pub fn master_clock(&self) -> &MasterClock {
        &self.clock
    }

    /// 新規入力チャンネルを追加し、UI制御用 `ChannelHandle` とキャプチャ用 `AudioInputProducer` を返す
    pub fn add_channel(
        &mut self,
        config: AudioInputConfig,
    ) -> (ChannelHandle, AudioInputProducer) {
        let id = ChannelId(self.next_id);
        self.next_id += 1;

        // SPSCリングバッファ容量: 48kHz * 2ch * 250ms = 24,000 -> 32,768 floats
        let ring_capacity = 32_768;
        let (producer, consumer) = spsc_channel::<f32>(ring_capacity);

        let ctrl = Arc::new(ChannelControl::new(id, &config));
        let delay_line = StereoDelayLine::new(config.initial_delay_ms);
        let resampler = PolyphaseResampler::new(config.sample_rate, config.channels);

        self.channels.push(ChannelRuntime {
            ctrl: Arc::clone(&ctrl),
            consumer,
            delay_line,
            scratch: vec![0.0f32; MIX_BLOCK_FRAMES * AUDIO_CHANNELS],
        });

        let handle = ChannelHandle { ctrl };
        let input_producer = AudioInputProducer {
            id,
            resampler,
            producer,
            scratch_f32: Vec::with_capacity(2048),
            scratch_48k: Vec::with_capacity(4096),
        };

        (handle, input_producer)
    }

    pub fn channel_handles(&self) -> Vec<ChannelHandle> {
        self.channels
            .iter()
            .map(|c| ChannelHandle {
                ctrl: Arc::clone(&c.ctrl),
            })
            .collect()
    }

    /// UI 送信用に、全チャンネルとマスターの現在の設定値・ピークレベル（リセット付き）を取得する
    pub fn take_snapshot(&self) -> MixerSnapshot {
        let (master_peak_l_dbfs, master_peak_r_dbfs) = self.master.peak.take_dbfs();
        let channels = self
            .channels
            .iter()
            .map(|c| {
                let (peak_l_dbfs, peak_r_dbfs) = c.ctrl.peak.take_dbfs();
                ChannelSnapshot {
                    id: c.ctrl.id,
                    name: c.ctrl.name.clone(),
                    gain_linear: f32::from_bits(c.ctrl.gain_bits.load(Ordering::Acquire)),
                    muted: c.ctrl.muted.load(Ordering::Acquire),
                    delay_ms: c.ctrl.delay_ms.load(Ordering::Acquire),
                    peak_l_dbfs,
                    peak_r_dbfs,
                }
            })
            .collect();

        MixerSnapshot {
            master_gain_linear: f32::from_bits(self.master.gain_bits.load(Ordering::Acquire)),
            master_muted: self.master.muted.load(Ordering::Acquire),
            master_peak_l_dbfs,
            master_peak_r_dbfs,
            channels,
        }
    }

    /// 1ブロック（`out_interleaved.len() / 2` フレーム）のミキシングを実行し、
    /// マスタークロックを進めた上で、このブロック開始時点の `SampleTime` を返す。
    pub fn mix_block(&mut self, out_interleaved: &mut [f32]) -> SampleTime {
        debug_assert_eq!(out_interleaved.len() % AUDIO_CHANNELS, 0);
        let sample_count = out_interleaved.len();
        let frame_count = sample_count / AUDIO_CHANNELS;
        let block_start_samples = self.clock.now_samples();

        out_interleaved.fill(0.0);

        for ch in &mut self.channels {
            if ch.scratch.len() < sample_count {
                ch.scratch.resize(sample_count, 0.0);
            }
            let buf = &mut ch.scratch[..sample_count];

            // 遅延蓄積が上限を超えている場合は古いサンプルを破棄してリアルタイム性を維持
            let avail = ch.consumer.available_read();
            if avail > self.max_input_backlog_samples {
                let excess = ((avail - sample_count) / 2) * 2;
                let mut discarded = 0;
                while discarded < excess && ch.consumer.try_pop().is_some() {
                    discarded += 1;
                }
            }

            let read = ch.consumer.pop_slice(buf);
            if read < sample_count {
                buf[read..].fill(0.0);
            }

            // 入力ごとのディレイラインはミュート中も連続更新して位相・残響の途切れを防ぐ
            let target_delay = ch.ctrl.delay_ms.load(Ordering::Acquire);
            ch.delay_line.set_delay_ms(target_delay);
            ch.delay_line.process_in_place(buf);

            let muted = ch.ctrl.muted.load(Ordering::Acquire);
            let gain = if muted {
                0.0
            } else {
                f32::from_bits(ch.ctrl.gain_bits.load(Ordering::Acquire))
            };

            let mut peak_l = 0.0f32;
            let mut peak_r = 0.0f32;

            for (dst, src) in out_interleaved.chunks_exact_mut(2).zip(buf.chunks_exact(2)) {
                let l = src[0] * gain;
                let r = src[1] * gain;
                peak_l = peak_l.max(l.abs());
                peak_r = peak_r.max(r.abs());
                dst[0] += l;
                dst[1] += r;
            }

            ch.ctrl.peak.observe(peak_l, peak_r);
        }

        // マスターボリューム・ミュート・ソフトクリッパー適用
        let master_muted = self.master.muted.load(Ordering::Acquire);
        let master_gain = if master_muted {
            0.0
        } else {
            f32::from_bits(self.master.gain_bits.load(Ordering::Acquire))
        };

        let mut master_peak_l = 0.0f32;
        let mut master_peak_r = 0.0f32;

        for frame in out_interleaved.chunks_exact_mut(2) {
            let l = soft_clip(frame[0] * master_gain);
            let r = soft_clip(frame[1] * master_gain);
            frame[0] = l;
            frame[1] = r;
            master_peak_l = master_peak_l.max(l.abs());
            master_peak_r = master_peak_r.max(r.abs());
        }

        self.master.peak.observe(master_peak_l, master_peak_r);
        self.clock.advance_samples(frame_count as u64);
        block_start_samples
    }
}

/// 0.85 (-1.4 dBFS) 未満は無歪みで通過させ、それ以上の過大入力は [-1.0, 1.0] へ滑らかに飽和させるソフトクリッパー
#[inline]
pub fn soft_clip(x: f32) -> f32 {
    const KNEE: f32 = 0.85;
    const HEADROOM: f32 = 1.0 - KNEE;
    let ax = x.abs();
    if ax <= KNEE {
        x
    } else {
        let excess = (ax - KNEE) / HEADROOM;
        let shaped = KNEE + HEADROOM * excess.tanh();
        x.signum() * shaped.min(1.0)
    }
}
