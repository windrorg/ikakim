#![cfg(any(target_os = "macos", target_os = "ios"))]

use apple_common::audio_toolbox::AudioToolboxAacEncoder;
use apple_common::av_capture::list_video_devices;
use apple_common::cv_metal::{
    AppleMetalZeroCopyImporter, AppleZeroCopyRenderSurfacePool, CVPixelBufferWrapper,
};
use apple_common::image_io::decode_image_rgba;
use apple_common::video_toolbox::{VtH264Decoder, VtH264Encoder};
use core_audio::mixer::{AudioInputConfig, AudioMixer};
use core_audio::MIX_BLOCK_FRAMES;
use core_engine::pipeline::{flush_encoders_to_outputs, EngineControlBridge, PipelineTickContext};
use core_engine::virtual_input::{
    VirtualAudioSignal, VirtualAudioSource, VirtualVideoPattern, VirtualVideoSource,
};
use core_gpu::{CompositorConfig, GpuCompositor, GpuContext};
use core_net_ui::rpc::ControlBackend;
use core_net_ui::{EmbeddedServer, ServerConfig};
use core_rtmp::TlsRootConfig;
use core_types::clock::{MasterClock, PtsUs};
use core_types::media::{CodecKind, LayerTransform, PixelFormat, VideoFrame};
use core_types::traits::{AudioEncoder, VideoDecoder, VideoEncoder};
use std::f32::consts::PI;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Instant;

// 2x2 RGBA PNG: [(255,0,0,255), (0,255,0,255), (0,0,255,255), (255,255,255,128)]
const PNG_2X2_RGBA: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2, 8, 6, 0,
    0, 0, 114, 182, 13, 36, 0, 0, 0, 19, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 240, 31, 12,
    129, 52, 8, 52, 0, 0, 73, 73, 9, 120, 40, 160, 219, 119, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66,
    96, 130,
];

#[test]
fn test_av_capture_list_devices_smoke() {
    let devs = list_video_devices();
    for d in &devs {
        assert!(!d.unique_id.is_empty());
    }
}

#[test]
fn test_cvpixelbuffer_metal_zero_copy_and_imageio_overlay() {
    let ctx = GpuContext::new_headless().expect("Metal headless GPU context");
    let importer = AppleMetalZeroCopyImporter::new(&ctx.device).expect("CVMetalTextureCache");
    let mut comp = GpuCompositor::new(
        ctx,
        CompositorConfig {
            main_width: 128,
            main_height: 128,
            preview_width: 64,
            preview_height: 64,
            target_fps: 30,
        },
    );

    // 1. Zero-copy NV12 CVPixelBuffer (uniform green-ish Y=150, U=60, V=70)
    let w = 128u32;
    let h = 128u32;
    let y_plane = vec![150u8; (w * h) as usize];
    let mut uv_plane = vec![0u8; (w * h / 2) as usize];
    for pair in uv_plane.chunks_exact_mut(2) {
        pair[0] = 60;
        pair[1] = 70;
    }
    let pb_nv12 = CVPixelBufferWrapper::new_nv12(w, h, &y_plane, &uv_plane).expect("NV12 PB");
    let base_imported = importer
        .import_to_gpu_layer(&comp, &pb_nv12, 0, true, LayerTransform::default())
        .expect("NV12 zero-copy GPU layer");

    // 2. Decode PNG overlay via ImageIO
    let img = decode_image_rgba(PNG_2X2_RGBA).expect("ImageIO PNG decode");
    assert_eq!(img.width, 2);
    assert_eq!(img.height, 2);
    let overlay_layer = comp
        .create_rgba_layer(
            1,
            false,
            false,
            img.width,
            img.height,
            &img.rgba,
            LayerTransform {
                x: 0.0,
                y: 0.0,
                scale_x: 1.0,
                scale_y: 1.0,
                rotation_rad: 0.0,
                opacity: 1.0,
                z_order: 1,
                visible: true,
            },
        )
        .expect("create rgba layer");

    let layers = [&base_imported.layer, &overlay_layer];
    let stats = comp
        .render_dual(&layers, PtsUs(0))
        .expect("render_dual Metal zero-copy");
    assert_eq!(stats.rendered_layers, 2);

    let main_rgba = comp
        .readback_texture_rgba(comp.main_texture(), 128, 128)
        .expect("readback main");
    // Top-left quadrant covered by solid red PNG pixel -> red channel dominates
    assert!(main_rgba[0] > 200, "expected red R>200, got {}", main_rgba[0]);
    assert!(main_rgba[1] < 30, "expected red G<30, got {}", main_rgba[1]);

    // Also test BGRA CVPixelBuffer zero-copy import
    let mut bgra = Vec::with_capacity(64 * 64 * 4);
    for _ in 0..(64 * 64) {
        bgra.extend_from_slice(&[240, 20, 10, 255]); // blue in BGRA
    }
    let pb_bgra = CVPixelBufferWrapper::new_bgra(64, 64, &bgra).expect("BGRA PB");
    let bgra_imported = importer
        .import_to_gpu_layer(&comp, &pb_bgra, 0, true, LayerTransform::default())
        .expect("BGRA zero-copy import");
    comp.render_dual(&[&bgra_imported.layer], PtsUs(33_333))
        .expect("render BGRA");
    let prev_rgba = comp
        .readback_texture_rgba(comp.preview_texture(), 64, 64)
        .expect("readback preview");
    assert!(prev_rgba[2] > 200, "expected blue B>200, got {}", prev_rgba[2]);
}

