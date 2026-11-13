//! Shared Linux I/O Layer:
//! - ALSA (`libasound.so`) direct capture & `libfdk-aac` AAC-LC encoder FFI (Issue #2)
//! - V4L2 (`DMA-BUF`), Surfaceless EGL / Vulkan HAL, & dynamic PNG/JPEG decode (Issue #6)

#[cfg(target_os = "linux")]
pub mod alsa;
#[cfg(target_os = "linux")]
pub mod dmabuf_gpu;
#[cfg(target_os = "linux")]
pub mod fdk_aac;
#[cfg(target_os = "linux")]
pub mod image_decode;
#[cfg(target_os = "linux")]
pub mod v4l2;

pub use core_types;
