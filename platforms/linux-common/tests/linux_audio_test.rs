#![cfg(target_os = "linux")]

use core_audio::mixer::{AudioInputConfig, AudioMixer};
use core_audio::MIX_BLOCK_FRAMES;
use core_types::clock::MasterClock;
use core_types::media::CodecKind;
use core_types::traits::AudioEncoder;
use linux_common::alsa::{AlsaCaptureConfig, AlsaCaptureThread};
use linux_common::fdk_aac::FdkAacEncoder;
use std::f32::consts::PI;
use std::fs::File;
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

#[test]
fn test_mixer_to_fdk_aac_lc_pipeline() {
    let clock = MasterClock::new();
    let mut mixer = AudioMixer::new(clock);

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

    let mut encoder = FdkAacEncoder::new(128_000).expect("FdkAacEncoder init");

    let seq = encoder
        .pull_packet()
        .expect("pull seq")
        .expect("sequence header present");
    assert_eq!(seq.codec, CodecKind::AacLc);
    assert!(seq.is_sequence_header);
    assert!(!seq.data.is_empty());

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

fn has_alsa_loopback() -> bool {
    if let Ok(mut f) = File::open("/proc/asound/cards") {
        let mut s = String::new();
        if f.read_to_string(&mut s).is_ok() {
            return s.contains("Loopback");
        }
    }
    false
}

#[test]
fn test_alsa_snd_aloop_capture_to_mixer_and_fdk_aac() {
    if !has_alsa_loopback() {
        eprintln!("SKIP: snd-aloop kernel module not loaded");
        return;
    }

    let clock = MasterClock::new();
    let mut mixer = AudioMixer::new(clock);
    let (ch, prod) = mixer.add_channel(AudioInputConfig {
        name: "aloop-cap".to_string(),
        sample_rate: 48_000,
        channels: 2,
        initial_gain: 1.0,
        initial_muted: false,
        initial_delay_ms: 4,
    });

    // card 10 (index=10 in setup-linux-vm.sh): device 1 = capture, device 0 = playback
    let mut cap = AlsaCaptureThread::spawn(
        AlsaCaptureConfig {
            device_name: "plughw:10,1".to_string(),
            sample_rate: 48_000,
            channels: 2,
            period_frames: 480,
        },
        prod,
    )
    .expect("spawn ALSA capture on plughw:10,1");

    // aplay で plughw:10,0 へ 400ms 分の矩形/トーン波を送り、ALSAループバック経由で捕捉する
    let mut raw_pcm = Vec::with_capacity(48_000 * 2 * 2 / 2);
    for n in 0..(48_000 / 2) {
        let val = (0.4 * (2.0 * PI * 1000.0 * (n as f32 / 48_000.0)).sin() * 32767.0) as i16;
        raw_pcm.extend_from_slice(&val.to_le_bytes());
        raw_pcm.extend_from_slice(&val.to_le_bytes());
    }

    let mut child = Command::new("aplay")
        .args(["-D", "plughw:10,0", "-f", "S16_LE", "-r", "48000", "-c", "2", "-t", "raw"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn aplay");

    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().expect("aplay stdin");
        let _ = stdin.write_all(&raw_pcm);
    }

    let mut encoder = FdkAacEncoder::new(128_000).expect("FdkAacEncoder init");
    let _seq = encoder.pull_packet().unwrap().unwrap();

    let mut mix_buf = vec![0.0f32; MIX_BLOCK_FRAMES * 2];
    let mut max_peak_dbfs = -96.0f32;
    let mut aac_count = 0usize;

    for _ in 0..35 {
        thread::sleep(Duration::from_millis(10));
        let st = mixer.mix_block(&mut mix_buf);
        encoder.push_pcm_f32(&mix_buf, st.to_pts_us()).unwrap();
        while encoder.pull_packet().unwrap().is_some() {
            aac_count += 1;
        }
        let (pl, _pr) = ch.take_peak_dbfs();
        if pl > max_peak_dbfs {
            max_peak_dbfs = pl;
        }
    }

    let _ = child.wait();
    cap.stop();

    assert!(
        max_peak_dbfs > -30.0,
        "expected captured audio signal from snd-aloop, peak was {} dBFS",
        max_peak_dbfs
    );
    assert!(aac_count >= 10, "expected >= 10 AAC packets, got {}", aac_count);
}
