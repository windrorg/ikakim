use core_net_ui::fmp4::{inspect_box_types, Fmp4Muxer};
use core_net_ui::json::{parse_json, JsonValue};
use core_net_ui::rpc::{handle_rpc_message, ControlBackend, DefaultControlBackend};
use core_net_ui::sha1_b64::compute_websocket_accept;
use core_net_ui::ws::{
    read_ws_message, write_masked_frame, write_server_frame, WsMessage, WsOpcode,
};
use core_net_ui::{EmbeddedServer, ServerConfig};
use core_types::clock::PtsUs;
use core_types::media::{CodecKind, EncodedPacket};
use core_types::state::PipelineState;
use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

#[test]
fn test_rfc6455_websocket_accept_and_framing() {
    // RFC 6455 Section 1.3 公式テストベクタ
    let accept = compute_websocket_accept("dGhlIHNhbXBsZSBub25jZQ==");
    assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");

    // 小サイズ (<126), 中サイズ (126..65535), 大サイズ (>=65536) のマスク付き往復テスト
    for size in [12usize, 500, 70_000] {
        let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let mut wire = Vec::new();
        write_masked_frame(&mut wire, WsOpcode::Binary, &payload, [0x12, 0x34, 0x56, 0x78]).unwrap();

        let mut cursor = Cursor::new(wire);
        let msg = read_ws_message(&mut cursor).unwrap();
        assert_eq!(msg, WsMessage::Binary(payload));
    }

    // Server -> Client（マスクなし）
    let mut wire_srv = Vec::new();
    write_server_frame(&mut wire_srv, WsOpcode::Text, br#"{"ok":true}"#).unwrap();
    let msg_srv = read_ws_message(&mut Cursor::new(wire_srv)).unwrap();
    assert_eq!(msg_srv, WsMessage::Text(r#"{"ok":true}"#.to_string()));
}

#[test]
fn test_json_parser_serializer_and_rpc_dispatch() {
    let raw = r#"{"a":123,"b":-4.5,"c":true,"d":"hello\n\"world\"","e":[null,false,[1,2]]}"#;
    let parsed = parse_json(raw).unwrap();
    assert_eq!(parsed.get("a").unwrap().as_i64(), Some(123));
    assert_eq!(parsed.get("b").unwrap().as_f64(), Some(-4.5));
    assert_eq!(parsed.get("c").unwrap().as_bool(), Some(true));
    assert_eq!(parsed.get("d").unwrap().as_str(), Some("hello\n\"world\""));

    let reserialized = parsed.to_json_string();
    let reparsed = parse_json(&reserialized).unwrap();
    assert_eq!(parsed, reparsed);

    // JSON-RPC 制御検証
    let backend = DefaultControlBackend::new();
    let r1 = handle_rpc_message(
        &*backend,
        r#"{"jsonrpc":"2.0","id":1,"method":"stream.start","params":{"rtmp_url":"rtmps://example.com/live/k","bitrate_bps":6000000}}"#,
    );
    let j1 = parse_json(&r1).unwrap();
    assert_eq!(
        j1.get("result")
            .unwrap()
            .get("stream")
            .unwrap()
            .get("state")
            .unwrap()
            .as_str(),
        Some("Streaming")
    );

    let _ = handle_rpc_message(
        &*backend,
        r#"{"jsonrpc":"2.0","id":2,"method":"audio.set_delay","params":{"channel":1,"delay_ms":85.5}}"#,
    );
    let _ = handle_rpc_message(
        &*backend,
        r#"{"jsonrpc":"2.0","id":3,"method":"audio.set_gain","params":{"channel":1,"gain_db":-6.0}}"#,
    );
    let _ = handle_rpc_message(
        &*backend,
        r#"{"jsonrpc":"2.0","id":4,"method":"audio.set_mute","params":{"channel":1,"muted":true}}"#,
    );
    let _ = handle_rpc_message(
        &*backend,
        r#"{"jsonrpc":"2.0","id":5,"method":"overlay.set_transform","params":{"layer_id":1,"x":0.25,"y":-0.25,"scale_x":0.6,"opacity":0.8,"visible":false}}"#,
    );

    let snap = backend.snapshot();
    assert_eq!(snap.state, PipelineState::Streaming);
    assert_eq!(snap.bitrate_bps, 6_000_000);
    let ch1 = snap.channels.iter().find(|c| c.id == 1).unwrap();
    assert!((ch1.delay_ms - 85.5).abs() < 1e-4);
    assert!((ch1.gain_db - (-6.0)).abs() < 1e-4);
    assert!(ch1.muted);
    let ov1 = snap.overlays.iter().find(|o| o.layer_id == 1).unwrap();
    assert!((ov1.transform.x - 0.25).abs() < 1e-4);
    assert!(!ov1.transform.visible);

    let _ = JsonValue::Null;
}

#[test]
fn test_fmp4_muxer_init_and_media_fragments_mse() {
    let mut muxer = Fmp4Muxer::new();

    // H.264 Baseline Profile Level 3.1 SPS + PPS (Annex B)
    let h264_seq = EncodedPacket {
        codec: CodecKind::H264,
        pts: PtsUs(0),
        dts: PtsUs(0),
        is_keyframe: true,
        is_sequence_header: true,
        data: vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xE0, 0x1F, 0xDA, 0x05, 0x07, 0xE8, 0x00, 0x00,
            0x00, 0x01, 0x68, 0xCE, 0x3C, 0x80,
        ],
    };

    let init_seg = muxer
        .push_packet(&h264_seq, 1280, 720, 30)
        .unwrap()
        .expect("init segment expected");
    assert!(init_seg.is_init);
    assert_eq!(
        inspect_box_types(&init_seg.bytes),
        vec![*b"ftyp", *b"moov"]
    );
    assert_eq!(
        muxer.mime_type().as_deref(),
        Some("video/mp4; codecs=\"avc1.42E01F\"")
    );

    // 音声トラック追加 -> MIME 更新 & Init 再生成
    let aac_seq = EncodedPacket {
        codec: CodecKind::AacLc,
        pts: PtsUs(0),
        dts: PtsUs(0),
        is_keyframe: true,
        is_sequence_header: true,
        data: vec![0x11, 0x90],
    };
    let init_av = muxer
        .push_packet(&aac_seq, 1280, 720, 30)
        .unwrap()
        .expect("updated init segment expected");
    assert!(init_av.is_init);
    assert_eq!(
        muxer.mime_type().as_deref(),
        Some("video/mp4; codecs=\"avc1.42E01F, mp4a.40.2\"")
    );
    assert!(init_av.bytes.windows(4).any(|w| w == b"avcC"));
    assert!(init_av.bytes.windows(4).any(|w| w == b"mp4a"));
    assert!(init_av.bytes.windows(4).any(|w| w == b"mvex"));

    // IDR 前の非キーフレームは待機中スキップされること
    let pre_p = EncodedPacket {
        codec: CodecKind::H264,
        pts: PtsUs(16_667),
        dts: PtsUs(16_667),
        is_keyframe: false,
        is_sequence_header: false,
        data: vec![0x00, 0x00, 0x00, 0x01, 0x41, 0x9A, 0x01, 0x02],
    };
    assert!(muxer.push_packet(&pre_p, 1280, 720, 30).unwrap().is_none());

    // AAC フレーム + IDR キーフレーム投入 -> moof + mdat 出力
    let aac_pkt = EncodedPacket {
        codec: CodecKind::AacLc,
        pts: PtsUs(33_333),
        dts: PtsUs(33_333),
        is_keyframe: true,
        is_sequence_header: false,
        data: vec![0xDE, 0xAD, 0xBE, 0xEF],
    };
    assert!(muxer.push_packet(&aac_pkt, 1280, 720, 30).unwrap().is_none());

    let idr = EncodedPacket {
        codec: CodecKind::H264,
        pts: PtsUs(33_333),
        dts: PtsUs(33_333),
        is_keyframe: true,
        is_sequence_header: false,
        data: vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x21, 0xA0],
    };
    let frag = muxer
        .push_packet(&idr, 1280, 720, 30)
        .unwrap()
        .expect("media fragment expected");
    assert!(!frag.is_init);
    assert!(frag.contains_keyframe);
    assert_eq!(frag.sequence_number, 1);
    assert_eq!(inspect_box_types(&frag.bytes), vec![*b"moof", *b"mdat"]);

    // mdat の先頭 4-byte length prefix が IDR NAL 長 (5 bytes) と一致すること
    let moof_len = u32::from_be_bytes(frag.bytes[0..4].try_into().unwrap()) as usize;
    let mdat_payload = &frag.bytes[moof_len + 8..];
    assert_eq!(u32::from_be_bytes(mdat_payload[0..4].try_into().unwrap()), 5);
    assert_eq!(&mdat_payload[4..9], &[0x65, 0x88, 0x84, 0x21, 0xA0]);
    assert_eq!(&mdat_payload[9..13], &[0xDE, 0xAD, 0xBE, 0xEF]);
}

#[test]
fn test_embedded_server_http_and_ws_jsonrpc_fmp4_30hz_meters() {
    let backend = DefaultControlBackend::new();
    backend.update_meters(-12.5, -14.0, &[(0, -18.0, -19.0)]);

    let cfg = ServerConfig {
        bind_addr: "127.0.0.1:0".to_string(),
        meter_interval_ms: 25,
        preview_width: 1280,
        preview_height: 720,
        preview_fps: 30,
    };

    let mut server = EmbeddedServer::start(cfg, backend.clone()).unwrap();
    let addr = server.local_addr();

    // 1. HTTP/1.1 静的アセット取得確認 (/, /app.js)
    let html_resp = http_get(addr, "/");
    assert!(html_resp.starts_with("HTTP/1.1 200 OK"), "resp={}", &html_resp[..html_resp.len().min(60)]);
    assert!(html_resp.contains("MSE Low-Latency Preview"));

    let js_resp = http_get(addr, "/app.js");
    assert!(js_resp.starts_with("HTTP/1.1 200 OK"));
    assert!(js_resp.contains("MAX_LATENCY_SEC = 0.30"));

    let api_resp = http_get(addr, "/api/status");
    assert!(api_resp.contains("Camera Mic"));

    // 2. WebSocket (RFC 6455) ハンドシェイク & JSON-RPC & 30Hz メーター & fMP4 バイナリ受信確認
    let mut tcp = TcpStream::connect(addr).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let ws_req = format!(
        "GET /ws HTTP/1.1\r\n\
         Host: {}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n",
        addr
    );
    tcp.write_all(ws_req.as_bytes()).unwrap();

    let mut reader = BufReader::new(tcp.try_clone().unwrap());
    let mut hdr_lines = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        hdr_lines.push_str(&line);
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }
    assert!(hdr_lines.contains("101 Switching Protocols"));
    assert!(hdr_lines.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));

    // クライアント -> JSON-RPC stream.start リクエスト送信
    let rpc_req = br#"{"jsonrpc":"2.0","id":42,"method":"stream.start","params":{"rtmp_url":"rtmps://live.example/app/key"}}"#;
    write_masked_frame(&mut tcp, WsOpcode::Text, rpc_req, [1, 2, 3, 4]).unwrap();

    // fMP4 H.264 Init + IDR パケットをプッシュ
    server
        .push_preview_packet(&EncodedPacket {
            codec: CodecKind::H264,
            pts: PtsUs(0),
            dts: PtsUs(0),
            is_keyframe: true,
            is_sequence_header: true,
            data: vec![
                0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xE0, 0x1F, 0xDA, 0x00, 0x00, 0x00, 0x01, 0x68,
                0xCE, 0x3C, 0x80,
            ],
        })
        .unwrap();
    server
        .push_preview_packet(&EncodedPacket {
            codec: CodecKind::H264,
            pts: PtsUs(33_333),
            dts: PtsUs(33_333),
            is_keyframe: true,
            is_sequence_header: false,
            data: vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x21],
        })
        .unwrap();

    let mut got_rpc_reply = false;
    let mut got_meter_update = false;
    let mut got_fmp4_init = false;
    let mut got_fmp4_moof = false;

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline
        && !(got_rpc_reply && got_meter_update && got_fmp4_init && got_fmp4_moof)
    {
        match read_ws_message(&mut reader).unwrap() {
            WsMessage::Text(txt) => {
                let v = parse_json(&txt).unwrap();
                if v.get("id").and_then(|x| x.as_i64()) == Some(42) {
                    assert_eq!(
                        v.get("result")
                            .unwrap()
                            .get("stream")
                            .unwrap()
                            .get("state")
                            .unwrap()
                            .as_str(),
                        Some("Streaming")
                    );
                    got_rpc_reply = true;
                } else if v.get("method").and_then(|x| x.as_str()) == Some("meter.update") {
                    got_meter_update = true;
                }
            }
            WsMessage::Binary(bin) => {
                let boxes = inspect_box_types(&bin);
                if boxes == vec![*b"ftyp", *b"moov"] {
                    got_fmp4_init = true;
                } else if boxes == vec![*b"moof", *b"mdat"] {
                    got_fmp4_moof = true;
                }
            }
            _ => {}
        }
    }

    assert!(got_rpc_reply, "missing JSON-RPC reply");
    assert!(got_meter_update, "missing 30Hz meter.update notification");
    assert!(got_fmp4_init, "missing fMP4 init segment");
    assert!(got_fmp4_moof, "missing fMP4 moof+mdat fragment");

    let _ = write_masked_frame(&mut tcp, WsOpcode::Close, &[0x03, 0xE8], [9, 8, 7, 6]);
    drop(tcp);
    drop(reader);
    server.stop();
}

fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let req = format!("GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", path, addr);
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).unwrap();
    buf
}

#[derive(Clone, Copy)]
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn range_usize(&mut self, min: usize, max_inclusive: usize) -> usize {
        if min >= max_inclusive {
            return min;
        }
        let span = (max_inclusive - min) + 1;
        min + (self.next_u64() as usize % span)
    }

    fn gen_json_string(&mut self) -> String {
        const POOL: &[char] = &[
            'a', 'Z', '0', '9', '_', '-', ' ', '"', '\\', '/', '\n', '\r', '\t', 'あ', '配', '信',
        ];
        let len = self.range_usize(0, 24);
        let mut out = String::with_capacity(len);
        for _ in 0..len {
            out.push(POOL[self.range_usize(0, POOL.len() - 1)]);
        }
        out
    }

    fn gen_json(&mut self, depth: usize) -> core_net_ui::json::JsonValue {
        use core_net_ui::json::JsonValue;
        let kind = if depth >= 2 {
            self.range_usize(0, 3)
        } else {
            self.range_usize(0, 5)
        };
        match kind {
            0 => JsonValue::Null,
            1 => JsonValue::Bool((self.next_u64() & 1) == 1),
            2 => JsonValue::Number((self.next_u64() % 1_000_000) as f64 - 500_000.0),
            3 => JsonValue::String(self.gen_json_string()),
            4 => {
                let n = self.range_usize(0, 5);
                JsonValue::Array((0..n).map(|_| self.gen_json(depth + 1)).collect())
            }
            _ => {
                let n = self.range_usize(0, 5);
                let mut pairs = Vec::with_capacity(n);
                for k in 0..n {
                    pairs.push((format!("k{}_{}", k, self.range_usize(0, 99)), self.gen_json(depth + 1)));
                }
                JsonValue::Object(pairs)
            }
        }
    }
}

