use core_audio::meter::{amplitude_to_dbfs, db_to_linear, DBFS_FLOOR};
use core_audio::mixer::{soft_clip, AudioInputConfig, AudioMixer};
use core_audio::{MIX_BLOCK_FRAMES};
use core_types::clock::{MasterClock, MASTER_SAMPLE_RATE};
use std::f32::consts::PI;

#[test]
fn test_per_channel_delay_exact_samples() {
    let clock = MasterClock::new();
    let mut mixer = AudioMixer::new(clock);

    let (handle, mut prod) = mixer.add_channel(AudioInputConfig {
        name: "mic".to_string(),
        sample_rate: 48_000,
        channels: 2,
        initial_gain: 1.0,
        initial_muted: false,
        initial_delay_ms: 10, // 10ms = 480 frames = 1 block
    });

    // Block 0: インパルス [0.5, -0.5] を先頭フレームに投入
    let mut in_block0 = vec![0.0f32; MIX_BLOCK_FRAMES * 2];
    in_block0[0] = 0.5;
    in_block0[1] = -0.5;
    assert_eq!(prod.push_f32(&in_block0), MIX_BLOCK_FRAMES * 2);

    // Block 1: 無音
    let in_block1 = vec![0.0f32; MIX_BLOCK_FRAMES * 2];
    assert_eq!(prod.push_f32(&in_block1), MIX_BLOCK_FRAMES * 2);

    // Mix Block 0 -> 10ms 遅延されているため出力はすべて 0.0 のはず
    let mut out0 = vec![0.0f32; MIX_BLOCK_FRAMES * 2];
    let t0 = mixer.mix_block(&mut out0);
    assert_eq!(t0.as_samples(), 0);
    assert!(out0.iter().all(|&s| s.abs() < 1e-6));

    // Mix Block 1 -> ちょうど 10ms (480 frames) 後の先頭にインパルスが現れるはず
    let mut out1 = vec![0.0f32; MIX_BLOCK_FRAMES * 2];
    let t1 = mixer.mix_block(&mut out1);
    assert_eq!(t1.as_samples(), MIX_BLOCK_FRAMES as u64);
    assert!((out1[0] - 0.5).abs() < 1e-5, "got {}", out1[0]);
    assert!((out1[1] - (-0.5)).abs() < 1e-5, "got {}", out1[1]);

    // ピークメーターが 0.5 (-6.02 dBFS) 近辺を観測していること
    let (pl, pr) = handle.take_peak_dbfs();
    assert!((pl - amplitude_to_dbfs(0.5)).abs() < 0.05);
    assert!((pr - amplitude_to_dbfs(0.5)).abs() < 0.05);

    // リセット後はフロア値に戻ること
    let (pl_after, _) = handle.peek_peak_dbfs();
    assert_eq!(pl_after, DBFS_FLOOR);
}

#[test]
fn test_dynamic_delay_crossfade_no_nan_and_gain_mute() {
    let clock = MasterClock::new();
    let mut mixer = AudioMixer::new(clock);

    let (ch1, mut prod1) = mixer.add_channel(AudioInputConfig {
        name: "ch1".to_string(),
        ..Default::default()
    });
    let (ch2, mut prod2) = mixer.add_channel(AudioInputConfig {
        name: "ch2".to_string(),
        initial_gain: 0.5,
        ..Default::default()
    });

    let block_a = vec![0.4f32; MIX_BLOCK_FRAMES * 2];
    let block_b = vec![0.4f32; MIX_BLOCK_FRAMES * 2];
    prod1.push_f32(&block_a);
    prod2.push_f32(&block_b);

    // 0.4 * 1.0 + 0.4 * 0.5 = 0.6
    let mut out = vec![0.0f32; MIX_BLOCK_FRAMES * 2];
    mixer.mix_block(&mut out);
    assert!((out[0] - 0.6).abs() < 1e-5);

    // ch1 をミュート、ch2 の遅延を 25ms に変更、マスターゲインを -6dB
    ch1.set_muted(true);
    ch2.set_delay_ms(25);
    let master = mixer.master_handle();
    master.set_gain_db(-6.0);

    prod1.push_f32(&block_a);
    prod2.push_f32(&block_b);
    mixer.mix_block(&mut out);

    assert!(out.iter().all(|s| s.is_finite()));
    let snap = mixer.take_snapshot();
    assert_eq!(snap.channels.len(), 2);
    assert!(snap.channels[0].muted);
    assert_eq!(snap.channels[1].delay_ms, 25);
    assert!((snap.master_gain_linear - db_to_linear(-6.0)).abs() < 1e-5);
}

