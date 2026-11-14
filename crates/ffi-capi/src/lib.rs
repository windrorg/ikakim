//! C-ABI FFI Exports & Native Mobile Host Engine Bridge (`IkakimEngine`) (M4: Issue #8)

pub mod android_jni;

pub use core_engine;

use android_jni::AhbDescriptor;
use core_audio::mixer::{AudioInputConfig, AudioInputProducer, AudioMixer};
use core_audio::MIX_BLOCK_FRAMES;
use core_engine::pipeline::{flush_encoders_to_outputs, EngineControlBridge, PipelineTickContext};
use core_engine::SwH264Encoder;
use core_gpu::{CompositorConfig, GpuCompositor, GpuContext, GpuLayer};
use core_net_ui::rpc::ControlBackend;
use core_net_ui::{EmbeddedServer, ServerConfig};
use core_rtmp::{AbrConfig, AimdAbrController, TlsRootConfig};
use core_types::clock::{MasterClock, PtsUs, MASTER_SAMPLE_RATE};
use core_types::media::{CodecKind, EncodedPacket, LayerTransform};
use core_types::traits::{AudioEncoder, VideoEncoder};
use core_types::CoreResult;
use std::collections::{BTreeMap, VecDeque};
use std::ffi::{c_char, c_void, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

#[cfg(any(target_os = "macos", target_os = "ios"))]
use apple_common::audio_toolbox::AudioToolboxAacEncoder;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use apple_common::cv_metal::{
    AppleMetalZeroCopyImporter, AppleZeroCopyRenderSurfacePool, CVPixelBufferWrapper,
    ImportedGpuLayer,
};
#[cfg(any(target_os = "macos", target_os = "ios"))]
use apple_common::video_toolbox::VtH264Encoder;

/// C-ABI `IkakimEngineConfig`
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct IkakimEngineConfig {
    pub main_width: u32,
    pub main_height: u32,
    pub preview_width: u32,
    pub preview_height: u32,
    pub target_fps: u32,
    pub initial_bitrate_bps: u32,
    pub http_port: u16,
}

/// C-ABI `IkakimLayerTransform`
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct IkakimLayerTransform {
    pub x: f32,
    pub y: f32,
    pub scale_x: f32,
    pub scale_y: f32,
    pub rotation_rad: f32,
    pub opacity: f32,
    pub z_order: i32,
    pub visible: bool,
}

/// C-ABI `IkakimEngineStats`
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct IkakimEngineStats {
    pub ticks: u64,
    pub ui_port: u16,
    pub current_bitrate_bps: u32,
    pub encoded_video_packets: u64,
    pub encoded_audio_packets: u64,
    pub active_gpu_layers: u32,
    pub streaming_active: bool,
    pub apple_zero_copy_active: bool,
    pub android_ahb_active: bool,
}

/// Fallback AAC-LC packetizer used when OS AAC encoders (AudioToolbox / MediaCodec / FDK-AAC)
/// are unavailable (e.g. generic Linux VM / unit-test / CI host without OS codecs).
struct FallbackAacEncoder {
    seq_sent: bool,
    queue: VecDeque<EncodedPacket>,
}

impl FallbackAacEncoder {
    fn new() -> Self {
        Self {
            seq_sent: false,
            queue: VecDeque::new(),
        }
    }
}

impl AudioEncoder for FallbackAacEncoder {
    fn push_pcm_f32(&mut self, interleaved_pcm: &[f32], pts: PtsUs) -> CoreResult<()> {
        if !self.seq_sent {
            self.seq_sent = true;
            // AudioSpecificConfig: AAC-LC (2), 48kHz (idx 3), 2ch (2) => 0x11 0x90
            self.queue.push_back(EncodedPacket {
                codec: CodecKind::AacLc,
                pts,
                dts: pts,
                is_keyframe: true,
                is_sequence_header: true,
                data: vec![0x11, 0x90],
            });
        }
        if !interleaved_pcm.is_empty() {
            // Minimal valid raw AAC silent payload placeholder (7 bytes)
            self.queue.push_back(EncodedPacket {
                codec: CodecKind::AacLc,
                pts,
                dts: pts,
                is_keyframe: true,
                is_sequence_header: false,
                data: vec![0x21, 0x10, 0x04, 0x60, 0x8C, 0x1C, 0x00],
            });
        }
        Ok(())
    }

    fn pull_packet(&mut self) -> CoreResult<Option<EncodedPacket>> {
        Ok(self.queue.pop_front())
    }
}

enum PlatformAudioEncoder {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    AudioToolbox(AudioToolboxAacEncoder),
    Fallback(FallbackAacEncoder),
}

impl PlatformAudioEncoder {
    fn new(bitrate_bps: u32) -> Self {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            if let Ok(enc) = AudioToolboxAacEncoder::new(bitrate_bps) {
                return Self::AudioToolbox(enc);
            }
        }
        let _ = bitrate_bps;
        Self::Fallback(FallbackAacEncoder::new())
    }

    fn as_dyn_mut(&mut self) -> &mut dyn AudioEncoder {
        match self {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            Self::AudioToolbox(enc) => enc,
            Self::Fallback(enc) => enc,
        }
    }
}

