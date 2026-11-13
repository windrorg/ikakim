//! JSON-RPC 2.0 制御プロトコルおよび 30Hz メーター・ステータス通知

use crate::json::{parse_json, JsonValue};
use core_types::media::LayerTransform;
use core_types::state::PipelineState;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// 音声チャンネル状態
#[derive(Debug, Clone)]
pub struct AudioChannelState {
    pub id: usize,
    pub name: String,
    pub gain_db: f32,
    pub delay_ms: f32,
    pub muted: bool,
    pub left_dbfs: f32,
    pub right_dbfs: f32,
}

impl Default for AudioChannelState {
    fn default() -> Self {
        Self {
            id: 0,
            name: "Channel".into(),
            gain_db: 0.0,
            delay_ms: 0.0,
            muted: false,
            left_dbfs: -96.0,
            right_dbfs: -96.0,
        }
    }
}

/// オーバーレイレイヤー状態
#[derive(Debug, Clone)]
pub struct OverlayLayerState {
    pub layer_id: usize,
    pub name: String,
    pub transform: LayerTransform,
}

/// 30Hz 配信メーターおよびエンジン状態のスナップショット
#[derive(Debug, Clone)]
pub struct MeterSnapshot {
    pub timestamp_ms: u64,
    pub state: PipelineState,
    pub stream_url: String,
    pub bitrate_bps: u32,
    pub fps: f32,
    pub preview_mime: Option<String>,
    pub master_left_dbfs: f32,
    pub master_right_dbfs: f32,
    pub channels: Vec<AudioChannelState>,
    pub overlays: Vec<OverlayLayerState>,
}

/// コアエンジン（またはテストモック）への制御・参照トレイト
pub trait ControlBackend: Send + Sync {
    fn snapshot(&self) -> MeterSnapshot;
    fn start_stream(&self, rtmp_url: &str, bitrate_bps: Option<u32>) -> Result<(), String>;
    fn stop_stream(&self) -> Result<(), String>;
    fn set_audio_gain(&self, channel: usize, gain_db: f32) -> Result<(), String>;
    fn set_audio_mute(&self, channel: usize, muted: bool) -> Result<(), String>;
    fn set_audio_delay(&self, channel: usize, delay_ms: f32) -> Result<(), String>;
    fn set_overlay_transform(&self, layer_id: usize, transform: LayerTransform)
        -> Result<(), String>;
}

/// 単独動作・テスト向けのインメモリ制御バックエンド
pub struct DefaultControlBackend {
    inner: Mutex<BackendData>,
}

struct BackendData {
    state: PipelineState,
    stream_url: String,
    bitrate_bps: u32,
    fps: f32,
    preview_mime: Option<String>,
    master_left_dbfs: f32,
    master_right_dbfs: f32,
    channels: Vec<AudioChannelState>,
    overlays: Vec<OverlayLayerState>,
}

impl DefaultControlBackend {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(BackendData {
                state: PipelineState::Idle,
                stream_url: "rtmps://a.rtmps.youtube.com/live2/".into(),
                bitrate_bps: 4_500_000,
                fps: 60.0,
                preview_mime: None,
                master_left_dbfs: -96.0,
                master_right_dbfs: -96.0,
                channels: vec![
                    AudioChannelState {
                        id: 0,
                        name: "Camera Mic".into(),
                        ..Default::default()
                    },
                    AudioChannelState {
                        id: 1,
                        name: "HDMI Capture".into(),
                        ..Default::default()
                    },
                    AudioChannelState {
                        id: 2,
                        name: "Video Overlay".into(),
                        ..Default::default()
                    },
                ],
                overlays: vec![
                    OverlayLayerState {
                        layer_id: 0,
                        name: "Main Camera".into(),
                        transform: LayerTransform::default(),
                    },
                    OverlayLayerState {
                        layer_id: 1,
                        name: "Overlay Video (Alpha)".into(),
                        transform: LayerTransform {
                            x: 0.0,
                            y: 0.0,
                            scale_x: 0.5,
                            scale_y: 0.5,
                            z_order: 1,
                            ..Default::default()
                        },
                    },
                ],
            }),
        })
    }

    pub fn update_meters(&self, master_l: f32, master_r: f32, ch_peaks: &[(usize, f32, f32)]) {
        let mut g = self.inner.lock().unwrap();
        g.master_left_dbfs = master_l;
        g.master_right_dbfs = master_r;
        for &(ch_id, l, r) in ch_peaks {
            if let Some(ch) = g.channels.iter_mut().find(|c| c.id == ch_id) {
                ch.left_dbfs = l;
                ch.right_dbfs = r;
            }
        }
    }

    pub fn set_preview_mime(&self, mime: Option<String>) {
        self.inner.lock().unwrap().preview_mime = mime;
    }

    pub fn set_bitrate_and_fps(&self, bitrate_bps: u32, fps: f32) {
        let mut g = self.inner.lock().unwrap();
        g.bitrate_bps = bitrate_bps;
        g.fps = fps;
    }
}

