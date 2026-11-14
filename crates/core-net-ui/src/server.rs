//! ゼロ依存 内蔵 HTTP/1.1 ＋ WebSocket サーバー（30Hz メーター配信・fMP4 ブロードキャスト対応）

use crate::fmp4::Fmp4Muxer;
use crate::http::{handle_http_request, read_http_request, write_websocket_upgrade};
use crate::rpc::{
    handle_rpc_message, make_meter_notification, make_preview_codec_notification, ControlBackend,
};
use crate::ws::{read_ws_message, write_server_frame, WsMessage, WsOpcode};
use core_types::media::EncodedPacket;
use core_types::{CoreError, CoreResult};
use std::io::{BufReader, ErrorKind};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind_addr: String,
    pub meter_interval_ms: u64,
    pub preview_width: u32,
    pub preview_height: u32,
    pub preview_fps: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:8080".to_string(),
            meter_interval_ms: 33, // 約 30Hz
            preview_width: 1280,
            preview_height: 720,
            preview_fps: 30,
        }
    }
}

#[derive(Debug, Clone)]
enum OutboundMsg {
    Text(Arc<str>),
    Binary(Arc<[u8]>),
    Pong(Vec<u8>),
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WsRole {
    Unified,
    ControlOnly,
    PreviewOnly,
}

struct ClientHandle {
    id: u64,
    role: WsRole,
    tx: SyncSender<OutboundMsg>,
}

struct PreviewState {
    muxer: Fmp4Muxer,
    init_segment: Option<Arc<[u8]>>,
    mime_type: Option<String>,
}

/// 内蔵 HTTP/1.1 & WebSocket サーバーハンドル
pub struct EmbeddedServer {
    local_addr: SocketAddr,
    stop_flag: Arc<AtomicBool>,
    clients: Arc<Mutex<Vec<ClientHandle>>>,
    preview_client_count: Arc<AtomicUsize>,
    preview: Arc<Mutex<PreviewState>>,
    config: ServerConfig,
    accept_handle: Option<JoinHandle<()>>,
    meter_handle: Option<JoinHandle<()>>,
}

impl EmbeddedServer {
    /// サーバーを起動し、バインド済みアドレスと制御ハンドルを返す
    pub fn start(config: ServerConfig, backend: Arc<dyn ControlBackend>) -> CoreResult<Self> {
        let listener = TcpListener::bind(&config.bind_addr)
            .map_err(|e| CoreError::Network(format!("bind {}: {}", config.bind_addr, e)))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| CoreError::Network(format!("local_addr: {}", e)))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| CoreError::Network(format!("set_nonblocking: {}", e)))?;

        let stop_flag = Arc::new(AtomicBool::new(false));
        let clients: Arc<Mutex<Vec<ClientHandle>>> = Arc::new(Mutex::new(Vec::new()));
        let preview_client_count = Arc::new(AtomicUsize::new(0));
        let preview = Arc::new(Mutex::new(PreviewState {
            muxer: Fmp4Muxer::new(),
            init_segment: None,
            mime_type: None,
        }));

