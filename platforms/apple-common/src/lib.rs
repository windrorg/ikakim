//! Shared Apple (macOS / iOS) Zero-Copy & Media I/O Bridge
//! - AudioToolbox AAC-LC Encoder & CoreAudio AudioQueue Capture (Issue #2)
//! - AVFoundation / CVMetalTextureCache / VideoToolbox (Issue #6)

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub mod audio_toolbox;

pub use core_types;
