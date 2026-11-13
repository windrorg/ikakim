//! Rockchip RK3588 (`Rock 5B`) MPP ハードウェアコーデック ゼロコピーバインディング
//! `librockchip_mpp.so.1` を `dlopen` で動的ロードし、`DMA-BUF` FD のまま
//! H.264/MJPEG デコードおよびデュアルストリーム H.264 エンコード＋動的 ABR を実行する。

use core_types::clock::PtsUs;
use core_types::media::{CodecKind, EncodedPacket, NativeBufferHandle, PixelFormat, VideoFrame};
use core_types::traits::{VideoDecoder, VideoEncoder};
use core_types::{CoreError, CoreResult};
use std::collections::VecDeque;
use std::ffi::CString;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::raw::{c_char, c_int, c_void};
use std::path::Path;
use std::ptr;
use std::sync::Arc;

const RTLD_LAZY: c_int = 1;
const F_DUPFD_CLOEXEC: c_int = 1030;

// Rockchip MPP API 定数
pub const MPP_OK: i32 = 0;
pub const MPP_CTX_DEC: i32 = 0;
pub const MPP_CTX_ENC: i32 = 1;

pub const MPP_VIDEO_CODING_AVC: i32 = 7;
pub const MPP_VIDEO_CODING_MJPEG: i32 = 8;

pub const MPP_FMT_YUV420SP: i32 = 0; // NV12
pub const MPP_FMT_RGBA8888: i32 = 0x10002;
pub const MPP_FMT_BGRA8888: i32 = 0x10003;

pub const MPP_BUFFER_TYPE_ION: i32 = 0;
pub const MPP_BUFFER_TYPE_EXT_DMA: i32 = 2;

pub const MPP_SET_INPUT_BLOCK: u32 = 0x0000_0003;
pub const MPP_SET_OUTPUT_BLOCK: u32 = 0x0000_0005;
pub const MPP_DEC_SET_EXT_BUF_GROUP: u32 = 0x0010_0002;
pub const MPP_DEC_SET_INFO_CHANGE_READY: u32 = 0x0010_0003;
pub const MPP_ENC_SET_IDR_FRAME: u32 = 0x0020_0002;
pub const MPP_ENC_SET_CFG: u32 = 0x0020_0006;
pub const MPP_ENC_GET_CFG: u32 = 0x0020_0007;
pub const MPP_ENC_GET_HDR_SYNC: u32 = 0x0020_0008;

pub type MppCtx = *mut c_void;
pub type MppFrame = *mut c_void;
pub type MppPacket = *mut c_void;
pub type MppBuffer = *mut c_void;
pub type MppBufferGroup = *mut c_void;
pub type MppEncCfg = *mut c_void;
pub type MppTask = *mut c_void;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MppBufferInfo {
    pub buf_type: i32,
    pub size: usize,
    pub ptr: *mut c_void,
    pub hnd: *mut c_void,
    pub fd: c_int,
    pub index: c_int,
}

#[repr(C)]
pub struct MppApi {
    pub size: u32,
    pub version: u32,
    pub decode: Option<unsafe extern "C" fn(MppCtx, MppTask) -> i32>,
    pub decode_put_packet: Option<unsafe extern "C" fn(MppCtx, MppPacket) -> i32>,
    pub decode_get_frame: Option<unsafe extern "C" fn(MppCtx, *mut MppFrame) -> i32>,
    pub encode: Option<unsafe extern "C" fn(MppCtx, MppTask) -> i32>,
    pub encode_put_frame: Option<unsafe extern "C" fn(MppCtx, MppFrame) -> i32>,
    pub encode_get_packet: Option<unsafe extern "C" fn(MppCtx, *mut MppPacket) -> i32>,
    pub poll: Option<unsafe extern "C" fn(MppCtx, i32, i32) -> i32>,
    pub control: Option<unsafe extern "C" fn(MppCtx, u32, *mut c_void) -> i32>,
    pub reset: Option<unsafe extern "C" fn(MppCtx) -> i32>,
}

extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
}

/// RK3588 MPP 利用可否の実行時プローブ結果
#[derive(Debug, Clone)]
pub struct MppCapability {
    pub has_mpp_device: bool,
    pub has_librockchip_mpp: bool,
    pub has_rga_device: bool,
}

impl MppCapability {
    pub fn detect() -> Self {
        let has_mpp_device = Path::new("/dev/mpp_service").exists();
        let has_rga_device = Path::new("/dev/rga").exists();
        let has_librockchip_mpp = unsafe {
            let mut h = dlopen(c"librockchip_mpp.so.1".as_ptr(), RTLD_LAZY);
            if h.is_null() {
                h = dlopen(c"librockchip_mpp.so".as_ptr(), RTLD_LAZY);
            }
            if h.is_null() {
                false
            } else {
                dlclose(h);
                true
            }
        };
        Self {
            has_mpp_device,
            has_librockchip_mpp,
            has_rga_device,
        }
    }

