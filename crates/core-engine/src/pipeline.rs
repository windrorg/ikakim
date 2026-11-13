//! ストリーミングパイプライン統合（Audio Mixer + GPU Compositor + RTMPS + HTTP/WS MSE）

use core_audio::{ChannelHandle, MasterHandle};
use core_gpu::{GpuCompositor, GpuLayer};
use core_net_ui::rpc::{
    AudioChannelState, ControlBackend, MeterSnapshot, OverlayLayerState,
};
use core_net_ui::EmbeddedServer;
use core_rtmp::{
    AbrConfig, PublisherConfig, PublisherEvent, RtmpPublisher, RtmpUrl, StreamMetadata,
    TlsRootConfig,
};
use core_types::clock::{MasterClock, PtsUs};
use core_types::media::LayerTransform;
use core_types::state::{PipelineState, StateMachine};
use core_types::traits::{AudioEncoder, VideoEncoder};
use core_types::CoreResult;
use std::sync::{Arc, Mutex};

/// Web UI JSON-RPC と `AudioMixer`（ロックフリー `ChannelHandle` / `MasterHandle`）・
/// `GpuLayer`・`RtmpPublisher` を橋渡しする共有制御ブリッジ
pub struct EngineControlBridge {
    inner: Mutex<BridgeInner>,
}

struct BridgeInner {
    state_machine: StateMachine,
    channels: Vec<ChannelHandle>,
    master: MasterHandle,
    stream_url: String,
    bitrate_bps: u32,
    fps: f32,
    tls: TlsRootConfig,
    publisher: Option<RtmpPublisher>,
    overlays: Vec<OverlayLayerState>,
    pending_transform_updates: Vec<(usize, LayerTransform)>,
    last_master_peak: (f32, f32),
    last_channel_peaks: Vec<(usize, f32, f32)>,
}

impl EngineControlBridge {
    pub fn new(
        channels: Vec<ChannelHandle>,
        master: MasterHandle,
        initial_bitrate_bps: u32,
        fps: f32,
        tls: TlsRootConfig,
    ) -> Arc<Self> {
        let last_channel_peaks = (0..channels.len()).map(|id| (id, -96.0, -96.0)).collect();
        Arc::new(Self {
            inner: Mutex::new(BridgeInner {
                state_machine: StateMachine::new(),
                channels,
                master,
                stream_url: String::new(),
                bitrate_bps: initial_bitrate_bps,
                fps,
                tls,
                publisher: None,
                overlays: vec![
                    OverlayLayerState {
                        layer_id: 0,
                        name: "Base Capture".into(),
                        transform: LayerTransform::default(),
                    },
                    OverlayLayerState {
                        layer_id: 1,
                        name: "Overlay Layer".into(),
                        transform: LayerTransform {
                            z_order: 1,
                            ..Default::default()
                        },
                    },
                ],
                pending_transform_updates: Vec::new(),
                last_master_peak: (-96.0, -96.0),
                last_channel_peaks,
            }),
        })
    }

    /// ピークメーター（30Hz 通知用）の最新値をロックフリーハンドルから読み取って更新する
    pub fn refresh_peaks(&self) {
        let mut g = self.inner.lock().unwrap();
        let peaks: Vec<(usize, f32, f32)> = g
            .channels
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let (l, r) = h.take_peak_dbfs();
                (i, l, r)
            })
            .collect();
        let master = g.master.take_peak_dbfs();
        g.last_master_peak = master;
        for (id, l, r) in peaks {
            if let Some(slot) = g.last_channel_peaks.iter_mut().find(|c| c.0 == id) {
                slot.1 = l;
                slot.2 = r;
            }
        }
    }

    /// 待機中の `LayerTransform` 変更を GPU レイヤー群へ反映する
    pub fn apply_pending_transforms(&self, compositor: &GpuCompositor, layers: &mut [&mut GpuLayer]) {
        let updates: Vec<(usize, LayerTransform)> = {
            let mut g = self.inner.lock().unwrap();
            std::mem::take(&mut g.pending_transform_updates)
        };
        for (layer_id, tr) in updates {
            for layer in layers.iter_mut() {
                if layer.layer_id == layer_id {
                    layer.set_transform(&compositor.context().queue, tr);
                }
            }
        }
    }

    /// 配信中であればエンコード済みパケットを RTMPS パブリッシャへ投入する
    pub fn forward_packet_to_rtmp(&self, packet: &core_types::media::EncodedPacket) {
        let g = self.inner.lock().unwrap();
        if let Some(pub_ref) = &g.publisher {
            let _ = pub_ref.enqueue(packet.clone());
        }
    }

    /// 配信状態・ビットレートを更新（ABR 通知反映等）
    pub fn set_current_bitrate(&self, bps: u32) {
        self.inner.lock().unwrap().bitrate_bps = bps;
    }

    /// ABR 決定ビットレートを HW/SW エンコーダと制御ブリッジ双方へ即時反映する
    pub fn apply_abr_bitrate(
        &self,
        target_bps: u32,
        main_encoder: &mut dyn VideoEncoder,
    ) -> CoreResult<()> {
        main_encoder.set_target_bitrate(target_bps)?;
        self.set_current_bitrate(target_bps);
        Ok(())
    }

    pub fn stop_all(&self) {
        let mut g = self.inner.lock().unwrap();
        if let Some(mut p) = g.publisher.take() {
            p.stop();
        }
        let _ = g.state_machine.transition_to(PipelineState::Idle);
    }
}

