//! Zero-Dependency Embedded HTTP/1.1 & WebSocket Server, JSON-RPC, and fMP4/MSE Muxer (Issue #4)

pub mod fmp4;
pub mod http;
pub mod json;
pub mod rpc;
pub mod server;
pub mod sha1_b64;
pub mod ws;

pub use fmp4::{AudioTrackConfig, Fmp4Muxer, Fmp4Segment, VideoTrackConfig};
pub use json::{parse_json, JsonValue};
pub use rpc::{
    handle_rpc_message, make_meter_notification, AudioChannelState, ControlBackend,
    DefaultControlBackend, MeterSnapshot, OverlayLayerState,
};
pub use server::{EmbeddedServer, ServerConfig};
