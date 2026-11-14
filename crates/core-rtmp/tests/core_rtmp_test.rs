use core_rtmp::abr::{AbrConfig, AimdAbrController};
use core_rtmp::amf0::{decode_all, encode_values, Amf0Value};
use core_rtmp::chunk::{
    ChunkReader, ChunkWriter, RtmpMessage, CSID_AUDIO, CSID_COMMAND, CSID_PROTOCOL_CONTROL,
    CSID_VIDEO, MSG_AUDIO, MSG_COMMAND_AMF0, MSG_DATA_AMF0, MSG_SET_CHUNK_SIZE, MSG_VIDEO,
    MSG_WINDOW_ACK_SIZE,
};
use core_rtmp::client::StreamMetadata;
use core_rtmp::flv::{pack_audio_tag, pack_video_tag};
use core_rtmp::handshake::perform_server_handshake;
use core_rtmp::publisher::{PublisherConfig, PublisherEvent, RtmpPublisher};
use core_rtmp::transport::{RtmpUrl, TlsRootConfig};
use core_types::clock::PtsUs;
use core_types::media::{CodecKind, EncodedPacket};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use std::io::Cursor;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const TEST_CA_CERT_PEM: &[u8] = br#"-----BEGIN CERTIFICATE-----
MIIBdjCCARygAwIBAgIUJG1eqjjuBn5pylRC67p+T2jJ0cgwCgYIKoZIzj0EAwIw
GTEXMBUGA1UEAwwOaWtha2ltLXRlc3QtY2EwHhcNMjYxMTEzMDg0MzQ2WhcNMzYx
MTEwMDg0MzQ2WjAZMRcwFQYDVQQDDA5pa2FraW0tdGVzdC1jYTBZMBMGByqGSM49
AgEGCCqGSM49AwEHA0IABDjr3WuizdJ+K5ikNRYpf4Jzte0rypLBK9gTtYzVzO3W
RJdgD5y01zt+idgxu733rnb7iaWV83pVs7ESAQ9UpP2jQjBAMA8GA1UdEwEB/wQF
MAMBAf8wDgYDVR0PAQH/BAQDAgEGMB0GA1UdDgQWBBTbaxREt4FYvOx3fX3Mh3GQ
V7jYOTAKBggqhkjOPQQDAgNIADBFAiEAqBNizT58WIrDyFNX1lCqBSLA6xKBovT9
SdVOz1Cyr4gCICDeBGkU3FtYO6/WChQqu3k/a/qQ9nOt0KU4sMCDtzz/
-----END CERTIFICATE-----
"#;

const TEST_LEAF_CERT_PEM: &[u8] = br#"-----BEGIN CERTIFICATE-----
MIIBvzCCAWWgAwIBAgIUOVH9KTZOg+naDb6dtG5jIglivZYwCgYIKoZIzj0EAwIw
GTEXMBUGA1UEAwwOaWtha2ltLXRlc3QtY2EwHhcNMjYxMTEzMDg0MzQ2WhcNMzYx
MTEwMDg0MzQ2WjAUMRIwEAYDVQQDDAlsb2NhbGhvc3QwWTATBgcqhkjOPQIBBggq
hkjOPQMBBwNCAATm77zS9WnklMvrsVuWUSus90bWI4n63evIBymNvzZc2ocipdRt
gCJGlTyKiN2SNeADOFcYJiT921cc+NbwXD+so4GPMIGMMAkGA1UdEwQCMAAwDgYD
VR0PAQH/BAQDAgWgMBMGA1UdJQQMMAoGCCsGAQUFBwMBMBoGA1UdEQQTMBGCCWxv
Y2FsaG9zdIcEfwAAATAdBgNVHQ4EFgQUjCD08A47feNG4hiscaLIvpr7598wHwYD
VR0jBBgwFoAU22sURLeBWLzsd319zIdxkFe42DkwCgYIKoZIzj0EAwIDSAAwRQIg
XgESjwBm9Z9BqSTuwiX1G9cktwZr8IMtnOgm/Ka642UCIQDNOaGBnYqFrAlnZ+j1
bOHXlUQnvNr0B5MjFVKZGDKzCg==
-----END CERTIFICATE-----
"#;