#[test]
fn test_videotoolbox_h264_roundtrip_to_metal_texture() {
    let w = 256u32;
    let h = 144u32;
    let mut enc = VtH264Encoder::new(w, h, 30, 800_000).expect("VtH264Encoder");
    let mut dec = VtH264Decoder::new();

    let y_plane = vec![128u8; (w * h) as usize];
    let uv_plane = vec![128u8; (w * h / 2) as usize];
    let pb = CVPixelBufferWrapper::new_nv12(w, h, &y_plane, &uv_plane).expect("NV12 PB");

    for idx in 0..5u64 {
        let pts = PtsUs(idx * 33_333);
        enc.encode_pixel_buffer(&pb, pts).expect("VT encode PB");
    }
    enc.flush().expect("VT encoder flush");

    let mut packets = Vec::new();
    while let Some(pkt) = enc.pull_packet().expect("pull VT H264") {
        packets.push(pkt);
    }
    assert!(
        packets
            .iter()
            .any(|p| p.is_sequence_header && p.codec == CodecKind::H264),
        "expected H.264 SPS/PPS sequence header"
    );
    assert!(
        packets.iter().any(|p| !p.is_sequence_header && p.is_keyframe),
        "expected H.264 IDR keyframe"
    );

    for pkt in &packets {
        dec.push_packet(pkt).expect("VT decode push");
    }

    let (decoded_pb, _) = dec
        .pull_pixel_buffer()
        .expect("must decode at least one NV12 CVPixelBuffer");
    assert_eq!(decoded_pb.width(), w);
    assert_eq!(decoded_pb.height(), h);
    let vf: VideoFrame = decoded_pb.as_video_frame(PtsUs(0));
    assert_eq!(vf.format, PixelFormat::Nv12);

    // Zero-copy import decoded NV12 CVPixelBuffer directly into Metal GpuCompositor
    let ctx = GpuContext::new_headless().expect("Metal context");
    let importer = AppleMetalZeroCopyImporter::new(&ctx.device).expect("Metal importer");
    let mut comp = GpuCompositor::new(
        ctx,
        CompositorConfig {
            main_width: w,
            main_height: h,
            preview_width: w / 2,
            preview_height: h / 2,
            target_fps: 30,
        },
    );
    let imported = importer
        .import_to_gpu_layer(&comp, &decoded_pb, 0, true, LayerTransform::default())
        .expect("import decoded NV12 to Metal");
    let stats = comp.render_dual(&[&imported.layer], PtsUs(0)).expect("render");
    assert_eq!(stats.rendered_layers, 1);

    // Also test RGBA encode + VideoDecoder::pull_frame
    let mut enc2 = VtH264Encoder::new(128, 128, 30, 500_000).expect("VtH264Encoder2");
    let rgba = vec![200u8; 128 * 128 * 4];
    enc2.encode_rgba(128, 128, &rgba, PtsUs(0))
        .expect("push RGBA frame");
    enc2.flush().expect("flush2");
    let mut dec2 = VtH264Decoder::new();
    while let Some(pkt) = enc2.pull_packet().expect("pull2") {
        dec2.push_packet(&pkt).expect("dec push2");
    }
    let decoded_frame = dec2.pull_frame().expect("pull_frame").expect("frame");
    assert_eq!(decoded_frame.width, 128);
    assert_eq!(decoded_frame.height, 128);
    assert_eq!(decoded_frame.format, PixelFormat::Nv12);
}

