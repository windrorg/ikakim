//! Android Native HAL Bridge (`AHardwareBuffer` + Vulkan External Memory, `AMediaCodec`, `AAudio`)

pub mod aaudio;
pub mod ahb_vulkan;
pub mod mediacodec;

pub use aaudio::{AAudioBackend, AAudioCaptureBridge, AAudioConfig, MockAAudioBackend};
pub use ahb_vulkan::{
    AHardwareBufferDesc, AhbVkImportPlan, AndroidAhbImporter, AndroidHalBackend,
    MockAndroidHalBackend, AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM,
    AHARDWAREBUFFER_FORMAT_Y8CBCR_420_888, AHARDWAREBUFFER_USAGE_GPU_FRAMEBUFFER,
    AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE,
    VK_ANDROID_EXTERNAL_MEMORY_ANDROID_HARDWARE_BUFFER_EXTENSION_NAME,
    VK_EXTERNAL_MEMORY_HANDLE_TYPE_ANDROID_HARDWARE_BUFFER_BIT_ANDROID,
};
pub use mediacodec::{
    AndroidMediaCodecEncoder, DualAndroidEncoder, MediaCodecBackend, MediaCodecEncoderConfig,
    MediaCodecStreamRole, MockMediaCodecBackend,
};