const TEST_LEAF_KEY_PEM: &[u8] = br#"-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgDB12jMzsJjtv0tue
81FOGE9ycRjcUKwylu6PHYY6huWhRANCAATm77zS9WnklMvrsVuWUSus90bWI4n6
3evIBymNvzZc2ocipdRtgCJGlTyKiN2SNeADOFcYJiT921cc+NbwXD+s
-----END PRIVATE KEY-----
"#;

#[test]
fn test_rtmp_url_parsing() {
    let u1 = RtmpUrl::parse("rtmps://a.rtmps.youtube.com/live2/xxxx-yyyy-zzzz").unwrap();
    assert!(u1.use_tls);
    assert_eq!(u1.host, "a.rtmps.youtube.com");
    assert_eq!(u1.port, 443);
    assert_eq!(u1.app, "live2");
    assert_eq!(u1.stream_key, "xxxx-yyyy-zzzz");
    assert_eq!(u1.tc_url, "rtmps://a.rtmps.youtube.com/live2");

    let u2 = RtmpUrl::parse("rtmp://127.0.0.1:19350/app/stream1?token=abc").unwrap();
    assert!(!u2.use_tls);
    assert_eq!(u2.port, 19350);
    assert_eq!(u2.stream_key, "stream1?token=abc");
    assert_eq!(u2.tc_url, "rtmp://127.0.0.1:19350/app");

    assert!(RtmpUrl::parse("http://example.com/live/key").is_err());
    assert!(RtmpUrl::parse("rtmps://example.com/live").is_err());
}

#[test]
fn test_amf0_roundtrip() {
    let values = vec![
        Amf0Value::String("connect".to_string()),
        Amf0Value::Number(1.0),
        Amf0Value::Object(vec![
            ("app".to_string(), Amf0Value::String("live".to_string())),
            ("fpad".to_string(), Amf0Value::Boolean(false)),
        ]),
        Amf0Value::Null,
        Amf0Value::EcmaArray(vec![("width".to_string(), Amf0Value::Number(1920.0))]),
        Amf0Value::StrictArray(vec![Amf0Value::Number(42.0), Amf0Value::Boolean(true)]),
    ];
    let encoded = encode_values(&values);
    let decoded = decode_all(&encoded).unwrap();
    assert_eq!(values, decoded);
}

#[test]
fn test_chunk_multiplexing_and_extended_timestamps() {
    let mut writer = ChunkWriter::new();
    let mut wire = Vec::new();

    // Chunk size を 4096 に変更
    writer
        .write_message(
            &mut wire,
            &RtmpMessage {
                csid: CSID_PROTOCOL_CONTROL,
                timestamp_ms: 0,
                msg_type: MSG_SET_CHUNK_SIZE,
                stream_id: 0,
                payload: 4096u32.to_be_bytes().to_vec(),
            },
        )
        .unwrap();
    writer.set_chunk_size(4096);

    let big_video = vec![0xABu8; 10_000];
    let audio_pkt = vec![0xCDu8; 256];

    writer
        .write_message(
            &mut wire,
            &RtmpMessage {
                csid: CSID_VIDEO,
                timestamp_ms: 0x0102_0304, // > 0xFFFFFF (Extended Timestamp)
                msg_type: MSG_VIDEO,
                stream_id: 1,
                payload: big_video.clone(),
            },
        )
        .unwrap();

    writer
        .write_message(
            &mut wire,
            &RtmpMessage {
                csid: CSID_AUDIO,
                timestamp_ms: 0x0102_0318,
                msg_type: MSG_AUDIO,
                stream_id: 1,
                payload: audio_pkt.clone(),
            },
        )
        .unwrap();

    writer
        .write_message(
            &mut wire,
            &RtmpMessage {
                csid: CSID_VIDEO,
                timestamp_ms: 0x0102_0325,
                msg_type: MSG_VIDEO,
                stream_id: 1,
                payload: vec![0xEF; 5000],
            },
        )
        .unwrap();

    let mut cursor = Cursor::new(wire);
    let mut reader = ChunkReader::new();
    let m0 = reader.read_message(&mut cursor).unwrap();
    assert_eq!(m0.msg_type, MSG_SET_CHUNK_SIZE);

    let m1 = reader.read_message(&mut cursor).unwrap();
    assert_eq!(m1.timestamp_ms, 0x0102_0304);
    assert_eq!(m1.payload, big_video);

    let m2 = reader.read_message(&mut cursor).unwrap();
    assert_eq!(m2.timestamp_ms, 0x0102_0318);
    assert_eq!(m2.payload, audio_pkt);

    let m3 = reader.read_message(&mut cursor).unwrap();
    assert_eq!(m3.timestamp_ms, 0x0102_0325);
    assert_eq!(m3.payload, vec![0xEF; 5000]);
}