enum PlatformVideoEncoder {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    VideoToolbox(VtH264Encoder),
    Software(SwH264Encoder),
}

impl PlatformVideoEncoder {
    fn new(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Self {
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            if let Ok(vt) = VtH264Encoder::new(width, height, fps, bitrate_bps) {
                return Self::VideoToolbox(vt);
            }
        }
        let _ = fps;
        Self::Software(SwH264Encoder::new(width, height, bitrate_bps))
    }

    fn as_dyn_mut(&mut self) -> &mut dyn VideoEncoder {
        match self {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            Self::VideoToolbox(enc) => enc,
            Self::Software(enc) => enc,
        }
    }

    fn encode_rgba_frame(
        &mut self,
        width: u32,
        height: u32,
        rgba: &[u8],
        pts: PtsUs,
        bgra_scratch: &mut Vec<u8>,
    ) -> CoreResult<()> {
        match self {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            Self::VideoToolbox(vt) => {
                bgra_scratch.resize(rgba.len(), 0);
                for (src, dst) in rgba.chunks_exact(4).zip(bgra_scratch.chunks_exact_mut(4)) {
                    dst[0] = src[2];
                    dst[1] = src[1];
                    dst[2] = src[0];
                    dst[3] = src[3];
                }
                let pb = CVPixelBufferWrapper::new_bgra(width, height, bgra_scratch)?;
                vt.encode_pixel_buffer(&pb, pts)
            }
            Self::Software(sw) => {
                let _ = (width, height, bgra_scratch);
                sw.encode_rgba(rgba, pts)
            }
        }
    }
}

struct CountingVideoEncoder<'a> {
    inner: &'a mut dyn VideoEncoder,
    count: &'a mut u64,
}

impl VideoEncoder for CountingVideoEncoder<'_> {
    fn set_target_bitrate(&mut self, bitrate_bps: u32) -> CoreResult<()> {
        self.inner.set_target_bitrate(bitrate_bps)
    }

    fn request_keyframe(&mut self) {
        self.inner.request_keyframe();
    }

    fn pull_packet(&mut self) -> CoreResult<Option<EncodedPacket>> {
        let pkt = self.inner.pull_packet()?;
        if pkt.is_some() {
            *self.count += 1;
        }
        Ok(pkt)
    }
}

struct CountingAudioEncoder<'a> {
    inner: &'a mut dyn AudioEncoder,
    count: &'a mut u64,
}

impl AudioEncoder for CountingAudioEncoder<'_> {
    fn push_pcm_f32(&mut self, interleaved_pcm: &[f32], pts: PtsUs) -> CoreResult<()> {
        self.inner.push_pcm_f32(interleaved_pcm, pts)
    }

    fn pull_packet(&mut self) -> CoreResult<Option<EncodedPacket>> {
        let pkt = self.inner.pull_packet()?;
        if pkt.is_some() {
            *self.count += 1;
        }
        Ok(pkt)
    }
}