impl ControlBackend for EngineControlBridge {
    fn snapshot(&self) -> MeterSnapshot {
        let g = self.inner.lock().unwrap();
        let channels = g
            .channels
            .iter()
            .enumerate()
            .map(|(id, ch)| {
                let (l, r) = g
                    .last_channel_peaks
                    .iter()
                    .find(|x| x.0 == id)
                    .map(|x| (x.1, x.2))
                    .unwrap_or((-96.0, -96.0));
                let lin = ch.gain_linear();
                let gain_db = if lin <= 1e-5 { -96.0 } else { 20.0 * lin.log10() };
                AudioChannelState {
                    id,
                    name: ch.name().to_string(),
                    gain_db,
                    delay_ms: ch.delay_ms() as f32,
                    muted: ch.is_muted(),
                    left_dbfs: l,
                    right_dbfs: r,
                }
            })
            .collect();

        MeterSnapshot {
            timestamp_ms: 0,
            state: g.state_machine.current(),
            stream_url: g.stream_url.clone(),
            bitrate_bps: g.bitrate_bps,
            fps: g.fps,
            preview_mime: None,
            master_left_dbfs: g.last_master_peak.0,
            master_right_dbfs: g.last_master_peak.1,
            channels,
            overlays: g.overlays.clone(),
        }
    }

    fn start_stream(&self, rtmp_url: &str, bitrate_bps: Option<u32>) -> Result<(), String> {
        let parsed = RtmpUrl::parse(rtmp_url).map_err(|e| e.to_string())?;
        let mut g = self.inner.lock().unwrap();
        if let Some(mut old) = g.publisher.take() {
            old.stop();
        }
        if let Some(br) = bitrate_bps {
            g.bitrate_bps = br;
        }
        g.stream_url = rtmp_url.to_string();
        let abr_cfg = AbrConfig {
            initial_bitrate_bps: g.bitrate_bps,
            ..AbrConfig::default()
        };
        let pub_cfg = PublisherConfig {
            url: parsed,
            metadata: StreamMetadata {
                video_bitrate_bps: g.bitrate_bps,
                fps: g.fps as f64,
                ..StreamMetadata::default()
            },
            tls: g.tls.clone(),
            abr: abr_cfg,
            max_queue_bytes: 4 * 1024 * 1024,
            reconnect_initial_ms: 250,
            reconnect_max_ms: 4000,
            max_reconnect_attempts: Some(5),
        };
        let publisher = RtmpPublisher::spawn(pub_cfg, |_ev: PublisherEvent| {});
        g.publisher = Some(publisher);
        if g.state_machine.current() == PipelineState::Idle {
            let _ = g.state_machine.transition_to(PipelineState::Previewing);
        }
        let _ = g.state_machine.transition_to(PipelineState::Streaming);
        Ok(())
    }

    fn stop_stream(&self) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap();
        if let Some(mut p) = g.publisher.take() {
            p.stop();
        }
        let _ = g.state_machine.transition_to(PipelineState::Previewing);
        Ok(())
    }

    fn set_audio_gain(&self, channel: usize, gain_db: f32) -> Result<(), String> {
        let g = self.inner.lock().unwrap();
        let ch = g
            .channels
            .get(channel)
            .ok_or_else(|| format!("invalid channel {}", channel))?;
        ch.set_gain_db(gain_db);
        Ok(())
    }

    fn set_audio_mute(&self, channel: usize, muted: bool) -> Result<(), String> {
        let g = self.inner.lock().unwrap();
        let ch = g
            .channels
            .get(channel)
            .ok_or_else(|| format!("invalid channel {}", channel))?;
        ch.set_muted(muted);
        Ok(())
    }

    fn set_audio_delay(&self, channel: usize, delay_ms: f32) -> Result<(), String> {
        let g = self.inner.lock().unwrap();
        let ch = g
            .channels
            .get(channel)
            .ok_or_else(|| format!("invalid channel {}", channel))?;
        ch.set_delay_ms(delay_ms.max(0.0).round() as u32);
        Ok(())
    }

    fn set_overlay_transform(
        &self,
        layer_id: usize,
        transform: LayerTransform,
    ) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap();
        if let Some(ov) = g.overlays.iter_mut().find(|o| o.layer_id == layer_id) {
            ov.transform = transform;
        } else {
            g.overlays.push(OverlayLayerState {
                layer_id,
                name: format!("Layer {}", layer_id),
                transform,
            });
        }
        g.pending_transform_updates.push((layer_id, transform));
        Ok(())
    }
}

/// 1 ティック分の A/V 出力を統合処理するコーディネータ
pub struct PipelineTickContext<'a> {
    pub clock: &'a mut MasterClock,
    pub bridge: &'a EngineControlBridge,
    pub compositor: &'a mut GpuCompositor,
    pub main_encoder: &'a mut dyn VideoEncoder,
    pub preview_encoder: &'a mut dyn VideoEncoder,
    pub audio_encoder: &'a mut dyn AudioEncoder,
    pub server: &'a EmbeddedServer,
}

/// 音声ミックス出力 + エンコード済み映像パケットを RTMPS & fMP4/MSE サーバーへ同時配信する
pub fn flush_encoders_to_outputs(ctx: &mut PipelineTickContext<'_>, _pts: PtsUs) -> CoreResult<()> {
    ctx.bridge.refresh_peaks();

    while let Some(pkt) = ctx.audio_encoder.pull_packet()? {
        ctx.bridge.forward_packet_to_rtmp(&pkt);
        let _ = ctx.server.push_preview_packet(&pkt);
    }

    while let Some(pkt) = ctx.main_encoder.pull_packet()? {
        ctx.bridge.forward_packet_to_rtmp(&pkt);
    }

    while let Some(pkt) = ctx.preview_encoder.pull_packet()? {
        let _ = ctx.server.push_preview_packet(&pkt);
    }

    Ok(())
}
