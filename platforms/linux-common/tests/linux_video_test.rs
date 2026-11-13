#![cfg(target_os = "linux")]

use core_audio::mixer::{AudioInputConfig, AudioMixer};
use core_audio::MIX_BLOCK_FRAMES;
use core_engine::pipeline::{flush_encoders_to_outputs, EngineControlBridge, PipelineTickContext};
use core_engine::{SwH264Decoder, SwH264Encoder};
use core_gpu::{CompositorConfig, GpuCompositor, GpuContext};
use core_net_ui::rpc::ControlBackend;
use core_net_ui::{EmbeddedServer, ServerConfig};
use core_rtmp::TlsRootConfig;
use core_types::clock::{MasterClock, PtsUs};
use core_types::media::{CodecKind, LayerTransform, NativeBufferHandle, PixelFormat};
use core_types::traits::{AudioEncoder, VideoDecoder, VideoEncoder};
use linux_common::dmabuf_gpu::{LinuxDmaBufImporter, LinuxImportMode, SurfacelessEglDmaBuf};
use linux_common::fdk_aac::FdkAacEncoder;
use linux_common::image_decode::{decode_image_rgba, decode_ppm_p6};
use linux_common::v4l2::{list_capture_devices, V4l2CaptureDevice};
use std::f32::consts::PI;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::sync::Mutex;

static VIVID_LOCK: Mutex<()> = Mutex::new(());

extern "C" {
    fn open(path: *const std::os::raw::c_char, oflag: std::os::raw::c_int, ...) -> std::os::raw::c_int;
    fn flock(fd: std::os::raw::c_int, operation: std::os::raw::c_int) -> std::os::raw::c_int;
}

#[allow(dead_code)]
struct VividProcLock(std::os::fd::OwnedFd);