enum StoredLayer {
    Cpu(GpuLayer),
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    AppleZeroCopy(ImportedGpuLayer),
}

impl StoredLayer {
    fn as_layer(&self) -> &GpuLayer {
        match self {
            Self::Cpu(l) => l,
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            Self::AppleZeroCopy(imp) => &imp.layer,
        }
    }

    fn as_layer_mut(&mut self) -> &mut GpuLayer {
        match self {
            Self::Cpu(l) => l,
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            Self::AppleZeroCopy(imp) => &mut imp.layer,
        }
    }
}

/// Complete native streaming engine owned behind `IkakimEngineHandle`.
pub struct IkakimEngineHandle {
    clock: MasterClock,
    mixer: AudioMixer,
    audio_producers: Vec<AudioInputProducer>,
    bridge: Arc<EngineControlBridge>,
    server: EmbeddedServer,
    compositor: GpuCompositor,
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    apple_importer: Option<AppleMetalZeroCopyImporter>,
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    apple_main_pool: Option<AppleZeroCopyRenderSurfacePool>,
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    apple_preview_pool: Option<AppleZeroCopyRenderSurfacePool>,
    layers: BTreeMap<usize, StoredLayer>,
    ahb_descriptors: BTreeMap<usize, AhbDescriptor>,
    main_encoder: PlatformVideoEncoder,
    preview_encoder: PlatformVideoEncoder,
    audio_encoder: PlatformAudioEncoder,
    abr: AimdAbrController,
    pcm_scratch: Vec<f32>,
    bgra_scratch: Vec<u8>,
    ticks: u64,
    encoded_video_packets: u64,
    encoded_audio_packets: u64,
    streaming_active: bool,
    apple_zero_copy_active: bool,
    android_ahb_active: bool,
    last_error: Option<String>,
}