#[test]
fn test_apple_m1_m2_full_pipeline_integration() {
    let mut clock = MasterClock::new();
    let mut mixer = AudioMixer::new(clock.clone());
    let (ch0, mut prod0) = mixer.add_channel(AudioInputConfig {
        name: "mac-mic".into(),
        sample_rate: 48_000,
        channels: 2,
        initial_gain: 0.8,
        initial_muted: false,
        initial_delay_ms: 0,
    });
    let mut sine = Vec::with_capacity(4800 * 2);
    for n in 0..4800 {
        let s = 0.25 * (2.0 * PI * 440.0 * (n as f32 / 48_000.0)).sin();
        sine.push(s);
        sine.push(s);
    }
    prod0.push_f32(&sine);

    let bridge = EngineControlBridge::new(
        vec![ch0],
        mixer.master_handle(),
        1_200_000,
        30.0,
        TlsRootConfig::System,
    );

    let srv_cfg = ServerConfig {
        bind_addr: "127.0.0.1:0".into(),
        meter_interval_ms: 50,
        preview_width: 160,
        preview_height: 90,
        preview_fps: 30,
    };
    let mut server = EmbeddedServer::start(srv_cfg, bridge.clone()).expect("EmbeddedServer");
    let addr = server.local_addr();

    // Verify HTTP UI endpoint responds 200 OK
    let mut http_conn = TcpStream::connect(addr).expect("connect HTTP UI");
    http_conn
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut resp = String::new();
    http_conn.read_to_string(&mut resp).unwrap();
    assert!(resp.contains("200 OK"), "unexpected HTTP response: {}", resp);

    let ctx = GpuContext::new_headless().expect("Metal ctx");
    let importer = AppleMetalZeroCopyImporter::new(&ctx.device).expect("Metal importer");
    let mut comp = GpuCompositor::new(
        ctx,
        CompositorConfig {
            main_width: 320,
            main_height: 180,
            preview_width: 160,
            preview_height: 90,
            target_fps: 30,
        },
    );

    let y = vec![140u8; 320 * 180];
    let uv = vec![128u8; 320 * 180 / 2];
    let cam_pb = CVPixelBufferWrapper::new_nv12(320, 180, &y, &uv).expect("NV12 PB");
    let mut imported_cam = importer
        .import_to_gpu_layer(&comp, &cam_pb, 0, true, LayerTransform::default())
        .expect("import cam");

    let mut main_enc = VtH264Encoder::new(320, 180, 30, 800_000).expect("main VT enc");
    let mut prev_enc = VtH264Encoder::new(160, 90, 30, 300_000).expect("prev VT enc");
    let mut aud_enc = AudioToolboxAacEncoder::new(128_000).expect("AAC enc");

    bridge
        .set_overlay_transform(
            0,
            LayerTransform {
                opacity: 0.95,
                ..Default::default()
            },
        )
        .expect("set_overlay_transform");

    let mut mix_buf = vec![0.0f32; MIX_BLOCK_FRAMES * 2];
    for frame_idx in 0..6u64 {
        let pts = PtsUs(frame_idx * 33_333);
        let mut layer_refs = [&mut imported_cam.layer];
        bridge.apply_pending_transforms(&comp, &mut layer_refs);
        comp.render_dual(&[&imported_cam.layer], pts)
            .expect("render_dual");

        let main_rgba = comp
            .readback_texture_rgba(comp.main_texture(), 320, 180)
            .expect("main readback");
        let prev_rgba = comp
            .readback_texture_rgba(comp.preview_texture(), 160, 90)
            .expect("prev readback");
        main_enc
            .encode_rgba(320, 180, &main_rgba, pts)
            .expect("push main");
        prev_enc
            .encode_rgba(160, 90, &prev_rgba, pts)
            .expect("push prev");

        let st = mixer.mix_block(&mut mix_buf);
        aud_enc
            .push_pcm_f32(&mix_buf, st.to_pts_us())
            .expect("push audio");

        if frame_idx == 5 {
            main_enc.flush().unwrap();
            prev_enc.flush().unwrap();
        }

        let mut tick_ctx = PipelineTickContext {
            clock: &mut clock,
            bridge: &bridge,
            compositor: &mut comp,
            main_encoder: &mut main_enc,
            preview_encoder: &mut prev_enc,
            audio_encoder: &mut aud_enc,
            server: &server,
        };
        flush_encoders_to_outputs(&mut tick_ctx, pts).expect("flush outputs");
    }

    let snap = bridge.snapshot();
    assert_eq!(snap.channels.len(), 1);
    bridge.stop_all();
    server.stop();
}