#[test]
fn test_polyphase_resampler_44100_to_48000_sine_wave() {
    let clock = MasterClock::new();
    let mut mixer = AudioMixer::new(clock);

    // 44.1kHz モノラル入力 -> 48kHz ステレオへ自動変換される
    let (_handle, mut prod) = mixer.add_channel(AudioInputConfig {
        name: "hdmi-44k".to_string(),
        sample_rate: 44_100,
        channels: 1,
        ..Default::default()
    });

    // 100ms 分の 1kHz サイン波 (4410 samples @ 44.1kHz)
    let mut in_44k = Vec::with_capacity(4410);
    for n in 0..4410 {
        let t = n as f32 / 44_100.0;
        in_44k.push(0.5 * (2.0 * PI * 1000.0 * t).sin());
    }
    let pushed = prod.push_f32(&in_44k);
    // 48kHz / 2ch * 0.1s = 約 9600 サンプル
    assert!(pushed > 9000, "pushed samples = {}", pushed);

    // 8ブロック (80ms) ミックスして、左右同一・振幅 <= 0.55 の滑らかな信号が出ることを確認
    let mut out = vec![0.0f32; MIX_BLOCK_FRAMES * 2];
    let mut max_amp = 0.0f32;
    for _ in 0..8 {
        mixer.mix_block(&mut out);
        for frame in out.chunks_exact(2) {
            assert!((frame[0] - frame[1]).abs() < 1e-5);
            max_amp = max_amp.max(frame[0].abs());
        }
    }
    assert!(max_amp > 0.45 && max_amp < 0.55, "max_amp = {}", max_amp);
}

#[test]
fn test_soft_clipper_bounds_and_master_clock_advance() {
    assert!((soft_clip(0.5) - 0.5).abs() < 1e-6);
    assert!((soft_clip(-0.8) - (-0.8)).abs() < 1e-6);
    let clipped = soft_clip(5.0);
    assert!(clipped > 0.85 && clipped <= 1.0);
    let clipped_neg = soft_clip(-5.0);
    assert!((-1.0..-0.85).contains(&clipped_neg));

    let clock = MasterClock::new();
    let mut mixer = AudioMixer::new(clock.clone());
    let mut out = vec![0.0f32; MIX_BLOCK_FRAMES * 2];
    for _ in 0..100 {
        mixer.mix_block(&mut out);
    }
    assert_eq!(clock.now_samples().as_samples(), MASTER_SAMPLE_RATE);
}

#[derive(Clone, Copy)]
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn next_f32_signed(&mut self) -> f32 {
        let u = (self.next_u64() >> 40) as f32 / ((1u64 << 24) as f32);
        u * 2.0 - 1.0
    }

    fn range_u32(&mut self, min: u32, max_inclusive: u32) -> u32 {
        if min >= max_inclusive {
            return min;
        }
        let span = (max_inclusive - min) + 1;
        min + (self.next_u64() as u32 % span)
    }
}

#[test]
fn test_property_soft_clip_monotonicity_symmetry_and_bounds() {
    let mut rng = SplitMix64(0xA0D1_0BEE_F123_4567);
    for _ in 0..2000 {
        let a = rng.next_f32_signed() * 20.0;
        let b = rng.next_f32_signed() * 20.0;
        let (x1, x2) = if a <= b { (a, b) } else { (b, a) };

        let y1 = soft_clip(x1);
        let y2 = soft_clip(x2);

        // 1. 境界性: 出力は常に [-1.0, 1.0] の範囲内かつ有限
        assert!(y1.is_finite() && (-1.0..=1.0).contains(&y1));
        assert!(y2.is_finite() && (-1.0..=1.0).contains(&y2));

        // 2. 広義単調増加性: x1 <= x2 => soft_clip(x1) <= soft_clip(x2)
        assert!(
            y1 <= y2 + 1e-6,
            "monotonicity violated: soft_clip({x1})={y1} > soft_clip({x2})={y2}"
        );

        // 3. 奇関数対称性: soft_clip(-x) == -soft_clip(x)
        let yn = soft_clip(-x1);
        assert!(
            (yn + y1).abs() < 1e-6,
            "odd symmetry violated: soft_clip({x1})={y1}, soft_clip({})={yn}",
            -x1
        );

        // 4. 線形領域不変性: |x| <= 0.85 では恒等写像
        if x1.abs() <= 0.85 {
            assert!((y1 - x1).abs() < 1e-6);
        }
    }
}