impl IkakimEngineHandle {
    pub fn try_new(cfg: IkakimEngineConfig) -> CoreResult<Self> {
        let main_w = cfg.main_width.max(16);
        let main_h = cfg.main_height.max(16);
        let prev_w = cfg.preview_width.max(16);
        let prev_h = cfg.preview_height.max(16);
        let fps = cfg.target_fps.max(1);
        let initial_bitrate = cfg.initial_bitrate_bps.clamp(400_000, 12_000_000);

        let clock = MasterClock::new();
        let mut mixer = AudioMixer::new(clock.clone());
        let (ch0_handle, ch0_prod) = mixer.add_channel(AudioInputConfig {
            name: "Mic / Device".into(),
            sample_rate: MASTER_SAMPLE_RATE as u32,
            channels: 2,
            initial_gain: 1.0,
            initial_muted: false,
            initial_delay_ms: 0,
        });
        let (ch1_handle, ch1_prod) = mixer.add_channel(AudioInputConfig {
            name: "External".into(),
            sample_rate: MASTER_SAMPLE_RATE as u32,
            channels: 2,
            initial_gain: 1.0,
            initial_muted: false,
            initial_delay_ms: 0,
        });
        let master = mixer.master_handle();
        let bridge = EngineControlBridge::new(
            vec![ch0_handle, ch1_handle],
            master,
            initial_bitrate,
            fps as f32,
            TlsRootConfig::System,
        );

        let backend_dyn: Arc<dyn ControlBackend> = Arc::clone(&bridge) as Arc<dyn ControlBackend>;
        let server = EmbeddedServer::start(
            ServerConfig {
                bind_addr: format!("127.0.0.1:{}", cfg.http_port),
                meter_interval_ms: 33,
                preview_width: prev_w,
                preview_height: prev_h,
                preview_fps: fps.min(30),
            },
            backend_dyn,
        )?;

        let gpu_ctx = GpuContext::new_headless()?;
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let apple_importer = AppleMetalZeroCopyImporter::new(&gpu_ctx.device).ok();
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let mut apple_main_pool = apple_importer.as_ref().and_then(|imp| {
            AppleZeroCopyRenderSurfacePool::new(imp, &gpu_ctx.device, main_w, main_h, 6).ok()
        });
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let apple_preview_pool = apple_importer.as_ref().and_then(|imp| {
            AppleZeroCopyRenderSurfacePool::new(imp, &gpu_ctx.device, prev_w, prev_h, 6).ok()
        });

        let compositor = GpuCompositor::new(
            gpu_ctx,
            CompositorConfig {
                main_width: main_w,
                main_height: main_h,
                preview_width: prev_w,
                preview_height: prev_h,
                target_fps: fps,
            },
        );
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if let Some(ref mut pool) = apple_main_pool {
            pool.prebind_preview_blit(&compositor);
        }

        let main_encoder = PlatformVideoEncoder::new(main_w, main_h, fps, initial_bitrate);
        let preview_encoder = PlatformVideoEncoder::new(prev_w, prev_h, fps.min(30), 1_000_000);
        let audio_encoder = PlatformAudioEncoder::new(128_000);
        let abr = AimdAbrController::new(AbrConfig {
            initial_bitrate_bps: initial_bitrate,
            ..AbrConfig::default()
        });
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        let apple_zero_copy_active = apple_main_pool.is_some()
            && apple_preview_pool.is_some()
            && matches!(main_encoder, PlatformVideoEncoder::VideoToolbox(_));
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        let apple_zero_copy_active = false;

        Ok(Self {
            clock,
            mixer,
            audio_producers: vec![ch0_prod, ch1_prod],
            bridge,
            server,
            compositor,
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            apple_importer,
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            apple_main_pool,
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            apple_preview_pool,
            layers: BTreeMap::new(),
            ahb_descriptors: BTreeMap::new(),
            main_encoder,
            preview_encoder,
            audio_encoder,
            abr,
            pcm_scratch: vec![0.0f32; MIX_BLOCK_FRAMES * 2],
            bgra_scratch: Vec::new(),
            ticks: 0,
            encoded_video_packets: 0,
            encoded_audio_packets: 0,
            streaming_active: false,
            apple_zero_copy_active,
            android_ahb_active: false,
            last_error: None,
        })
    }

    fn set_err(&mut self, msg: String) -> i32 {
        self.last_error = Some(msg);
        -1
    }