fn lock_vivid_across_procs() -> VividProcLock {
    use std::ffi::CString;
    use std::os::fd::FromRawFd;
    let path = CString::new("/tmp/ikakim_vivid_video0.lock").unwrap();
    let fd = unsafe { open(path.as_ptr(), 0o100 | 0o2, 0o666) };
    assert!(fd >= 0);
    assert_eq!(unsafe { flock(fd, 2) }, 0);
    VividProcLock(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

const PNG_2X2_RGBA: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2, 8, 6, 0,
    0, 0, 114, 182, 13, 36, 0, 0, 0, 19, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 240, 31, 12,
    129, 52, 8, 52, 0, 0, 73, 73, 9, 120, 40, 160, 219, 119, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66,
    96, 130,
];

#[test]
fn test_v4l2_vivid_dmabuf_to_egl_and_gpu_compositor() {
    let _vivid_guard = VIVID_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _proc_lock = lock_vivid_across_procs();
    let devices = list_capture_devices();
    let vivid = devices
        .iter()
        .find(|d| d.driver.contains("vivid") || d.card.contains("vivid"))
        .expect("vivid V4L2 device must be present on Lima VM");

    // 1. YUYV DMA-BUF capture + Surfaceless EGL zero-copy bind + Vulkan llvmpipe composite
    let mut cap_yuyv =
        V4l2CaptureDevice::open(&vivid.path, 640, 360, PixelFormat::Yuyv, 4).expect("open vivid YUYV");
    let frame_yuyv = cap_yuyv
        .capture_frame(PtsUs(0), 2000)
        .expect("capture vivid YUYV frame");
    assert_eq!(frame_yuyv.width, 640);
    assert_eq!(frame_yuyv.height, 360);
    assert_eq!(frame_yuyv.format, PixelFormat::Yuyv);

    // Explicit Surfaceless EGL DMA-BUF verification
    if let NativeBufferHandle::DmaBuf {
        ref fd,
        offset,
        stride,
        ..
    } = frame_yuyv.handle
    {
        let egl = SurfacelessEglDmaBuf::new().expect("Surfaceless EGL init");
        let egl_tex = egl
            .bind_dmabuf_external_texture(
                fd.as_raw_fd(),
                frame_yuyv.width,
                frame_yuyv.height,
                frame_yuyv.format,
                offset,
                stride,
            )
            .expect("Surfaceless EGL DMA-BUF bind");
        assert!(!egl_tex.image.is_null());
        assert_ne!(egl_tex.gl_tex, 0);
    } else {
        panic!("expected NativeBufferHandle::DmaBuf");
    }

    let ctx = GpuContext::new_headless().expect("llvmpipe Vulkan GpuContext");
    let importer = LinuxDmaBufImporter::new(&ctx.device);
    assert!(importer.has_surfaceless_egl());

    let mut comp = GpuCompositor::new(
        ctx,
        CompositorConfig {
            main_width: 640,
            main_height: 360,
            preview_width: 320,
            preview_height: 180,
            target_fps: 30,
        },
    );

    let base_layer = importer
        .import_frame_to_layer(&comp, &frame_yuyv, 0, true, LayerTransform::default())
        .expect("import vivid YUYV DMA-BUF");
    assert_eq!(base_layer.mode, LinuxImportMode::SurfacelessEglAndMmap);

    // 2. Decode PNG via libpng16 dlopen and composite overlay on top of vivid frame
    let png = decode_image_rgba(PNG_2X2_RGBA).expect("libpng16 PNG decode");
    assert_eq!(png.width, 2);
    assert_eq!(png.height, 2);
    assert_eq!(&png.rgba[0..4], &[255, 0, 0, 255]);

    let ov_layer = comp
        .create_rgba_layer(1, false, false, png.width, png.height, &png.rgba, LayerTransform::default())
        .expect("PNG overlay layer");

    let stats = comp
        .render_dual(&[&base_layer.layer, &ov_layer], PtsUs(0))
        .expect("render_dual YUYV + PNG overlay");
    assert_eq!(stats.rendered_layers, 2);

    let main_rgba = comp
        .readback_texture_rgba(comp.main_texture(), 640, 360)
        .expect("readback main");
    assert!(main_rgba[0] > 200, "expected PNG red R>200, got {}", main_rgba[0]);
    drop(cap_yuyv);

    // 3. NV12 DMA-BUF capture on vivid -> GPU compositor
    let mut cap_nv12 =
        V4l2CaptureDevice::open(&vivid.path, 640, 360, PixelFormat::Nv12, 4).expect("open vivid NV12");
    let frame_nv12 = cap_nv12
        .capture_frame(PtsUs(33_333), 2000)
        .expect("capture vivid NV12 frame");
    let nv12_layer = importer
        .import_frame_to_layer(&comp, &frame_nv12, 0, true, LayerTransform::default())
        .expect("import vivid NV12 DMA-BUF");
    let stats_nv12 = comp
        .render_dual(&[&nv12_layer.layer], PtsUs(33_333))
        .expect("render NV12");
    assert_eq!(stats_nv12.rendered_layers, 1);
}

#[test]
fn test_image_decode_png_and_ppm_and_swh264_roundtrip() {
    let png = decode_image_rgba(PNG_2X2_RGBA).expect("PNG decode");
    assert_eq!(png.width, 2);
    assert_eq!(png.height, 2);
    assert_eq!(&png.rgba[0..8], &[255, 0, 0, 255, 0, 255, 0, 255]);

    const JPEG_2X2: &[u8] = &[
        255, 216, 255, 224, 0, 16, 74, 70, 73, 70, 0, 1, 1, 0, 0, 72, 0, 72, 0, 0, 255, 225, 0, 76,
        69, 120, 105, 102, 0, 0, 77, 77, 0, 42, 0, 0, 0, 8, 0, 1, 135, 105, 0, 4, 0, 0, 0, 1, 0, 0,
        0, 26, 0, 0, 0, 0, 0, 3, 160, 1, 0, 3, 0, 0, 0, 1, 0, 1, 0, 0, 160, 2, 0, 4, 0, 0, 0, 1, 0,
        0, 0, 2, 160, 3, 0, 4, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 0, 255, 237, 0, 56, 80, 104, 111,
        116, 111, 115, 104, 111, 112, 32, 51, 46, 48, 0, 56, 66, 73, 77, 4, 4, 0, 0, 0, 0, 0, 0, 56,
        66, 73, 77, 4, 37, 0, 0, 0, 0, 0, 16, 212, 29, 140, 217, 143, 0, 178, 4, 233, 128, 9, 152,
        236, 248, 66, 126, 255, 192, 0, 17, 8, 0, 2, 0, 2, 3, 1, 34, 0, 2, 17, 1, 3, 17, 1, 255,
        196, 0, 31, 0, 0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8,
        9, 10, 11, 255, 196, 0, 181, 16, 0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 125, 1, 2,
        3, 0, 4, 17, 5, 18, 33, 49, 65, 6, 19, 81, 97, 7, 34, 113, 20, 50, 129, 145, 161, 8, 35,
        66, 177, 193, 21, 82, 209, 240, 36, 51, 98, 114, 130, 9, 10, 22, 23, 24, 25, 26, 37, 38,
        39, 40, 41, 42, 52, 53, 54, 55, 56, 57, 58, 67, 68, 69, 70, 71, 72, 73, 74, 83, 84, 85, 86,
        87, 88, 89, 90, 99, 100, 101, 102, 103, 104, 105, 106, 115, 116, 117, 118, 119, 120, 121,
        122, 131, 132, 133, 134, 135, 136, 137, 138, 146, 147, 148, 149, 150, 151, 152, 153, 154,
        162, 163, 164, 165, 166, 167, 168, 169, 170, 178, 179, 180, 181, 182, 183, 184, 185, 186,
        194, 195, 196, 197, 198, 199, 200, 201, 202, 210, 211, 212, 213, 214, 215, 216, 217, 218,
        225, 226, 227, 228, 229, 230, 231, 232, 233, 234, 241, 242, 243, 244, 245, 246, 247, 248,
        249, 250, 255, 196, 0, 31, 1, 0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 1, 2, 3,
        4, 5, 6, 7, 8, 9, 10, 11, 255, 196, 0, 181, 17, 0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1,
        2, 119, 0, 1, 2, 3, 17, 4, 5, 33, 49, 6, 18, 65, 81, 7, 97, 113, 19, 34, 50, 129, 8, 20,
        66, 145, 161, 177, 193, 9, 35, 51, 82, 240, 21, 98, 114, 209, 10, 22, 36, 52, 225, 37, 241,
        23, 24, 25, 26, 38, 39, 40, 41, 42, 53, 54, 55, 56, 57, 58, 67, 68, 69, 70, 71, 72, 73, 74,
        83, 84, 85, 86, 87, 88, 89, 90, 99, 100, 101, 102, 103, 104, 105, 106, 115, 116, 117, 118,
        119, 120, 121, 122, 130, 131, 132, 133, 134, 135, 136, 137, 138, 146, 147, 148, 149, 150,
        151, 152, 153, 154, 162, 163, 164, 165, 166, 167, 168, 169, 170, 178, 179, 180, 181, 182,
        183, 184, 185, 186, 194, 195, 196, 197, 198, 199, 200, 201, 202, 210, 211, 212, 213, 214,
        215, 216, 217, 218, 226, 227, 228, 229, 230, 231, 232, 233, 234, 242, 243, 244, 245, 246,
        247, 248, 249, 250, 255, 219, 0, 67, 0, 2, 2, 2, 2, 2, 2, 3, 2, 2, 3, 5, 3, 3, 3, 5, 6, 5,
        5, 5, 5, 6, 8, 6, 6, 6, 6, 6, 8, 10, 8, 8, 8, 8, 8, 8, 10, 10, 10, 10, 10, 10, 10, 10, 12,
        12, 12, 12, 12, 12, 14, 14, 14, 14, 14, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 255, 219,
        0, 67, 1, 2, 2, 2, 4, 4, 4, 7, 4, 4, 7, 16, 11, 9, 11, 16, 16, 16, 16, 16, 16, 16, 16, 16,
        16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
        16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 255, 221, 0, 4, 0,
        1, 255, 218, 0, 12, 3, 1, 0, 2, 17, 3, 17, 0, 63, 0, 249, 94, 138, 40, 175, 230, 179, 253,
        184, 63, 255, 217,
    ];
    let jpg = decode_image_rgba(JPEG_2X2).expect("libjpeg JPEG decode");
    assert_eq!(jpg.width, 2);
    assert_eq!(jpg.height, 2);
    assert_eq!(jpg.rgba.len(), 16);
    assert!(jpg.rgba[0] > 180, "expected red-ish px0 R>180, got {}", jpg.rgba[0]);
    assert!(jpg.rgba[1] < 60, "expected red-ish px0 G<60, got {}", jpg.rgba[1]);
    assert_eq!(jpg.rgba[3], 255);

    let ppm_bytes = b"P6\n2 1\n255\n\xaa\xbb\xcc\x11\x22\x33";
    let ppm = decode_ppm_p6(ppm_bytes).expect("PPM P6 decode");
    assert_eq!(ppm.width, 2);
    assert_eq!(ppm.height, 1);
    assert_eq!(ppm.rgba, vec![0xaa, 0xbb, 0xcc, 255, 0x11, 0x22, 0x33, 255]);

    let mut enc = SwH264Encoder::new(16, 16, 400_000);
    let mut rgba = Vec::with_capacity(16 * 16 * 4);
    for px in 0..(16 * 16) {
        let v = (px % 256) as u8;
        rgba.extend_from_slice(&[v, 100, 200, 255]);
    }
    enc.encode_rgba(&rgba, PtsUs(0)).expect("SwH264 encode");
    let seq = enc.pull_packet().unwrap().expect("SPS/PPS seq");
    assert!(seq.is_sequence_header);
    assert_eq!(seq.codec, CodecKind::H264);
    let idr = enc.pull_packet().unwrap().expect("IDR packet");
    assert!(idr.is_keyframe);

    let mut dec = SwH264Decoder::new();
    dec.push_packet(&seq).unwrap();
    dec.push_packet(&idr).unwrap();
    let decoded = dec.pull_frame().unwrap().expect("decoded frame");
    assert_eq!(decoded.width, 16);
    assert_eq!(decoded.height, 16);
    assert_eq!(decoded.format, PixelFormat::Nv12);
}

#[test]
fn test_linux_m1_m2_full_pipeline_integration() {
    let _vivid_guard = VIVID_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _proc_lock = lock_vivid_across_procs();
    let mut clock = MasterClock::new();
    let mut mixer = AudioMixer::new(clock.clone());
    let (ch0, mut prod0) = mixer.add_channel(AudioInputConfig {
        name: "alsa-mic".into(),
        sample_rate: 48_000,
        channels: 2,
        initial_gain: 0.75,
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

    let mut http_conn = TcpStream::connect(addr).expect("connect HTTP UI");
    http_conn
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut resp = String::new();
    http_conn.read_to_string(&mut resp).unwrap();
    assert!(resp.contains("200 OK"), "unexpected HTTP response: {}", resp);

    let devices = list_capture_devices();
    let vivid = devices
        .iter()
        .find(|d| d.driver.contains("vivid") || d.card.contains("vivid"))
        .expect("vivid device");
    let mut cap =
        V4l2CaptureDevice::open(&vivid.path, 320, 180, PixelFormat::Yuyv, 3).expect("open vivid");

    let ctx = GpuContext::new_headless().expect("Vulkan llvmpipe ctx");
    let importer = LinuxDmaBufImporter::new(&ctx.device);
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

    let mut main_enc = SwH264Encoder::new(320, 180, 800_000);
    let mut prev_enc = SwH264Encoder::new(160, 90, 300_000);
    let mut aud_enc = FdkAacEncoder::new(128_000).expect("FdkAacEncoder");

    let mut mix_buf = vec![0.0f32; MIX_BLOCK_FRAMES * 2];
    for idx in 0..4u64 {
        let pts = PtsUs(idx * 33_333);
        let cam_frame = cap.capture_frame(pts, 2000).expect("vivid capture");
        let mut cam_layer = importer
            .import_frame_to_layer(&comp, &cam_frame, 0, true, LayerTransform::default())
            .expect("import cam");
        let mut layer_refs = [&mut cam_layer.layer];
        bridge.apply_pending_transforms(&comp, &mut layer_refs);
        comp.render_dual(&[&cam_layer.layer], pts)
            .expect("render_dual");

        let main_rgba = comp
            .readback_texture_rgba(comp.main_texture(), 320, 180)
            .expect("main readback");
        let prev_rgba = comp
            .readback_texture_rgba(comp.preview_texture(), 160, 90)
            .expect("prev readback");
        main_enc
            .encode_rgba(&main_rgba, pts)
            .expect("encode main");
        prev_enc
            .encode_rgba(&prev_rgba, pts)
            .expect("encode prev");

        let st = mixer.mix_block(&mut mix_buf);
        aud_enc
            .push_pcm_f32(&mix_buf, st.to_pts_us())
            .expect("push audio");

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

    assert_eq!(bridge.snapshot().channels.len(), 1);
    bridge.stop_all();
    server.stop();
}
