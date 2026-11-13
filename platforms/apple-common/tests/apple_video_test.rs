#![cfg(any(target_os = "macos", target_os = "ios"))]

use apple_common::audio_toolbox::AudioToolboxAacEncoder;
use apple_common::av_capture::list_video_devices;
use apple_common::cv_metal::{AppleMetalZeroCopyImporter, CVPixelBufferWrapper};
use apple_common::image_io::decode_image_rgba;
use apple_common::video_toolbox::{VtH264Decoder, VtH264Encoder};
use core_audio::mixer::{AudioInputConfig, AudioMixer};
use core_audio::MIX_BLOCK_FRAMES;
use core_engine::pipeline::{flush_encoders_to_outputs, EngineControlBridge, PipelineTickContext};
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
