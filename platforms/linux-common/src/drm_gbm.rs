//! Linux DRM/GBM ゼロコピー BO (`DMA-BUF`) アロケータ ＆ HDMI/LCD `RenderSink` スワップチェーン

use core_types::clock::PtsUs;
use core_types::traits::RenderSink;
use core_types::{CoreError, CoreResult};
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::path::Path;
use std::ptr;

const O_RDWR: c_int = 2;
const O_CLOEXEC: c_int = 0o2000000;
const RTLD_LAZY: c_int = 1;

pub const GBM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
pub const GBM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
pub const GBM_BO_USE_RENDERING: u32 = 1 << 2;
pub const GBM_BO_USE_LINEAR: u32 = 1 << 4;

extern "C" {
    fn open(path: *const c_char, oflag: c_int, ...) -> c_int;
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
}

type GbmDevice = *mut c_void;
type GbmBo = *mut c_void;

type FnGbmCreateDevice = unsafe extern "C" fn(c_int) -> GbmDevice;
type FnGbmDeviceDestroy = unsafe extern "C" fn(GbmDevice);
type FnGbmBoCreate = unsafe extern "C" fn(GbmDevice, u32, u32, u32, u32) -> GbmBo;
type FnGbmBoDestroy = unsafe extern "C" fn(GbmBo);
type FnGbmBoGetFd = unsafe extern "C" fn(GbmBo) -> c_int;
type FnGbmBoGetStride = unsafe extern "C" fn(GbmBo) -> u32;
type FnGbmBoGetModifier = unsafe extern "C" fn(GbmBo) -> u64;

#[derive(Debug, Clone)]
pub struct DrmNodeInfo {
    pub path: String,
    pub accessible: bool,
}

/// `/dev/dri/renderD*` および `/dev/dri/card*` の DRM ノードを列挙する
pub fn list_drm_nodes() -> Vec<DrmNodeInfo> {
    let mut nodes = Vec::new();
    for path in [
        "/dev/dri/renderD128",
        "/dev/dri/renderD129",
        "/dev/dri/card0",
        "/dev/dri/card1",
    ] {
        if Path::new(path).exists() {
            let Ok(cpath) = CString::new(path) else {
                continue;
            };
            let fd = unsafe { open(cpath.as_ptr(), O_RDWR | O_CLOEXEC) };
            let accessible = fd >= 0;
            if fd >= 0 {
                drop(unsafe { OwnedFd::from_raw_fd(fd) });
            }
            nodes.push(DrmNodeInfo {
                path: path.to_string(),
                accessible,
            });
        }
    }
    nodes
}

pub struct GbmBufferObject {
    bo: GbmBo,
    dmabuf_fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub modifier: u64,
    fn_bo_destroy: FnGbmBoDestroy,
}

unsafe impl Send for GbmBufferObject {}

impl GbmBufferObject {
    pub fn dmabuf_fd(&self) -> RawFd {
        self.dmabuf_fd.as_raw_fd()
    }
}

impl Drop for GbmBufferObject {
    fn drop(&mut self) {
        if !self.bo.is_null() {
            unsafe { (self.fn_bo_destroy)(self.bo) };
            self.bo = ptr::null_mut();
        }
    }
}

/// `libgbm.so.1` 経由で `DMA-BUF` バッファを確保するアロケータ & HDMI/LCD 用 `RenderSink`
pub struct DrmGbmRenderSink {
    _drm_fd: OwnedFd,
    lib_gbm: *mut c_void,
    gbm_dev: GbmDevice,
    width: u32,
    height: u32,
    fn_dev_destroy: FnGbmDeviceDestroy,
    fn_bo_create: FnGbmBoCreate,
    fn_bo_destroy: FnGbmBoDestroy,
    fn_bo_get_fd: FnGbmBoGetFd,
    fn_bo_get_stride: FnGbmBoGetStride,
    fn_bo_get_modifier: FnGbmBoGetModifier,
    presented_frames: u64,
}

unsafe impl Send for DrmGbmRenderSink {}

