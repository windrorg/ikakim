use android_native::aaudio::{AAudioCaptureBridge, AAudioConfig, MockAAudioBackend};
use android_native::ahb_vulkan::{
    AHardwareBufferDesc, AndroidAhbImporter, MockAndroidHalBackend,
    AHARDWAREBUFFER_FORMAT_Y8CBCR_420_888, AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE,
    VK_ANDROID_EXTERNAL_MEMORY_ANDROID_HARDWARE_BUFFER_EXTENSION_NAME,
    VK_EXTERNAL_MEMORY_HANDLE_TYPE_ANDROID_HARDWARE_BUFFER_BIT_ANDROID,
};
use android_native::mediacodec::{
    DualAndroidEncoder, MediaCodecEncoderConfig, MediaCodecStreamRole, MockMediaCodecBackend,
};
use core_audio::mixer::{AudioInputConfig, AudioMixer};
use core_audio::MIX_BLOCK_FRAMES;
use core_engine::pipeline::EngineControlBridge;
use core_gpu::{CompositorConfig, GpuCompositor, GpuContext};
use core_rtmp::TlsRootConfig;
use core_types::clock::{MasterClock, PtsUs, MASTER_SAMPLE_RATE};
use core_types::media::LayerTransform;
use core_types::traits::VideoEncoder;
use std::sync::Arc;

#[test]
fn android_ahb_vulkan_dual_mediacodec_and_aaudio_pipeline_e2e() {
    // 1. AHardwareBuffer + VK_ANDROID_external_memory_android_hardware_buffer import
    let hal = Arc::new(MockAndroidHalBackend::new());
    let importer = AndroidAhbImporter::new(Arc::clone(&hal) as _);

    let ctx = GpuContext::new_headless().expect("headless GPU context");
    let mut compositor = GpuCompositor::new(
        ctx,
        CompositorConfig {
            main_width: 64,
            main_height: 64,
            preview_width: 32,
            preview_height: 32,
            target_fps: 60,
        },
    );

    let ahb_id = 0xCAFE_BABE_0001u64;
    let desc = AHardwareBufferDesc {
        width: 64,
        height: 64,
        layers: 1,
        format: AHARDWAREBUFFER_FORMAT_Y8CBCR_420_888,
        usage: AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE,
        stride: 64,
        rfu0: 0,
        rfu1: 0,
    };

    let (plan, layer) = importer
        .import_to_gpu_layer(
            &compositor,
            ahb_id,
            desc,
            0,
            true,
            LayerTransform::default(),
            None,
        )
        .expect("import AHB into GPU layer");

    assert_eq!(
        plan.required_vk_extension,
        VK_ANDROID_EXTERNAL_MEMORY_ANDROID_HARDWARE_BUFFER_EXTENSION_NAME
    );
    assert_eq!(
        plan.handle_type_bit,
        VK_EXTERNAL_MEMORY_HANDLE_TYPE_ANDROID_HARDWARE_BUFFER_BIT_ANDROID
    );
    assert!(plan.allocation_size >= 64 * 64);
    assert_eq!(hal.active_refcount(ahb_id), 1);

    // Composite imported AHB layer on main + preview render targets
    let render_stats = compositor
        .render_dual(&[&layer], PtsUs(0))
        .expect("render AHB GPU layer");
    assert_eq!(render_stats.rendered_layers, 1);

    // 2. Dual AMediaCodec (createInputSurface) + live AIMD ABR bitrate + request-sync IDR
    let codec_backend = Arc::new(MockMediaCodecBackend::new());
    let mut dual = DualAndroidEncoder::new(
        Arc::clone(&codec_backend) as _,
        MediaCodecEncoderConfig {
            width: 1920,
            height: 1080,
            fps: 60,
            initial_bitrate_bps: 4_000_000,
            role: MediaCodecStreamRole::MainBroadcast,
        },
        MediaCodecEncoderConfig {
            width: 1280,
            height: 720,
            fps: 30,
            initial_bitrate_bps: 1_000_000,
            role: MediaCodecStreamRole::PreviewConsole,
        },
    )
    .expect("create DualAndroidEncoder");

    assert!(dual.main.input_surface_handle() > 0);
    assert!(dual.preview.input_surface_handle() > 0);
    assert_ne!(
        dual.main.input_surface_handle(),
        dual.preview.input_surface_handle()
    );

    // 3. AAudio capture -> AudioMixer + EngineControlBridge ABR sync
    let clock = MasterClock::new();
    let mut mixer = AudioMixer::new(clock);
    let (ch0, mut ch0_prod) = mixer.add_channel(AudioInputConfig {
        name: "AAudio Mic".into(),
        sample_rate: MASTER_SAMPLE_RATE as u32,
        channels: 2,
        initial_gain: 1.0,
        initial_muted: false,
        initial_delay_ms: 0,
    });
    let master = mixer.master_handle();
    let bridge = EngineControlBridge::new(
        vec![ch0],
        master,
        4_000_000,
        60.0,
        TlsRootConfig::System,
    );

    let aaudio_backend = Arc::new(MockAAudioBackend::new());
    let mut aaudio = AAudioCaptureBridge::open(aaudio_backend, AAudioConfig::default())
        .expect("open mock AAudio stream");
    let pushed = aaudio
        .pump_into_producer(&mut ch0_prod)
        .expect("pump AAudio PCM");
    assert_eq!(pushed, MIX_BLOCK_FRAMES * 2);

    let mut mix_out = vec![0.0f32; MIX_BLOCK_FRAMES * 2];
    let sample_t = mixer.mix_block(&mut mix_out);
    assert!(mix_out.iter().any(|&v| v.abs() > 0.01));

    // Apply live ABR drop to AMediaCodec main encoder & request IDR
    bridge
        .apply_abr_bitrate(2_500_000, &mut dual.main)
        .expect("ABR bitrate propagation");
    dual.submit_composited_tick(sample_t.to_pts_us(), false)
        .expect("submit dual surface frame 0");
    dual.main.request_keyframe();
    dual.submit_composited_tick(PtsUs(33_333), false)
        .expect("submit dual surface frame 1");

    let mut main_pkts = Vec::new();
    while let Some(p) = dual.main.pull_packet().unwrap() {
        main_pkts.push(p);
    }
    let mut prev_pkts = Vec::new();
    while let Some(p) = dual.preview.pull_packet().unwrap() {
        prev_pkts.push(p);
    }

    // Main should have SPS/PPS sequence header + IDR frame 0 + IDR frame 1 (after request_keyframe)
    assert_eq!(main_pkts.len(), 3);
    assert!(main_pkts[0].is_sequence_header);
    assert!(main_pkts[1].is_keyframe);
    assert!(main_pkts[2].is_keyframe);
    assert_eq!(
        codec_backend.bitrate_history(dual.main.codec_id()),
        vec![4_000_000, 2_500_000]
    );
    assert!(!prev_pkts.is_empty());

    // Release AHB and verify refcount hits zero cleanly
    importer.release_buffer(ahb_id).expect("release AHB");
    assert_eq!(hal.active_refcount(ahb_id), 0);
}