        // 1. Accept ループスレッド
        let stop_acc = Arc::clone(&stop_flag);
        let clients_acc = Arc::clone(&clients);
        let preview_cnt_acc = Arc::clone(&preview_client_count);
        let preview_acc = Arc::clone(&preview);
        let backend_acc = Arc::clone(&backend);
        let accept_handle = thread::Builder::new()
            .name("ikakim-http-accept".into())
            .spawn(move || {
                let mut next_client_id = 1u64;
                while !stop_acc.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let cid = next_client_id;
                            next_client_id += 1;
                            let s_stop = Arc::clone(&stop_acc);
                            let s_clients = Arc::clone(&clients_acc);
                            let s_prev_cnt = Arc::clone(&preview_cnt_acc);
                            let s_preview = Arc::clone(&preview_acc);
                            let s_backend = Arc::clone(&backend_acc);
                            let _ = thread::Builder::new()
                                .name(format!("ikakim-conn-{}", cid))
                                .spawn(move || {
                                    handle_connection(
                                        cid,
                                        stream,
                                        s_stop,
                                        s_clients,
                                        s_prev_cnt,
                                        s_preview,
                                        s_backend,
                                    );
                                });
                        }
                        Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(15));
                        }
                        Err(_) => {
                            if stop_acc.load(Ordering::Acquire) {
                                break;
                            }
                            thread::sleep(Duration::from_millis(20));
                        }
                    }
                }
            })
            .map_err(|e| CoreError::Network(format!("spawn accept thread: {}", e)))?;

        // 2. 30Hz ピークメーター & ステータス通知スレッド
        let stop_meter = Arc::clone(&stop_flag);
        let clients_meter = Arc::clone(&clients);
        let backend_meter = Arc::clone(&backend);
        let interval_ms = config.meter_interval_ms.max(10);
        let meter_handle = thread::Builder::new()
            .name("ikakim-ws-meter-30hz".into())
            .spawn(move || {
                while !stop_meter.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(interval_ms));
                    if stop_meter.load(Ordering::Acquire) {
                        break;
                    }
                    let has_ctrl_clients = {
                        let guard = clients_meter.lock().unwrap();
                        guard
                            .iter()
                            .any(|c| matches!(c.role, WsRole::Unified | WsRole::ControlOnly))
                    };
                    if has_ctrl_clients {
                        let snap = backend_meter.snapshot();
                        let json: Arc<str> = Arc::from(make_meter_notification(&snap));
                        broadcast_msg(&clients_meter, OutboundMsg::Text(json), true, false);
                    }
                }
            })
            .map_err(|e| CoreError::Network(format!("spawn meter thread: {}", e)))?;

        Ok(Self {
            local_addr,
            stop_flag,
            clients,
            preview_client_count,
            preview,
            config,
            accept_handle: Some(accept_handle),
            meter_handle: Some(meter_handle),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn connected_ws_clients(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    /// プレビュー用 H.264 / AAC `EncodedPacket` を fMP4 へパッケージングし、接続中の WS クライアントへ配信する。
    /// プレビュークライアントが 0 人の時は `moof+mdat` メディアセグメントのメモリ構築をバイパスする。
    pub fn push_preview_packet(&self, packet: &EncodedPacket) -> CoreResult<()> {
        if !packet.is_sequence_header && self.preview_client_count.load(Ordering::Acquire) == 0 {
            let mut prev = self.preview.lock().unwrap();
            prev.muxer.note_skipped_media_packet(packet);
            return Ok(());
        }

        let seg_opt = {
            let mut prev = self.preview.lock().unwrap();
            let seg = prev.muxer.push_packet(
                packet,
                self.config.preview_width,
                self.config.preview_height,
                self.config.preview_fps,
            )?;
            let mut arc_bytes: Option<Arc<[u8]>> = None;
            if let Some(s) = &seg {
                let shared: Arc<[u8]> = Arc::from(s.bytes.as_slice());
                if s.is_init {
                    prev.init_segment = Some(Arc::clone(&shared));
                    prev.mime_type = prev.muxer.mime_type();
                }
                arc_bytes = Some(shared);
            }
            (seg.map(|s| (s.is_init, arc_bytes.unwrap())), prev.mime_type.clone())
        };

        if let (Some((is_init, seg_bytes)), mime_opt) = seg_opt {
            if is_init {
                if let Some(mime) = mime_opt {
                    let note: Arc<str> = Arc::from(make_preview_codec_notification(&mime));
                    broadcast_msg(&self.clients, OutboundMsg::Text(note), true, false);
                }
            }
            broadcast_msg(&self.clients, OutboundMsg::Binary(seg_bytes), false, true);
        }
        Ok(())
    }

    /// 任意のテキスト JSON-RPC 通知を全制御クライアントへ送信する
    pub fn broadcast_text(&self, text: String) {
        broadcast_msg(&self.clients, OutboundMsg::Text(Arc::from(text)), true, false);
    }

    /// サーバーを停止し、スレッドを終了する
    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Release);
        {
            let mut guard = self.clients.lock().unwrap();
            for c in guard.iter() {
                let _ = c.tx.try_send(OutboundMsg::Close);
            }
            guard.clear();
        }
        if let Some(h) = self.meter_handle.take() {
            let _ = h.join();
        }
        if let Some(h) = self.accept_handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for EmbeddedServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn broadcast_msg(
    clients: &Arc<Mutex<Vec<ClientHandle>>>,
    msg: OutboundMsg,
    for_control: bool,
    for_preview: bool,
) {
    let guard = clients.lock().unwrap();
    for c in guard.iter() {
        let wants = match c.role {
            WsRole::Unified => for_control || for_preview,
            WsRole::ControlOnly => for_control,
            WsRole::PreviewOnly => for_preview,
        };
        if wants {
            // スロークライアントが他を止めないよう non-blocking try_send
            let _ = c.tx.try_send(msg.clone());
        }
    }
}

fn handle_connection(
    client_id: u64,
    stream: TcpStream,
    stop_flag: Arc<AtomicBool>,
    clients: Arc<Mutex<Vec<ClientHandle>>>,
    preview_client_count: Arc<AtomicUsize>,
    preview: Arc<Mutex<PreviewState>>,
    backend: Arc<dyn ControlBackend>,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_nodelay(true);

    let mut write_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut buf_reader = BufReader::new(stream);

    let req = match read_http_request(&mut buf_reader) {
        Ok(r) => r,
        Err(_) => return,
    };

    if !req.is_websocket_upgrade() {
        let _ = handle_http_request(&mut write_stream, &req, &*backend);
        return;
    }

    let role = match req.path.as_str() {
        "/ws/control" => WsRole::ControlOnly,
        "/ws/preview" => WsRole::PreviewOnly,
        _ => WsRole::Unified,
    };

    let ws_key = match req.header("sec-websocket-key") {
        Some(k) => k,
        None => return,
    };

    let (tx, rx) = mpsc::sync_channel::<OutboundMsg>(128);
    let is_preview_client = matches!(role, WsRole::Unified | WsRole::PreviewOnly);

    // 101 Switching Protocols を返す前にクライアント登録と初期セグメント投入を完了させ、
    // ハンドシェイク直後の push_preview_packet との競合を防ぐ
    if is_preview_client {
        preview_client_count.fetch_add(1, Ordering::AcqRel);
        let (init_opt, mime_opt) = {
            let g = preview.lock().unwrap();
            (g.init_segment.clone(), g.mime_type.clone())
        };
        if matches!(role, WsRole::Unified) {
            if let Some(mime) = mime_opt {
                let _ = tx.try_send(OutboundMsg::Text(Arc::from(
                    make_preview_codec_notification(&mime),
                )));
            }
        }
        if let Some(init_bytes) = init_opt {
            let _ = tx.try_send(OutboundMsg::Binary(init_bytes));
        }
    }

    {
        let mut guard = clients.lock().unwrap();
        guard.push(ClientHandle {
            id: client_id,
            role,
            tx: tx.clone(),
        });
    }

    if write_websocket_upgrade(&mut write_stream, ws_key).is_err() {
        {
            let mut guard = clients.lock().unwrap();
            guard.retain(|c| c.id != client_id);
        }
        if is_preview_client {
            preview_client_count.fetch_sub(1, Ordering::AcqRel);
        }
        return;
    }

    // 送信専用スレッド
    let writer_handle = thread::Builder::new()
        .name(format!("ikakim-ws-tx-{}", client_id))
        .spawn(move || {
            for msg in rx {
                match msg {
                    OutboundMsg::Text(t) => {
                        if write_server_frame(&mut write_stream, WsOpcode::Text, t.as_bytes()).is_err() {
                            break;
                        }
                    }
                    OutboundMsg::Binary(b) => {
                        if write_server_frame(&mut write_stream, WsOpcode::Binary, &b).is_err() {
                            break;
                        }
                    }
                    OutboundMsg::Pong(p) => {
                        if write_server_frame(&mut write_stream, WsOpcode::Pong, &p).is_err() {
                            break;
                        }
                    }
                    OutboundMsg::Close => {
                        let _ = write_server_frame(&mut write_stream, WsOpcode::Close, &[0x03, 0xE8]);
                        break;
                    }
                }
            }
        })
        .ok();

    // 受信ループ（短タイムアウトで stop_flag を確認しながら読み取る）
    let _ = buf_reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(100)));

    while !stop_flag.load(Ordering::Acquire) {
        // Note: BufReader の未読バッファ分とソケット読み取りの両方を正しく扱う
        match read_ws_message(&mut buf_reader) {
            Ok(WsMessage::Text(txt)) => {
                let resp: Arc<str> = Arc::from(handle_rpc_message(&*backend, &txt));
                if tx.send(OutboundMsg::Text(resp)).is_err() {
                    break;
                }
            }
            Ok(WsMessage::Ping(p)) => {
                if tx.send(OutboundMsg::Pong(p)).is_err() {
                    break;
                }
            }
            Ok(WsMessage::Close(_, _)) => {
                let _ = tx.send(OutboundMsg::Close);
                break;
            }
            Ok(WsMessage::Binary(_)) | Ok(WsMessage::Pong(_)) => {}
            Err(CoreError::Network(msg))
                if msg.contains("TimedOut")
                    || msg.contains("WouldBlock")
                    || msg.contains("timed out")
                    || msg.contains("Resource temporarily unavailable") =>
            {
                continue;
            }
            Err(_) => break,
        }
    }

    {
        let mut guard = clients.lock().unwrap();
        guard.retain(|c| c.id != client_id);
    }
    if is_preview_client {
        preview_client_count.fetch_sub(1, Ordering::AcqRel);
    }
    let _ = tx.try_send(OutboundMsg::Close);
    drop(tx);
    if let Some(h) = writer_handle {
        let _ = h.join();
    }
}
