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
