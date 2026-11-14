use crate::abr::{AbrConfig, AbrDecision, AimdAbrController};
use crate::client::{RtmpClient, StreamMetadata};
use crate::transport::{RtmpUrl, TlsRootConfig};
use core_types::media::{CodecKind, EncodedPacket};
use core_types::state::PipelineState;
use core_types::{CoreError, CoreResult};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct PublisherConfig {
    pub url: RtmpUrl,
    pub metadata: StreamMetadata,
    pub tls: TlsRootConfig,
    pub abr: AbrConfig,
    /// キューの最大バイト数（デフォルト: 8 MiB）
    pub max_queue_bytes: usize,
    /// 再接続の初回バックオフ（既定: 1000ms）
    pub reconnect_initial_ms: u64,
    /// 再接続の最大バックオフ（既定: 30000ms）
    pub reconnect_max_ms: u64,
    /// 最大再接続回数（None = 無制限）
    pub max_reconnect_attempts: Option<u32>,
}

#[derive(Debug, Clone)]
pub enum PublisherEvent {
    StateChanged(PipelineState),
    AbrUpdated(AbrDecision),
    Error(String),
}

#[derive(Debug, Default, Clone)]
pub struct PublisherStats {
    pub bytes_sent: u64,
    pub packets_sent: u64,
    pub packets_dropped_abr: u64,
    pub reconnect_count: u64,
    pub queued_bytes: usize,
    pub current_bitrate_bps: u32,
}

struct SharedQueue {
    items: VecDeque<EncodedPacket>,
    queued_bytes: usize,
    closed: bool,
    video_seq_header: Option<EncodedPacket>,
    audio_seq_header: Option<EncodedPacket>,
    stream_start_dts: Option<core_types::clock::PtsUs>,
}

/// バックグラウンド送信スレッド・AIMD ABR 監視・自動再接続バックオフを備えた RTMPS パブリッシャー
pub struct RtmpPublisher {
    queue: Arc<(Mutex<SharedQueue>, Condvar)>,
    queued_bytes_atomic: Arc< std::sync::atomic::AtomicUsize>,
    stop_flag: Arc<AtomicBool>,
    bytes_sent: Arc<AtomicU64>,
    packets_sent: Arc<AtomicU64>,
    packets_dropped_abr: Arc<AtomicU64>,
    reconnect_count: Arc<AtomicU64>,
    current_bitrate_bps: Arc<AtomicU64>,
    drop_non_idr_flag: Arc<AtomicBool>,
    max_queue_bytes: usize,
    join_handle: Option<JoinHandle<()>>,
    abr_handle: Option<JoinHandle<()>>,
}

fn sleep_interruptible(duration_ms: u64, stop_flag: &AtomicBool) {
    let sleep_step = Duration::from_millis(50);
    let mut waited = 0u64;
    while waited < duration_ms && !stop_flag.load(Ordering::Acquire) {
        thread::sleep(sleep_step);
        waited += 50;
    }
}