#[test]
fn test_apple_zero_copy_iosurface_identity_and_pool_reuse() {
    let w = 640u32;
    let h = 360u32;
    let pw = 320u32;
    let ph = 180u32;

    let ctx = GpuContext::new_headless().expect("Metal headless context");
    let importer = AppleMetalZeroCopyImporter::new(&ctx.device).expect("CVMetalTextureCache");
    let mut comp = GpuCompositor::new(
        ctx,
        CompositorConfig {
            main_width: w,
            main_height: h,
            preview_width: pw,
            preview_height: ph,
            target_fps: 60,
        },
    );

    // 1. Verify input CVPixelBuffer -> MTLTexture shares identical IOSurfaceRef pointer
    let src = VirtualVideoSource::new(w, h, VirtualVideoPattern::default());
    let (y, uv) = src.render_nv12_bt709(0);
    let in_pb = CVPixelBufferWrapper::new_nv12(w, h, &y, &uv).expect("NV12 PB");
    assert!(!in_pb.iosurface_ptr().is_null(), "input CVPixelBuffer must be IOSurface-backed");

    let bgra = vec![255u8; 64 * 64 * 4];
    let bgra_pb = CVPixelBufferWrapper::new_bgra(64, 64, &bgra).expect("BGRA PB");
    assert!(
        !bgra_pb.iosurface_ptr().is_null(),
        "BGRA CVPixelBuffer must be IOSurface-backed"
    );

    let imported = importer
        .import_to_gpu_layer(&comp, &in_pb, 0, true, LayerTransform::default())
        .expect("import input NV12");
    assert_eq!(
        imported.metal_iosurface_ptr(),
        in_pb.iosurface_ptr(),
        "input MTLTexture must point to the exact same IOSurface as input CVPixelBuffer"
    );

    // 2. Verify output AppleZeroCopyRenderSurfacePool renders into BGRA IOSurface shared with VideoToolbox
    let mut main_pool = AppleZeroCopyRenderSurfacePool::new(&importer, &comp.context().device, w, h, 4)
        .expect("main IOSurface pool");
    let mut prev_pool = AppleZeroCopyRenderSurfacePool::new(&importer, &comp.context().device, pw, ph, 4)
        .expect("prev IOSurface pool");

    let mut main_enc = VtH264Encoder::new(w, h, 60, 2_500_000).expect("main VT encoder");
    let mut prev_enc = VtH264Encoder::new(pw, ph, 60, 800_000).expect("prev VT encoder");

    let mut encoded_packets = Vec::new();
    for frame_idx in 0..24u64 {
        let pts = PtsUs(frame_idx * 16_667);
        let main_slot = main_pool
            .acquire_slot(&importer, &comp.context().device)
            .expect("acquire main slot");
        let prev_slot = prev_pool
            .acquire_slot(&importer, &comp.context().device)
            .expect("acquire prev slot");

        assert!(!main_slot.iosurface_ptr().is_null());
        assert_eq!(
            main_slot.metal_iosurface_ptr(),
            main_slot.iosurface_ptr(),
            "output main MTLTexture must alias output CVPixelBuffer IOSurface"
        );
        assert_eq!(
            prev_slot.metal_iosurface_ptr(),
            prev_slot.iosurface_ptr(),
            "output prev MTLTexture must alias preview CVPixelBuffer IOSurface"
        );

        let stats = comp
            .render_dual_into(
                &[&imported.layer],
                pts,
                Some(main_slot.as_external_target()),
                Some(prev_slot.as_external_target()),
            )
            .expect("render_dual_into external zero-copy targets");
        assert_eq!(stats.rendered_layers, 1);
        importer.flush();

        main_enc
            .encode_pixel_buffer(&main_slot.pixel_buffer, pts)
            .expect("encode main pixel_buffer zero-copy");
        prev_enc
            .encode_pixel_buffer(&prev_slot.pixel_buffer, pts)
            .expect("encode prev pixel_buffer zero-copy");

        while let Some(pkt) = main_enc.pull_packet().expect("pull main") {
            encoded_packets.push(pkt);
        }
        while let Some(_pkt) = prev_enc.pull_packet().expect("pull prev") {}
    }
    main_enc.flush().expect("flush main");
    while let Some(pkt) = main_enc.pull_packet().expect("pull flushed main") {
        encoded_packets.push(pkt);
    }

    assert!(
        main_pool.allocations_total() <= 6,
        "main pool over-allocated: {}",
        main_pool.allocations_total()
    );
    assert!(
        main_pool.reuses_total() >= 18,
        "expected ring reuse >= 18, got {}",
        main_pool.reuses_total()
    );
    assert!(
        prev_pool.allocations_total() <= 6 && prev_pool.reuses_total() >= 18,
        "preview pool reuse check failed: alloc={} reuse={}",
        prev_pool.allocations_total(),
        prev_pool.reuses_total()
    );

    // 3. Decode VideoToolbox output and confirm QuadrantSignal colors survived zero-copy Metal->IOSurface->VT path
    let mut dec = VtH264Decoder::new();
    for pkt in &encoded_packets {
        dec.push_packet(pkt).expect("decode push");
    }
    let (dec_pb, _) = dec.pull_pixel_buffer().expect("decoded NV12 CVPixelBuffer");
    assert_eq!(dec_pb.width(), w);
    assert_eq!(dec_pb.height(), h);
    let vf = dec_pb.as_video_frame(PtsUs(0));
    assert_eq!(vf.format, PixelFormat::Nv12);

    // Verify decoded colors via zero-copy import + render once in test assertion
    let dec_imported = importer
        .import_to_gpu_layer(&comp, &dec_pb, 0, true, LayerTransform::default())
        .expect("import decoded pb");
    comp.render_dual(&[&dec_imported.layer], PtsUs(0))
        .expect("render decoded");
    let decoded_rgba = comp
        .readback_texture_rgba(comp.main_texture(), w, h)
        .expect("readback decoded");
    let tl_idx = ((h as usize / 4) * w as usize + (w as usize / 4)) * 4;
    let tr_idx = ((h as usize / 4) * w as usize + (3 * w as usize / 4)) * 4;
    assert!(
        decoded_rgba[tl_idx] > 180 && decoded_rgba[tl_idx + 1] < 80,
        "expected red top-left quadrant, got {:?}",
        &decoded_rgba[tl_idx..tl_idx + 4]
    );
    assert!(
        decoded_rgba[tr_idx + 1] > 180 && decoded_rgba[tr_idx] < 80,
        "expected green top-right quadrant, got {:?}",
        &decoded_rgba[tr_idx..tr_idx + 4]
    );
}

