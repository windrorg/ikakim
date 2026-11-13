//! Streaming Pipeline Coordinator & A/V Sync Scheduler

pub mod pipeline;
pub mod sw_h264;

pub use pipeline::{flush_encoders_to_outputs, EngineControlBridge, PipelineTickContext};
pub use sw_h264::{SwH264Decoder, SwH264Encoder};
