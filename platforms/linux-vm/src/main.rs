//! `ikakim` Linux VM / Arm64 ヘッドレスホストバイナリ（V4L2 DMA-BUF + Surfaceless EGL/Vulkan + SwH264 + FDK-AAC + Web UI/RTMPS）

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "ikakim Linux VM headless host ({} Hz)\nUsage: platform-linux-vm [--probe | --frames N [bind_addr]]",
            core_types::clock::MASTER_SAMPLE_RATE
        );
        return;
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = &args;
        println!("platform-linux-vm requires Linux runtime");
    }

    #[cfg(target_os = "linux")]
    {
        linux_main(&args);
    }
}

#[cfg(target_os = "linux")]
fn linux_main(args: &[String]) {
    use core_audio::mixer::{AudioInputConfig, AudioMixer};
    use core_audio::MIX_BLOCK_FRAMES;
    use core_engine::pipeline::{flush_encoders_to_outputs, EngineControlBridge, PipelineTickContext};
    use core_engine::SwH264Encoder;
    use core_gpu::{CompositorConfig, GpuCompositor, GpuContext};
    use core_net_ui::{EmbeddedServer, ServerConfig};
    use core_rtmp::TlsRootConfig;
    use core_types::clock::{MasterClock, PtsUs};
    use core_types::media::{LayerTransform, PixelFormat};
    use core_types::traits::AudioEncoder;
    use linux_common::dmabuf_gpu::LinuxDmaBufImporter;
    use linux_common::fdk_aac::FdkAacEncoder;
    use linux_common::v4l2::{list_capture_devices, V4l2CaptureDevice};
    use std::time::Duration;

    let probe_only = args.iter().any(|a| a == "--probe");
    let devices = list_capture_devices();
    let gpu = GpuContext::new_headless().expect("Linux Vulkan/GLES GpuContext");
    let importer = LinuxDmaBufImporter::new(&gpu.device);

    println!(
        "ikakim Linux host: GPU={} ({}) | surfaceless_egl={} | v4l2_devices={}",
        gpu.adapter_name,
        gpu.backend_name,
        importer.has_surfaceless_egl(),
        devices.len()
    );
    for d in &devices {
        println!("  v4l2: {} (driver={}, card={})", d.path, d.driver, d.card);
    }
    if probe_only || args.len() == 1 {
        return;
    }

    let max_frames: u64 = args
        .windows(2)
        .find(|w| w[0] == "--frames")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(5);
    let bind_addr = args
        .iter()
        .skip(1)
        .find(|a| a.contains(':') && !a.starts_with('-'))
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:0".to_string());

    let mut clock = MasterClock::new();
    let mut mixer = AudioMixer::new(clock.clone());
    let (ch0, _prod0) = mixer.add_channel(AudioInputConfig {
        name: "alsa-mic".into(),
        sample_rate: core_types::clock::MASTER_SAMPLE_RATE as u32,
        channels: 2,
        initial_gain: 1.0,
        initial_muted: false,
        initial_delay_ms: 0,
    });
    let bridge = EngineControlBridge::new(
        vec![ch0],
        mixer.master_handle(),
        1_500_000,
        30.0,
        TlsRootConfig::System,
    );
    let mut server = EmbeddedServer::start(
        ServerConfig {
            bind_addr,
            meter_interval_ms: 33,
            preview_width: 320,
            preview_height: 180,
            preview_fps: 30,
        },
        bridge.clone(),
    )
    .expect("EmbeddedServer");
    println!("Web UI listening at http://{}", server.local_addr());

    let dev_path = devices
        .first()
        .map(|d| d.path.as_str())
        .unwrap_or("/dev/video0");
    let mut cap = V4l2CaptureDevice::open(dev_path, 640, 360, PixelFormat::Yuyv, 4).expect("V4L2");
    let mut comp = GpuCompositor::new(
        gpu,
        CompositorConfig {
            main_width: 640,
            main_height: 360,
            preview_width: 320,
            preview_height: 180,
            target_fps: 30,
        },
    );
    let mut main_enc = SwH264Encoder::new(640, 360, 1_500_000);
    let mut prev_enc = SwH264Encoder::new(320, 180, 400_000);
    let mut aud_enc = FdkAacEncoder::new(128_000).expect("FdkAacEncoder");
    let mut mix_buf = vec![0.0f32; MIX_BLOCK_FRAMES * 2];

    for i in 0..max_frames {
        let pts = PtsUs(i * 33_333);
        let vf = cap.capture_frame(pts, 2000).expect("V4L2 capture");
        let mut layer = importer
            .import_frame_to_layer(&comp, &vf, 0, true, LayerTransform::default())
            .expect("import DMA-BUF");
        let mut refs = [&mut layer.layer];
        bridge.apply_pending_transforms(&comp, &mut refs);
        comp.render_dual(&[&layer.layer], pts).expect("render");
        let main_rgba = comp
            .readback_texture_rgba(comp.main_texture(), 640, 360)
            .expect("main rb");
        let prev_rgba = comp
            .readback_texture_rgba(comp.preview_texture(), 320, 180)
            .expect("prev rb");
        main_enc.encode_rgba(&main_rgba, pts).expect("encode main");
        prev_enc.encode_rgba(&prev_rgba, pts).expect("encode prev");
        let st = mixer.mix_block(&mut mix_buf);
        aud_enc
            .push_pcm_f32(&mix_buf, st.to_pts_us())
            .expect("encode audio");

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
        std::thread::sleep(Duration::from_millis(5));
    }

    bridge.stop_all();
    server.stop();
}
