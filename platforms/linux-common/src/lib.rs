//! Shared Linux I/O Layer:
//! - ALSA (`libasound.so`) direct capture & `libfdk-aac` AAC-LC encoder FFI (Issue #2)
//! - V4L2 (`DMA-BUF`) & Surfaceless EGL / Vulkan HAL (Issue #6)

#[cfg(target_os = "linux")]
pub mod alsa;
#[cfg(target_os = "linux")]
pub mod fdk_aac;

pub use core_types;
