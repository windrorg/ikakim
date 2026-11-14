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

    fn range_usize(&mut self, min: usize, max_inclusive: usize) -> usize {
        if min >= max_inclusive {
            return min;
        }
        let span = (max_inclusive - min) + 1;
        min + (self.next_u64() as usize % span)
    }
}

#[test]
fn test_property_spsc_random_interleaved_ops_and_conservation() {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // 1. Copy 型スライス・単要素・discard の混合ランダム操作が参照実装 VecDeque と完全一致する性質
    for seed in 1u64..=32 {
        let mut rng = SplitMix64(seed.wrapping_mul(0xDEAD_BEEF_CAFE_BABE));
        let req_cap = rng.range_usize(1, 64);
        let (mut tx, mut rx) = spsc_channel::<u64>(req_cap);
        let cap = tx.capacity();
        assert!(cap.is_power_of_two() && cap >= req_cap);

        let mut oracle = VecDeque::<u64>::with_capacity(cap);
        let mut next_seq = 1u64;

        for _step in 0..500 {
            assert_eq!(rx.available_read(), oracle.len());
            assert_eq!(tx.available_write(), cap - oracle.len());

            match rng.range_usize(0, 4) {
                0 => {
                    // try_push
                    let val = next_seq;
                    next_seq += 1;
                    let res = tx.try_push(val);
                    if oracle.len() < cap {
                        assert_eq!(res, Ok(()));
                        oracle.push_back(val);
                    } else {
                        assert_eq!(res, Err(val));
                    }
                }
                1 => {
                    // push_slice
                    let len = rng.range_usize(0, cap + 8);
                    let items: Vec<u64> = (0..len)
                        .map(|_| {
                            let v = next_seq;
                            next_seq += 1;
                            v
                        })
                        .collect();
                    let pushed = tx.push_slice(&items);
                    let expected = len.min(cap - oracle.len());
                    assert_eq!(pushed, expected);
                    for &v in &items[..pushed] {
                        oracle.push_back(v);
                    }
                }
                2 => {
                    // try_pop
                    let got = rx.try_pop();
                    let exp = oracle.pop_front();
                    assert_eq!(got, exp);
                }
                3 => {
                    // pop_slice
                    let len = rng.range_usize(0, cap + 8);
                    let mut out = vec![0u64; len];
                    let popped = rx.pop_slice(&mut out);
                    let expected = len.min(oracle.len());
                    assert_eq!(popped, expected);
                    for &got in &out[..popped] {
                        assert_eq!(Some(got), oracle.pop_front());
                    }
                }
                _ => {
                    // discard
                    let max_drop = rng.range_usize(0, cap + 4);
                    let dropped = rx.discard(max_drop);
                    let expected = max_drop.min(oracle.len());
                    assert_eq!(dropped, expected);
                    for _ in 0..dropped {
                        oracle.pop_front();
                    }
                }
            }
        }
    }

    // 2. Drop 型における discard / try_pop / チャネル Drop のライフサイクル保存則
    struct TrackedItem(#[allow(dead_code)] u64, Arc<AtomicUsize>);
    impl Drop for TrackedItem {
        fn drop(&mut self) {
            self.1.fetch_add(1, Ordering::Relaxed);
        }
    }

    let live = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    {
        let mut rng = SplitMix64(0x1234_5678_9ABC_DEF0);
        let (mut tx, mut rx) = spsc_channel::<TrackedItem>(16);
        for i in 0..300u64 {
            if rng.range_usize(0, 2) <= 1 {
                let item = TrackedItem(i, Arc::clone(&dropped));
                live.fetch_add(1, Ordering::Relaxed);
                let _ = tx.try_push(item);
            } else if rng.range_usize(0, 1) == 0 {
                let _ = rx.try_pop();
            } else {
                let _ = rx.discard(rng.range_usize(1, 5));
            }
        }
    }
    assert_eq!(
        live.load(Ordering::Relaxed),
        dropped.load(Ordering::Relaxed),
        "all constructed Drop items must be dropped exactly once"
    );
}

#[test]
fn test_property_clock_and_state_machine_invariants() {
    use core_types::clock::PtsUs;

    let mut rng = SplitMix64(0xCAFE_F00D_1234_5678);
    let clock = MasterClock::new();
    let mut prev_pts = PtsUs(0);
    let mut prev_ms = 0u32;
    let mut total_samples = 0u64;

    for _ in 0..500 {
        let delta = rng.range_usize(0, 9600) as u64;
        let now = clock.advance_samples(delta);
        total_samples += delta;
        assert_eq!(now, SampleTime(total_samples));

        let pts = now.to_pts_us();
        let ms = now.to_timestamp_ms().0;
        // 単調非減少性
        assert!(pts >= prev_pts);
        assert!(ms >= prev_ms);

        // 逆変換 `pts.0 * 48_000 / 1_000_000` との誤差が高々 1 サンプル周期以内である性質
        let roundtrip_samples =
            (pts.0 as u128 * MASTER_SAMPLE_RATE as u128).div_ceil(1_000_000);
        assert!((roundtrip_samples as u64).abs_diff(total_samples) <= 1);
        assert_eq!(pts.to_timestamp_ms().0, ms);

        prev_pts = pts;
        prev_ms = ms;
    }

    // ランダムな状態遷移試行において仕様テーブルと transition_to の結果が常に一致する性質
    let all_states = [
        PipelineState::Idle,
        PipelineState::Previewing,
        PipelineState::Streaming,
        PipelineState::Reconnecting,
        PipelineState::Error,
    ];
    let mut sm = StateMachine::new();
    for _ in 0..300 {
        let next = all_states[rng.range_usize(0, all_states.len() - 1)];
        let cur = sm.current();
        let res = sm.transition_to(next);
        if cur == next || next == PipelineState::Error {
            // 同一状態または Error への遷移は常に成功する不変条件
            assert_eq!(res, Ok(next));
        }
        if res.is_ok() {
            assert_eq!(sm.current(), next);
        } else {
            assert_eq!(sm.current(), cur);
        }
    }
}
