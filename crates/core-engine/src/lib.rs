//! Streaming Pipeline Coordinator & A/V Sync Scheduler

pub mod pipeline;
pub mod sw_h264;
pub mod virtual_input;

pub use pipeline::{flush_encoders_to_outputs, EngineControlBridge, PipelineTickContext};
pub use sw_h264::{SwH264Decoder, SwH264Encoder};
pub use virtual_input::{
    channel_rms_dbfs, goertzel_magnitude, VirtualAudioSignal, VirtualAudioSource,
    VirtualVideoPattern, VirtualVideoSource,
};