    pub fn is_available(&self) -> bool {
        self.has_mpp_device && self.has_librockchip_mpp
    }
}

/// `librockchip_mpp.so` シンボルテーブル（実機 FFI / テスト用インプロセスモック共通抽象）
///
/// # Safety
/// 全メソッドは MPP FFI の不透明ポインタを受け渡すため、有効なハンドルまたは NULL チェック済みのポインタを渡すこと。
#[allow(clippy::missing_safety_doc)]
pub unsafe trait MppSysBackend: Send + Sync {
    unsafe fn create(&self, ctx: *mut MppCtx, mpi: *mut *const MppApi) -> i32;
    unsafe fn init(&self, ctx: MppCtx, ctx_type: i32, coding: i32) -> i32;
    unsafe fn destroy(&self, ctx: MppCtx) -> i32;
    unsafe fn control(&self, ctx: MppCtx, mpi: *const MppApi, cmd: u32, param: *mut c_void) -> i32;

    unsafe fn enc_cfg_init(&self, cfg: *mut MppEncCfg) -> i32;
    unsafe fn enc_cfg_deinit(&self, cfg: MppEncCfg) -> i32;
    unsafe fn enc_cfg_set_s32(&self, cfg: MppEncCfg, name: &str, val: i32) -> i32;

    unsafe fn buffer_group_get_internal(&self, group: *mut MppBufferGroup, buf_type: i32) -> i32;
    unsafe fn buffer_group_put(&self, group: MppBufferGroup) -> i32;
    unsafe fn buffer_import_fd(&self, group: MppBufferGroup, fd: RawFd, size: usize) -> CoreResult<MppBuffer>;
    unsafe fn buffer_get_fd(&self, buf: MppBuffer) -> RawFd;
    unsafe fn buffer_get_ptr(&self, buf: MppBuffer) -> *mut c_void;
    unsafe fn buffer_put(&self, buf: MppBuffer);

    unsafe fn frame_init(&self, frame: *mut MppFrame) -> i32;
    unsafe fn frame_deinit(&self, frame: *mut MppFrame);
    unsafe fn frame_set_geometry(&self, frame: MppFrame, w: u32, h: u32, hor: u32, ver: u32, fmt: i32);
    unsafe fn frame_set_buffer(&self, frame: MppFrame, buf: MppBuffer);
    unsafe fn frame_set_pts(&self, frame: MppFrame, pts: i64);
    unsafe fn frame_get_meta(
        &self,
        frame: MppFrame,
    ) -> (u32, u32, u32, i32, i64, MppBuffer, bool);

    unsafe fn packet_init(&self, packet: *mut MppPacket, data: *const u8, size: usize) -> i32;
    unsafe fn packet_init_with_buffer(&self, packet: *mut MppPacket, buf: MppBuffer) -> i32;
    unsafe fn packet_deinit(&self, packet: *mut MppPacket);
    unsafe fn packet_set_pts(&self, packet: MppPacket, pts: i64);
    unsafe fn packet_data_slice(&self, packet: MppPacket) -> (&[u8], i64);

    unsafe fn encode_put_frame(&self, ctx: MppCtx, mpi: *const MppApi, frame: MppFrame) -> i32;
    unsafe fn encode_get_packet(&self, ctx: MppCtx, mpi: *const MppApi, packet: *mut MppPacket) -> i32;
    unsafe fn decode_put_packet(&self, ctx: MppCtx, mpi: *const MppApi, packet: MppPacket) -> i32;
    unsafe fn decode_get_frame(&self, ctx: MppCtx, mpi: *const MppApi, frame: *mut MppFrame) -> i32;
}