    fn submit_rgba(
        &mut self,
        layer_id: usize,
        is_base_layer: bool,
        top_bottom_alpha: bool,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> CoreResult<()> {
        let transform = self
            .layers
            .get(&layer_id)
            .map(|l| l.as_layer().transform)
            .unwrap_or_else(|| LayerTransform {
                z_order: if is_base_layer { 0 } else { layer_id as i32 },
                ..Default::default()
            });
        let layer = self.compositor.create_rgba_layer(
            layer_id,
            is_base_layer,
            top_bottom_alpha,
            width,
            height,
            rgba,
            transform,
        )?;
        self.layers.insert(layer_id, StoredLayer::Cpu(layer));
        Ok(())
    }

    fn step_tick(&mut self, queued_rtmp_bytes: usize, elapsed_ms: u64) -> CoreResult<()> {
        let block_start = self.mixer.mix_block(&mut self.pcm_scratch);
        let pts = block_start.to_pts_us();
        self.audio_encoder
            .as_dyn_mut()
            .push_pcm_f32(&self.pcm_scratch, pts)?;

        // Sync UI overlay transforms to GPU layers
        {
            let mut mut_refs: Vec<&mut GpuLayer> =
                self.layers.values_mut().map(StoredLayer::as_layer_mut).collect();
            self.bridge
                .apply_pending_transforms(&self.compositor, &mut mut_refs);
        }

        // Evaluate AIMD ABR first so we know whether non-IDR encode should be skipped
        let abr_decision = self.abr.evaluate(queued_rtmp_bytes, elapsed_ms);
        if let Some(new_bps) = abr_decision.new_bitrate_bps {
            self.bridge
                .apply_abr_bitrate(new_bps, self.main_encoder.as_dyn_mut())?;
        }
        if abr_decision.request_keyframe {
            self.main_encoder.as_dyn_mut().request_keyframe();
        }

        // Render & encode main + preview targets (Apple Metal->IOSurface->VT zero-copy when active)
        #[allow(unused_mut)]
        let mut rendered_via_apple_zero_copy = false;
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        if let (
            Some(imp),
            Some(main_pool),
            Some(prev_pool),
            PlatformVideoEncoder::VideoToolbox(main_vt),
            PlatformVideoEncoder::VideoToolbox(prev_vt),
        ) = (
            self.apple_importer.as_ref(),
            self.apple_main_pool.as_mut(),
            self.apple_preview_pool.as_mut(),
            &mut self.main_encoder,
            &mut self.preview_encoder,
        ) {
            let main_slot = main_pool.acquire_slot(imp, &self.compositor.context().device)?;
            let prev_slot = prev_pool.acquire_slot(imp, &self.compositor.context().device)?;
            {
                let ro_refs: Vec<&GpuLayer> =
                    self.layers.values().map(StoredLayer::as_layer).collect();
                let _stats = self.compositor.render_dual_into(
                    &ro_refs,
                    pts,
                    Some(main_slot.as_external_target()),
                    Some(prev_slot.as_external_target()),
                )?;
            }
            if !abr_decision.drop_non_idr {
                main_vt.encode_pixel_buffer(&main_slot.pixel_buffer, pts)?;
            }
            prev_vt.encode_pixel_buffer(&prev_slot.pixel_buffer, pts)?;
            rendered_via_apple_zero_copy = true;
            self.apple_zero_copy_active = true;
        }

        if !rendered_via_apple_zero_copy {
            {
                let ro_refs: Vec<&GpuLayer> =
                    self.layers.values().map(StoredLayer::as_layer).collect();
                let _stats = self.compositor.render_dual(&ro_refs, pts)?;
            }

            let (mw, mh) = self.compositor.main_size();
            if !abr_decision.drop_non_idr {
                let main_rgba = self
                    .compositor
                    .readback_texture_rgba(self.compositor.main_texture(), mw, mh)?;
                self.main_encoder.encode_rgba_frame(
                    mw,
                    mh,
                    &main_rgba,
                    pts,
                    &mut self.bgra_scratch,
                )?;
            }

            let (pw, ph) = self.compositor.preview_size();
            let prev_rgba = self
                .compositor
                .readback_texture_rgba(self.compositor.preview_texture(), pw, ph)?;
            self.preview_encoder.encode_rgba_frame(
                pw,
                ph,
                &prev_rgba,
                pts,
                &mut self.bgra_scratch,
            )?;
        }

        let mut main_video_pkts = 0u64;
        let mut prev_video_pkts = 0u64;
        let mut audio_pkts = 0u64;
        {
            let mut main_wrap = CountingVideoEncoder {
                inner: self.main_encoder.as_dyn_mut(),
                count: &mut main_video_pkts,
            };
            let mut prev_wrap = CountingVideoEncoder {
                inner: self.preview_encoder.as_dyn_mut(),
                count: &mut prev_video_pkts,
            };
            let mut audio_wrap = CountingAudioEncoder {
                inner: self.audio_encoder.as_dyn_mut(),
                count: &mut audio_pkts,
            };
            let mut tick_ctx = PipelineTickContext {
                clock: &mut self.clock,
                bridge: &self.bridge,
                compositor: &mut self.compositor,
                main_encoder: &mut main_wrap,
                preview_encoder: &mut prev_wrap,
                audio_encoder: &mut audio_wrap,
                server: &self.server,
            };
            flush_encoders_to_outputs(&mut tick_ctx, pts)?;
        }

        self.encoded_video_packets += main_video_pkts + prev_video_pkts;
        self.encoded_audio_packets += audio_pkts;
        self.ticks += 1;
        Ok(())
    }
}

impl Drop for IkakimEngineHandle {
    fn drop(&mut self) {
        self.bridge.stop_all();
        self.server.stop();
    }
}

#[no_mangle]
pub extern "C" fn ikakim_engine_default_config() -> IkakimEngineConfig {
    IkakimEngineConfig {
        main_width: 1920,
        main_height: 1080,
        preview_width: 1280,
        preview_height: 720,
        target_fps: 60,
        initial_bitrate_bps: 4_000_000,
        http_port: 0,
    }
}

#[no_mangle]
/// # Safety
/// `config` must either be null or point to a valid `IkakimEngineConfig`.
pub unsafe extern "C" fn ikakim_engine_create(
    config: *const IkakimEngineConfig,
) -> *mut IkakimEngineHandle {
    catch_unwind(AssertUnwindSafe(|| {
        let cfg = if config.is_null() {
            ikakim_engine_default_config()
        } else {
            unsafe { *config }
        };
        match IkakimEngineHandle::try_new(cfg) {
            Ok(engine) => Box::into_raw(Box::new(engine)),
            Err(_) => std::ptr::null_mut(),
        }
    }))
    .unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
/// # Safety
/// `handle` must be null or a pointer previously returned by `ikakim_engine_create` that has not yet been freed.
pub unsafe extern "C" fn ikakim_engine_destroy(handle: *mut IkakimEngineHandle) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !handle.is_null() {
            unsafe {
                drop(Box::from_raw(handle));
            }
        }
    }));
}

