#![cfg(any(target_os = "macos", target_os = "ios"))]

use apple_common::audio_toolbox::{extract_asc_from_esds, AudioToolboxAacEncoder};
use core_audio::mixer::{AudioInputConfig, AudioMixer};
use core_audio::MIX_BLOCK_FRAMES;
use core_types::clock::MasterClock;
use core_types::media::CodecKind;
use core_types::traits::AudioEncoder;
use std::f32::consts::PI;

#[test]
fn test_esds_audio_specific_config_extraction() {
    // 典型的な MPEG-4 ESDS クッキー (tag 0x03 -> 0x04 -> 0x05 [0x11, 0x90])
    let sample_esds = [
        0x03, 0x19, 0x00, 0x01, 0x00, // ES_Descriptor
        0x04, 0x11, 0x40, 0x15, 0x00, 0x06, 0x00, 0x00, 0x01, 0xF4, 0x00, 0x00, 0x01, 0xF4, 0x00,
        0x05, 0x02, 0x11, 0x90, // DecoderSpecificInfo -> 48kHz Stereo AAC-LC
        0x06, 0x01, 0x02,
    ];
    let asc = extract_asc_from_esds(&sample_esds).expect("must extract ASC");
    assert_eq!(asc, vec![0x11, 0x90]);
}

#[test]
fn test_mixer_to_audiotoolbox_aac_lc_pipeline() {
    let clock = MasterClock::new();
    let mut mixer = AudioMixer::new(clock.clone());

    let (ch1, mut prod1) = mixer.add_channel(AudioInputConfig {
        name: "mic-48k".to_string(),
        sample_rate: 48_000,
        channels: 2,
        initial_gain: 0.7,
        initial_muted: false,
        initial_delay_ms: 5,
    });
    let (_ch2, mut prod2) = mixer.add_channel(AudioInputConfig {
        name: "aux-44k".to_string(),
        sample_rate: 44_100,
        channels: 1,
        initial_gain: 0.3,
        initial_muted: false,
        initial_delay_ms: 0,
    });

    // 250ms 分のテストサイン波を入力
    let mut sine_48k = Vec::with_capacity(12_000 * 2);
    for n in 0..12_000 {
        let v = 0.4 * (2.0 * PI * 440.0 * (n as f32 / 48_000.0)).sin();
        sine_48k.push(v);
        sine_48k.push(v);
    }
    prod1.push_f32(&sine_48k);

    let mut sine_44k = Vec::with_capacity(11_025);
    for n in 0..11_025 {
        let v = 0.3 * (2.0 * PI * 880.0 * (n as f32 / 44_100.0)).sin();
        sine_44k.push(v);
    }
    prod2.push_f32(&sine_44k);

    let mut encoder = AudioToolboxAacEncoder::new(128_000).expect("AudioToolbox AAC init");

    // 先頭パケットは AAC Sequence Header (AudioSpecificConfig)
    let seq = encoder
        .pull_packet()
        .expect("pull seq")
        .expect("sequence header present");
    assert_eq!(seq.codec, CodecKind::AacLc);
    assert!(seq.is_sequence_header);
    assert_eq!(seq.data, vec![0x11, 0x90]);

    // 20ブロック (200ms) ミキシングしながらリアルタイムでゲイン・遅延変更
    let mut mix_buf = vec![0.0f32; MIX_BLOCK_FRAMES * 2];
    let mut aac_packets = Vec::new();

    for i in 0..20 {
        if i == 10 {
            ch1.set_gain_db(-3.0);
            ch1.set_delay_ms(12);
        }
        let sample_time = mixer.mix_block(&mut mix_buf);
        encoder
            .push_pcm_f32(&mix_buf, sample_time.to_pts_us())
            .expect("push pcm");

        while let Some(pkt) = encoder.pull_packet().expect("pull aac") {
            aac_packets.push(pkt);
        }
    }

    // 200ms = 9600 samples -> 1024 samples/frame なので 9 個の AAC パケットが出るはず
    assert!(
        aac_packets.len() >= 8,
        "expected >= 8 AAC frames, got {}",
        aac_packets.len()
    );
    for (idx, pkt) in aac_packets.iter().enumerate() {
        assert_eq!(pkt.codec, CodecKind::AacLc);
        assert!(!pkt.is_sequence_header);
        assert!(!pkt.data.is_empty());
        if idx > 0 {
            assert!(pkt.pts > aac_packets[idx - 1].pts);
        }
    }

    let snap = mixer.take_snapshot();
    assert!(snap.master_peak_l_dbfs > -40.0);
    assert!(snap.master_peak_r_dbfs > -40.0);
}