/// 動的ロードされた `librockchip_mpp.so` 実装
pub struct DlopenMppSys {
    lib: *mut c_void,
    fn_create: unsafe extern "C" fn(*mut MppCtx, *mut *const MppApi) -> i32,
    fn_init: unsafe extern "C" fn(MppCtx, i32, i32) -> i32,
    fn_destroy: unsafe extern "C" fn(MppCtx) -> i32,
    fn_enc_cfg_init: unsafe extern "C" fn(*mut MppEncCfg) -> i32,
    fn_enc_cfg_deinit: unsafe extern "C" fn(MppEncCfg) -> i32,
    fn_enc_cfg_set_s32: unsafe extern "C" fn(MppEncCfg, *const c_char, i32) -> i32,
    fn_buf_grp_get: unsafe extern "C" fn(*mut MppBufferGroup, i32, *const c_char) -> i32,
    fn_buf_grp_put: unsafe extern "C" fn(MppBufferGroup) -> i32,
    fn_buf_import: unsafe extern "C" fn(MppBufferGroup, *mut MppBuffer, *mut MppBufferInfo, *const c_char) -> i32,
    fn_buf_get_fd: unsafe extern "C" fn(MppBuffer, *const c_char) -> c_int,
    fn_buf_get_ptr: unsafe extern "C" fn(MppBuffer, *const c_char) -> *mut c_void,
    fn_buf_put: unsafe extern "C" fn(MppBuffer, *const c_char) -> i32,
    fn_frame_init: unsafe extern "C" fn(*mut MppFrame) -> i32,
    fn_frame_deinit: unsafe extern "C" fn(*mut MppFrame) -> i32,
    fn_frame_set_w: unsafe extern "C" fn(MppFrame, u32),
    fn_frame_set_h: unsafe extern "C" fn(MppFrame, u32),
    fn_frame_set_hs: unsafe extern "C" fn(MppFrame, u32),
    fn_frame_set_vs: unsafe extern "C" fn(MppFrame, u32),
    fn_frame_set_fmt: unsafe extern "C" fn(MppFrame, i32),
    fn_frame_set_buf: unsafe extern "C" fn(MppFrame, MppBuffer),
    fn_frame_set_pts: unsafe extern "C" fn(MppFrame, i64),
    fn_frame_get_w: unsafe extern "C" fn(MppFrame) -> u32,
    fn_frame_get_h: unsafe extern "C" fn(MppFrame) -> u32,
    fn_frame_get_hs: unsafe extern "C" fn(MppFrame) -> u32,
    fn_frame_get_fmt: unsafe extern "C" fn(MppFrame) -> i32,
    fn_frame_get_buf: unsafe extern "C" fn(MppFrame) -> MppBuffer,
    fn_frame_get_pts: unsafe extern "C" fn(MppFrame) -> i64,
    fn_frame_get_info_chg: unsafe extern "C" fn(MppFrame) -> u32,
    fn_pkt_init: unsafe extern "C" fn(*mut MppPacket, *const c_void, usize) -> i32,
    fn_pkt_init_buf: unsafe extern "C" fn(*mut MppPacket, MppBuffer) -> i32,
    fn_pkt_deinit: unsafe extern "C" fn(*mut MppPacket) -> i32,
    fn_pkt_set_pts: unsafe extern "C" fn(MppPacket, i64),
    fn_pkt_get_data: unsafe extern "C" fn(MppPacket) -> *const c_void,
    fn_pkt_get_len: unsafe extern "C" fn(MppPacket) -> usize,
    fn_pkt_get_pts: unsafe extern "C" fn(MppPacket) -> i64,
}

unsafe impl Send for DlopenMppSys {}
unsafe impl Sync for DlopenMppSys {}

impl DlopenMppSys {
    pub fn open() -> CoreResult<Arc<Self>> {
        unsafe {
            let mut lib = dlopen(c"librockchip_mpp.so.1".as_ptr(), RTLD_LAZY);
            if lib.is_null() {
                lib = dlopen(c"librockchip_mpp.so".as_ptr(), RTLD_LAZY);
            }
            if lib.is_null() {
                return Err(CoreError::PlatformIo(
                    "librockchip_mpp.so.1 not available on this host".into(),
                ));
            }
            macro_rules! sym {
                ($name:literal, $ty:ty) => {{
                    let p = dlsym(lib, $name.as_ptr());
                    if p.is_null() {
                        dlclose(lib);
                        return Err(CoreError::PlatformIo(format!(
                            "MPP dlsym({}) missing",
                            $name.to_string_lossy()
                        )));
                    }
                    #[allow(clippy::missing_transmute_annotations)]
                    std::mem::transmute::<*mut c_void, $ty>(p)
                }};
            }

            Ok(Arc::new(Self {
                lib,
                fn_create: sym!(c"mpp_create", _),
                fn_init: sym!(c"mpp_init", _),
                fn_destroy: sym!(c"mpp_destroy", _),
                fn_enc_cfg_init: sym!(c"mpp_enc_cfg_init", _),
                fn_enc_cfg_deinit: sym!(c"mpp_enc_cfg_deinit", _),
                fn_enc_cfg_set_s32: sym!(c"mpp_enc_cfg_set_s32", _),
                fn_buf_grp_get: sym!(c"mpp_buffer_group_get_internal", _),
                fn_buf_grp_put: sym!(c"mpp_buffer_group_put", _),
                fn_buf_import: sym!(c"mpp_buffer_import_with_tag", _),
                fn_buf_get_fd: sym!(c"mpp_buffer_get_fd_with_caller", _),
                fn_buf_get_ptr: sym!(c"mpp_buffer_get_ptr_with_caller", _),
                fn_buf_put: sym!(c"mpp_buffer_put_with_caller", _),
                fn_frame_init: sym!(c"mpp_frame_init", _),
                fn_frame_deinit: sym!(c"mpp_frame_deinit", _),
                fn_frame_set_w: sym!(c"mpp_frame_set_width", _),
                fn_frame_set_h: sym!(c"mpp_frame_set_height", _),
                fn_frame_set_hs: sym!(c"mpp_frame_set_hor_stride", _),
                fn_frame_set_vs: sym!(c"mpp_frame_set_ver_stride", _),
                fn_frame_set_fmt: sym!(c"mpp_frame_set_fmt", _),
                fn_frame_set_buf: sym!(c"mpp_frame_set_buffer", _),
                fn_frame_set_pts: sym!(c"mpp_frame_set_pts", _),
                fn_frame_get_w: sym!(c"mpp_frame_get_width", _),
                fn_frame_get_h: sym!(c"mpp_frame_get_height", _),
                fn_frame_get_hs: sym!(c"mpp_frame_get_hor_stride", _),
                fn_frame_get_fmt: sym!(c"mpp_frame_get_fmt", _),
                fn_frame_get_buf: sym!(c"mpp_frame_get_buffer", _),
                fn_frame_get_pts: sym!(c"mpp_frame_get_pts", _),
                fn_frame_get_info_chg: sym!(c"mpp_frame_get_info_change", _),
                fn_pkt_init: sym!(c"mpp_packet_init", _),
                fn_pkt_init_buf: sym!(c"mpp_packet_init_with_buffer", _),
                fn_pkt_deinit: sym!(c"mpp_packet_deinit", _),
                fn_pkt_set_pts: sym!(c"mpp_packet_set_pts", _),
                fn_pkt_get_data: sym!(c"mpp_packet_get_data", _),
                fn_pkt_get_len: sym!(c"mpp_packet_get_length", _),
                fn_pkt_get_pts: sym!(c"mpp_packet_get_pts", _),
            }))
        }
    }
}