#[test]
fn test_flv_h264_annexb_and_aac_packing() {
    let sps_pps = vec![
        0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xE0, 0x1F, 0xDA, // SPS
        0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x3C, 0x80, // PPS
    ];
    let v_seq = pack_video_tag(&EncodedPacket {
        codec: CodecKind::H264,
        pts: PtsUs(0),
        dts: PtsUs(0),
        is_keyframe: true,
        is_sequence_header: true,
        data: sps_pps,
    })
    .unwrap();
    assert_eq!(&v_seq[..5], &[0x17, 0x00, 0x00, 0x00, 0x00]);
    assert_eq!(v_seq[5], 0x01); // AVCDecoderConfigurationRecord version
    assert_eq!(v_seq[6], 0x42); // Profile

    let idr = vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x21];
    let v_idr = pack_video_tag(&EncodedPacket {
        codec: CodecKind::H264,
        pts: PtsUs(33_000),
        dts: PtsUs(0),
        is_keyframe: true,
        is_sequence_header: false,
        data: idr,
    })
    .unwrap();
    assert_eq!(&v_idr[..5], &[0x17, 0x01, 0x00, 0x00, 33]);
    assert_eq!(&v_idr[5..9], &4u32.to_be_bytes());
    assert_eq!(&v_idr[9..13], &[0x65, 0x88, 0x84, 0x21]);

    let a_seq = pack_audio_tag(&EncodedPacket {
        codec: CodecKind::AacLc,
        pts: PtsUs(0),
        dts: PtsUs(0),
        is_keyframe: true,
        is_sequence_header: true,
        data: vec![0x11, 0x90],
    })
    .unwrap();
    assert_eq!(a_seq, vec![0xAF, 0x00, 0x11, 0x90]);
}

#[test]
fn test_aimd_abr_controller_cycle() {
    let mut abr = AimdAbrController::new(AbrConfig::default());
    assert_eq!(abr.current_bitrate_bps(), 4_000_000);

    // 4 Mbps 時: 250_000 bytes = 2_000_000 bits -> 500ms 滞留 (> 400ms) -> 0.75x = 3_000_000 bps
    let d1 = abr.evaluate(250_000, 200);
    assert_eq!(d1.dwell_ms, 500);
    assert_eq!(d1.new_bitrate_bps, Some(3_000_000));
    assert!(!d1.drop_non_idr);

    // クールダウン中 (200ms後) は再減衰しないが、滞留 1200ms 超 (> 1000ms) で緊急フレームスキップ発動
    // 3 Mbps 時: 450_000 bytes = 3_600_000 bits -> 1200ms
    let d2 = abr.evaluate(450_000, 200);
    assert_eq!(d2.dwell_ms, 1200);
    assert!(d2.drop_non_idr);
    assert_eq!(d2.new_bitrate_bps, None);

    // 滞留解消 (< 100ms): 緊急スキップ解除 + キーフレーム要求
    let d3 = abr.evaluate(10_000, 200);
    assert!(!d3.drop_non_idr);
    assert!(d3.request_keyframe);

    // < 100ms を 5秒 (あと 4800ms) 継続すると +250_000 bps 回復
    for _ in 0..23 {
        let d = abr.evaluate(10_000, 200);
        assert_eq!(d.new_bitrate_bps, None);
    }
    let d_recover = abr.evaluate(10_000, 200);
    assert_eq!(d_recover.new_bitrate_bps, Some(3_250_000));
}