#[test]
fn test_property_json_roundtrip_and_escaping() {
    use core_net_ui::json::parse_json;

    let mut rng = SplitMix64(0x750A_BEEF_1234_5678);
    for _ in 0..250 {
        let val = rng.gen_json(0);
        let text = val.to_json_string();
        let parsed = parse_json(&text).expect("generated JSON must parse cleanly");
        assert_eq!(parsed, val);
    }
}

#[test]
fn test_property_ws_framing_and_fmp4_box_integrity() {
    use core_net_ui::ws::write_server_frame;

    let mut rng = SplitMix64(0xF394_8765_4321_ABCD);

    // 1. WebSocket フレーミング: 7bit (<126B), 16bit (126..=65535B), 64bit (>65535B) 全領域往復不変条件
    let test_lengths = [0usize, 1, 63, 125, 126, 127, 512, 4096, 65_535, 65_536, 70_000];
    for &len in &test_lengths {
        let payload: Vec<u8> = (0..len).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
        let mask = [
            (rng.next_u64() & 0xFF) as u8,
            ((rng.next_u64() >> 8) & 0xFF) as u8,
            ((rng.next_u64() >> 16) & 0xFF) as u8,
            ((rng.next_u64() >> 24) & 0xFF) as u8,
        ];

        let mut wire = Vec::with_capacity(len + 16);
        write_masked_frame(&mut wire, WsOpcode::Binary, &payload, mask).unwrap();
        let mut cursor = std::io::Cursor::new(wire);
        let msg = read_ws_message(&mut cursor).unwrap();
        assert_eq!(msg, WsMessage::Binary(payload.clone()));

        let mut srv_wire = Vec::with_capacity(len + 16);
        write_server_frame(&mut srv_wire, WsOpcode::Binary, &payload).unwrap();
        let mut srv_cursor = std::io::Cursor::new(srv_wire);
        let srv_msg = read_ws_message(&mut srv_cursor).unwrap();
        assert_eq!(srv_msg, WsMessage::Binary(payload));
    }

    // 2. fMP4 Muxer: 任意のサイズ・枚数の H.264 + AAC パケット列に対する ISO-BMFF ボックス長整合性
    let mut muxer = Fmp4Muxer::new();
    let seq_hdr = EncodedPacket {
        codec: CodecKind::H264,
        pts: PtsUs(0),
        dts: PtsUs(0),
        is_keyframe: true,
        is_sequence_header: true,
        data: vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x64, 0x00, 0x1F, 0xAC, 0xD9, 0x40, 0x50, 0x05, 0xBB,
            0x01, 0x10, 0x00, 0x00, 0x00, 0x01, 0x68, 0xEE, 0x3C, 0x80,
        ],
    };
    let init = muxer.push_packet(&seq_hdr, 1280, 720, 60).unwrap().unwrap();
    assert!(init.is_init);
    assert_eq!(inspect_box_types(&init.bytes), vec![*b"ftyp", *b"moov"]);

    let aac_seq = EncodedPacket {
        codec: CodecKind::AacLc,
        pts: PtsUs(0),
        dts: PtsUs(0),
        is_keyframe: true,
        is_sequence_header: true,
        data: vec![0x11, 0x90],
    };
    let init2 = muxer.push_packet(&aac_seq, 1280, 720, 60).unwrap().unwrap();
    assert!(init2.is_init);

    let mut pts_us = 0u64;
    for f in 0..50 {
        let aac_count = rng.range_usize(0, 3);
        for _ in 0..aac_count {
            let a_len = rng.range_usize(16, 256);
            let a_pkt = EncodedPacket {
                codec: CodecKind::AacLc,
                pts: PtsUs(pts_us),
                dts: PtsUs(pts_us),
                is_keyframe: true,
                is_sequence_header: false,
                data: vec![0xAA; a_len],
            };
            assert!(muxer.push_packet(&a_pkt, 1280, 720, 60).unwrap().is_none());
        }

        let v_len = rng.range_usize(32, 4096);
        let is_key = f == 0 || (f % 15 == 0);
        let mut v_data = vec![0x00, 0x00, 0x00, 0x01, if is_key { 0x65 } else { 0x41 }];
        v_data.extend(std::iter::repeat(0x5A).take(v_len));
        let v_pkt = EncodedPacket {
            codec: CodecKind::H264,
            pts: PtsUs(pts_us),
            dts: PtsUs(pts_us),
            is_keyframe: is_key,
            is_sequence_header: false,
            data: v_data,
        };
        let seg = muxer.push_packet(&v_pkt, 1280, 720, 60).unwrap().unwrap();
        assert!(!seg.is_init);
        assert_eq!(seg.contains_keyframe, is_key);
        assert_eq!(seg.sequence_number, (f + 1) as u32);
        assert_eq!(inspect_box_types(&seg.bytes), vec![*b"moof", *b"mdat"]);
        pts_us += 16_667;
    }
}