impl ControlBackend for DefaultControlBackend {
    fn snapshot(&self) -> MeterSnapshot {
        let g = self.inner.lock().unwrap();
        MeterSnapshot {
            timestamp_ms: now_epoch_ms(),
            state: g.state,
            stream_url: g.stream_url.clone(),
            bitrate_bps: g.bitrate_bps,
            fps: g.fps,
            preview_mime: g.preview_mime.clone(),
            master_left_dbfs: g.master_left_dbfs,
            master_right_dbfs: g.master_right_dbfs,
            channels: g.channels.clone(),
            overlays: g.overlays.clone(),
        }
    }

    fn start_stream(&self, rtmp_url: &str, bitrate_bps: Option<u32>) -> Result<(), String> {
        if rtmp_url.trim().is_empty() {
            return Err("rtmp_url must not be empty".into());
        }
        let mut g = self.inner.lock().unwrap();
        if let Some(br) = bitrate_bps {
            g.bitrate_bps = br;
        }
        g.stream_url = rtmp_url.to_string();
        g.state = PipelineState::Streaming;
        Ok(())
    }

    fn stop_stream(&self) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap();
        g.state = PipelineState::Previewing;
        Ok(())
    }

    fn set_audio_gain(&self, channel: usize, gain_db: f32) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap();
        let ch = g
            .channels
            .iter_mut()
            .find(|c| c.id == channel)
            .ok_or_else(|| format!("unknown audio channel {}", channel))?;
        ch.gain_db = gain_db.clamp(-96.0, 24.0);
        Ok(())
    }

    fn set_audio_mute(&self, channel: usize, muted: bool) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap();
        let ch = g
            .channels
            .iter_mut()
            .find(|c| c.id == channel)
            .ok_or_else(|| format!("unknown audio channel {}", channel))?;
        ch.muted = muted;
        Ok(())
    }

    fn set_audio_delay(&self, channel: usize, delay_ms: f32) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap();
        let ch = g
            .channels
            .iter_mut()
            .find(|c| c.id == channel)
            .ok_or_else(|| format!("unknown audio channel {}", channel))?;
        ch.delay_ms = delay_ms.clamp(0.0, 2000.0);
        Ok(())
    }

    fn set_overlay_transform(
        &self,
        layer_id: usize,
        transform: LayerTransform,
    ) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap();
        if let Some(layer) = g.overlays.iter_mut().find(|l| l.layer_id == layer_id) {
            layer.transform = transform;
        } else {
            g.overlays.push(OverlayLayerState {
                layer_id,
                name: format!("Layer {}", layer_id),
                transform,
            });
        }
        Ok(())
    }
}

/// JSON-RPC 2.0 テキストメッセージを処理し、レスポンス JSON 文字列を返す
pub fn handle_rpc_message(backend: &dyn ControlBackend, text: &str) -> String {
    let parsed = match parse_json(text) {
        Ok(v) => v,
        Err(e) => {
            return make_error_response(&JsonValue::Null, -32700, &format!("Parse error: {}", e));
        }
    };

    let id = parsed.get("id").cloned().unwrap_or(JsonValue::Null);
    let method = match parsed.get("method").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => {
            return make_error_response(&id, -32600, "Invalid Request: missing method");
        }
    };
    let params = parsed
        .get("params")
        .cloned()
        .unwrap_or(JsonValue::Object(Vec::new()));

    match dispatch_method(backend, method, &params) {
        Ok(result) => make_ok_response(&id, result),
        Err((code, msg)) => make_error_response(&id, code, &msg),
    }
}

