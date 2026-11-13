use ffi_capi::android_jni::{portable_create, portable_submit_ahb_meta, AhbDescriptor};
use ffi_capi::{
    ikakim_engine_create, ikakim_engine_default_config, ikakim_engine_destroy,
    ikakim_engine_get_stats, ikakim_engine_last_error, ikakim_engine_push_audio_f32,
    ikakim_engine_request_keyframe, ikakim_engine_set_layer_transform,
    ikakim_engine_start_rtmps, ikakim_engine_step_tick, ikakim_engine_stop_rtmps,
    ikakim_engine_submit_rgba_layer, ikakim_engine_ui_port, IkakimEngineStats,
    IkakimLayerTransform,
};
use std::ffi::{c_char, CStr, CString};
use std::io::{Read, Write};
use std::net::TcpStream;

#[test]
fn capi_full_engine_lifecycle_http_ui_abr_and_layers() {
    let mut cfg = ikakim_engine_default_config();
    cfg.main_width = 64;
    cfg.main_height = 64;
    cfg.preview_width = 32;
    cfg.preview_height = 32;
    cfg.target_fps = 30;
    cfg.initial_bitrate_bps = 2_000_000;
    cfg.http_port = 0;

    unsafe {
        let handle = ikakim_engine_create(&cfg);
        assert!(!handle.is_null(), "ikakim_engine_create failed");

        let port = ikakim_engine_ui_port(handle);
        assert!(port > 0, "embedded UI server port must be bound");

        // Verify local Web UI HTTP GET for WKWebView / Android WebView host
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect embedded UI");
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut resp = String::new();
        stream.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "unexpected HTTP: {}", resp);

        // Submit base RGBA camera/test pattern + top-bottom alpha overlay
        let rgba = vec![200u8; 64 * 64 * 4];
        assert_eq!(
            ikakim_engine_submit_rgba_layer(handle, 0, true, false, 64, 64, rgba.as_ptr(), rgba.len()),
            0
        );
        assert_eq!(
            ikakim_engine_submit_rgba_layer(handle, 1, false, true, 64, 64, rgba.as_ptr(), rgba.len()),
            0
        );

        let tr = IkakimLayerTransform {
            x: 0.1,
            y: 0.1,
            scale_x: 0.5,
            scale_y: 0.5,
            rotation_rad: 0.0,
            opacity: 0.9,
            z_order: 2,
            visible: true,
        };
        assert_eq!(ikakim_engine_set_layer_transform(handle, 1, &tr), 0);

        // Push stereo f32 PCM samples (20ms at 48kHz = 1920 floats)
        let pcm = vec![0.1f32; 1920];
        assert_eq!(
            ikakim_engine_push_audio_f32(handle, 0, pcm.as_ptr(), pcm.len()),
            0
        );

        // Run ticks and confirm AIMD ABR lowers bitrate under congestion
        assert_eq!(ikakim_engine_step_tick(handle, 0, 33), 0);
        assert_eq!(ikakim_engine_step_tick(handle, 300_000, 200), 0);
        assert_eq!(ikakim_engine_request_keyframe(handle), 0);
        assert_eq!(ikakim_engine_step_tick(handle, 0, 33), 0);

        let mut stats = IkakimEngineStats::default();
        assert_eq!(ikakim_engine_get_stats(handle, &mut stats), 0);
        assert_eq!(stats.ticks, 3);
        assert_eq!(stats.ui_port, port);
        assert_eq!(stats.active_gpu_layers, 2);
        assert!(
            stats.current_bitrate_bps < 2_000_000,
            "expected AIMD ABR reduction under 300KB queue, got {}",
            stats.current_bitrate_bps
        );
        assert!(stats.encoded_video_packets > 0);
        assert!(stats.encoded_audio_packets > 0);

        // Invalid RTMP URL records last error cleanly instead of crashing
        let bad_url = CString::new("http://not-rtmp.example/live").unwrap();
        assert_ne!(ikakim_engine_start_rtmps(handle, bad_url.as_ptr()), 0);
        let mut err_buf = [0 as c_char; 256];
        let err_len = ikakim_engine_last_error(handle, err_buf.as_mut_ptr(), err_buf.len());
        assert!(err_len > 0);
        let err_msg = CStr::from_ptr(err_buf.as_ptr()).to_str().unwrap();
        assert!(!err_msg.is_empty());

        // Android AHB descriptor portable helper updates AHB active stat
        assert_eq!(
            portable_submit_ahb_meta(
                handle,
                0,
                true,
                AhbDescriptor {
                    buffer_id: 0xDEAD_BEEF,
                    width: 1920,
                    height: 1080,
                    android_format: 35,
                    usage_flags: 0x0100,
                }
            ),
            0
        );
        assert_eq!(ikakim_engine_get_stats(handle, &mut stats), 0);
        assert!(stats.android_ahb_active);

        let _ = ikakim_engine_stop_rtmps(handle);
        ikakim_engine_destroy(handle);
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
#[test]
fn capi_apple_cv_pixel_buffer_zero_copy_path() {
    use apple_common::cv_metal::CVPixelBufferWrapper;
    use ffi_capi::ikakim_engine_submit_cv_pixel_buffer;

    let mut cfg = ikakim_engine_default_config();
    cfg.main_width = 64;
    cfg.main_height = 64;
    cfg.preview_width = 32;
    cfg.preview_height = 32;
    cfg.http_port = 0;

    unsafe {
        let handle = ikakim_engine_create(&cfg);
        assert!(!handle.is_null());

        let y = vec![128u8; 64 * 64];
        let uv = vec![128u8; 64 * 32];
        let pb = CVPixelBufferWrapper::new_nv12(64, 64, &y, &uv).expect("create NV12 CVPixelBuffer");
        let rc = ikakim_engine_submit_cv_pixel_buffer(handle, 0, true, pb.raw());
        assert_eq!(rc, 0, "CVPixelBuffer zero-copy import failed");

        assert_eq!(ikakim_engine_step_tick(handle, 0, 33), 0);
        let mut stats = IkakimEngineStats::default();
        assert_eq!(ikakim_engine_get_stats(handle, &mut stats), 0);
        assert!(stats.apple_zero_copy_active);
        assert_eq!(stats.active_gpu_layers, 1);

        ikakim_engine_destroy(handle);
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
#[test]
fn capi_1080p60_apple_zero_copy_engine_test() {
    use apple_common::cv_metal::CVPixelBufferWrapper;
    use ffi_capi::ikakim_engine_submit_cv_pixel_buffer;
    use std::time::Instant;

    let mut cfg = ikakim_engine_default_config();
    cfg.main_width = 1920;
    cfg.main_height = 1080;
    cfg.preview_width = 960;
    cfg.preview_height = 540;
    cfg.target_fps = 60;
    cfg.initial_bitrate_bps = 6_000_000;
    cfg.http_port = 0;

    unsafe {
        let handle = ikakim_engine_create(&cfg);
        assert!(!handle.is_null());

        // Pre-bake 4 NV12 1080p CVPixelBuffers so CPU generation does not skew tick latency
        let pbs: Vec<CVPixelBufferWrapper> = (0..4u8)
            .map(|k| {
                let y = vec![80u8.saturating_add(k * 20); 1920 * 1080];
                let uv = vec![128u8; 1920 * 540];
                CVPixelBufferWrapper::new_nv12(1920, 1080, &y, &uv).expect("create 1080p NV12 PB")
            })
            .collect();
        let audio_block = vec![0.15f32; 480 * 2];

        const WARMUP: usize = 6;
        const MEASURED: usize = 60;
        let mut total_ms = 0.0f64;

        for idx in 0..(WARMUP + MEASURED) {
            let t0 = Instant::now();
            let pb = &pbs[idx % pbs.len()];
            assert_eq!(ikakim_engine_submit_cv_pixel_buffer(handle, 0, true, pb.raw()), 0);
            let blocks = if idx % 3 == 2 { 1 } else { 2 };
            for _ in 0..blocks {
                assert_eq!(
                    ikakim_engine_push_audio_f32(handle, 0, audio_block.as_ptr(), audio_block.len()),
                    0
                );
            }
            assert_eq!(ikakim_engine_step_tick(handle, 0, 12), 0);
            if idx >= WARMUP {
                total_ms += t0.elapsed().as_secs_f64() * 1000.0;
            }
        }

        let avg_ms = total_ms / MEASURED as f64;
        eprintln!("[C-API 1080p60 zero-copy] avg_tick={avg_ms:.2}ms");

        let mut stats = IkakimEngineStats::default();
        assert_eq!(ikakim_engine_get_stats(handle, &mut stats), 0);
        assert!(stats.apple_zero_copy_active, "Apple zero-copy pipeline must be active");
        assert_eq!(stats.ticks, (WARMUP + MEASURED) as u64);
        assert_eq!(stats.active_gpu_layers, 1);
        assert!(stats.encoded_video_packets > 0, "expected VideoToolbox zero-copy encoded packets");
        assert!(stats.encoded_audio_packets > 0, "expected AAC encoded audio packets");
        assert!(
            avg_ms < 16.67,
            "C-API 1080p60 zero-copy avg tick {avg_ms:.2}ms >= 16.67ms"
        );

        ikakim_engine_destroy(handle);
    }
}

#[test]
fn capi_portable_jni_create_and_destroy_smoke() {
    let raw = portable_create(64, 64, 32, 32, 30, 1_500_000, 0);
    assert!(raw > 0);
    unsafe {
        let h = ffi_capi::android_jni::jlong_to_handle(raw);
        assert!(ikakim_engine_ui_port(h) > 0);
        ikakim_engine_destroy(h);
    }
}