#[test]
fn test_property_stereo_delay_line_dynamic_jumps_and_impulse_accuracy() {
    use core_audio::delay::StereoDelayLine;

    let mut rng = SplitMix64(0xDE1A_7123_9876_5432);
    let mut delay = StereoDelayLine::new(0);
    let mut block = vec![0.0f32; 256 * 2];

    // 1. ランダムな遅延時間ジャンプ（0..=1200ms、上限1000msクランプ含む）とランダム入力に対する有界安定性
    for _ in 0..250 {
        let target_ms = rng.range_u32(0, 1200);
        delay.set_delay_ms(target_ms);
        assert!(delay.target_delay_ms() <= 1000);

        for s in &mut block {
            *s = rng.next_f32_signed();
        }
        delay.process_in_place(&mut block);
        for &s in &block {
            assert!(
                s.is_finite() && s.abs() <= 1.0001,
                "delay output out of bounds: {s}"
            );
        }
    }

    // 2. 任意の遅延設定 (0, 5, 17, 50ms) においてクロスフェード完了後にインパルスが正確に N*48 フレーム遅れて出力される性質
    for &ms in &[0u32, 5, 17, 50] {
        delay.clear();
        delay.set_delay_ms(ms);
        // クロスフェード完了まで無音を通す
        let mut warmup = vec![0.0f32; 512 * 2];
        delay.process_in_place(&mut warmup);

        let expected_frames = (ms as usize) * 48;
        let total_frames = expected_frames + 32;
        let mut sig = vec![0.0f32; total_frames * 2];
        sig[0] = 0.75;
        sig[1] = -0.5;
        delay.process_in_place(&mut sig);

        for f in 0..total_frames {
            let l = sig[f * 2];
            let r = sig[f * 2 + 1];
            if f == expected_frames {
                assert!((l - 0.75).abs() < 1e-6 && (r - (-0.5)).abs() < 1e-6);
            } else {
                assert!(l.abs() < 1e-6 && r.abs() < 1e-6);
            }
        }
    }
}

#[test]
fn test_property_resampler_arbitrary_rates_and_chunk_sizes() {
    use core_audio::resampler::PolyphaseResampler;

    let mut rng = SplitMix64(0x4800_0441_0096_0000);
    let sample_rates = [16_000u32, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000];

    for &src_rate in &sample_rates {
        for &channels in &[1usize, 2usize] {
            let mut resampler = PolyphaseResampler::new(src_rate, channels);
            let total_in_frames = (src_rate as usize) / 2; // 0.5 秒相当
            let mut remaining = total_in_frames;
            let mut frame_cursor = 0usize;
            let mut out = Vec::new();
            let mut total_out_samples = 0usize;

            while remaining > 0 {
                let chunk_frames = (rng.range_u32(1, 512) as usize).min(remaining);
                remaining -= chunk_frames;
                let mut input = Vec::with_capacity(chunk_frames * channels);
                for _ in 0..chunk_frames {
                    let v = (2.0 * std::f32::consts::PI * 440.0 * (frame_cursor as f32)
                        / (src_rate as f32))
                        .sin()
                        * 0.5;
                    frame_cursor += 1;
                    input.push(v);
                    if channels == 2 {
                        input.push(v);
                    }
                }
                out.clear();
                let written = resampler.process(&input, &mut out);
                assert_eq!(written, out.len());
                assert_eq!(written % 2, 0);
                for &s in &out {
                    assert!(s.is_finite() && s.abs() < 0.65);
                }
                total_out_samples += written;
            }

            let total_out_frames = total_out_samples / 2;
            // 0.5秒 = 24,000 フレーム (48kHz) に対してフィルタ群遅延分以内の誤差で収束する性質
            assert!(
                total_out_frames.abs_diff(24_000) <= 32,
                "src_rate={src_rate}, ch={channels}: got {total_out_frames} frames, expected ~24000"
            );
        }
    }
}

