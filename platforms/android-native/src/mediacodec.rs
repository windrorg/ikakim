//! Android `AMediaCodec` (`createInputSurface`) Dual H.264 Hardware Encoder Bridge (M4: Issue #8)

use core_types::clock::PtsUs;
use core_types::media::{CodecKind, EncodedPacket};
use core_types::traits::VideoEncoder;
use core_types::{CoreError, CoreResult};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

pub const AMEDIAFORMAT_KEY_VIDEO_BITRATE: &str = "video-bitrate";
pub const AMEDIACODEC_PARAMETER_KEY_REQUEST_SYNC_FRAME: &str = "request-sync";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCodecStreamRole {
    MainBroadcast,
    PreviewConsole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaCodecEncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub initial_bitrate_bps: u32,
    pub role: MediaCodecStreamRole,
}

/// Abstract backend for Android NDK `AMediaCodec` (`libmediandk.so`).
pub trait MediaCodecBackend: Send + Sync {
    fn create_encoder(&self, cfg: &MediaCodecEncoderConfig) -> CoreResult<u64>;
    /// Calls `AMediaCodec_createInputSurface` returning an `ANativeWindow*` / surface handle
    /// directly connected to GPU/EGL/Vulkan output without CPU readback.
    fn create_input_surface(&self, codec_id: u64) -> CoreResult<u64>;
    fn set_video_bitrate(&self, codec_id: u64, bitrate_bps: u32) -> CoreResult<()>;
    fn request_sync_frame(&self, codec_id: u64) -> CoreResult<()>;
    fn submit_surface_frame(&self, codec_id: u64, pts: PtsUs) -> CoreResult<()>;
    fn dequeue_encoded_packet(&self, codec_id: u64) -> CoreResult<Option<EncodedPacket>>;
    fn destroy_encoder(&self, codec_id: u64);
}

#[derive(Debug)]
struct MockCodecSlot {
    cfg: MediaCodecEncoderConfig,
    input_surface_id: Option<u64>,
    current_bitrate_bps: u32,
    bitrate_history: Vec<u32>,
    pending_sync_frame: bool,
    seq_header_emitted: bool,
    packets: VecDeque<EncodedPacket>,
}

/// Deterministic mock `AMediaCodec` backend simulating `createInputSurface` + ABR + IDR request.
#[derive(Debug, Default)]
pub struct MockMediaCodecBackend {
    state: Mutex<(u64, HashMap<u64, MockCodecSlot>)>,
}

impl MockMediaCodecBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bitrate_history(&self, codec_id: u64) -> Vec<u32> {
        self.state
            .lock()
            .unwrap()
            .1
            .get(&codec_id)
            .map(|s| s.bitrate_history.clone())
            .unwrap_or_default()
    }

    pub fn input_surface_id(&self, codec_id: u64) -> Option<u64> {
        self.state
            .lock()
            .unwrap()
            .1
            .get(&codec_id)
            .and_then(|s| s.input_surface_id)
    }
}

impl MediaCodecBackend for MockMediaCodecBackend {
    fn create_encoder(&self, cfg: &MediaCodecEncoderConfig) -> CoreResult<u64> {
        if cfg.width < 16 || cfg.height < 16 {
            return Err(CoreError::Codec("MediaCodec dimensions must be >= 16".into()));
        }
        let mut guard = self.state.lock().unwrap();
        guard.0 += 1;
        let id = guard.0;
        guard.1.insert(
            id,
            MockCodecSlot {
                cfg: cfg.clone(),
                input_surface_id: None,
                current_bitrate_bps: cfg.initial_bitrate_bps,
                bitrate_history: vec![cfg.initial_bitrate_bps],
                pending_sync_frame: true,
                seq_header_emitted: false,
                packets: VecDeque::new(),
            },
        );
        Ok(id)
    }

