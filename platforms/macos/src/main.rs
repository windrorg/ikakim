//! `ikakim` macOS ホストバイナリ（Metal ゼロコピー + VideoToolbox + AudioToolbox + Web UI/RTMPS）

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "ikakim macOS host ({} Hz)\nUsage: platform-macos [--probe | [--frames N] [--rtmp URL] [bind_addr]]\n  --frames 0   Run continuously at 30fps until terminated",
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
    use apple_common::cv_metal::{AppleMetalZeroCopyImporter, CVPixelBufferWrapper};
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
        2_500_000,
        30.0,
        TlsRootConfig::System,
    );
    let mut server = EmbeddedServer::start(
        ServerConfig {
            bind_addr,
            meter_interval_ms: 33,
            preview_width: 640,
            preview_height: 360,
            preview_fps: 30,
        },
        bridge.clone(),
    )
    .expect("EmbeddedServer");
    println!("Web UI listening at http://{}", server.local_addr());

    if let Some(ref url) = rtmp_url {
        bridge
            .start_stream(url, None)
            .expect("start initial RTMP stream");
        println!("RTMP stream started -> {}", url);
    }

    let mut comp = GpuCompositor::new(
        gpu,
        CompositorConfig {
            main_width: 1280,
            main_height: 720,
            preview_width: 640,
            preview_height: 360,
            target_fps: 30,
        },
    );

    let mut main_enc = VtH264Encoder::new(1280, 720, 30, 2_500_000).expect("VT main enc");
    let mut prev_enc = VtH264Encoder::new(640, 360, 30, 600_000).expect("VT prev enc");
    let mut aud_enc = AudioToolboxAacEncoder::new(128_000).expect("AAC enc");
    let mut mix_buf = vec![0.0f32; MIX_BLOCK_FRAMES * 2];
    let v_src = VirtualVideoSource::new(1280, 720, VirtualVideoPattern::ColorbarsMotion);
    let mut a_src = VirtualAudioSource::new(
        core_types::clock::MASTER_SAMPLE_RATE as u32,
        VirtualAudioSignal::SineTone {
            freq_hz: 440.0,
            amplitude: 0.15,
        },
    );

    let mut i: u64 = 0;
    let frame_dur = Duration::from_nanos(33_333_333);
    let pace_realtime = max_frames == 0 || rtmp_url.is_some();

    loop {
        if max_frames > 0 && i >= max_frames {
            break;
        }
        let tick_start = Instant::now();
        let pts = PtsUs(i * 33_333);

        let (y, uv) = v_src.render_nv12_bt709(i);
        let pb = CVPixelBufferWrapper::new_nv12(1280, 720, &y, &uv).expect("NV12 PB");
        let mut layer = importer
            .import_to_gpu_layer(&comp, &pb, 0, true, bridge.layer_transform(0))
            .expect("import layer");

        let mut refs = [&mut layer.layer];
        bridge.apply_pending_transforms(&comp, &mut refs);
        comp.render_dual(&[&layer.layer], pts).expect("render");
        let main_rgba = comp
            .readback_texture_rgba(comp.main_texture(), 1280, 720)
            .expect("main rb");
        let prev_rgba = comp
            .readback_texture_rgba(comp.preview_texture(), 640, 360)
            .expect("prev rb");
        main_enc
            .encode_rgba(1280, 720, &main_rgba, pts)
            .expect("encode main");
        prev_enc
            .encode_rgba(640, 360, &prev_rgba, pts)
            .expect("encode prev");

        let blocks = VirtualAudioSource::mix_blocks_for_30fps_frame(i);
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
        i += 1;

        if pace_realtime {
            let elapsed = tick_start.elapsed();
            if elapsed < frame_dur {
                std::thread::sleep(frame_dur - elapsed);
            }
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    let _ = main_enc.flush();
    let _ = prev_enc.flush();
    bridge.stop_all();
    server.stop();
}