#[test]
fn test_resampler_48khz_mono_upmix_fast_path_exact_identity() {
    use core_audio::resampler::PolyphaseResampler;

    let mut resampler = PolyphaseResampler::new(48_000, 1);
    assert!(!resampler.is_passthrough());
    assert!(resampler.is_mono_upmix_only());

    let mono_in: Vec<f32> = (0..480).map(|i| ((i as f32) * 0.01).sin()).collect();
    let mut stereo_out = Vec::new();
    let written = resampler.process(&mono_in, &mut stereo_out);
    assert_eq!(written, mono_in.len() * 2);
    for (i, &m) in mono_in.iter().enumerate() {
        assert_eq!(stereo_out[i * 2], m);
        assert_eq!(stereo_out[i * 2 + 1], m);
    }
}

#[test]
fn test_mixer_click_free_gain_and_mute_dezippering_ramp() {
    let clock = MasterClock::new();
    let mut mixer = AudioMixer::new(clock);

    let (ch, mut prod) = mixer.add_channel(AudioInputConfig {
        name: "ramp-test".to_string(),
        sample_rate: 48_000,
        channels: 2,
        initial_gain: 0.8,
        initial_muted: false,
        initial_delay_ms: 0,
    });

    let dc_block = vec![0.5f32; MIX_BLOCK_FRAMES * 2];
    let mut out = vec![0.0f32; MIX_BLOCK_FRAMES * 2];

    // Block 0: 定常状態 (0.5 * 0.8 = 0.4)
    prod.push_f32(&dc_block);
    mixer.mix_block(&mut out);
    for &s in &out {
        assert!((s - 0.4).abs() < 1e-6);
    }

    // Block 1: ミュート ON -> 0.4 から 0.0 へ 480 フレームかけて滑らかに線形減衰し、ステップ不連続が無いこと
    ch.set_muted(true);
    prod.push_f32(&dc_block);
    let mut prev = 0.4f32;
    let expected_step = 0.4 / (MIX_BLOCK_FRAMES as f32);
    mixer.mix_block(&mut out);
    for frame in out.chunks_exact(2) {
        let l = frame[0];
        assert!((l - frame[1]).abs() < 1e-6);
        let diff = prev - l;
        assert!(
            (diff - expected_step).abs() < 1e-5,
            "step discontinuity detected on mute ramp: prev={prev}, cur={l}, diff={diff}, expected={expected_step}"
        );
        prev = l;
    }
    assert!(out[out.len() - 1].abs() < 1e-6);

    // Block 2: ミュート定常状態 -> 完全 0.0
    prod.push_f32(&dc_block);
    mixer.mix_block(&mut out);
    assert!(out.iter().all(|&s| s == 0.0));

    // Block 3: ミュート解除 & ゲイン変更 (1.2 -> 出力 0.5 * 1.2 = 0.6) -> 0.0 から 0.6 へ線形ランプ
    ch.set_gain_linear(1.2);
    ch.set_muted(false);
    prod.push_f32(&dc_block);
    mixer.mix_block(&mut out);
    let mut prev_unmute = 0.0f32;
    let expected_up_step = 0.6 / (MIX_BLOCK_FRAMES as f32);
    for frame in out.chunks_exact(2) {
        let l = frame[0];
        let diff = l - prev_unmute;
        assert!(
            (diff - expected_up_step).abs() < 1e-5,
            "step discontinuity on unmute ramp: diff={diff}, expected={expected_up_step}"
        );
        prev_unmute = l;
    }
    assert!((out[out.len() - 1] - 0.6).abs() < 1e-5);
}