#[derive(Default)]
struct ServerCaptured {
    app: String,
    stream_key: String,
    metadata_received: bool,
    video_tags: Vec<Vec<u8>>,
    audio_tags: Vec<Vec<u8>>,
}

#[test]
fn test_full_rtmps_tls_end_to_end_with_local_server() {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(TEST_LEAF_CERT_PEM)
        .map(|r| r.unwrap())
        .collect();
    let key = PrivateKeyDer::from_pem_slice(TEST_LEAF_KEY_PEM).unwrap();

    let server_cfg = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap(),
    );

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured = Arc::new(Mutex::new(ServerCaptured::default()));
    let cap_srv = Arc::clone(&captured);

    let srv_handle = thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        let _ = tcp.set_read_timeout(Some(Duration::from_secs(5)));
        let conn = ServerConnection::new(server_cfg).unwrap();
        let mut tls = StreamOwned::new(conn, tcp);
        while tls.conn.is_handshaking() {
            tls.conn.complete_io(&mut tls.sock).unwrap();
        }

        perform_server_handshake(&mut tls).unwrap();

        let mut reader = ChunkReader::new();
        let mut writer = ChunkWriter::new();

        // SetChunkSize + WindowAckSize をサーバーから送る
        writer
            .write_message(
                &mut tls,
                &RtmpMessage {
                    csid: CSID_PROTOCOL_CONTROL,
                    timestamp_ms: 0,
                    msg_type: MSG_WINDOW_ACK_SIZE,
                    stream_id: 0,
                    payload: 2_500_000u32.to_be_bytes().to_vec(),
                },
            )
            .unwrap();
        writer
            .write_message(
                &mut tls,
                &RtmpMessage {
                    csid: CSID_PROTOCOL_CONTROL,
                    timestamp_ms: 0,
                    msg_type: MSG_SET_CHUNK_SIZE,
                    stream_id: 0,
                    payload: 4096u32.to_be_bytes().to_vec(),
                },
            )
            .unwrap();
        writer.set_chunk_size(4096);

        while let Ok(msg) = reader.read_message(&mut tls) {
            match msg.msg_type {
                MSG_COMMAND_AMF0 => {
                    let vals = decode_all(&msg.payload).unwrap();
                    let cmd = vals.first().and_then(|v| v.as_str()).unwrap_or("");
                    let txn = vals.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
                    match cmd {
                        "connect" => {
                            if let Some(app) = vals
                                .get(2)
                                .and_then(|o| o.get("app"))
                                .and_then(|v| v.as_str())
                            {
                                cap_srv.lock().unwrap().app = app.to_string();
                            }
                            let resp = core_rtmp::amf0::encode_values(&[
                                Amf0Value::String("_result".into()),
                                Amf0Value::Number(txn),
                                Amf0Value::Object(vec![("fmsVer".into(), Amf0Value::String("FMS/3,0".into()))]),
                                Amf0Value::Object(vec![
                                    ("level".into(), Amf0Value::String("status".into())),
                                    ("code".into(), Amf0Value::String("NetConnection.Connect.Success".into())),
                                ]),
                            ]);
                            writer
                                .write_message(
                                    &mut tls,
                                    &RtmpMessage {
                                        csid: CSID_COMMAND,
                                        timestamp_ms: 0,
                                        msg_type: MSG_COMMAND_AMF0,
                                        stream_id: 0,
                                        payload: resp,
                                    },
                                )
                                .unwrap();
                        }
                        "createStream" => {
                            let resp = core_rtmp::amf0::encode_values(&[
                                Amf0Value::String("_result".into()),
                                Amf0Value::Number(txn),
                                Amf0Value::Null,
                                Amf0Value::Number(1.0),
                            ]);
                            writer
                                .write_message(
                                    &mut tls,
                                    &RtmpMessage {
                                        csid: CSID_COMMAND,
                                        timestamp_ms: 0,
                                        msg_type: MSG_COMMAND_AMF0,
                                        stream_id: 0,
                                        payload: resp,
                                    },
                                )
                                .unwrap();
                        }
                        "publish" => {
                            if let Some(key) = vals.get(3).and_then(|v| v.as_str()) {
                                cap_srv.lock().unwrap().stream_key = key.to_string();
                            }
                            let resp = core_rtmp::amf0::encode_values(&[
                                Amf0Value::String("onStatus".into()),
                                Amf0Value::Number(0.0),
                                Amf0Value::Null,
                                Amf0Value::Object(vec![
                                    ("level".into(), Amf0Value::String("status".into())),
                                    ("code".into(), Amf0Value::String("NetStream.Publish.Start".into())),
                                ]),
                            ]);
                            writer
                                .write_message(
                                    &mut tls,
                                    &RtmpMessage {
                                        csid: CSID_COMMAND,
                                        timestamp_ms: 0,
                                        msg_type: MSG_COMMAND_AMF0,
                                        stream_id: 1,
                                        payload: resp,
                                    },
                                )
                                .unwrap();
                        }
                        "deleteStream" => break,
                        _ => {}
                    }
                }
                MSG_DATA_AMF0 => {
                    cap_srv.lock().unwrap().metadata_received = true;
                }
                MSG_VIDEO => {
                    cap_srv.lock().unwrap().video_tags.push(msg.payload);
                }
                MSG_AUDIO => {
                    cap_srv.lock().unwrap().audio_tags.push(msg.payload);
                }
                _ => {}
            }
        }
    });

    let url = RtmpUrl::parse(&format!("rtmps://127.0.0.1:{}/live/secret-stream", port)).unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let ev_cb = Arc::clone(&events);

    let mut publisher = RtmpPublisher::spawn(
        PublisherConfig {
            url,
            metadata: StreamMetadata::default(),
            tls: TlsRootConfig::CustomPem(TEST_CA_CERT_PEM.to_vec()),
            abr: AbrConfig::default(),
            max_queue_bytes: 2 * 1024 * 1024,
            reconnect_initial_ms: 200,
            reconnect_max_ms: 1000,
            max_reconnect_attempts: Some(3),
        },
        move |ev| {
            ev_cb.lock().unwrap().push(ev);
        },
    );

    // Sequence headers + media packets
    publisher
        .enqueue(EncodedPacket {
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
    publisher
        .enqueue(EncodedPacket {
            codec: CodecKind::AacLc,
            pts: PtsUs(0),
            dts: PtsUs(0),
            is_keyframe: true,
            is_sequence_header: true,
            data: vec![0x11, 0x90],
        })
        .unwrap();
    publisher
        .enqueue(EncodedPacket {
            codec: CodecKind::H264,
            pts: PtsUs(33_333),
            dts: PtsUs(33_333),
            is_keyframe: true,
            is_sequence_header: false,
            data: vec![0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x21],
        })
        .unwrap();
    publisher
        .enqueue(EncodedPacket {
            codec: CodecKind::AacLc,
            pts: PtsUs(21_333),
            dts: PtsUs(21_333),
            is_keyframe: true,
            is_sequence_header: false,
            data: vec![0xDE, 0xAD, 0xBE, 0xEF],
        })
        .unwrap();

    // 送信完了を少し待つ
    for _ in 0..50 {
        if publisher.stats().packets_sent >= 4 {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    let stats = publisher.stats();
    publisher.stop();
    srv_handle.join().unwrap();

    assert!(stats.packets_sent >= 4, "sent={}", stats.packets_sent);
    assert!(stats.bytes_sent > 0);

    let cap = captured.lock().unwrap();
    assert_eq!(cap.app, "live");
    assert_eq!(cap.stream_key, "secret-stream");
    assert!(cap.metadata_received);
    assert_eq!(cap.video_tags.len(), 2);
    assert_eq!(cap.audio_tags.len(), 2);
    assert_eq!(&cap.video_tags[0][..2], &[0x17, 0x00]); // AVC seq header
    assert_eq!(&cap.video_tags[1][..2], &[0x17, 0x01]); // IDR frame
    assert_eq!(cap.audio_tags[0], vec![0xAF, 0x00, 0x11, 0x90]);
    assert_eq!(cap.audio_tags[1], vec![0xAF, 0x01, 0xDE, 0xAD, 0xBE, 0xEF]);
    let _ = events.lock().unwrap().len();
    let _ = PublisherEvent::Error(String::new());
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

    fn gen_ascii(&mut self, max_len: usize) -> String {
        let len = self.range_usize(0, max_len);
        let mut s = String::with_capacity(len);
        for _ in 0..len {
            let ch = (b'!' + (self.next_u64() as u8 % 90)) as char;
            s.push(ch);
        }
        s
    }

    fn gen_amf0(&mut self, depth: usize) -> Amf0Value {
        let choice = if depth >= 2 {
            self.range_usize(0, 3)
        } else {
            self.range_usize(0, 5)
        };
        match choice {
            0 => Amf0Value::Number((self.next_u64() as i64 as f64) * 1e-6),
            1 => Amf0Value::Boolean((self.next_u64() & 1) == 1),
            2 => Amf0Value::String(self.gen_ascii(48)),
            3 => Amf0Value::Null,
            4 => {
                let n = self.range_usize(0, 5);
                let mut pairs = Vec::with_capacity(n);
                for k in 0..n {
                    let key = format!("k{}_{}", k, self.gen_ascii(8));
                    pairs.push((key, self.gen_amf0(depth + 1)));
                }
                Amf0Value::Object(pairs)
            }
            _ => {
                let n = self.range_usize(0, 5);
                let mut pairs = Vec::with_capacity(n);
                for k in 0..n {
                    let key = format!("e{}_{}", k, self.gen_ascii(8));
                    pairs.push((key, self.gen_amf0(depth + 1)));
                }
                Amf0Value::EcmaArray(pairs)
            }
        }
    }
}

#[test]
fn test_property_amf0_arbitrary_roundtrip() {
    let mut rng = SplitMix64(0xA3F0_1234_5678_9ABC);
    for _ in 0..200 {
        let count = rng.range_usize(1, 8);
        let values: Vec<Amf0Value> = (0..count).map(|_| rng.gen_amf0(0)).collect();
        let bytes = encode_values(&values);
        let decoded = decode_all(&bytes).expect("AMF0 decode must succeed for valid encoded bytes");
        assert_eq!(decoded, values);
    }
}

#[test]
fn test_property_chunk_stream_arbitrary_csids_sizes_and_extended_timestamps() {
    let mut rng = SplitMix64(0xC40C_8765_4321_CAFE);

    for trial in 0..40 {
        let chunk_size = match trial % 5 {
            0 => 1,
            1 => 7,
            2 => 128,
            3 => 4096,
            _ => rng.range_usize(16, 2048),
        };
        let mut writer = ChunkWriter::new();
        let mut reader = ChunkReader::new();
        writer.set_chunk_size(chunk_size);
        reader.set_chunk_size(chunk_size);

        let msg_count = rng.range_usize(8, 28);
        let mut wire = Vec::new();
        let mut expected = Vec::with_capacity(msg_count);

        for m in 0..msg_count {
            // CSID ranges: 2..=63 (1B), 64..=319 (2B), 320..=2000 (3B)
            let csid = match rng.range_usize(0, 2) {
                0 => rng.range_usize(2, 63) as u32,
                1 => rng.range_usize(64, 319) as u32,
                _ => rng.range_usize(320, 2000) as u32,
            };
            // Include extended timestamps (>= 0x00FF_FFFF) and zero-delta continuations
            let ts = match m % 4 {
                0 => rng.range_usize(0, 100_000) as u32,
                1 => 0x00FF_FFFF + (rng.range_usize(0, 50_000) as u32),
                2 => 0x0100_0000, // repeated extended timestamp (delta == 0)
                _ => 0x0100_0000,
            };
            let payload_len = rng.range_usize(0, chunk_size * 3 + 19);
            let payload: Vec<u8> = (0..payload_len)
                .map(|_| (rng.next_u64() & 0xFF) as u8)
                .collect();
            let msg = RtmpMessage {
                csid,
                timestamp_ms: ts,
                msg_type: if (m & 1) == 0 { MSG_VIDEO } else { MSG_AUDIO },
                stream_id: 1,
                payload,
            };
            writer.write_message(&mut wire, &msg).unwrap();
            expected.push(msg);
        }

        let mut cursor = std::io::Cursor::new(wire);
        for exp in &expected {
            let got = reader.read_message(&mut cursor).unwrap();
            assert_eq!(&got, exp);
        }
    }
}

#[test]
fn test_property_annexb_to_avcc_and_abr_invariants() {
    use core_rtmp::flv::{annexb_to_avcc, split_annexb_nalus};

    let mut rng = SplitMix64(0x264A_B123_4567_8901);

    // 1. スタートコード種別 (3-byte / 4-byte) と任意の非スタートコードNALU列の往復分解・AVCC変換不変条件
    for _ in 0..100 {
        let nalu_count = rng.range_usize(1, 8);
        let mut expected_nalus: Vec<Vec<u8>> = Vec::with_capacity(nalu_count);
        let mut annexb = Vec::new();

        for idx in 0..nalu_count {
            if (rng.next_u64() & 1) == 0 || idx == 0 {
                annexb.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
            } else {
                annexb.extend_from_slice(&[0x00, 0x00, 0x01]);
            }
            let len = rng.range_usize(1, 256);
            let mut nalu = Vec::with_capacity(len);
            for _ in 0..len {
                // Emulation prevention: avoid generating 00 00 01 or 00 00 00 inside payload
                let b = (rng.range_usize(2, 255)) as u8;
                nalu.push(b);
            }
            annexb.extend_from_slice(&nalu);
            expected_nalus.push(nalu);
        }

        let split = split_annexb_nalus(&annexb);
        assert_eq!(split.len(), expected_nalus.len());
        for (got, exp) in split.iter().zip(expected_nalus.iter()) {
            assert_eq!(*got, exp.as_slice());
        }

        let avcc = annexb_to_avcc(&annexb);
        let mut pos = 0usize;
        for exp in &expected_nalus {
            let len = u32::from_be_bytes(avcc[pos..pos + 4].try_into().unwrap()) as usize;
            assert_eq!(len, exp.len());
            assert_eq!(&avcc[pos + 4..pos + 4 + len], exp.as_slice());
            pos += 4 + len;
        }
        assert_eq!(pos, avcc.len());
    }

    // 2. AimdAbrController の任意のキュー変動・経過時間に対するビットレート境界不変条件
    let cfg = AbrConfig {
        initial_bitrate_bps: 3_000_000,
        min_bitrate_bps: 500_000,
        max_bitrate_bps: 6_000_000,
        ..AbrConfig::default()
    };
    let mut abr = AimdAbrController::new(cfg.clone());
    for _ in 0..1000 {
        let q_bytes = rng.range_usize(0, 1_500_000);
        let dt_ms = rng.range_usize(1, 1000) as u64;
        let dec = abr.evaluate(q_bytes, dt_ms);
        assert!(
            dec.current_bitrate_bps >= cfg.min_bitrate_bps
                && dec.current_bitrate_bps <= cfg.max_bitrate_bps,
            "ABR bitrate {} out of [{}..={}]",
            dec.current_bitrate_bps,
            cfg.min_bitrate_bps,
            cfg.max_bitrate_bps
        );
        if let Some(nb) = dec.new_bitrate_bps {
            assert_eq!(nb, dec.current_bitrate_bps);
        }
    }
}

#[test]
fn test_chunk_writer_single_write_coalescing_and_nonblocking_try_read() {
    use core_rtmp::flv::{pack_audio_tag_into, pack_video_tag_into};
    use std::io::{self, Read, Write};

    struct CountingWriter {
        buf: Vec<u8>,
        write_calls: usize,
    }
    impl Write for CountingWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.write_calls += 1;
            self.buf.extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct WouldBlockReader;
    impl Read for WouldBlockReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "would block"))
        }
    }

    // 1. 100 KB の映像メッセージを 4096B チャンク (25 チャンク) に分割しても write_all が 1 回に集約されること
    let mut writer = ChunkWriter::new();
    writer.set_chunk_size(4096);
    let mut cw = CountingWriter {
        buf: Vec::new(),
        write_calls: 0,
    };

    let mut tag_scratch = Vec::with_capacity(128 * 1024);
    let mut nalu = vec![0x00, 0x00, 0x00, 0x01, 0x65];
    nalu.resize(100_000, 0x42);
    let v_pkt = EncodedPacket {
        codec: CodecKind::H264,
        pts: PtsUs(33_333),
        dts: PtsUs(16_667),
        is_keyframe: true,
        is_sequence_header: false,
        data: nalu,
    };
    pack_video_tag_into(&v_pkt, &mut tag_scratch).unwrap();
    assert_eq!(tag_scratch, pack_video_tag(&v_pkt).unwrap());

    writer
        .write_message_payload(&mut cw, CSID_VIDEO, 16, MSG_VIDEO, 1, &tag_scratch)
        .unwrap();
    assert_eq!(
        cw.write_calls, 1,
        "25 chunks must be coalesced into 1 write syscall"
    );

    // 同じ tag_scratch を音声タグ生成に再利用してもアロケーションなしで一致すること
    let a_pkt = EncodedPacket {
        codec: CodecKind::AacLc,
        pts: PtsUs(21_333),
        dts: PtsUs(21_333),
        is_keyframe: true,
        is_sequence_header: false,
        data: vec![0x21, 0x10, 0x04, 0x60, 0x8C, 0x1C],
    };
    let cap_before = tag_scratch.capacity();
    pack_audio_tag_into(&a_pkt, &mut tag_scratch).unwrap();
    assert_eq!(tag_scratch.capacity(), cap_before);
    assert_eq!(tag_scratch, pack_audio_tag(&a_pkt).unwrap());

    // 2. try_read_message: 読み取り可能メッセージは Some(msg)、WouldBlock 時はエラー文字列生成なしで Ok(None)
    let mut reader = ChunkReader::new();
    reader.set_chunk_size(4096);
    let mut cursor = Cursor::new(cw.buf);
    let msg = reader
        .try_read_message(&mut cursor)
        .unwrap()
        .expect("message expected");
    assert_eq!(msg.csid, CSID_VIDEO);
    assert_eq!(msg.timestamp_ms, 16);
    assert_eq!(msg.payload, pack_video_tag(&v_pkt).unwrap());

    let mut wb = WouldBlockReader;
    assert_eq!(reader.try_read_message(&mut wb).unwrap(), None);
}