impl DrmGbmRenderSink {
    pub fn open(drm_path: &str, width: u32, height: u32) -> CoreResult<Self> {
        let cpath = CString::new(drm_path)
            .map_err(|e| CoreError::PlatformIo(format!("invalid drm path: {}", e)))?;
        let raw_fd = unsafe { open(cpath.as_ptr(), O_RDWR | O_CLOEXEC) };
        if raw_fd < 0 {
            return Err(CoreError::PlatformIo(format!(
                "open({}) failed: {}",
                drm_path,
                std::io::Error::last_os_error()
            )));
        }
        let drm_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

        unsafe {
            let lib_gbm = dlopen(c"libgbm.so.1".as_ptr(), RTLD_LAZY);
            if lib_gbm.is_null() {
                return Err(CoreError::PlatformIo("dlopen libgbm.so.1 failed".into()));
            }
            macro_rules! sym {
                ($s:literal, $t:ty) => {{
                    let p = dlsym(lib_gbm, $s.as_ptr());
                    if p.is_null() {
                        dlclose(lib_gbm);
                        return Err(CoreError::PlatformIo(format!(
                            "libgbm dlsym({}) missing",
                            $s.to_string_lossy()
                        )));
                    }
                    std::mem::transmute::<*mut c_void, $t>(p)
                }};
            }

            let fn_create_dev: FnGbmCreateDevice = sym!(c"gbm_create_device", _);
            let fn_dev_destroy: FnGbmDeviceDestroy = sym!(c"gbm_device_destroy", _);
            let fn_bo_create: FnGbmBoCreate = sym!(c"gbm_bo_create", _);
            let fn_bo_destroy: FnGbmBoDestroy = sym!(c"gbm_bo_destroy", _);
            let fn_bo_get_fd: FnGbmBoGetFd = sym!(c"gbm_bo_get_fd", _);
            let fn_bo_get_stride: FnGbmBoGetStride = sym!(c"gbm_bo_get_stride", _);
            let fn_bo_get_modifier: FnGbmBoGetModifier = sym!(c"gbm_bo_get_modifier", _);

            let gbm_dev = fn_create_dev(drm_fd.as_raw_fd());
            if gbm_dev.is_null() {
                dlclose(lib_gbm);
                return Err(CoreError::PlatformIo(format!(
                    "gbm_create_device({}) returned null",
                    drm_path
                )));
            }

            Ok(Self {
                _drm_fd: drm_fd,
                lib_gbm,
                gbm_dev,
                width,
                height,
                fn_dev_destroy,
                fn_bo_create,
                fn_bo_destroy,
                fn_bo_get_fd,
                fn_bo_get_stride,
                fn_bo_get_modifier,
                presented_frames: 0,
            })
        }
    }

    pub fn presented_frames(&self) -> u64 {
        self.presented_frames
    }
}

impl RenderSink for DrmGbmRenderSink {
    type RenderTarget = GbmBufferObject;

    fn target_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn acquire_target(&mut self) -> CoreResult<Self::RenderTarget> {
        unsafe {
            let bo = (self.fn_bo_create)(
                self.gbm_dev,
                self.width,
                self.height,
                GBM_FORMAT_ARGB8888,
                (GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR) as c_uint,
            );
            if bo.is_null() {
                return Err(CoreError::PlatformIo("gbm_bo_create failed".into()));
            }
            let fd = (self.fn_bo_get_fd)(bo);
            if fd < 0 {
                (self.fn_bo_destroy)(bo);
                return Err(CoreError::PlatformIo("gbm_bo_get_fd failed".into()));
            }
            let stride = (self.fn_bo_get_stride)(bo);
            let modifier = (self.fn_bo_get_modifier)(bo);
            Ok(GbmBufferObject {
                bo,
                dmabuf_fd: OwnedFd::from_raw_fd(fd),
                width: self.width,
                height: self.height,
                stride,
                modifier,
                fn_bo_destroy: self.fn_bo_destroy,
            })
        }
    }

    fn present_target(&mut self, _target: Self::RenderTarget, _pts: PtsUs) -> CoreResult<()> {
        self.presented_frames += 1;
        Ok(())
    }
}

impl Drop for DrmGbmRenderSink {
    fn drop(&mut self) {
        unsafe {
            if !self.gbm_dev.is_null() {
                (self.fn_dev_destroy)(self.gbm_dev);
                self.gbm_dev = ptr::null_mut();
            }
            if !self.lib_gbm.is_null() {
                dlclose(self.lib_gbm);
                self.lib_gbm = ptr::null_mut();
            }
        }
    }
}
