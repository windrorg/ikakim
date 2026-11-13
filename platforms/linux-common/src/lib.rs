//! Shared Linux I/O Layer:
//! - ALSA (`libasound.so`) direct capture & `libfdk-aac` AAC-LC encoder FFI (Issue #2)
//! - V4L2 (`DMA-BUF`), Surfaceless EGL / Vulkan HAL, & dynamic PNG/JPEG decode (Issue #6)
//! - RK3588 MPP, Amlogic/V4L2 M2M HW Codecs, DRM/GBM Swapchain & Codec Auto-Select (Issue #7)

#[cfg(target_os = "linux")]
pub mod alsa;
#[cfg(target_os = "linux")]
pub mod codec_factory;
#[cfg(target_os = "linux")]
pub mod dmabuf_gpu;
#[cfg(target_os = "linux")]
pub mod drm_gbm;
#[cfg(target_os = "linux")]
pub mod fdk_aac;
#[cfg(target_os = "linux")]
pub mod image_decode;
#[cfg(target_os = "linux")]
pub mod rockchip_mpp;
#[cfg(target_os = "linux")]
pub mod v4l2;
#[cfg(target_os = "linux")]
pub mod v4l2_m2m;

pub use core_types;
