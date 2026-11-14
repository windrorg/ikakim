//! ストリーミングパイプライン統合（Audio Mixer + GPU Compositor + RTMPS + HTTP/WS MSE）

use core_audio::{ChannelHandle, MasterHandle};
use core_gpu::{GpuCompositor, GpuLayer};
use core_net_ui::rpc::{
    AudioChannelState, ControlBackend, MeterSnapshot, OverlayLayerState,
};
use core_net_ui::EmbeddedServer;
use core_rtmp::{
    AbrConfig, AbrDecision, PublisherConfig, PublisherEvent, PublisherStats, RtmpPublisher,
    RtmpUrl, StreamMetadata, TlsRootConfig,
};
use core_types::clock::{MasterClock, PtsUs};
use core_types::media::{CodecKind, EncodedPacket, LayerTransform};
use core_types::state::{PipelineState, StateMachine};
use core_types::traits::{AudioEncoder, VideoEncoder};
use core_types::CoreResult;
use std::sync::{Arc, Mutex, Weak};

/// Web UI JSON-RPC と `AudioMixer`（ロックフリー `ChannelHandle` / `MasterHandle`）・
/// `GpuLayer`・`RtmpPublisher` を橋渡しする共有制御ブリッジ
pub struct EngineControlBridge {
    inner: Arc<Mutex<BridgeInner>>,
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
    cached_video_seq_header: Option<EncodedPacket>,
    cached_audio_seq_header: Option<EncodedPacket>,
    pending_bitrate_bps: Option<u32>,
    pending_keyframe_request: bool,
    abr_override: Option<AbrConfig>,
    reconnect_initial_ms: u64,
    reconnect_max_ms: u64,
    max_reconnect_attempts: Option<u32>,
    abr_adjustment_count: u64,
    last_abr_decision: Option<AbrDecision>,
    last_error: Option<String>,
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
            inner: Arc::new(Mutex::new(BridgeInner {
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
                cached_video_seq_header: None,
                cached_audio_seq_header: None,
                pending_bitrate_bps: None,
                pending_keyframe_request: false,
                abr_override: None,
                reconnect_initial_ms: 250,
                reconnect_max_ms: 4_000,
                max_reconnect_attempts: Some(5),
                abr_adjustment_count: 0,
                last_abr_decision: None,
                last_error: None,
            })),
        })
    }

    pub fn set_abr_config(&self, cfg: AbrConfig) {
        self.inner.lock().unwrap().abr_override = Some(cfg);
    }

    pub fn set_reconnect_policy(
        &self,
        initial_ms: u64,
        max_ms: u64,
        max_attempts: Option<u32>,
    ) {
        let mut g = self.inner.lock().unwrap();
        g.reconnect_initial_ms = initial_ms;
        g.reconnect_max_ms = max_ms;
        g.max_reconnect_attempts = max_attempts;
    }

    pub fn publisher_stats(&self) -> Option<PublisherStats> {
        self.inner
            .lock()
            .unwrap()
            .publisher
            .as_ref()
            .map(RtmpPublisher::stats)
    }

    pub fn last_error(&self) -> Option<String> {
        self.inner.lock().unwrap().last_error.clone()
    }

    pub fn abr_adjustment_count(&self) -> u64 {
        self.inner.lock().unwrap().abr_adjustment_count
    }

    pub fn last_abr_decision(&self) -> Option<AbrDecision> {
        self.inner.lock().unwrap().last_abr_decision.clone()
    }

    /// ピークメーター（30Hz 通知用）の最新値をロックフリーハンドルから読み取って更新する（ヒープ確保ゼロ）
    pub fn refresh_peaks(&self) {
        let mut g = self.inner.lock().unwrap();
        let BridgeInner {
            ref channels,
            ref master,
            ref mut last_master_peak,
            ref mut last_channel_peaks,
            ..
        } = *g;
        *last_master_peak = master.take_peak_dbfs();
        for (id, h) in channels.iter().enumerate() {
            let (l, r) = h.take_peak_dbfs();
            if let Some(slot) = last_channel_peaks.iter_mut().find(|c| c.0 == id) {
                slot.1 = l;
                slot.2 = r;
            }
        }
    }

    /// 指定レイヤーの現在の `LayerTransform` を取得する（毎フレームレイヤーを再生成する経路向け）
    pub fn layer_transform(&self, layer_id: usize) -> LayerTransform {
        let g = self.inner.lock().unwrap();
        g.overlays
            .iter()
            .find(|o| o.layer_id == layer_id)
            .map(|o| o.transform)
            .unwrap_or_default()
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
        let mut g = self.inner.lock().unwrap();
        if packet.is_sequence_header {
            match packet.codec {
                CodecKind::H264 => g.cached_video_seq_header = Some(packet.clone()),
                CodecKind::AacLc => g.cached_audio_seq_header = Some(packet.clone()),
                _ => {}
            }
        }
        if let Some(pub_ref) = &g.publisher {
            let _ = pub_ref.enqueue(packet.clone());
        }
    }

    /// 所有権付き `EncodedPacket` をクローンなしで RTMPS パブリッシャへゼロコピー転送する
    pub fn forward_owned_packet_to_rtmp(&self, packet: core_types::media::EncodedPacket) {
        let mut g = self.inner.lock().unwrap();
        if packet.is_sequence_header {
            match packet.codec {
                CodecKind::H264 => g.cached_video_seq_header = Some(packet.clone()),
                CodecKind::AacLc => g.cached_audio_seq_header = Some(packet.clone()),
                _ => {}
            }
        }
        if let Some(pub_ref) = &g.publisher {
            let _ = pub_ref.enqueue(packet);
        }
    }

    /// 待機中の ABR ビットレート変更・キーフレーム要求を映像エンコーダへ反映する
    pub fn apply_pending_encoder_controls(
        &self,
        main_encoder: &mut dyn VideoEncoder,
    ) -> CoreResult<()> {
        let (bps_opt, keyframe_req) = {
            let mut g = self.inner.lock().unwrap();
            let b = g.pending_bitrate_bps.take();
            let k = g.pending_keyframe_request;
            g.pending_keyframe_request = false;
            (b, k)
        };
        if let Some(bps) = bps_opt {
            main_encoder.set_target_bitrate(bps)?;
            self.set_current_bitrate(bps);
        }
        if keyframe_req {
            main_encoder.request_keyframe();
        }
        Ok(())
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
        let old = {
            let mut g = self.inner.lock().unwrap();
            let p = g.publisher.take();
            let _ = g.state_machine.transition_to(PipelineState::Idle);
            p
        };
        if let Some(mut p) = old {
            p.stop();
        }
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
        let old_pub = {
            let mut g = self.inner.lock().unwrap();
            g.publisher.take()
        };
        if let Some(mut old) = old_pub {
            old.stop();
        }

        let mut g = self.inner.lock().unwrap();
        if let Some(br) = bitrate_bps {
            g.bitrate_bps = br;
            g.pending_bitrate_bps = Some(br);
        }
        g.stream_url = rtmp_url.to_string();
        g.last_error = None;
        g.last_abr_decision = None;
        g.pending_keyframe_request = true;

        let mut abr_cfg = g.abr_override.clone().unwrap_or_default();
        abr_cfg.initial_bitrate_bps = g.bitrate_bps;

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
            reconnect_initial_ms: g.reconnect_initial_ms,
            reconnect_max_ms: g.reconnect_max_ms,
            max_reconnect_attempts: g.max_reconnect_attempts,
        };

        let weak_inner: Weak<Mutex<BridgeInner>> = Arc::downgrade(&self.inner);
        let publisher = RtmpPublisher::spawn(pub_cfg, move |ev: PublisherEvent| {
            if let Some(arc) = weak_inner.upgrade() {
                if let Ok(mut guard) = arc.lock() {
                    if guard.publisher.is_none() {
                        return;
                    }
                    match ev {
                        PublisherEvent::StateChanged(next_state) => {
                            let _ = guard.state_machine.transition_to(next_state);
                        }
                        PublisherEvent::AbrUpdated(decision) => {
                            guard.bitrate_bps = decision.current_bitrate_bps;
                            if let Some(new_bps) = decision.new_bitrate_bps {
                                guard.pending_bitrate_bps = Some(new_bps);
                                guard.abr_adjustment_count += 1;
                            }
                            if decision.request_keyframe {
                                guard.pending_keyframe_request = true;
                            }
                            guard.last_abr_decision = Some(decision);
                        }
                        PublisherEvent::Error(msg) => {
                            guard.last_error = Some(msg);
                        }
                    }
                }
            }
        });

        if let Some(ref v_seq) = g.cached_video_seq_header {
            let _ = publisher.enqueue(v_seq.clone());
        }
        if let Some(ref a_seq) = g.cached_audio_seq_header {
            let _ = publisher.enqueue(a_seq.clone());
        }

        g.publisher = Some(publisher);
        if g.state_machine.current() == PipelineState::Idle {
            let _ = g.state_machine.transition_to(PipelineState::Previewing);
        }
        let _ = g.state_machine.transition_to(PipelineState::Streaming);
        Ok(())
    }

    fn stop_stream(&self) -> Result<(), String> {
        let old = {
            let mut g = self.inner.lock().unwrap();
            let p = g.publisher.take();
            let _ = g.state_machine.transition_to(PipelineState::Previewing);
            p
        };
        if let Some(mut p) = old {
            p.stop();
        }
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
    ctx.bridge
        .apply_pending_encoder_controls(ctx.main_encoder)?;

    while let Some(pkt) = ctx.audio_encoder.pull_packet()? {
        ctx.bridge.forward_packet_to_rtmp(&pkt);
        let _ = ctx.server.push_preview_packet(&pkt);
    }

    while let Some(pkt) = ctx.main_encoder.pull_packet()? {
        ctx.bridge.forward_owned_packet_to_rtmp(pkt);
    }

    while let Some(pkt) = ctx.preview_encoder.pull_packet()? {
        let _ = ctx.server.push_preview_packet(&pkt);
    }

    Ok(())
}