impl RtmpPublisher {
    pub fn spawn<F>(config: PublisherConfig, on_event: F) -> Self
    where
        F: FnMut(PublisherEvent) + Send + 'static,
    {
        let initial_bps = config.abr.initial_bitrate_bps;
        let max_queue_bytes = config.max_queue_bytes.max(256 * 1024);

        let queue = Arc::new((
            Mutex::new(SharedQueue {
                items: VecDeque::with_capacity(256),
                queued_bytes: 0,
                closed: false,
                video_seq_header: None,
                audio_seq_header: None,
                stream_start_dts: None,
            }),
            Condvar::new(),
        ));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let queued_bytes_atomic = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let is_connected = Arc::new(AtomicBool::new(false));
        let drop_non_idr_flag = Arc::new(AtomicBool::new(false));
        let bytes_sent = Arc::new(AtomicU64::new(0));
        let packets_sent = Arc::new(AtomicU64::new(0));
        let packets_dropped_abr = Arc::new(AtomicU64::new(0));
        let reconnect_count = Arc::new(AtomicU64::new(0));
        let current_bitrate_bps = Arc::new(AtomicU64::new(initial_bps as u64));
        let on_event_shared = Arc::new(Mutex::new(on_event));

        let q_clone = Arc::clone(&queue);
        let q_bytes_pub = Arc::clone(&queued_bytes_atomic);
        let stop_clone = Arc::clone(&stop_flag);
        let conn_clone = Arc::clone(&is_connected);
        let drop_flag_pub = Arc::clone(&drop_non_idr_flag);
        let bytes_clone = Arc::clone(&bytes_sent);
        let pkts_clone = Arc::clone(&packets_sent);
        let drops_clone = Arc::clone(&packets_dropped_abr);
        let reconn_clone = Arc::clone(&reconnect_count);
        let br_clone = Arc::clone(&current_bitrate_bps);
        let on_event_pub = Arc::clone(&on_event_shared);

        // ABR 評価を送信ソケット I/O から独立したスレッドで実行し、TCP 詰まり中でも即座にエンコーダ減速させる
        let abr_cfg = config.abr.clone();
        let q_bytes_abr = Arc::clone(&queued_bytes_atomic);
        let stop_abr = Arc::clone(&stop_flag);
        let conn_abr = Arc::clone(&is_connected);
        let drop_flag_abr = Arc::clone(&drop_non_idr_flag);
        let br_abr = Arc::clone(&current_bitrate_bps);
        let on_event_abr = Arc::clone(&on_event_shared);
        let abr_handle = thread::Builder::new()
            .name("rtmp-abr-monitor".to_string())
            .spawn(move || {
                let mut abr = AimdAbrController::new(abr_cfg);
                let mut last_tick = Instant::now();
                while !stop_abr.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(50));
                    if stop_abr.load(Ordering::Acquire) {
                        break;
                    }
                    if !conn_abr.load(Ordering::Acquire) {
                        last_tick = Instant::now();
                        continue;
                    }
                    let elapsed = last_tick.elapsed();
                    if elapsed >= Duration::from_millis(200) {
                        let q_bytes = q_bytes_abr.load(Ordering::Acquire);
                        let decision = abr.evaluate(q_bytes, elapsed.as_millis() as u64);
                        last_tick = Instant::now();
                        drop_flag_abr.store(decision.drop_non_idr, Ordering::Release);
                        br_abr.store(decision.current_bitrate_bps as u64, Ordering::Relaxed);
                        if decision.new_bitrate_bps.is_some()
                            || decision.drop_non_idr
                            || decision.request_keyframe
                        {
                            (on_event_abr.lock().unwrap())(PublisherEvent::AbrUpdated(decision));
                        }
                    }
                }
            })
            .expect("spawn rtmp-abr-monitor thread");

        let join_handle = thread::Builder::new()
            .name("rtmp-publisher".to_string())
            .spawn(move || {
                let initial_backoff = config.reconnect_initial_ms.max(50);
                let mut backoff_ms = initial_backoff;
                let mut attempts = 0u32;
                let mut has_connected_before = false;
                let mut in_reconnect = false;
                let emit = move |ev: PublisherEvent| {
                    (on_event_pub.lock().unwrap())(ev);
                };

                'outer: while !stop_clone.load(Ordering::Acquire) {
                    conn_clone.store(false, Ordering::Release);
                    let reconnecting_round = std::mem::take(&mut in_reconnect) || attempts > 0;
                    if reconnecting_round {
                        emit(PublisherEvent::StateChanged(PipelineState::Reconnecting));
                    }

                    let mut client = match RtmpClient::connect_and_publish(
                        &config.url,
                        &config.metadata,
                        &config.tls,
                    ) {
                        Ok(c) => {
                            if reconnecting_round {
                                reconn_clone.fetch_add(1, Ordering::Relaxed);
                            }
                            attempts = 0;
                            backoff_ms = initial_backoff;
                            conn_clone.store(true, Ordering::Release);
                            emit(PublisherEvent::StateChanged(PipelineState::Streaming));
                            c
                        }
                        Err(e) => {
                            attempts += 1;
                            emit(PublisherEvent::Error(e.to_string()));
                            if let Some(max) = config.max_reconnect_attempts {
                                if attempts >= max {
                                    {
                                        let (lock, cvar) = &*q_clone;
                                        let mut guard = lock.lock().unwrap();
                                        guard.closed = true;
                                        cvar.notify_all();
                                    }
                                    emit(PublisherEvent::StateChanged(PipelineState::Error));
                                    break 'outer;
                                }
                            }
                            emit(PublisherEvent::StateChanged(PipelineState::Reconnecting));
                            sleep_interruptible(backoff_ms, &stop_clone);
                            backoff_ms = (backoff_ms * 2).min(config.reconnect_max_ms.max(1000));
                            continue 'outer;
                        }
                    };

                    // 再接続時はキャッシュ済みのシーケンスヘッダ（SPS/PPS・AAC AudioSpecificConfig）を先頭に再送し、
                    // 上流エンコーダへ IDR キーフレーム再送を要求する
                    let mut waiting_for_keyframe = true;
                    if has_connected_before {
                        let (v_seq, a_seq) = {
                            let guard = q_clone.0.lock().unwrap();
                            (guard.video_seq_header.clone(), guard.audio_seq_header.clone())
                        };
                        if let Some(pkt) = v_seq {
                            match client.send_packet(&pkt) {
                                Ok(n) => {
                                    bytes_clone.fetch_add(n as u64, Ordering::Relaxed);
                                    pkts_clone.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(e) => {
                                    in_reconnect = true;
                                    emit(PublisherEvent::Error(format!(
                                        "RTMP reconnect seq header failed: {}",
                                        e
                                    )));
                                    continue 'outer;
                                }
                            }
                        }
                        if let Some(pkt) = a_seq {
                            match client.send_packet(&pkt) {
                                Ok(n) => {
                                    bytes_clone.fetch_add(n as u64, Ordering::Relaxed);
                                    pkts_clone.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(e) => {
                                    in_reconnect = true;
                                    emit(PublisherEvent::Error(format!(
                                        "RTMP reconnect audio seq failed: {}",
                                        e
                                    )));
                                    continue 'outer;
                                }
                            }
                        }
                        emit(PublisherEvent::AbrUpdated(crate::abr::AbrDecision {
                            current_bitrate_bps: br_clone.load(Ordering::Relaxed) as u32,
                            new_bitrate_bps: None,
                            drop_non_idr: false,
                            request_keyframe: true,
                            dwell_ms: 0,
                        }));
                    }
                    has_connected_before = true;

                    // メディア送信ループ
                    let mut last_inbound_poll = Instant::now();
                    loop {
                        if stop_clone.load(Ordering::Acquire) {
                            client.close();
                            break 'outer;
                        }

                        let pkt_opt = {
                            let (lock, cvar) = &*q_clone;
                            let mut guard = lock.lock().unwrap();
                            if guard.items.is_empty() && !guard.closed {
                                let (g, _) = cvar
                                    .wait_timeout(guard, Duration::from_millis(20))
                                    .unwrap();
                                guard = g;
                            }
                            if let Some(pkt) = guard.items.pop_front() {
                                guard.queued_bytes = guard.queued_bytes.saturating_sub(pkt.data.len());
                                q_bytes_pub.store(guard.queued_bytes, Ordering::Release);
                                Some(pkt)
                            } else if guard.closed {
                                client.close();
                                break 'outer;
                            } else {
                                None
                            }
                        };

                        let queue_was_idle = pkt_opt.is_none();
                        if let Some(pkt) = pkt_opt {
                            if waiting_for_keyframe
                                && pkt.codec == CodecKind::H264
                                && !pkt.is_sequence_header
                            {
                                if pkt.is_keyframe {
                                    waiting_for_keyframe = false;
                                } else {
                                    drops_clone.fetch_add(1, Ordering::Relaxed);
                                    continue;
                                }
                            }

                            // 滞留 > 1000ms の緊急時は非IDR映像フレームを破棄し音声とリアルタイム性を保護
                            if drop_flag_pub.load(Ordering::Acquire)
                                && pkt.codec == CodecKind::H264
                                && !pkt.is_keyframe
                                && !pkt.is_sequence_header
                            {
                                drops_clone.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }

                            match client.send_packet(&pkt) {
                                Ok(n) => {
                                    bytes_clone.fetch_add(n as u64, Ordering::Relaxed);
                                    pkts_clone.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(e) => {
                                    in_reconnect = true;
                                    conn_clone.store(false, Ordering::Release);
                                    emit(PublisherEvent::StateChanged(PipelineState::Reconnecting));
                                    emit(PublisherEvent::Error(format!(
                                        "RTMP send failed, reconnecting: {}",
                                        e
                                    )));
                                    sleep_interruptible(backoff_ms, &stop_clone);
                                    break;
                                }
                            }
                        }

                        if queue_was_idle || last_inbound_poll.elapsed() >= Duration::from_millis(25) {
                            last_inbound_poll = Instant::now();
                            if let Err(e) = client.poll_inbound() {
                                in_reconnect = true;
                                conn_clone.store(false, Ordering::Release);
                                emit(PublisherEvent::StateChanged(PipelineState::Reconnecting));
                                emit(PublisherEvent::Error(format!(
                                    "RTMP connection closed/error from peer, reconnecting: {}",
                                    e
                                )));
                                sleep_interruptible(backoff_ms, &stop_clone);
                                break;
                            }
                        }
                    }
                }
                conn_clone.store(false, Ordering::Release);
            })
            .expect("spawn rtmp-publisher thread");

        Self {
            queue,
            queued_bytes_atomic,
            stop_flag,
            bytes_sent,
            packets_sent,
            packets_dropped_abr,
            reconnect_count,
            current_bitrate_bps,
            drop_non_idr_flag,
            max_queue_bytes,
            join_handle: Some(join_handle),
            abr_handle: Some(abr_handle),
        }
    }

    /// エンコード済みパケットを送信キューへ積む。緊急ABRドロップ中やキュー上限超過時は非キーフレームを破棄または `BufferFull` を返す。
    pub fn enqueue(&self, mut packet: EncodedPacket) -> CoreResult<()> {
        let (lock, cvar) = &*self.queue;
        let mut guard = lock.lock().unwrap();
        if guard.closed {
            return Err(CoreError::Network("publisher is closed".into()));
        }

        if packet.is_sequence_header {
            packet.pts = core_types::clock::PtsUs(0);
            packet.dts = core_types::clock::PtsUs(0);
            match packet.codec {
                CodecKind::H264 => guard.video_seq_header = Some(packet.clone()),
                CodecKind::AacLc => guard.audio_seq_header = Some(packet.clone()),
                _ => {}
            }
        } else {
            let base = *guard.stream_start_dts.get_or_insert(packet.dts);
            packet.pts = core_types::clock::PtsUs(packet.pts.0.saturating_sub(base.0));
            packet.dts = core_types::clock::PtsUs(packet.dts.0.saturating_sub(base.0));
        }

        if (self.drop_non_idr_flag.load(Ordering::Acquire)
            || guard.queued_bytes + packet.data.len() > self.max_queue_bytes)
            && !packet.is_sequence_header
            && packet.codec == CodecKind::H264
            && !packet.is_keyframe
        {
            self.packets_dropped_abr.fetch_add(1, Ordering::Relaxed);
            return Err(CoreError::BufferFull);
        }

        guard.queued_bytes += packet.data.len();
        self.queued_bytes_atomic
            .store(guard.queued_bytes, Ordering::Release);
        guard.items.push_back(packet);
        cvar.notify_one();
        Ok(())
    }

    pub fn stats(&self) -> PublisherStats {
        let queued_bytes = self.queued_bytes_atomic.load(Ordering::Acquire);
        PublisherStats {
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            packets_sent: self.packets_sent.load(Ordering::Relaxed),
            packets_dropped_abr: self.packets_dropped_abr.load(Ordering::Relaxed),
            reconnect_count: self.reconnect_count.load(Ordering::Relaxed),
            queued_bytes,
            current_bitrate_bps: self.current_bitrate_bps.load(Ordering::Relaxed) as u32,
        }
    }

    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        {
            let (lock, cvar) = &*self.queue;
            let mut guard = lock.lock().unwrap();
            guard.closed = true;
            cvar.notify_all();
        }
        if let Some(h) = self.abr_handle.take() {
            let _ = h.join();
        }
        if let Some(h) = self.join_handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for RtmpPublisher {
    fn drop(&mut self) {
        self.stop();
    }
}