impl Drop for DlopenMppSys {
    fn drop(&mut self) {
        if !self.lib.is_null() {
            unsafe { dlclose(self.lib) };
            self.lib = ptr::null_mut();
        }
    }
}

unsafe impl MppSysBackend for DlopenMppSys {
    unsafe fn create(&self, ctx: *mut MppCtx, mpi: *mut *const MppApi) -> i32 {
        (self.fn_create)(ctx, mpi)
    }
    unsafe fn init(&self, ctx: MppCtx, ctx_type: i32, coding: i32) -> i32 {
        (self.fn_init)(ctx, ctx_type, coding)
    }
    unsafe fn destroy(&self, ctx: MppCtx) -> i32 {
        (self.fn_destroy)(ctx)
    }
    unsafe fn control(&self, ctx: MppCtx, mpi: *const MppApi, cmd: u32, param: *mut c_void) -> i32 {
        if let Some(f) = (*mpi).control {
            f(ctx, cmd, param)
        } else {
            -1
        }
    }
    unsafe fn enc_cfg_init(&self, cfg: *mut MppEncCfg) -> i32 {
        (self.fn_enc_cfg_init)(cfg)
    }
    unsafe fn enc_cfg_deinit(&self, cfg: MppEncCfg) -> i32 {
        (self.fn_enc_cfg_deinit)(cfg)
    }
    unsafe fn enc_cfg_set_s32(&self, cfg: MppEncCfg, name: &str, val: i32) -> i32 {
        let Ok(cname) = CString::new(name) else {
            return -1;
        };
        (self.fn_enc_cfg_set_s32)(cfg, cname.as_ptr(), val)
    }
    unsafe fn buffer_group_get_internal(&self, group: *mut MppBufferGroup, buf_type: i32) -> i32 {
        (self.fn_buf_grp_get)(group, buf_type, c"ikakim".as_ptr())
    }
    unsafe fn buffer_group_put(&self, group: MppBufferGroup) -> i32 {
        (self.fn_buf_grp_put)(group)
    }
    unsafe fn buffer_import_fd(&self, group: MppBufferGroup, fd: RawFd, size: usize) -> CoreResult<MppBuffer> {
        let mut info = MppBufferInfo {
            buf_type: MPP_BUFFER_TYPE_EXT_DMA,
            size,
            ptr: ptr::null_mut(),
            hnd: ptr::null_mut(),
            fd,
            index: 0,
        };
        let mut out: MppBuffer = ptr::null_mut();
        let rc = (self.fn_buf_import)(group, &mut out, &mut info, c"ikakim".as_ptr());
        if rc != MPP_OK || out.is_null() {
            return Err(CoreError::PlatformIo(format!(
                "mpp_buffer_import(DMA-BUF fd={}) failed: {}",
                fd, rc
            )));
        }
        Ok(out)
    }
    unsafe fn buffer_get_fd(&self, buf: MppBuffer) -> RawFd {
        (self.fn_buf_get_fd)(buf, c"ikakim".as_ptr())
    }
    unsafe fn buffer_get_ptr(&self, buf: MppBuffer) -> *mut c_void {
        (self.fn_buf_get_ptr)(buf, c"ikakim".as_ptr())
    }
    unsafe fn buffer_put(&self, buf: MppBuffer) {
        let _ = (self.fn_buf_put)(buf, c"ikakim".as_ptr());
    }
    unsafe fn frame_init(&self, frame: *mut MppFrame) -> i32 {
        (self.fn_frame_init)(frame)
    }
    unsafe fn frame_deinit(&self, frame: *mut MppFrame) {
        let _ = (self.fn_frame_deinit)(frame);
    }
    unsafe fn frame_set_geometry(&self, frame: MppFrame, w: u32, h: u32, hor: u32, ver: u32, fmt: i32) {
        (self.fn_frame_set_w)(frame, w);
        (self.fn_frame_set_h)(frame, h);
        (self.fn_frame_set_hs)(frame, hor);
        (self.fn_frame_set_vs)(frame, ver);
        (self.fn_frame_set_fmt)(frame, fmt);
    }
    unsafe fn frame_set_buffer(&self, frame: MppFrame, buf: MppBuffer) {
        (self.fn_frame_set_buf)(frame, buf);
    }
    unsafe fn frame_set_pts(&self, frame: MppFrame, pts: i64) {
        (self.fn_frame_set_pts)(frame, pts);
    }
    unsafe fn frame_get_meta(&self, frame: MppFrame) -> (u32, u32, u32, i32, i64, MppBuffer, bool) {
        (
            (self.fn_frame_get_w)(frame),
            (self.fn_frame_get_h)(frame),
            (self.fn_frame_get_hs)(frame),
            (self.fn_frame_get_fmt)(frame),
            (self.fn_frame_get_pts)(frame),
            (self.fn_frame_get_buf)(frame),
            (self.fn_frame_get_info_chg)(frame) != 0,
        )
    }
    unsafe fn packet_init(&self, packet: *mut MppPacket, data: *const u8, size: usize) -> i32 {
        (self.fn_pkt_init)(packet, data as *const c_void, size)
    }
    unsafe fn packet_init_with_buffer(&self, packet: *mut MppPacket, buf: MppBuffer) -> i32 {
        (self.fn_pkt_init_buf)(packet, buf)
    }
    unsafe fn packet_deinit(&self, packet: *mut MppPacket) {
        let _ = (self.fn_pkt_deinit)(packet);
    }
    unsafe fn packet_set_pts(&self, packet: MppPacket, pts: i64) {
        (self.fn_pkt_set_pts)(packet, pts);
    }
    unsafe fn packet_data_slice(&self, packet: MppPacket) -> (&[u8], i64) {
        let p = (self.fn_pkt_get_data)(packet) as *const u8;
        let l = (self.fn_pkt_get_len)(packet);
        let pts = (self.fn_pkt_get_pts)(packet);
        if p.is_null() || l == 0 {
            (&[], pts)
        } else {
            (std::slice::from_raw_parts(p, l), pts)
        }
    }
    unsafe fn encode_put_frame(&self, ctx: MppCtx, mpi: *const MppApi, frame: MppFrame) -> i32 {
        if let Some(f) = (*mpi).encode_put_frame {
            f(ctx, frame)
        } else {
            -1
        }
    }
    unsafe fn encode_get_packet(&self, ctx: MppCtx, mpi: *const MppApi, packet: *mut MppPacket) -> i32 {
        if let Some(f) = (*mpi).encode_get_packet {
            f(ctx, packet)
        } else {
            -1
        }
    }
    unsafe fn decode_put_packet(&self, ctx: MppCtx, mpi: *const MppApi, packet: MppPacket) -> i32 {
        if let Some(f) = (*mpi).decode_put_packet {
            f(ctx, packet)
        } else {
            -1
        }
    }
    unsafe fn decode_get_frame(&self, ctx: MppCtx, mpi: *const MppApi, frame: *mut MppFrame) -> i32 {
        if let Some(f) = (*mpi).decode_get_frame {
            f(ctx, frame)
        } else {
            -1
        }
    }
}

