use core_types::clock::{MasterClock, SampleTime, MASTER_SAMPLE_RATE};
use core_types::ring_buffer::spsc_channel;
use core_types::state::{PipelineState, StateMachine};
use std::thread;

#[test]
fn test_spsc_multithreaded_integrity() {
    const TOTAL_ITEMS: usize = 100_000;
    let (mut prod, mut cons) = spsc_channel::<usize>(1024);

    let producer_handle = thread::spawn(move || {
        for i in 0..TOTAL_ITEMS {
            let mut val = i;
            loop {
                match prod.try_push(val) {
                    Ok(()) => break,
                    Err(returned) => {
                        val = returned;
                        std::hint::spin_loop();
                    }
                }
            }
        }
    });

    let consumer_handle = thread::spawn(move || {
        let mut expected = 0;
        while expected < TOTAL_ITEMS {
            if let Some(v) = cons.try_pop() {
                assert_eq!(v, expected);
                expected += 1;
            } else {
                std::hint::spin_loop();
            }
        }
    });

    producer_handle.join().unwrap();
    consumer_handle.join().unwrap();
}

#[test]
fn test_spsc_audio_slice_wraparound() {
    let (mut prod, mut cons) = spsc_channel::<f32>(8);
    assert_eq!(prod.push_slice(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 6);

    let mut out = [0.0f32; 4];
    assert_eq!(cons.pop_slice(&mut out), 4);
    assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);

    // リング境界を跨ぐ書き込み・読み出しの検証
    assert_eq!(prod.push_slice(&[7.0, 8.0, 9.0, 10.0, 11.0]), 5);

    let mut out2 = [0.0f32; 7];
    assert_eq!(cons.pop_slice(&mut out2), 7);
    assert_eq!(out2, [5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0]);
}

#[test]
fn test_master_clock_48khz_precision() {
    let clock = MasterClock::new();
    // 10ms 分のフレーム（480 samples）を 100 回進める = 1.0 秒
    for _ in 0..100 {
        clock.advance_samples(480);
    }
    assert_eq!(clock.now_samples(), SampleTime(MASTER_SAMPLE_RATE));
    assert_eq!(clock.now_pts_us().0, 1_000_000);
    assert_eq!(clock.now_samples().to_timestamp_ms().0, 1_000);

    // 24時間連続稼働相当のサンプル数でもオーバーフローしないことを確認
    let samples_24h = SampleTime(MASTER_SAMPLE_RATE * 86_400);
    assert_eq!(samples_24h.to_pts_us().0, 86_400_000_000);
    assert_eq!(samples_24h.to_timestamp_ms().0, 86_400_000);
}

#[test]
fn test_pipeline_state_machine() {
    let mut sm = StateMachine::new();
    assert_eq!(sm.current(), PipelineState::Idle);

    // Idle から直接 Streaming への遷移は禁止
    assert!(sm.transition_to(PipelineState::Streaming).is_err());

    // 正常遷移: Idle -> Previewing -> Streaming -> Reconnecting -> Streaming
    assert!(sm.transition_to(PipelineState::Previewing).is_ok());
    assert!(sm.transition_to(PipelineState::Streaming).is_ok());
    assert!(sm.transition_to(PipelineState::Reconnecting).is_ok());
    assert!(sm.transition_to(PipelineState::Streaming).is_ok());
}
