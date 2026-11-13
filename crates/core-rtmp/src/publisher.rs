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
}

/// バックグラウンド送信スレッド・AIMD ABR 監視・自動再接続バックオフを備えた RTMPS パブリッシャー
pub struct RtmpPublisher {
    queue: Arc<(Mutex<SharedQueue>, Condvar)>,
    stop_flag: Arc<AtomicBool>,
    bytes_sent: Arc<AtomicU64>,
    packets_sent: Arc<AtomicU64>,
    packets_dropped_abr: Arc<AtomicU64>,
    reconnect_count: Arc<AtomicU64>,
    current_bitrate_bps: Arc<AtomicU64>,
    max_queue_bytes: usize,
    join_handle: Option<JoinHandle<()>>,
}

impl RtmpPublisher {
    pub fn spawn<F>(config: PublisherConfig, mut on_event: F) -> Self
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
            }),
            Condvar::new(),
        ));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let bytes_sent = Arc::new(AtomicU64::new(0));
        let packets_sent = Arc::new(AtomicU64::new(0));
        let packets_dropped_abr = Arc::new(AtomicU64::new(0));
        let reconnect_count = Arc::new(AtomicU64::new(0));
        let current_bitrate_bps = Arc::new(AtomicU64::new(initial_bps as u64));

        let q_clone = Arc::clone(&queue);
        let stop_clone = Arc::clone(&stop_flag);
        let bytes_clone = Arc::clone(&bytes_sent);
        let pkts_clone = Arc::clone(&packets_sent);
        let drops_clone = Arc::clone(&packets_dropped_abr);
        let reconn_clone = Arc::clone(&reconnect_count);
        let br_clone = Arc::clone(&current_bitrate_bps);

        let join_handle = thread::Builder::new()
            .name("rtmp-publisher".to_string())
            .spawn(move || {
                let mut abr = AimdAbrController::new(config.abr.clone());
                let mut backoff_ms = config.reconnect_initial_ms.max(100);
                let mut attempts = 0u32;
                let mut has_connected_before = false;
                let mut last_abr_tick = Instant::now();
                let mut drop_non_idr = false;

                'outer: while !stop_clone.load(Ordering::Acquire) {
                    on_event(PublisherEvent::StateChanged(if attempts == 0 {
                        PipelineState::Streaming
                    } else {
                        PipelineState::Reconnecting
                    }));

                    let mut client = match RtmpClient::connect_and_publish(
                        &config.url,
                        &config.metadata,
                        &config.tls,
                    ) {
                        Ok(c) => {
                            if attempts > 0 {
                                reconn_clone.fetch_add(1, Ordering::Relaxed);
                            }
                            attempts = 0;
                            backoff_ms = config.reconnect_initial_ms.max(100);
                            on_event(PublisherEvent::StateChanged(PipelineState::Streaming));
                            c
                        }
                        Err(e) => {
                            attempts += 1;
                            on_event(PublisherEvent::Error(e.to_string()));
                            if let Some(max) = config.max_reconnect_attempts {
                                if attempts >= max {
                                    on_event(PublisherEvent::StateChanged(PipelineState::Error));
                                    break 'outer;
                                }
                            }
                            on_event(PublisherEvent::StateChanged(PipelineState::Reconnecting));
                            let sleep_step = Duration::from_millis(50);
                            let mut waited = 0u64;
                            while waited < backoff_ms && !stop_clone.load(Ordering::Acquire) {
                                thread::sleep(sleep_step);
                                waited += 50;
                            }
                            backoff_ms = (backoff_ms * 2).min(config.reconnect_max_ms.max(1000));
                            continue 'outer;
                        }
                    };

                    // 再接続時はキャッシュ済みのシーケンスヘッダ（SPS/PPS・AAC AudioSpecificConfig）を先頭に再送
                    if has_connected_before {
                        let (v_seq, a_seq) = {
                            let guard = q_clone.0.lock().unwrap();
                            (guard.video_seq_header.clone(), guard.audio_seq_header.clone())
                        };
                        if let Some(pkt) = v_seq {
                            if let Ok(n) = client.send_packet(&pkt) {
                                bytes_clone.fetch_add(n as u64, Ordering::Relaxed);
                                pkts_clone.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        if let Some(pkt) = a_seq {
                            if let Ok(n) = client.send_packet(&pkt) {
                                bytes_clone.fetch_add(n as u64, Ordering::Relaxed);
                                pkts_clone.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    has_connected_before = true;

                    // メディア送信ループ
                    loop {
                        if stop_clone.load(Ordering::Acquire) {
                            client.close();
                            break 'outer;
                        }

                        // 200ms 周期で AIMD ABR 評価
                        let elapsed = last_abr_tick.elapsed();
                        if elapsed >= Duration::from_millis(200) {
                            let q_bytes = { q_clone.0.lock().unwrap().queued_bytes };
                            let decision = abr.evaluate(q_bytes, elapsed.as_millis() as u64);
                            last_abr_tick = Instant::now();
                            drop_non_idr = decision.drop_non_idr;
                            br_clone.store(decision.current_bitrate_bps as u64, Ordering::Relaxed);
                            if decision.new_bitrate_bps.is_some()
                                || decision.drop_non_idr
                                || decision.request_keyframe
                            {
                                on_event(PublisherEvent::AbrUpdated(decision));
                            }
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
                                Some(pkt)
                            } else if guard.closed {
                                client.close();
                                break 'outer;
                            } else {
                                None
                            }
                        };

                        if let Some(pkt) = pkt_opt {
                            // 滞留 > 1000ms の緊急時は非IDR映像フレームを破棄し音声とリアルタイム性を保護
                            if drop_non_idr
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
                                    on_event(PublisherEvent::Error(format!(
                                        "RTMP send failed, reconnecting: {}",
                                        e
                                    )));
                                    break;
                                }
                            }
                        }

                        let _ = client.poll_inbound();
                    }
                }
            })
            .expect("spawn rtmp-publisher thread");

        Self {
            queue,
            stop_flag,
            bytes_sent,
            packets_sent,
            packets_dropped_abr,
            reconnect_count,
            current_bitrate_bps,
            max_queue_bytes,
            join_handle: Some(join_handle),
        }
    }

    /// エンコード済みパケットを送信キューへ積む。キュー上限超過時は古い非キーフレームを破棄または `BufferFull` を返す。
    pub fn enqueue(&self, packet: EncodedPacket) -> CoreResult<()> {
        let (lock, cvar) = &*self.queue;
        let mut guard = lock.lock().unwrap();
        if guard.closed {
            return Err(CoreError::Network("publisher is closed".into()));
        }

        if packet.is_sequence_header {
            match packet.codec {
                CodecKind::H264 => guard.video_seq_header = Some(packet.clone()),
                CodecKind::AacLc => guard.audio_seq_header = Some(packet.clone()),
                _ => {}
            }
        }

        if guard.queued_bytes + packet.data.len() > self.max_queue_bytes
            && !packet.is_sequence_header
            && packet.codec == CodecKind::H264
            && !packet.is_keyframe
        {
            self.packets_dropped_abr.fetch_add(1, Ordering::Relaxed);
            return Err(CoreError::BufferFull);
        }

        guard.queued_bytes += packet.data.len();
        guard.items.push_back(packet);
        cvar.notify_one();
        Ok(())
    }

    pub fn stats(&self) -> PublisherStats {
        let queued_bytes = self.queue.0.lock().unwrap().queued_bytes;
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
