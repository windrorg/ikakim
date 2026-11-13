//! Shared Apple (macOS / iOS) Zero-Copy & Media I/O Bridge
//! - AudioToolbox AAC-LC Encoder & CoreAudio AudioQueue Capture (Issue #2)
//! - AVFoundation / CVMetalTextureCache / VideoToolbox / ImageIO (Issue #6)

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod audio_toolbox;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod av_capture;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod cv_metal;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod image_io;
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod video_toolbox;

pub use core_types;
