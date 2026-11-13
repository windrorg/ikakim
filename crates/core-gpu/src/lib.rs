//! wgpu / wgpu-hal Zero-Copy Compositor & WGSL Shaders (NV12/YUYV, Top-Bottom Alpha) (Issue #5)

pub mod async_rt;
pub mod compositor;
pub mod governor;

pub use compositor::{
    ColorSpace, CompositorConfig, GpuCompositor, GpuContext, GpuLayer, LayerInputKind,
    OffscreenRenderSink, RenderStats,
};
pub use governor::{GovernorStats, OverlayGovernor};
