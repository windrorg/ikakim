//! `ikakim` macOS ホストバイナリ（Metal ゼロコピー + VideoToolbox + AudioToolbox + Web UI/RTMPS）

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "ikakim macOS host ({} Hz)\nUsage: platform-macos [--probe | [--frames N] [--width W] [--height H] [--fps F] [--rtmp URL] [bind_addr]]\n  Defaults: 1920x1080 @ 60fps zero-copy Metal->IOSurface->VideoToolbox\n  --frames 0   Run continuously until terminated",
            core_types::clock::MASTER_SAMPLE_RATE
        );
        return;
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = &args;
        println!("platform-macos requires macOS runtime");
    }

    #[cfg(target_os = "macos")]
    {
        macos_main(&args);
    }
}

#[cfg(target_os = "macos")]
fn macos_main(args: &[String]) {
    use apple_common::audio_toolbox::AudioToolboxAacEncoder;
    use apple_common::av_capture::list_video_devices;
    use apple_common::cv_metal::{
        AppleMetalZeroCopyImporter, AppleZeroCopyRenderSurfacePool, CVPixelBufferWrapper,
    };
    use apple_common::video_toolbox::VtH264Encoder;
    use core_audio::mixer::{AudioInputConfig, AudioMixer};
    use core_audio::MIX_BLOCK_FRAMES;
    use core_engine::pipeline::{flush_encoders_to_outputs, EngineControlBridge, PipelineTickContext};
    use core_engine::{VirtualAudioSignal, VirtualAudioSource, VirtualVideoPattern, VirtualVideoSource};
    use core_gpu::{CompositorConfig, GpuCompositor, GpuContext};
    use core_net_ui::rpc::ControlBackend;
    use core_net_ui::{EmbeddedServer, ServerConfig};
    use core_rtmp::TlsRootConfig;
    use core_types::clock::{MasterClock, PtsUs};
    use core_types::traits::AudioEncoder;
    use std::time::{Duration, Instant};

    let probe_only = args.iter().any(|a| a == "--probe");
    let devices = list_video_devices();
    let gpu = GpuContext::new_headless().expect("Metal GpuContext");
    let importer = AppleMetalZeroCopyImporter::new(&gpu.device).expect("CVMetalTextureCache");

    println!(
        "ikakim macOS host: GPU={} ({}) | cameras={} | audio={}Hz",
        gpu.adapter_name,
        gpu.backend_name,
        devices.len(),
        core_types::clock::MASTER_SAMPLE_RATE
    );
    for d in &devices {
        println!("  camera: {} ({})", d.localized_name, d.unique_id);
    }
    if probe_only || args.len() == 1 {
        return;
    }

    let max_frames: u64 = args
        .windows(2)
        .find(|w| w[0] == "--frames")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(10);
    let width: u32 = args
        .windows(2)
        .find(|w| w[0] == "--width")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(1920);
    let height: u32 = args
        .windows(2)
        .find(|w| w[0] == "--height")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(1080);
    let fps: u32 = args
        .windows(2)
        .find(|w| w[0] == "--fps")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(60)
        .max(1);
    let prev_width = (width / 2).max(16);
    let prev_height = (height / 2).max(16);
    let initial_bitrate_bps: u32 = if width >= 1920 && fps >= 60 {
        6_000_000
    } else if width >= 1920 {
        4_500_000
    } else {
        2_500_000
    };
    let rtmp_url: Option<String> = args
        .windows(2)
        .find(|w| w[0] == "--rtmp")
        .map(|w| w[1].clone());
    let bind_addr = args
        .iter()
        .skip(1)
        .find(|a| a.contains(':') && !a.starts_with('-') && !a.starts_with("rtmp://") && !a.starts_with("rtmps://"))
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:0".to_string());

    let mut clock = MasterClock::new();
    let mut mixer = AudioMixer::new(clock.clone());
    let (ch0, mut prod0) = mixer.add_channel(AudioInputConfig {
        name: "mac-mic".into(),
        sample_rate: core_types::clock::MASTER_SAMPLE_RATE as u32,
        channels: 2,
        initial_gain: 0.7,
        initial_muted: false,
        initial_delay_ms: 0,
    });
    let bridge = EngineControlBridge::new(
        vec![ch0],
        mixer.master_handle(),
        initial_bitrate_bps,
        fps as f32,
        TlsRootConfig::System,
    );
    let mut server = EmbeddedServer::start(
        ServerConfig {
            bind_addr,
            meter_interval_ms: 33,
            preview_width: prev_width,
            preview_height: prev_height,
            preview_fps: fps.min(30),
        },
        bridge.clone(),
    )
    .expect("EmbeddedServer");
    println!(
        "Web UI listening at http://{} | main={}x{}@{}fps | prev={}x{}",
        server.local_addr(),
        width,
        height,
        fps,
        prev_width,
        prev_height
    );

    if let Some(ref url) = rtmp_url {
        bridge
            .start_stream(url, Some(initial_bitrate_bps))
            .expect("start initial RTMP stream");
        println!("RTMP stream started -> {}", url);
    }

    let mut main_pool =
        AppleZeroCopyRenderSurfacePool::new(&importer, &gpu.device, width, height, 6)
            .expect("Main IOSurface render pool");
    let mut prev_pool =
        AppleZeroCopyRenderSurfacePool::new(&importer, &gpu.device, prev_width, prev_height, 6)
            .expect("Preview IOSurface render pool");

    let mut comp = GpuCompositor::new(
        gpu,
        CompositorConfig {
            main_width: width,
            main_height: height,
            preview_width: prev_width,
            preview_height: prev_height,
            target_fps: fps,
        },
    );

    let mut main_enc =
        VtH264Encoder::new(width, height, fps, initial_bitrate_bps).expect("VT main enc");
    let mut prev_enc =
        VtH264Encoder::new(prev_width, prev_height, fps.min(30), 800_000).expect("VT prev enc");
    let mut aud_enc = AudioToolboxAacEncoder::new(128_000).expect("AAC enc");
    let mut mix_buf = vec![0.0f32; MIX_BLOCK_FRAMES * 2];

    // Pre-allocate IOSurface-backed camera ring so zero CPU pixel synthesis pollutes 60fps loop timing
    let v_src = VirtualVideoSource::new(width, height, VirtualVideoPattern::ColorbarsMotion);
    let ring_len = if max_frames > 0 {
        (max_frames as usize).min(12)
    } else {
        12
    };
    let input_pbs: Vec<CVPixelBufferWrapper> = (0..ring_len)
        .map(|k| {
            let (y, uv) = v_src.render_nv12_bt709(k as u64);
            CVPixelBufferWrapper::new_nv12(width, height, &y, &uv).expect("NV12 PB ring")
        })
        .collect();

    let mut a_src = VirtualAudioSource::new(
        core_types::clock::MASTER_SAMPLE_RATE as u32,
        VirtualAudioSignal::SineTone {
            freq_hz: 440.0,
            amplitude: 0.15,
        },
    );

    let mut i: u64 = 0;
    let frame_dur = Duration::from_nanos(1_000_000_000u64 / fps as u64);
    let pace_realtime = max_frames == 0 || rtmp_url.is_some();
    let mut frame_us_samples: Vec<u64> = Vec::with_capacity(max_frames.min(1024) as usize);

    loop {
        if max_frames > 0 && i >= max_frames {
            break;
        }
        let tick_start = Instant::now();
        let pts = PtsUs((i * 1_000_000) / fps as u64);

        let pb = &input_pbs[(i as usize) % input_pbs.len()];
        let mut layer = importer
            .import_to_gpu_layer(&comp, pb, 0, true, bridge.layer_transform(0))
            .expect("import layer");

        let mut refs = [&mut layer.layer];
        bridge.apply_pending_transforms(&comp, &mut refs);
        {
            let device = &comp.context().device;
            let main_slot = main_pool.acquire_slot(&importer, device).expect("main slot");
            let prev_slot = prev_pool.acquire_slot(&importer, device).expect("prev slot");
            comp.render_dual_into(
                &[&layer.layer],
                pts,
                Some(main_slot.as_external_target()),
                Some(prev_slot.as_external_target()),
            )
            .expect("render zero-copy");
            main_enc
                .encode_pixel_buffer(&main_slot.pixel_buffer, pts)
                .expect("encode main zero-copy");
            prev_enc
                .encode_pixel_buffer(&prev_slot.pixel_buffer, pts)
                .expect("encode prev zero-copy");
        }

        let blocks = VirtualAudioSource::mix_blocks_for_fps_frame(fps, i);
        for _ in 0..blocks {
            let pcm = a_src.next_stereo_f32(MIX_BLOCK_FRAMES);
            prod0.push_f32(&pcm);
            let st = mixer.mix_block(&mut mix_buf);
            aud_enc
                .push_pcm_f32(&mix_buf, st.to_pts_us())
                .expect("encode audio");
        }

        let mut tick = PipelineTickContext {
            clock: &mut clock,
            bridge: &bridge,
            compositor: &mut comp,
            main_encoder: &mut main_enc,
            preview_encoder: &mut prev_enc,
            audio_encoder: &mut aud_enc,
            server: &server,
        };
        flush_encoders_to_outputs(&mut tick, pts).expect("flush");
        let work_us = tick_start.elapsed().as_micros() as u64;
        if frame_us_samples.len() < 4096 {
            frame_us_samples.push(work_us);
        }
        i += 1;

        if pace_realtime {
            let elapsed = tick_start.elapsed();
            if elapsed < frame_dur {
                std::thread::sleep(frame_dur - elapsed);
            }
        }
    }

    if !frame_us_samples.is_empty() {
        let mut sorted = frame_us_samples.clone();
        sorted.sort_unstable();
        let avg_ms = (frame_us_samples.iter().sum::<u64>() as f64)
            / (frame_us_samples.len() as f64 * 1000.0);
        let p95_ms = (sorted[(sorted.len() * 95) / 100] as f64) / 1000.0;
        println!(
            "ikakim zero-copy summary: frames={} | avg={:.2}ms | p95={:.2}ms | main_pool(alloc={}, reuse={}) | prev_pool(alloc={}, reuse={})",
            i,
            avg_ms,
            p95_ms,
            main_pool.allocations_total(),
            main_pool.reuses_total(),
            prev_pool.allocations_total(),
            prev_pool.reuses_total()
        );
    }

    let _ = main_enc.flush();
    let _ = prev_enc.flush();
    bridge.stop_all();
    server.stop();
}