#[test]
fn test_settled_silent_delay_fast_path_and_wake_recovery() {
    use core_audio::delay::StereoDelayLine;

    let mut delay = StereoDelayLine::new(10); // 10ms = 480 frames
    assert!(delay.is_settled_silent());

    let mut buf = vec![0.0f32; MIX_BLOCK_FRAMES * 2];
    // 初期状態で無音入力なら即座に fast path (true) が返ること
    assert!(delay.process_known_silent_in_place(&mut buf));

    // インパルスを1発入れると settled_silent が解除されること
    buf[0] = 0.9;
    buf[1] = -0.9;
    delay.process_in_place(&mut buf);
    assert!(!delay.is_settled_silent());
    // 10ms ディレイなのでこのブロックの出力はまだ 0.0
    assert!(buf.iter().all(|&s| s == 0.0));

    // 次のブロックは無音入力だが、10ms 前のインパルスが出力されるため fast path には入らない (false)
    let mut next_buf = vec![0.0f32; MIX_BLOCK_FRAMES * 2];
    assert!(!delay.process_known_silent_in_place(&mut next_buf));
    assert!((next_buf[0] - 0.9).abs() < 1e-6);
    assert!((next_buf[1] - (-0.9)).abs() < 1e-6);
}

#[test]
fn test_atomic_peak_stereo_fetch_max_and_db_conversions() {
    use core_audio::meter::{linear_to_db, AtomicPeakStereo};

    let peak = AtomicPeakStereo::new();
    // NaN や負のゼロ、微小値を混ぜても最大振幅が正確に保持されること
    peak.observe(f32::NAN, -0.0);
    peak.observe(0.25, 0.75);
    peak.observe(0.5, 0.1);
    peak.observe(0.125, 0.6);

    let (l_db, r_db) = peak.take_dbfs();
    assert!((l_db - amplitude_to_dbfs(0.5)).abs() < 1e-4);
    assert!((r_db - amplitude_to_dbfs(0.75)).abs() < 1e-4);

    // dB <-> Linear 往復変換の整合性
    for &db in &[-60.0f32, -24.0, -12.0, -6.0, 0.0, 6.0, 12.0] {
        let lin = db_to_linear(db);
        let roundtrip_db = linear_to_db(lin);
        assert!(
            (roundtrip_db - db).abs() < 1e-3,
            "db={db} -> lin={lin} -> roundtrip={roundtrip_db}"
        );
    }
    assert_eq!(db_to_linear(-96.0), 0.0);
    assert_eq!(linear_to_db(0.0), DBFS_FLOOR);
}

#[test]
fn test_push_i16_and_i32_normalization_accuracy() {
    let clock = MasterClock::new();
    let mut mixer = AudioMixer::new(clock);
    let (_ch, mut prod) = mixer.add_channel(AudioInputConfig::default());

    let mut samples_i16 = vec![0i16; MIX_BLOCK_FRAMES * 2];
    samples_i16[0] = -16384; // -0.5 (linear region)
    samples_i16[1] = 16384; // +0.5 (linear region)
    samples_i16[2] = i16::MIN; // -1.0 -> master soft_clip(-1.0)
    assert_eq!(prod.push_i16(&samples_i16), MIX_BLOCK_FRAMES * 2);

    let mut out = vec![0.0f32; MIX_BLOCK_FRAMES * 2];
    mixer.mix_block(&mut out);
    assert!((out[0] - (-0.5)).abs() < 1e-5);
    assert!((out[1] - 0.5).abs() < 1e-5);
    assert!((out[2] - soft_clip(-1.0)).abs() < 1e-5);

    let mut samples_i32 = vec![0i32; MIX_BLOCK_FRAMES * 2];
    samples_i32[0] = -1_073_741_824; // -0.5 (linear region)
    samples_i32[1] = 1_073_741_824; // +0.5 (linear region)
    samples_i32[2] = i32::MIN; // -1.0 -> master soft_clip(-1.0)
    assert_eq!(prod.push_i32(&samples_i32), MIX_BLOCK_FRAMES * 2);

    mixer.mix_block(&mut out);
    assert!((out[0] - (-0.5)).abs() < 1e-5);
    assert!((out[1] - 0.5).abs() < 1e-5);
    assert!((out[2] - soft_clip(-1.0)).abs() < 1e-5);
}