fn dispatch_method(
    backend: &dyn ControlBackend,
    method: &str,
    params: &JsonValue,
) -> Result<JsonValue, (i32, String)> {
    match method {
        "engine.get_status" => {
            let snap = backend.snapshot();
            Ok(snapshot_to_json(&snap))
        }
        "stream.start" => {
            let url = params
                .get("rtmp_url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| (-32602, "missing rtmp_url".to_string()))?;
            let br = params
                .get("bitrate_bps")
                .and_then(|v| v.as_u64())
                .map(|b| b as u32);
            backend
                .start_stream(url, br)
                .map_err(|e| (-32000, e))?;
            Ok(snapshot_to_json(&backend.snapshot()))
        }
        "stream.stop" => {
            backend.stop_stream().map_err(|e| (-32000, e))?;
            Ok(snapshot_to_json(&backend.snapshot()))
        }
        "audio.set_gain" => {
            let ch = params
                .get("channel")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| (-32602, "missing channel".to_string()))? as usize;
            let gain = params
                .get("gain_db")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| (-32602, "missing gain_db".to_string()))? as f32;
            backend
                .set_audio_gain(ch, gain)
                .map_err(|e| (-32000, e))?;
            Ok(JsonValue::Object(vec![("ok".into(), JsonValue::Bool(true))]))
        }
        "audio.set_mute" => {
            let ch = params
                .get("channel")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| (-32602, "missing channel".to_string()))? as usize;
            let muted = params
                .get("muted")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| (-32602, "missing muted".to_string()))?;
            backend
                .set_audio_mute(ch, muted)
                .map_err(|e| (-32000, e))?;
            Ok(JsonValue::Object(vec![("ok".into(), JsonValue::Bool(true))]))
        }
        "audio.set_delay" => {
            let ch = params
                .get("channel")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| (-32602, "missing channel".to_string()))? as usize;
            let delay = params
                .get("delay_ms")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| (-32602, "missing delay_ms".to_string()))? as f32;
            backend
                .set_audio_delay(ch, delay)
                .map_err(|e| (-32000, e))?;
            Ok(JsonValue::Object(vec![("ok".into(), JsonValue::Bool(true))]))
        }
        "overlay.set_transform" => {
            let layer_id = params
                .get("layer_id")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| (-32602, "missing layer_id".to_string()))?
                as usize;
            let existing = backend
                .snapshot()
                .overlays
                .into_iter()
                .find(|l| l.layer_id == layer_id)
                .map(|l| l.transform)
                .unwrap_or_default();
            let tr = LayerTransform {
                x: params
                    .get("x")
                    .and_then(|v| v.as_f64())
                    .map(|v| v as f32)
                    .unwrap_or(existing.x),
                y: params
                    .get("y")
                    .and_then(|v| v.as_f64())
                    .map(|v| v as f32)
                    .unwrap_or(existing.y),
                scale_x: params
                    .get("scale_x")
                    .and_then(|v| v.as_f64())
                    .map(|v| v as f32)
                    .unwrap_or(existing.scale_x),
                scale_y: params
                    .get("scale_y")
                    .and_then(|v| v.as_f64())
                    .map(|v| v as f32)
                    .unwrap_or(existing.scale_y),
                rotation_rad: params
                    .get("rotation_rad")
                    .and_then(|v| v.as_f64())
                    .map(|v| v as f32)
                    .unwrap_or(existing.rotation_rad),
                opacity: params
                    .get("opacity")
                    .and_then(|v| v.as_f64())
                    .map(|v| v as f32)
                    .unwrap_or(existing.opacity),
                z_order: params
                    .get("z_order")
                    .and_then(|v| v.as_i64())
                    .map(|v| v as i32)
                    .unwrap_or(existing.z_order),
                visible: params
                    .get("visible")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(existing.visible),
            };
            backend
                .set_overlay_transform(layer_id, tr)
                .map_err(|e| (-32000, e))?;
            Ok(JsonValue::Object(vec![("ok".into(), JsonValue::Bool(true))]))
        }
        "overlay.set_visible" => {
            let layer_id = params
                .get("layer_id")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| (-32602, "missing layer_id".to_string()))?
                as usize;
            let visible = params
                .get("visible")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| (-32602, "missing visible".to_string()))?;
            let mut tr = backend
                .snapshot()
                .overlays
                .into_iter()
                .find(|l| l.layer_id == layer_id)
                .map(|l| l.transform)
                .unwrap_or_default();
            tr.visible = visible;
            backend
                .set_overlay_transform(layer_id, tr)
                .map_err(|e| (-32000, e))?;
            Ok(JsonValue::Object(vec![("ok".into(), JsonValue::Bool(true))]))
        }
        _ => Err((-32601, format!("Method not found: {}", method))),
    }
}