    fn create_input_surface(&self, codec_id: u64) -> CoreResult<u64> {
        let mut guard = self.state.lock().unwrap();
        let slot = guard
            .1
            .get_mut(&codec_id)
            .ok_or_else(|| CoreError::Codec(format!("unknown MediaCodec {}", codec_id)))?;
        let surface_handle = 0x7000_0000 | codec_id;
        slot.input_surface_id = Some(surface_handle);
        Ok(surface_handle)
    }

    fn set_video_bitrate(&self, codec_id: u64, bitrate_bps: u32) -> CoreResult<()> {
        let mut guard = self.state.lock().unwrap();
        let slot = guard
            .1
            .get_mut(&codec_id)
            .ok_or_else(|| CoreError::Codec(format!("unknown MediaCodec {}", codec_id)))?;
        slot.current_bitrate_bps = bitrate_bps;
        slot.bitrate_history.push(bitrate_bps);
        Ok(())
    }

    fn request_sync_frame(&self, codec_id: u64) -> CoreResult<()> {
        let mut guard = self.state.lock().unwrap();
        let slot = guard
            .1
            .get_mut(&codec_id)
            .ok_or_else(|| CoreError::Codec(format!("unknown MediaCodec {}", codec_id)))?;
        slot.pending_sync_frame = true;
        Ok(())
    }

    fn submit_surface_frame(&self, codec_id: u64, pts: PtsUs) -> CoreResult<()> {
        let mut guard = self.state.lock().unwrap();
        let slot = guard
            .1
            .get_mut(&codec_id)
            .ok_or_else(|| CoreError::Codec(format!("unknown MediaCodec {}", codec_id)))?;
        if slot.input_surface_id.is_none() {
            return Err(CoreError::Codec(
                "createInputSurface must be bound before submitting frames".into(),
            ));
        }

        if !slot.seq_header_emitted {
            slot.seq_header_emitted = true;
            // Minimal Annex B SPS + PPS
            slot.packets.push_back(EncodedPacket {
                codec: CodecKind::H264,
                pts,
                dts: pts,
                is_keyframe: true,
                is_sequence_header: true,
                data: vec![0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xC0, 0x1F, 0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x3C, 0x80],
            });
        }

        let is_idr = slot.pending_sync_frame;
        slot.pending_sync_frame = false;
        let nal_byte = if is_idr { 0x65 } else { 0x41 };
        let marker = (slot.cfg.width as u8) ^ (slot.current_bitrate_bps >> 12) as u8;
        slot.packets.push_back(EncodedPacket {
            codec: CodecKind::H264,
            pts,
            dts: pts,
            is_keyframe: is_idr,
            is_sequence_header: false,
            data: vec![0x00, 0x00, 0x00, 0x01, nal_byte, 0x88, marker],
        });
        Ok(())
    }

    fn dequeue_encoded_packet(&self, codec_id: u64) -> CoreResult<Option<EncodedPacket>> {
        let mut guard = self.state.lock().unwrap();
        let slot = guard
            .1
            .get_mut(&codec_id)
            .ok_or_else(|| CoreError::Codec(format!("unknown MediaCodec {}", codec_id)))?;
        Ok(slot.packets.pop_front())
    }

    fn destroy_encoder(&self, codec_id: u64) {
        self.state.lock().unwrap().1.remove(&codec_id);
    }
}

/// Real Android NDK `libmediandk.so` dynamic symbol probe (`#[cfg(target_os = "android")]`).
#[cfg(target_os = "android")]
pub struct NdkMediaCodecLibrary {
    pub handle: *mut std::ffi::c_void,
    pub create_encoder_sym: *mut std::ffi::c_void,
    pub create_input_surface_sym: *mut std::ffi::c_void,
    pub set_parameters_sym: *mut std::ffi::c_void,
}