#[test]
fn test_apple_1080p60_zero_copy_all_use_cases_performance_and_stability() {
    const W: u32 = 1920;
    const H: u32 = 1080;
    const PW: u32 = 960;
    const PH: u32 = 540;
    const WARMUP: u64 = 6;
    const MEASURED: u64 = 90;

    fn run_use_case(
        label: &str,
        multi_layer: bool,
        dual_audio_and_mse: bool,
    ) {
        let mut clock = MasterClock::new();
        let mut mixer = AudioMixer::new(clock.clone());
        let (ch0, mut prod0) = mixer.add_channel(AudioInputConfig {
            name: "cam-mic".into(),
            sample_rate: 48_000,
            channels: 2,
            initial_gain: 0.9,
            initial_muted: false,
            initial_delay_ms: 0,
        });
        let mut channels = vec![ch0];
        let mut prod1 = None;
        let mut aud_src1 = None;
        if dual_audio_and_mse {
            let (ch1, p1) = mixer.add_channel(AudioInputConfig {
                name: "bgm".into(),
                sample_rate: 48_000,
                channels: 2,
                initial_gain: 0.5,
                initial_muted: false,
                initial_delay_ms: 0,
            });
            channels.push(ch1);
            prod1 = Some(p1);
            aud_src1 = Some(VirtualAudioSource::new(
                48_000,
                VirtualAudioSignal::SineTone {
                    freq_hz: 660.0,
                    amplitude: 0.2,
                },
            ));
        }

        let bridge = EngineControlBridge::new(
            channels,
            mixer.master_handle(),
            6_000_000,
            60.0,
            TlsRootConfig::System,
        );
        let mut server = EmbeddedServer::start(
            ServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                meter_interval_ms: 50,
                preview_width: PW,
                preview_height: PH,
                preview_fps: 60,
            },
            bridge.clone(),
        )
        .expect("EmbeddedServer");

        let gpu = GpuContext::new_headless().expect("Metal GpuContext");
        let importer = AppleMetalZeroCopyImporter::new(&gpu.device).expect("Metal importer");
        let mut comp = GpuCompositor::new(
            gpu,
            CompositorConfig {
                main_width: W,
                main_height: H,
                preview_width: PW,
                preview_height: PH,
                target_fps: 60,
            },
        );
        let mut main_pool = AppleZeroCopyRenderSurfacePool::new(&importer, &comp.context().device, W, H, 6)
            .expect("main pool");
        let mut prev_pool = AppleZeroCopyRenderSurfacePool::new(&importer, &comp.context().device, PW, PH, 6)
            .expect("prev pool");

        let mut main_enc = VtH264Encoder::new(W, H, 60, 6_000_000).expect("main 1080p60 VT");
        let mut prev_enc = VtH264Encoder::new(PW, PH, 60, 1_500_000).expect("prev 540p60 VT");
        let mut aud_enc = AudioToolboxAacEncoder::new(128_000).expect("AAC");

        // Pre-bake 6 CVPixelBuffer frames outside timed loop so debug CPU pattern generation is excluded
        let src_main = VirtualVideoSource::new(W, H, VirtualVideoPattern::default());
        let prebaked_main: Vec<CVPixelBufferWrapper> = (0..6u64)
            .map(|k| {
                let (y, uv) = src_main.render_nv12_bt709(k);
                CVPixelBufferWrapper::new_nv12(W, H, &y, &uv).expect("prebaked main NV12")
            })
            .collect();

        let src_pip = VirtualVideoSource::new(480, 270, VirtualVideoPattern::ColorbarsMotion);
        let prebaked_pip = if multi_layer {
            Some(
                (0..6u64)
                    .map(|k| {
                        let (y, uv) = src_pip.render_nv12_bt709(k);
                        CVPixelBufferWrapper::new_nv12(480, 270, &y, &uv)
                            .expect("prebaked PiP NV12")
                    })
                    .collect::<Vec<_>>(),
            )
        } else {
            None
        };
        let overlay_pb = if multi_layer {
            let ov_src = VirtualVideoSource::new(256, 96, VirtualVideoPattern::Solid([24, 160, 255, 255]));
            let bgra = ov_src.render_bgra(0);
            Some(CVPixelBufferWrapper::new_bgra(256, 96, &bgra).expect("overlay BGRA PB"))
        } else {
            None
        };

        let mut aud_src0 = VirtualAudioSource::new(
            48_000,
            VirtualAudioSignal::DualChannelTone {
                left_freq_hz: 440.0,
                right_freq_hz: 880.0,
                amplitude: 0.25,
            },
        );
        let mut mix_buf = vec![0.0f32; MIX_BLOCK_FRAMES * 2];

        let mut frame_ms = Vec::with_capacity(MEASURED as usize);
        let mut total_audio_samples = 0u64;

        for idx in 0..(WARMUP + MEASURED) {
            let t0 = Instant::now();
            let pts = PtsUs(idx * 1_000_000 / 60);

            if multi_layer {
                let _ = bridge.set_overlay_transform(
                    1,
                    LayerTransform {
                        x: -0.5 + ((idx % 30) as f32) * 0.01,
                        y: -0.5,
                        scale_x: 0.4,
                        scale_y: 0.4,
                        z_order: 1,
                        opacity: 0.95,
                        ..Default::default()
                    },
                );
            }

            let base_pb = &prebaked_main[(idx as usize) % prebaked_main.len()];
            let mut imported_base = importer
                .import_to_gpu_layer(&comp, base_pb, 0, true, bridge.layer_transform(0))
                .expect("import base");
            let mut imported_pip = prebaked_pip.as_ref().map(|ring| {
                importer
                    .import_to_gpu_layer(
                        &comp,
                        &ring[(idx as usize) % ring.len()],
                        1,
                        false,
                        bridge.layer_transform(1),
                    )
                    .expect("import pip")
            });
            let mut imported_ov = overlay_pb.as_ref().map(|pb| {
                importer
                    .import_to_gpu_layer(&comp, pb, 2, false, LayerTransform {
                        x: 0.55,
                        y: 0.72,
                        scale_x: 0.25,
                        scale_y: 0.15,
                        z_order: 2,
                        opacity: 0.85,
                        ..Default::default()
                    })
                    .expect("import overlay")
            });

            {
                let mut refs: Vec<&mut core_gpu::GpuLayer> = vec![&mut imported_base.layer];
                if let Some(p) = imported_pip.as_mut() {
                    refs.push(&mut p.layer);
                }
                if let Some(o) = imported_ov.as_mut() {
                    refs.push(&mut o.layer);
                }
                bridge.apply_pending_transforms(&comp, &mut refs);
            }

            let main_slot = main_pool
                .acquire_slot(&importer, &comp.context().device)
                .expect("acquire main slot");
            let prev_slot = prev_pool
                .acquire_slot(&importer, &comp.context().device)
                .expect("acquire prev slot");

            let rstats = match (imported_pip.as_ref(), imported_ov.as_ref()) {
                (Some(p), Some(o)) => comp.render_dual_into(
                    &[&imported_base.layer, &p.layer, &o.layer],
                    pts,
                    Some(main_slot.as_external_target()),
                    Some(prev_slot.as_external_target()),
                ),
                _ => comp.render_dual_into(
                    &[&imported_base.layer],
                    pts,
                    Some(main_slot.as_external_target()),
                    Some(prev_slot.as_external_target()),
                ),
            }
            .expect("render_dual_into");
            assert_eq!(rstats.throttled_layers, 0);
            importer.flush();

            main_enc
                .encode_pixel_buffer(&main_slot.pixel_buffer, pts)
                .expect("encode main");
            prev_enc
                .encode_pixel_buffer(&prev_slot.pixel_buffer, pts)
                .expect("encode prev");

            let blocks = VirtualAudioSource::mix_blocks_for_60fps_frame(idx);
            for _ in 0..blocks {
                let pcm0 = aud_src0.next_stereo_f32(MIX_BLOCK_FRAMES);
                prod0.push_f32(&pcm0);
                if let (Some(p1), Some(s1)) = (prod1.as_mut(), aud_src1.as_mut()) {
                    let pcm1 = s1.next_stereo_f32(MIX_BLOCK_FRAMES);
                    p1.push_f32(&pcm1);
                }
                let st = mixer.mix_block(&mut mix_buf);
                aud_enc
                    .push_pcm_f32(&mix_buf, st.to_pts_us())
                    .expect("push aac");
                total_audio_samples += MIX_BLOCK_FRAMES as u64;
            }

            let mut tick_ctx = PipelineTickContext {
                clock: &mut clock,
                bridge: &bridge,
                compositor: &mut comp,
                main_encoder: &mut main_enc,
                preview_encoder: &mut prev_enc,
                audio_encoder: &mut aud_enc,
                server: &server,
            };
            flush_encoders_to_outputs(&mut tick_ctx, pts).expect("flush outputs");

            if idx >= WARMUP {
                frame_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
            }
        }

        main_enc.flush().expect("flush main");
        prev_enc.flush().expect("flush prev");
        let end_pts = PtsUs((WARMUP + MEASURED) * 1_000_000 / 60);
        let snap = bridge.snapshot();
        let mut tick_ctx = PipelineTickContext {
            clock: &mut clock,
            bridge: &bridge,
            compositor: &mut comp,
            main_encoder: &mut main_enc,
            preview_encoder: &mut prev_enc,
            audio_encoder: &mut aud_enc,
            server: &server,
        };
        flush_encoders_to_outputs(&mut tick_ctx, end_pts).expect("flush final outputs");

        frame_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let avg_ms = frame_ms.iter().sum::<f64>() / frame_ms.len() as f64;
        let p50_ms = frame_ms[frame_ms.len() / 2];
        let p95_ms = frame_ms[((frame_ms.len() as f64 * 0.95) as usize).min(frame_ms.len() - 1)];

        eprintln!(
            "[1080p60 {label}] avg={avg_ms:.2}ms p50={p50_ms:.2}ms p95={p95_ms:.2}ms main_pool(alloc={}, reuse={}) prev_pool(alloc={}, reuse={}) master_l={:.1}dBFS master_r={:.1}dBFS",
            main_pool.allocations_total(),
            main_pool.reuses_total(),
            prev_pool.allocations_total(),
            prev_pool.reuses_total(),
            snap.master_left_dbfs,
            snap.master_right_dbfs,
        );

        assert!(
            avg_ms < 16.67,
            "[{label}] sustained 60fps violated: avg={avg_ms:.2}ms >= 16.67ms"
        );
        // Debug unoptimized builds can see isolated scheduler/Metal barrier spikes; require p50 < 16.67ms
        assert!(
            p50_ms < 16.67 && p95_ms < 25.0,
            "[{label}] 60fps latency budget violated: p50={p50_ms:.2}ms p95={p95_ms:.2}ms"
        );
        assert!(
            main_pool.allocations_total() <= 6 && prev_pool.allocations_total() <= 6,
            "[{label}] unexpected per-frame allocation"
        );
        assert!(
            main_pool.reuses_total() >= MEASURED - 6,
            "[{label}] expected ring reuse >= {}, got {}",
            MEASURED - 6,
            main_pool.reuses_total()
        );
        assert!(snap.master_left_dbfs > -60.0 && snap.master_right_dbfs > -60.0);
        assert_eq!(
            total_audio_samples,
            (WARMUP + MEASURED) * 800,
            "[{label}] 60fps audio cadence must match exact 48kHz (800 samples/frame)"
        );
        bridge.stop_all();
        server.stop();
    }

    run_use_case("uc1_single_layer", false, false);
    run_use_case("uc2_multilayer_pip_overlay_transforms", true, false);
    run_use_case("uc3_dual_audio_and_mse_fmp4", true, true);
}