/// 30Hz メーター通知用 JSON-RPC Notification 文字列を生成する
pub fn make_meter_notification(snap: &MeterSnapshot) -> String {
    JsonValue::Object(vec![
        ("jsonrpc".into(), JsonValue::String("2.0".into())),
        ("method".into(), JsonValue::String("meter.update".into())),
        ("params".into(), snapshot_to_json(snap)),
    ])
    .to_json_string()
}

/// プレビュー MIME 通知 JSON-RPC Notification 文字列を生成する
pub fn make_preview_codec_notification(mime: &str) -> String {
    JsonValue::Object(vec![
        ("jsonrpc".into(), JsonValue::String("2.0".into())),
        ("method".into(), JsonValue::String("preview.codec".into())),
        (
            "params".into(),
            JsonValue::Object(vec![("mime".into(), JsonValue::String(mime.to_string()))]),
        ),
    ])
    .to_json_string()
}

pub fn snapshot_to_json(snap: &MeterSnapshot) -> JsonValue {
    let channels_json = snap
        .channels
        .iter()
        .map(|c| {
            JsonValue::Object(vec![
                ("id".into(), JsonValue::Number(c.id as f64)),
                ("name".into(), JsonValue::String(c.name.clone())),
                ("gain_db".into(), JsonValue::Number(c.gain_db as f64)),
                ("delay_ms".into(), JsonValue::Number(c.delay_ms as f64)),
                ("muted".into(), JsonValue::Bool(c.muted)),
                ("left_dbfs".into(), JsonValue::Number(c.left_dbfs as f64)),
                ("right_dbfs".into(), JsonValue::Number(c.right_dbfs as f64)),
            ])
        })
        .collect();

    let overlays_json = snap
        .overlays
        .iter()
        .map(|o| {
            JsonValue::Object(vec![
                ("layer_id".into(), JsonValue::Number(o.layer_id as f64)),
                ("name".into(), JsonValue::String(o.name.clone())),
                ("x".into(), JsonValue::Number(o.transform.x as f64)),
                ("y".into(), JsonValue::Number(o.transform.y as f64)),
                ("scale_x".into(), JsonValue::Number(o.transform.scale_x as f64)),
                ("scale_y".into(), JsonValue::Number(o.transform.scale_y as f64)),
                (
                    "rotation_rad".into(),
                    JsonValue::Number(o.transform.rotation_rad as f64),
                ),
                ("opacity".into(), JsonValue::Number(o.transform.opacity as f64)),
                ("z_order".into(), JsonValue::Number(o.transform.z_order as f64)),
                ("visible".into(), JsonValue::Bool(o.transform.visible)),
            ])
        })
        .collect();

    let state_str = format!("{:?}", snap.state);
    JsonValue::Object(vec![
        ("timestamp_ms".into(), JsonValue::Number(snap.timestamp_ms as f64)),
        (
            "stream".into(),
            JsonValue::Object(vec![
                ("state".into(), JsonValue::String(state_str)),
                ("rtmp_url".into(), JsonValue::String(snap.stream_url.clone())),
                ("bitrate_bps".into(), JsonValue::Number(snap.bitrate_bps as f64)),
                ("fps".into(), JsonValue::Number(snap.fps as f64)),
            ]),
        ),
        (
            "preview_mime".into(),
            match &snap.preview_mime {
                Some(m) => JsonValue::String(m.clone()),
                None => JsonValue::Null,
            },
        ),
        (
            "master".into(),
            JsonValue::Object(vec![
                (
                    "left_dbfs".into(),
                    JsonValue::Number(snap.master_left_dbfs as f64),
                ),
                (
                    "right_dbfs".into(),
                    JsonValue::Number(snap.master_right_dbfs as f64),
                ),
            ]),
        ),
        ("channels".into(), JsonValue::Array(channels_json)),
        ("overlays".into(), JsonValue::Array(overlays_json)),
    ])
}

fn make_ok_response(id: &JsonValue, result: JsonValue) -> String {
    JsonValue::Object(vec![
        ("jsonrpc".into(), JsonValue::String("2.0".into())),
        ("id".into(), id.clone()),
        ("result".into(), result),
    ])
    .to_json_string()
}

fn make_error_response(id: &JsonValue, code: i32, message: &str) -> String {
    JsonValue::Object(vec![
        ("jsonrpc".into(), JsonValue::String("2.0".into())),
        ("id".into(), id.clone()),
        (
            "error".into(),
            JsonValue::Object(vec![
                ("code".into(), JsonValue::Number(code as f64)),
                ("message".into(), JsonValue::String(message.to_string())),
            ]),
        ),
    ])
    .to_json_string()
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