#[cfg(target_os = "android")]
impl NdkMediaCodecLibrary {
    pub fn probe() -> CoreResult<Self> {
        extern "C" {
            fn dlopen(filename: *const i8, flag: i32) -> *mut std::ffi::c_void;
            fn dlsym(handle: *mut std::ffi::c_void, symbol: *const i8) -> *mut std::ffi::c_void;
        }
        unsafe {
            let handle = dlopen(b"libmediandk.so\0".as_ptr().cast(), 2);
            if handle.is_null() {
                return Err(CoreError::PlatformIo("dlopen libmediandk.so failed".into()));
            }
            let create_encoder_sym = dlsym(handle, b"AMediaCodec_createEncoderByType\0".as_ptr().cast());
            let create_input_surface_sym = dlsym(handle, b"AMediaCodec_createInputSurface\0".as_ptr().cast());
            let set_parameters_sym = dlsym(handle, b"AMediaCodec_setParameters\0".as_ptr().cast());
            if create_encoder_sym.is_null()
                || create_input_surface_sym.is_null()
                || set_parameters_sym.is_null()
            {
                return Err(CoreError::PlatformIo(
                    "required AMediaCodec symbols missing in libmediandk.so".into(),
                ));
            }
            Ok(Self {
                handle,
                create_encoder_sym,
                create_input_surface_sym,
                set_parameters_sym,
            })
        }
    }
}

/// Single `AMediaCodec` hardware H.264 encoder bound to a persistent `ANativeWindow` input surface.
pub struct AndroidMediaCodecEncoder {
    backend: Arc<dyn MediaCodecBackend>,
    codec_id: u64,
    input_surface_handle: u64,
    cfg: MediaCodecEncoderConfig,
}

impl AndroidMediaCodecEncoder {
    pub fn new(
        backend: Arc<dyn MediaCodecBackend>,
        cfg: MediaCodecEncoderConfig,
    ) -> CoreResult<Self> {
        let codec_id = backend.create_encoder(&cfg)?;
        let input_surface_handle = backend.create_input_surface(codec_id)?;
        Ok(Self {
            backend,
            codec_id,
            input_surface_handle,
            cfg,
        })
    }

    pub fn codec_id(&self) -> u64 {
        self.codec_id
    }

    pub fn input_surface_handle(&self) -> u64 {
        self.input_surface_handle
    }

    pub fn config(&self) -> &MediaCodecEncoderConfig {
        &self.cfg
    }

    pub fn submit_surface_frame(&mut self, pts: PtsUs) -> CoreResult<()> {
        self.backend.submit_surface_frame(self.codec_id, pts)
    }
}

impl VideoEncoder for AndroidMediaCodecEncoder {
    fn set_target_bitrate(&mut self, bitrate_bps: u32) -> CoreResult<()> {
        self.backend.set_video_bitrate(self.codec_id, bitrate_bps)
    }

    fn request_keyframe(&mut self) {
        let _ = self.backend.request_sync_frame(self.codec_id);
    }

    fn pull_packet(&mut self) -> CoreResult<Option<EncodedPacket>> {
        self.backend.dequeue_encoded_packet(self.codec_id)
    }
}

impl Drop for AndroidMediaCodecEncoder {
    fn drop(&mut self) {
        self.backend.destroy_encoder(self.codec_id);
    }
}

/// Dual hardware encoder setup matching iOS/Linux M4 spec:
/// Main 1080p60 (`RTMPS` stream) + Preview 720p30 (`WebView MSE` console).
pub struct DualAndroidEncoder {
    pub main: AndroidMediaCodecEncoder,
    pub preview: AndroidMediaCodecEncoder,
}

impl DualAndroidEncoder {
    pub fn new(
        backend: Arc<dyn MediaCodecBackend>,
        main_cfg: MediaCodecEncoderConfig,
        preview_cfg: MediaCodecEncoderConfig,
    ) -> CoreResult<Self> {
        let main = AndroidMediaCodecEncoder::new(Arc::clone(&backend), main_cfg)?;
        let preview = AndroidMediaCodecEncoder::new(backend, preview_cfg)?;
        Ok(Self { main, preview })
    }

    pub fn submit_composited_tick(&mut self, pts: PtsUs, drop_main_non_idr: bool) -> CoreResult<()> {
        if !drop_main_non_idr {
            self.main.submit_surface_frame(pts)?;
        }
        self.preview.submit_surface_frame(pts)?;
        Ok(())
    }
}