#[no_mangle]
/// # Safety
/// `handle` must be null or point to a valid `IkakimEngineHandle`.
pub unsafe extern "C" fn ikakim_engine_ui_port(handle: *const IkakimEngineHandle) -> u16 {
    catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() {
            return 0;
        }
        unsafe { &*handle }.server.local_addr().port()
    }))
    .unwrap_or(0)
}

#[no_mangle]
/// # Safety
/// `handle` and `rgba_ptr` must be valid pointers covering `rgba_len` readable bytes.
pub unsafe extern "C" fn ikakim_engine_submit_rgba_layer(
    handle: *mut IkakimEngineHandle,
    layer_id: u32,
    is_base_layer: bool,
    top_bottom_alpha: bool,
    width: u32,
    height: u32,
    rgba_ptr: *const u8,
    rgba_len: usize,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() || rgba_ptr.is_null() || width == 0 || height == 0 {
            return -1;
        }
        let eng = unsafe { &mut *handle };
        let rgba = unsafe { std::slice::from_raw_parts(rgba_ptr, rgba_len) };
        match eng.submit_rgba(
            layer_id as usize,
            is_base_layer,
            top_bottom_alpha,
            width,
            height,
            rgba,
        ) {
            Ok(()) => 0,
            Err(e) => eng.set_err(e.to_string()),
        }
    }))
    .unwrap_or(-99)
}