/// RK3588 MPP ハードウェア H.264 エンコーダ（`DMA-BUF` 直接入力 & ABR 対応）
pub struct MppH264Encoder {
    sys: Arc<dyn MppSysBackend>,
    ctx: MppCtx,
    mpi: *const MppApi,
    cfg: MppEncCfg,
    ext_group: MppBufferGroup,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
    force_keyframe: bool,
    seq_header_emitted: bool,
    queue: VecDeque<EncodedPacket>,
}

unsafe impl Send for MppH264Encoder {}

impl MppH264Encoder {
    /// 実機の `librockchip_mpp.so.1` で初期化する
    pub fn open_hardware(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> CoreResult<Self> {
        let sys = DlopenMppSys::open()?;
        Self::with_backend(sys, width, height, fps, bitrate_bps)
    }

    /// 任意の `MppSysBackend`（実機またはテスト用モック）で初期化する
    pub fn with_backend(
        sys: Arc<dyn MppSysBackend>,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u32,
    ) -> CoreResult<Self> {
        let mut ctx: MppCtx = ptr::null_mut();
        let mut mpi: *const MppApi = ptr::null();
        let (cfg, ext_group) = unsafe {
            if sys.create(&mut ctx, &mut mpi) != MPP_OK || ctx.is_null() || mpi.is_null() {
                return Err(CoreError::Codec("mpp_create(ENC) failed".into()));
            }
            if sys.init(ctx, MPP_CTX_ENC, MPP_VIDEO_CODING_AVC) != MPP_OK {
                sys.destroy(ctx);
                return Err(CoreError::Codec("mpp_init(ENC, AVC) failed".into()));
            }

            let mut cfg: MppEncCfg = ptr::null_mut();
            if sys.enc_cfg_init(&mut cfg) != MPP_OK || cfg.is_null() {
                sys.destroy(ctx);
                return Err(CoreError::Codec("mpp_enc_cfg_init failed".into()));
            }

            let hor_stride = align16(width);
            let ver_stride = align16(height);
            sys.enc_cfg_set_s32(cfg, "prep:width", width as i32);
            sys.enc_cfg_set_s32(cfg, "prep:height", height as i32);
            sys.enc_cfg_set_s32(cfg, "prep:hor_stride", hor_stride as i32);
            sys.enc_cfg_set_s32(cfg, "prep:ver_stride", ver_stride as i32);
            sys.enc_cfg_set_s32(cfg, "prep:format", MPP_FMT_NV12_COMPAT);
            sys.enc_cfg_set_s32(cfg, "rc:mode", 1); // CBR
            sys.enc_cfg_set_s32(cfg, "rc:bps_target", bitrate_bps as i32);
            sys.enc_cfg_set_s32(cfg, "rc:bps_max", ((bitrate_bps as u64) * 5 / 4) as i32);
            sys.enc_cfg_set_s32(cfg, "rc:bps_min", ((bitrate_bps as u64) * 3 / 4) as i32);
            sys.enc_cfg_set_s32(cfg, "rc:fps_in_num", fps.max(1) as i32);
            sys.enc_cfg_set_s32(cfg, "rc:fps_in_denom", 1);
            sys.enc_cfg_set_s32(cfg, "rc:fps_out_num", fps.max(1) as i32);
            sys.enc_cfg_set_s32(cfg, "rc:fps_out_denom", 1);
            sys.enc_cfg_set_s32(cfg, "rc:gop", (fps.max(1) * 2) as i32);
            sys.enc_cfg_set_s32(cfg, "codec:type", MPP_VIDEO_CODING_AVC);
            sys.enc_cfg_set_s32(cfg, "h264:profile", 100); // High
            sys.enc_cfg_set_s32(cfg, "h264:level", 41);

            if sys.control(ctx, mpi, MPP_ENC_SET_CFG, cfg) != MPP_OK {
                sys.enc_cfg_deinit(cfg);
                sys.destroy(ctx);
                return Err(CoreError::Codec("MPP_ENC_SET_CFG failed".into()));
            }

            let mut ext_group: MppBufferGroup = ptr::null_mut();
            sys.buffer_group_get_internal(&mut ext_group, MPP_BUFFER_TYPE_EXT_DMA);
            (cfg, ext_group)
        };

        let mut enc = Self {
            sys,
            ctx,
            mpi,
            cfg,
            ext_group,
            width,
            height,
            fps: fps.max(1),
            bitrate_bps,
            force_keyframe: true,
            seq_header_emitted: false,
            queue: VecDeque::new(),
        };
        enc.emit_header_sync_if_available();
        Ok(enc)
    }

    fn emit_header_sync_if_available(&mut self) {
        let mut hdr_pkt: MppPacket = ptr::null_mut();
        unsafe {
            if self.sys.control(
                self.ctx,
                self.mpi,
                MPP_ENC_GET_HDR_SYNC,
                (&mut hdr_pkt as *mut MppPacket) as *mut c_void,
            ) == MPP_OK
                && !hdr_pkt.is_null()
            {
                let (slice, _) = self.sys.packet_data_slice(hdr_pkt);
                if !slice.is_empty() {
                    self.queue.push_back(EncodedPacket {
                        codec: CodecKind::H264,
                        pts: PtsUs(0),
                        dts: PtsUs(0),
                        is_keyframe: true,
                        is_sequence_header: true,
                        data: slice.to_vec(),
                    });
                    self.seq_header_emitted = true;
                }
                self.sys.packet_deinit(&mut hdr_pkt);
            }
        }
    }

    pub fn bitrate_bps(&self) -> u32 {
        self.bitrate_bps
    }

    pub fn fps(&self) -> u32 {
        self.fps
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// GPU コンポジタまたは V4L2 から得た `DMA-BUF` FD を CPU コピーゼロで直接エンコード投入する
    pub fn encode_dmabuf(
        &mut self,
        fd: RawFd,
        buf_size: usize,
        hor_stride: u32,
        ver_stride: u32,
        format: PixelFormat,
        pts: PtsUs,
    ) -> CoreResult<()> {
        let mpp_fmt = match format {
            PixelFormat::Nv12 => MPP_FMT_NV12_COMPAT,
            PixelFormat::Rgba8 => MPP_FMT_RGBA8888,
            PixelFormat::Bgra8 => MPP_FMT_BGRA8888,
            other => {
                return Err(CoreError::UnsupportedFormat(format!(
                    "MPP encoder expects NV12/RGBA8/BGRA8 DMA-BUF, got {:?}",
                    other
                )))
            }
        };

        unsafe {
            if self.force_keyframe {
                let _ = self
                    .sys
                    .control(self.ctx, self.mpi, MPP_ENC_SET_IDR_FRAME, ptr::null_mut());
                self.force_keyframe = false;
            }

            let mpp_buf = self.sys.buffer_import_fd(self.ext_group, fd, buf_size)?;
            let mut frame: MppFrame = ptr::null_mut();
            if self.sys.frame_init(&mut frame) != MPP_OK || frame.is_null() {
                self.sys.buffer_put(mpp_buf);
                return Err(CoreError::Codec("mpp_frame_init failed".into()));
            }
            self.sys.frame_set_geometry(
                frame,
                self.width,
                self.height,
                hor_stride.max(self.width),
                ver_stride.max(self.height),
                mpp_fmt,
            );
            self.sys.frame_set_buffer(frame, mpp_buf);
            self.sys.frame_set_pts(frame, pts.0 as i64);

            let rc = self.sys.encode_put_frame(self.ctx, self.mpi, frame);
            self.sys.frame_deinit(&mut frame);
            self.sys.buffer_put(mpp_buf);
            if rc != MPP_OK {
                return Err(CoreError::Codec(format!("mpp encode_put_frame failed: {}", rc)));
            }

            let mut pkt: MppPacket = ptr::null_mut();
            if self.sys.encode_get_packet(self.ctx, self.mpi, &mut pkt) == MPP_OK && !pkt.is_null()
            {
                let (slice, out_pts) = self.sys.packet_data_slice(pkt);
                if !slice.is_empty() {
                    let is_kf = contains_h264_idr(slice);
                    self.queue.push_back(EncodedPacket {
                        codec: CodecKind::H264,
                        pts: PtsUs(out_pts.max(0) as u64),
                        dts: PtsUs(out_pts.max(0) as u64),
                        is_keyframe: is_kf,
                        is_sequence_header: !self.seq_header_emitted && contains_h264_sps(slice),
                        data: slice.to_vec(),
                    });
                    if contains_h264_sps(slice) {
                        self.seq_header_emitted = true;
                    }
                }
                self.sys.packet_deinit(&mut pkt);
            }
        }

        Ok(())
    }
}

const MPP_FMT_NV12_COMPAT: i32 = MPP_FMT_YUV420SP;

impl VideoEncoder for MppH264Encoder {
    /// ABR からの動的ビットレート変更 (`rc:bps_target` + `MPP_ENC_SET_CFG` = MPP/RK_MPI ABR 更新)
    fn set_target_bitrate(&mut self, bitrate_bps: u32) -> CoreResult<()> {
        let clamped = bitrate_bps.max(100_000);
        unsafe {
            self.sys
                .enc_cfg_set_s32(self.cfg, "rc:bps_target", clamped as i32);
            self.sys
                .enc_cfg_set_s32(self.cfg, "rc:bps_max", ((clamped as u64) * 5 / 4) as i32);
            self.sys
                .enc_cfg_set_s32(self.cfg, "rc:bps_min", ((clamped as u64) * 3 / 4) as i32);
            if self.sys.control(self.ctx, self.mpi, MPP_ENC_SET_CFG, self.cfg) != MPP_OK {
                return Err(CoreError::Codec("MPP dynamic bitrate update failed".into()));
            }
        }
        self.bitrate_bps = clamped;
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn pull_packet(&mut self) -> CoreResult<Option<EncodedPacket>> {
        Ok(self.queue.pop_front())
    }
}

impl Drop for MppH264Encoder {
    fn drop(&mut self) {
        unsafe {
            if !self.ext_group.is_null() {
                self.sys.buffer_group_put(self.ext_group);
            }
            if !self.cfg.is_null() {
                self.sys.enc_cfg_deinit(self.cfg);
            }
            if !self.ctx.is_null() {
                self.sys.destroy(self.ctx);
            }
        }
    }
}

/// RK3588 デュアル HW エンコーダ（Main 1080p60 + Preview 720p30）
pub struct RockchipDualEncoders {
    pub main: MppH264Encoder,
    pub preview: MppH264Encoder,
}

/// RK3588 MPP ハードウェアデコーダ（H.264 / MJPEG 入力 → `DMA-BUF` ゼロコピーフレーム出力）
pub struct MppDecoder {
    sys: Arc<dyn MppSysBackend>,
    ctx: MppCtx,
    mpi: *const MppApi,
    buf_group: MppBufferGroup,
    decoded_frames: VecDeque<VideoFrame>,
}

unsafe impl Send for MppDecoder {}

impl MppDecoder {
    pub fn open_hardware(codec: CodecKind) -> CoreResult<Self> {
        let sys = DlopenMppSys::open()?;
        let coding = match codec {
            CodecKind::H264 => MPP_VIDEO_CODING_AVC,
            CodecKind::Hevc | CodecKind::AacLc => {
                return Err(CoreError::UnsupportedFormat("use H264 or MJPEG".into()))
            }
        };
        Self::with_backend(sys, coding)
    }

    pub fn open_hardware_mjpeg() -> CoreResult<Self> {
        let sys = DlopenMppSys::open()?;
        Self::with_backend(sys, MPP_VIDEO_CODING_MJPEG)
    }

    pub fn with_backend(sys: Arc<dyn MppSysBackend>, mpp_coding: i32) -> CoreResult<Self> {
        let mut ctx: MppCtx = ptr::null_mut();
        let mut mpi: *const MppApi = ptr::null();
        let buf_group = unsafe {
            if sys.create(&mut ctx, &mut mpi) != MPP_OK || ctx.is_null() || mpi.is_null() {
                return Err(CoreError::Codec("mpp_create(DEC) failed".into()));
            }
            if sys.init(ctx, MPP_CTX_DEC, mpp_coding) != MPP_OK {
                sys.destroy(ctx);
                return Err(CoreError::Codec("mpp_init(DEC) failed".into()));
            }
            let mut buf_group: MppBufferGroup = ptr::null_mut();
            sys.buffer_group_get_internal(&mut buf_group, MPP_BUFFER_TYPE_ION);
            let _ = sys.control(ctx, mpi, MPP_DEC_SET_EXT_BUF_GROUP, buf_group);
            buf_group
        };

        Ok(Self {
            sys,
            ctx,
            mpi,
            buf_group,
            decoded_frames: VecDeque::new(),
        })
    }
}

impl VideoDecoder for MppDecoder {
    fn push_packet(&mut self, packet: &EncodedPacket) -> CoreResult<()> {
        unsafe {
            let mut pkt: MppPacket = ptr::null_mut();
            if self
                .sys
                .packet_init(&mut pkt, packet.data.as_ptr(), packet.data.len())
                != MPP_OK
                || pkt.is_null()
            {
                return Err(CoreError::Codec("mpp_packet_init failed".into()));
            }
            self.sys.packet_set_pts(pkt, packet.pts.0 as i64);
            let rc = self.sys.decode_put_packet(self.ctx, self.mpi, pkt);
            self.sys.packet_deinit(&mut pkt);
            if rc != MPP_OK {
                return Err(CoreError::Codec(format!("mpp decode_put_packet failed: {}", rc)));
            }

            let mut frame: MppFrame = ptr::null_mut();
            if self.sys.decode_get_frame(self.ctx, self.mpi, &mut frame) == MPP_OK
                && !frame.is_null()
            {
                let (w, h, stride, _fmt, pts, buf, info_chg) = self.sys.frame_get_meta(frame);
                if info_chg {
                    let _ = self.sys.control(
                        self.ctx,
                        self.mpi,
                        MPP_DEC_SET_INFO_CHANGE_READY,
                        ptr::null_mut(),
                    );
                } else if w > 0 && h > 0 && !buf.is_null() {
                    let raw_fd = self.sys.buffer_get_fd(buf);
                    if raw_fd >= 0 {
                        let dup_fd = fcntl(raw_fd, F_DUPFD_CLOEXEC, 0);
                        if dup_fd >= 0 {
                            let owned = OwnedFd::from_raw_fd(dup_fd);
                            self.decoded_frames.push_back(VideoFrame {
                                width: w,
                                height: h,
                                format: PixelFormat::Nv12,
                                pts: PtsUs(pts.max(0) as u64),
                                handle: NativeBufferHandle::DmaBuf {
                                    fd: owned,
                                    offset: 0,
                                    stride,
                                    modifier: 0,
                                },
                            });
                        }
                    }
                }
                self.sys.frame_deinit(&mut frame);
            }
        }
        Ok(())
    }

    fn pull_frame(&mut self) -> CoreResult<Option<VideoFrame>> {
        Ok(self.decoded_frames.pop_front())
    }
}

impl Drop for MppDecoder {
    fn drop(&mut self) {
        unsafe {
            if !self.buf_group.is_null() {
                self.sys.buffer_group_put(self.buf_group);
            }
            if !self.ctx.is_null() {
                self.sys.destroy(self.ctx);
            }
        }
    }
}

fn align16(v: u32) -> u32 {
    v.div_ceil(16) * 16
}

fn contains_h264_sps(annexb: &[u8]) -> bool {
    scan_nal_types(annexb).any(|nt| nt == 7)
}

fn contains_h264_idr(annexb: &[u8]) -> bool {
    scan_nal_types(annexb).any(|nt| nt == 5)
}

fn scan_nal_types(data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    data.windows(5).filter_map(|w| {
        if w[0..4] == [0, 0, 0, 1] {
            Some(w[4] & 0x1f)
        } else if w[0..3] == [0, 0, 1] {
            Some(w[3] & 0x1f)
        } else {
            None
        }
    })
}
