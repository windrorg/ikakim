//! `ikakim` Linux arm64 実機ターゲットバイナリ (Rock 5B RK3588 / Amlogic S905X2 / Headless Soak)

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "ikakim Linux arm64 runtime (RK3588 MPP / Amlogic S905X2 V4L2-M2M)\n\
             Usage: platform-linux-arm64 [--probe | --soak --frames N [--width W --height H --fps F] [bind_addr]]"
        );
        return;
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = &args;
        println!("platform-linux-arm64 requires Linux runtime");
    }

    #[cfg(target_os = "linux")]
    {
        arm64_main(&args);
    }
}

#[cfg(target_os = "linux")]
fn arm64_main(args: &[String]) {
    use core_audio::mixer::{AudioInputConfig, AudioMixer};
    use core_audio::MIX_BLOCK_FRAMES;
    use core_engine::pipeline::{flush_encoders_to_outputs, EngineControlBridge, PipelineTickContext};
    use core_gpu::{CompositorConfig, GpuCompositor, GpuContext};
    use core_net_ui::{EmbeddedServer, ServerConfig};
    use core_rtmp::{AbrConfig, AimdAbrController, TlsRootConfig};
    use core_types::clock::{MasterClock, PtsUs};
    use core_types::media::{LayerTransform, NativeBufferHandle, PixelFormat, VideoFrame};
    use core_types::traits::AudioEncoder;
    use linux_common::codec_factory::{sync_abr_step, HardwareProbeReport, LinuxVideoEncoder};
    use linux_common::dmabuf_gpu::LinuxDmaBufImporter;
    use linux_common::drm_gbm::list_drm_nodes;
    use linux_common::fdk_aac::FdkAacEncoder;
    use linux_common::v4l2::{enumerate_capture_formats, list_capture_devices, V4l2CaptureDevice};
    use linux_common::v4l2_m2m::{fourcc_to_string, list_m2m_codecs};
    use std::fs;
    use std::time::Instant;

    let probe_report = HardwareProbeReport::detect();
    let capture_devs = list_capture_devices();
    let m2m_codecs = list_m2m_codecs();
    let drm_nodes = list_drm_nodes();
    let gpu = GpuContext::new_headless().expect("Linux GPU context");
    let importer = LinuxDmaBufImporter::new(&gpu.device);

    println!(
        "ikakim arm64 probe: GPU={} ({}) | selected_codec={:?} | mpp_dev={} libmpp={}",
        gpu.adapter_name,
        gpu.backend_name,
        probe_report.selected_backend,
        probe_report.mpp.has_mpp_device,
        probe_report.mpp.has_librockchip_mpp
    );
    if let Some(egl) = importer.surfaceless_egl() {
        let gles_ver = egl.gles_version();
        let essl3 = egl.verify_gles32_external_shader().unwrap_or(false);
        println!("  EGL Surfaceless: {} | external_essl3={}", gles_ver, essl3);
    }
    for d in &capture_devs {
        let fmts = enumerate_capture_formats(&d.path).unwrap_or_default();
        let names: Vec<String> = fmts.iter().map(|f| f.fourcc_str.clone()).collect();
        println!("  V4L2 Capture: {} ({} / {}) formats={:?}", d.path, d.driver, d.card, names);
    }
    for c in &m2m_codecs {
        let outs: Vec<String> = c.output_fourccs.iter().map(|&f| fourcc_to_string(f)).collect();
        let caps: Vec<String> = c.capture_fourccs.iter().map(|&f| fourcc_to_string(f)).collect();
        println!(
            "  V4L2 M2M: {} ({}) mplane={} out={:?} cap={:?}",
            c.path, c.driver, c.is_mplane, outs, caps
        );
    }
    for n in &drm_nodes {
        println!("  DRM Node: {} (accessible={})", n.path, n.accessible);
    }

    let soak_mode = args.iter().any(|a| a == "--soak");
    if !soak_mode {
        return;
    }

    let width: u32 = parse_flag(args, "--width").unwrap_or(1920);
    let height: u32 = parse_flag(args, "--height").unwrap_or(1080);
    let fps: u32 = parse_flag(args, "--fps").unwrap_or(60);
    let frames: u64 = parse_flag(args, "--frames").unwrap_or(60);
    let bind_addr = args
        .iter()
        .skip(1)
        .find(|a| a.contains(':') && !a.starts_with('-'))
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:0".to_string());

    let mut clock = MasterClock::new();
    let mut mixer = AudioMixer::new(clock.clone());
    let (ch0, _prod0) = mixer.add_channel(AudioInputConfig {
        name: "arm64-mic".into(),
        sample_rate: core_types::clock::MASTER_SAMPLE_RATE as u32,
        channels: 2,
        initial_gain: 1.0,
        initial_muted: false,
        initial_delay_ms: 0,
    });
    let bridge = EngineControlBridge::new(
        vec![ch0],
        mixer.master_handle(),
        4_000_000,
        fps as f32,
        TlsRootConfig::System,
    );
    let mut server = EmbeddedServer::start(
        ServerConfig {
            bind_addr,
            meter_interval_ms: 33,
            preview_width: (width / 2).max(320),
            preview_height: (height / 2).max(180),
            preview_fps: 30,
        },
        bridge.clone(),
    )
    .expect("EmbeddedServer");

    let mut comp = GpuCompositor::new(
        gpu,
        CompositorConfig {
            main_width: width,
            main_height: height,
            preview_width: (width / 2).max(320),
            preview_height: (height / 2).max(180),
            target_fps: fps,
        },
    );

    // UVC / MIPI-CSI キャプチャがあれば開く、無ければゼロコピー互換 CpuFallback 合成フレームで負荷テスト
    let mut v4l2_cap = capture_devs
        .first()
        .and_then(|d| V4l2CaptureDevice::open(&d.path, width.min(640), height.min(360), PixelFormat::Yuyv, 4).ok());

    let mut main_enc = LinuxVideoEncoder::open_best_available(width, height, fps, 4_000_000);
    let mut prev_enc =
        LinuxVideoEncoder::open_best_available((width / 2).max(320), (height / 2).max(180), 30, 800_000);
    let mut aud_enc = FdkAacEncoder::new(128_000).expect("FdkAacEncoder");
    let mut abr = AimdAbrController::new(AbrConfig::default());

    let fd_before = fs::read_dir("/proc/self/fd").map(|d| d.count()).unwrap_or(0);
    let rss_before = read_rss_pages();
    let started = Instant::now();
    let fallback_frame = VideoFrame {
        width,
        height,
        format: PixelFormat::Yuyv,
        pts: PtsUs(0),
        handle: NativeBufferHandle::CpuFallback {
            data: vec![0x80u8; (width as usize) * (height as usize) * 2],
            stride: width * 2,
        },
    };
    let static_fallback_imported = if v4l2_cap.is_none() {
        Some(
            importer
                .import_frame_to_layer(&comp, &fallback_frame, 0, true, LayerTransform::default())
                .expect("import static fallback frame"),
        )
    } else {
        None
    };
    let mut mix_buf = vec![0.0f32; MIX_BLOCK_FRAMES * 2];

    for i in 0..frames {
        let pts = PtsUs(i * (1_000_000 / u64::from(fps.max(1))));
        if let Some(ref mut dev) = v4l2_cap {
            let cam_frame = dev.capture_frame(pts, 2000).expect("V4L2 capture");
            let imported = importer
                .import_frame_to_layer(&comp, &cam_frame, 0, true, LayerTransform::default())
                .expect("import frame");
            comp.render_dual(&[&imported.layer], pts)
                .expect("render_dual");
        } else if let Some(ref imported) = static_fallback_imported {
            comp.render_dual(&[&imported.layer], pts)
                .expect("render_dual");
        }
        main_enc
            .encode_compositor_target(&comp, comp.main_texture(), None, width, height, pts)
            .expect("encode main");
        prev_enc
            .encode_compositor_target(
                &comp,
                comp.preview_texture(),
                None,
                (width / 2).max(320),
                (height / 2).max(180),
                pts,
            )
            .expect("encode preview");

        let st = mixer.mix_block(&mut mix_buf);
        aud_enc
            .push_pcm_f32(&mix_buf, st.to_pts_us())
            .expect("encode audio");

        // 定期 ABR シミュレーション（疑似滞留なし 200ms 周期）
        if i % 10 == 5 {
            let _ = sync_abr_step(&mut abr, &bridge, &mut main_enc, 0, 200);
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
    }

    let elapsed = started.elapsed();
    let fd_after = fs::read_dir("/proc/self/fd").map(|d| d.count()).unwrap_or(0);
    let rss_after = read_rss_pages();
    println!(
        "soak complete: {} frames ({}x{}@{}fps) in {:?} | encoder={:?} | fds: {} -> {} | rss_pages: {} -> {}",
        frames, width, height, fps, elapsed, main_enc.backend_kind(), fd_before, fd_after, rss_before, rss_after
    );

    bridge.stop_all();
    server.stop();
}

#[cfg(target_os = "linux")]
fn parse_flag<T: std::str::FromStr>(args: &[String], flag: &str) -> Option<T> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .and_then(|w| w[1].parse().ok())
}

#[cfg(target_os = "linux")]
fn read_rss_pages() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
        .unwrap_or(0)
}