#[no_mangle]
/// # Safety
/// `handle` must be a valid `IkakimEngineHandle`; on Apple OSes `cv_pixel_buffer_ref` must be a valid `CVPixelBufferRef`.
pub unsafe extern "C" fn ikakim_engine_submit_cv_pixel_buffer(
    handle: *mut IkakimEngineHandle,
    layer_id: u32,
    is_base_layer: bool,
    cv_pixel_buffer_ref: *mut c_void,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() || cv_pixel_buffer_ref.is_null() {
            return -1;
        }
        let eng = unsafe { &mut *handle };
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            let Some(importer) = eng.apple_importer.as_ref() else {
                return eng.set_err("AppleMetalZeroCopyImporter unavailable".into());
            };
            let pb = match unsafe { CVPixelBufferWrapper::from_retained(cv_pixel_buffer_ref) } {
                Ok(pb) => pb,
                Err(e) => return eng.set_err(e.to_string()),
            };
            if let Some(StoredLayer::AppleZeroCopy(existing)) =
                eng.layers.get_mut(&(layer_id as usize))
            {
                return match importer.update_gpu_layer(&eng.compositor, existing, &pb) {
                    Ok(()) => {
                        eng.apple_zero_copy_active = true;
                        0
                    }
                    Err(e) => eng.set_err(e.to_string()),
                };
            }
            let transform = eng
                .layers
                .get(&(layer_id as usize))
                .map(|l| l.as_layer().transform)
                .unwrap_or_else(|| LayerTransform {
                    z_order: if is_base_layer { 0 } else { layer_id as i32 },
                    ..Default::default()
                });
            match importer.import_to_gpu_layer(
                &eng.compositor,
                &pb,
                layer_id as usize,
                is_base_layer,
                transform,
            ) {
                Ok(imported) => {
                    eng.layers
                        .insert(layer_id as usize, StoredLayer::AppleZeroCopy(imported));
                    eng.apple_zero_copy_active = true;
                    0
                }
                Err(e) => eng.set_err(e.to_string()),
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        {
            let _ = (eng, layer_id, is_base_layer, cv_pixel_buffer_ref);
            -2
        }
    }))
    .unwrap_or(-99)
}

#[no_mangle]
/// # Safety
/// `handle` must be a valid `IkakimEngineHandle`.
pub unsafe extern "C" fn ikakim_engine_submit_ahardware_buffer_meta(
    handle: *mut IkakimEngineHandle,
    layer_id: u32,
    is_base_layer: bool,
    buffer_id: u64,
    width: u32,
    height: u32,
    android_format: u32,
    usage_flags: u64,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() {
            return -1;
        }
        let eng = unsafe { &mut *handle };
        let desc = AhbDescriptor {
            buffer_id,
            width,
            height,
            android_format,
            usage_flags,
        };
        if let Err(msg) = desc.validate() {
            return eng.set_err(msg.into());
        }
        let _ = is_base_layer;
        eng.ahb_descriptors.insert(layer_id as usize, desc);
        eng.android_ahb_active = true;
        0
    }))
    .unwrap_or(-99)
}

#[no_mangle]
/// # Safety
/// `handle` and `interleaved_pcm` must be valid pointers covering `sample_count` readable `f32`s.
pub unsafe extern "C" fn ikakim_engine_push_audio_f32(
    handle: *mut IkakimEngineHandle,
    channel_index: u32,
    interleaved_pcm: *const f32,
    sample_count: usize,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() || interleaved_pcm.is_null() {
            return -1;
        }
        let eng = unsafe { &mut *handle };
        let Some(prod) = eng.audio_producers.get_mut(channel_index as usize) else {
            return eng.set_err(format!("invalid audio channel {}", channel_index));
        };
        let slice = unsafe { std::slice::from_raw_parts(interleaved_pcm, sample_count) };
        prod.push_f32(slice);
        0
    }))
    .unwrap_or(-99)
}

#[no_mangle]
/// # Safety
/// `handle` and `transform` must point to valid structs.
pub unsafe extern "C" fn ikakim_engine_set_layer_transform(
    handle: *mut IkakimEngineHandle,
    layer_id: u32,
    transform: *const IkakimLayerTransform,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() || transform.is_null() {
            return -1;
        }
        let eng = unsafe { &mut *handle };
        let tr = unsafe { *transform };
        let lt = LayerTransform {
            x: tr.x,
            y: tr.y,
            scale_x: tr.scale_x,
            scale_y: tr.scale_y,
            rotation_rad: tr.rotation_rad,
            opacity: tr.opacity,
            z_order: tr.z_order,
            visible: tr.visible,
        };
        match eng.bridge.set_overlay_transform(layer_id as usize, lt) {
            Ok(()) => 0,
            Err(e) => eng.set_err(e),
        }
    }))
    .unwrap_or(-99)
}

