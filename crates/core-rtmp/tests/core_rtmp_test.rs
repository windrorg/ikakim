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