#[no_mangle]
/// # Safety
/// `handle` must be a valid `IkakimEngineHandle`.
pub unsafe extern "C" fn ikakim_engine_step_tick(
    handle: *mut IkakimEngineHandle,
    queued_rtmp_bytes: u64,
    elapsed_ms: u64,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() {
            return -1;
        }
        let eng = unsafe { &mut *handle };
        match eng.step_tick(queued_rtmp_bytes as usize, elapsed_ms) {
            Ok(()) => 0,
            Err(e) => eng.set_err(e.to_string()),
        }
    }))
    .unwrap_or(-99)
}

#[no_mangle]
/// # Safety
/// `handle` must be valid and `url_utf8` must be a valid NUL-terminated UTF-8 C string.
pub unsafe extern "C" fn ikakim_engine_start_rtmps(
    handle: *mut IkakimEngineHandle,
    url_utf8: *const c_char,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() || url_utf8.is_null() {
            return -1;
        }
        let eng = unsafe { &mut *handle };
        let url_str = match unsafe { CStr::from_ptr(url_utf8) }.to_str() {
            Ok(s) => s,
            Err(_) => return eng.set_err("invalid UTF-8 RTMP URL".into()),
        };
        match eng.bridge.start_stream(url_str, None) {
            Ok(()) => {
                eng.streaming_active = true;
                0
            }
            Err(e) => eng.set_err(e),
        }
    }))
    .unwrap_or(-99)
}

#[no_mangle]
/// # Safety
/// `handle` must be a valid `IkakimEngineHandle`.
pub unsafe extern "C" fn ikakim_engine_stop_rtmps(handle: *mut IkakimEngineHandle) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() {
            return -1;
        }
        let eng = unsafe { &mut *handle };
        match eng.bridge.stop_stream() {
            Ok(()) => {
                eng.streaming_active = false;
                0
            }
            Err(e) => eng.set_err(e),
        }
    }))
    .unwrap_or(-99)
}

#[no_mangle]
/// # Safety
/// `handle` must be a valid `IkakimEngineHandle`.
pub unsafe extern "C" fn ikakim_engine_request_keyframe(handle: *mut IkakimEngineHandle) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() {
            return -1;
        }
        let eng = unsafe { &mut *handle };
        eng.main_encoder.as_dyn_mut().request_keyframe();
        0
    }))
    .unwrap_or(-99)
}

#[no_mangle]
/// # Safety
/// `handle` and `out_stats` must be valid pointers.
pub unsafe extern "C" fn ikakim_engine_get_stats(
    handle: *const IkakimEngineHandle,
    out_stats: *mut IkakimEngineStats,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() || out_stats.is_null() {
            return -1;
        }
        let eng = unsafe { &*handle };
        let stats = IkakimEngineStats {
            ticks: eng.ticks,
            ui_port: eng.server.local_addr().port(),
            current_bitrate_bps: eng.abr.current_bitrate_bps(),
            encoded_video_packets: eng.encoded_video_packets,
            encoded_audio_packets: eng.encoded_audio_packets,
            active_gpu_layers: eng.layers.len() as u32,
            streaming_active: eng.streaming_active,
            apple_zero_copy_active: eng.apple_zero_copy_active,
            android_ahb_active: eng.android_ahb_active,
        };
        unsafe {
            *out_stats = stats;
        }
        0
    }))
    .unwrap_or(-99)
}

#[no_mangle]
/// # Safety
/// `handle` and `buf` must be valid pointers with `buf_len` writable bytes.
pub unsafe extern "C" fn ikakim_engine_last_error(
    handle: *const IkakimEngineHandle,
    buf: *mut c_char,
    buf_len: usize,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if handle.is_null() || buf.is_null() || buf_len == 0 {
            return 0;
        }
        let eng = unsafe { &*handle };
        let Some(msg) = eng.last_error.as_deref() else {
            unsafe {
                *buf = 0;
            }
            return 0;
        };
        let bytes = msg.as_bytes();
        let copy_len = bytes.len().min(buf_len - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.cast::<u8>(), copy_len);
            *buf.add(copy_len) = 0;
        }
        copy_len as i32
    }))
    .unwrap_or(0)
}
