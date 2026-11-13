use core_audio::mixer::{AudioInputConfig, AudioMixer};
use core_audio::MIX_BLOCK_FRAMES;
use core_engine::pipeline::{flush_encoders_to_outputs, EngineControlBridge, PipelineTickContext};
use core_engine::virtual_input::{
    channel_rms_dbfs, goertzel_magnitude, VirtualAudioSignal, VirtualAudioSource,
    VirtualVideoPattern, VirtualVideoSource,
};
use core_engine::{SwH264Decoder, SwH264Encoder};
use core_gpu::{ColorSpace, CompositorConfig, GpuCompositor, GpuContext};
use core_net_ui::rpc::ControlBackend;
use core_net_ui::{EmbeddedServer, ServerConfig};
use core_rtmp::abr::AbrConfig;
use core_rtmp::amf0::{decode_all, encode_values, Amf0Value};
use core_rtmp::chunk::{
    ChunkReader, ChunkWriter, RtmpMessage, CSID_COMMAND, CSID_PROTOCOL_CONTROL, MSG_AUDIO,
    MSG_COMMAND_AMF0, MSG_DATA_AMF0, MSG_SET_CHUNK_SIZE, MSG_VIDEO, MSG_WINDOW_ACK_SIZE,
};
use core_rtmp::handshake::perform_server_handshake;
use core_rtmp::TlsRootConfig;
use core_types::clock::{MasterClock, PtsUs};
use core_types::media::{CodecKind, EncodedPacket, LayerTransform, NativeBufferHandle, VideoFrame};
use core_types::state::PipelineState;
use core_types::traits::{AudioEncoder, VideoDecoder, VideoEncoder};
use core_types::CoreResult;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

struct MockAudioEncoder {
    queue: VecDeque<EncodedPacket>,
    seq_sent: bool,
    base_pts: Option<PtsUs>,
    step_idx: u64,
}

impl MockAudioEncoder {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            seq_sent: false,
            base_pts: None,
            step_idx: 0,
        }
    }
}

impl AudioEncoder for MockAudioEncoder {
    fn push_pcm_f32(&mut self, _interleaved_pcm: &[f32], pts: PtsUs) -> CoreResult<()> {
        if !self.seq_sent {
            self.seq_sent = true;
            self.queue.push_back(EncodedPacket {
                codec: CodecKind::AacLc,
                pts: PtsUs(0),
                dts: PtsUs(0),
                is_keyframe: true,
                is_sequence_header: true,
                data: vec![0x11, 0x90], // 48kHz stereo AAC-LC ASC
            });
        }
        let base = *self.base_pts.get_or_insert(pts);
        let p = PtsUs(base.0 + self.step_idx * 21_333);
        self.step_idx += 1;
        self.queue.push_back(EncodedPacket {
            codec: CodecKind::AacLc,
            pts: p,
            dts: p,
            is_keyframe: true,
            is_sequence_header: false,
            data: vec![0x21, 0x10, 0x04, 0x60, 0x8C, 0x1C],
        });
        Ok(())
    }

    fn pull_packet(&mut self) -> CoreResult<Option<EncodedPacket>> {
        Ok(self.queue.pop_front())
    }
}

/// Capturing VideoEncoder wrapper that observes `set_target_bitrate` and `request_keyframe`
/// calls forwarded by `EngineControlBridge` + `flush_encoders_to_outputs`.
struct SpyVideoEncoder {
    inner: SwH264Encoder,
    width: u32,
    height: u32,
    bitrate_updates: Arc<Mutex<Vec<u32>>>,
    keyframe_requests: Arc<AtomicUsize>,
}

impl SpyVideoEncoder {
    fn new(
        width: u32,
        height: u32,
        initial_bitrate_bps: u32,
    ) -> (Self, Arc<Mutex<Vec<u32>>>, Arc<AtomicUsize>) {
        let bitrates = Arc::new(Mutex::new(Vec::new()));
        let kf_reqs = Arc::new(AtomicUsize::new(0));
        (
            Self {
                inner: SwH264Encoder::new(width, height, initial_bitrate_bps),
                width,
                height,
                bitrate_updates: Arc::clone(&bitrates),
                keyframe_requests: Arc::clone(&kf_reqs),
            },
            bitrates,
            kf_reqs,
        )
    }

    fn encode_dummy(&mut self, pts: PtsUs) {
        let rgba = vec![128u8; self.width as usize * self.height as usize * 4];
        self.inner.encode_rgba(&rgba, pts).expect("encode_rgba");
    }
}

impl VideoEncoder for SpyVideoEncoder {
    fn set_target_bitrate(&mut self, bitrate_bps: u32) -> CoreResult<()> {
        self.bitrate_updates.lock().unwrap().push(bitrate_bps);
        self.inner.set_target_bitrate(bitrate_bps)
    }

    fn request_keyframe(&mut self) {
        self.keyframe_requests.fetch_add(1, Ordering::Relaxed);
        self.inner.request_keyframe();
    }

    fn pull_packet(&mut self) -> CoreResult<Option<EncodedPacket>> {
        self.inner.pull_packet()
    }
}

#[derive(Default, Clone)]
struct RecordedConnection {
    app: String,
    stream_key: String,
    metadata_count: usize,
    video_timestamps: Vec<u32>,
    video_tags: Vec<Vec<u8>>,
    audio_tags: Vec<Vec<u8>>,
}

#[derive(Clone, Copy)]
enum ServerBehavior {
    Normal,
    DropAfterVideoTags { limit: usize },
    SendUnpublishAfterVideoTags { limit: usize },
    ThrottlePerMessage { delay_ms: u64, active: &'static AtomicBool },
}

fn handle_rtmp_session(
    mut stream: TcpStream,
    records: Arc<Mutex<Vec<RecordedConnection>>>,
    behavior: ServerBehavior,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    if perform_server_handshake(&mut stream).is_err() {
        return;
    }
    let mut reader = ChunkReader::new();
    let mut writer = ChunkWriter::new();

    let _ = writer.write_message(
        &mut stream,
        &RtmpMessage {
            csid: CSID_PROTOCOL_CONTROL,
            timestamp_ms: 0,
            msg_type: MSG_WINDOW_ACK_SIZE,
            stream_id: 0,
            payload: 2_500_000u32.to_be_bytes().to_vec(),
        },
    );
    let _ = writer.write_message(
        &mut stream,
        &RtmpMessage {
            csid: CSID_PROTOCOL_CONTROL,
            timestamp_ms: 0,
            msg_type: MSG_SET_CHUNK_SIZE,
            stream_id: 0,
            payload: 4096u32.to_be_bytes().to_vec(),
        },
    );
    writer.set_chunk_size(4096);

    let mut rec = RecordedConnection::default();

    while let Ok(msg) = reader.read_message(&mut stream) {
        match msg.msg_type {
            MSG_COMMAND_AMF0 => {
                let Ok(vals) = decode_all(&msg.payload) else {
                    break;
                };
                let cmd = vals.first().and_then(|v| v.as_str()).unwrap_or("");
                let txn = vals.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
                match cmd {
                    "connect" => {
                        if let Some(app) = vals
                            .get(2)
                            .and_then(|o| o.get("app"))
                            .and_then(|v| v.as_str())
                        {
                            rec.app = app.to_string();
                        }
                        let resp = encode_values(&[
                            Amf0Value::String("_result".into()),
                            Amf0Value::Number(txn),
                            Amf0Value::Object(vec![("fmsVer".into(), Amf0Value::String("FMS/3,0".into()))]),
                            Amf0Value::Object(vec![
                                ("level".into(), Amf0Value::String("status".into())),
                                ("code".into(), Amf0Value::String("NetConnection.Connect.Success".into())),
                            ]),
                        ]);
                        let _ = writer.write_message(
                            &mut stream,
                            &RtmpMessage {
                                csid: CSID_COMMAND,
                                timestamp_ms: 0,
                                msg_type: MSG_COMMAND_AMF0,
                                stream_id: 0,
                                payload: resp,
                            },
                        );
                    }
                    "createStream" => {
                        let resp = encode_values(&[
                            Amf0Value::String("_result".into()),
                            Amf0Value::Number(txn),
                            Amf0Value::Null,
                            Amf0Value::Number(1.0),
                        ]);
                        let _ = writer.write_message(
                            &mut stream,
                            &RtmpMessage {
                                csid: CSID_COMMAND,
                                timestamp_ms: 0,
                                msg_type: MSG_COMMAND_AMF0,
                                stream_id: 0,
                                payload: resp,
                            },
                        );
                    }
                    "publish" => {
                        if let Some(key) = vals.get(3).and_then(|v| v.as_str()) {
                            rec.stream_key = key.to_string();
                        }
                        let resp = encode_values(&[
                            Amf0Value::String("onStatus".into()),
                            Amf0Value::Number(0.0),
                            Amf0Value::Null,
                            Amf0Value::Object(vec![
                                ("level".into(), Amf0Value::String("status".into())),
                                ("code".into(), Amf0Value::String("NetStream.Publish.Start".into())),
                            ]),
                        ]);
                        let _ = writer.write_message(
                            &mut stream,
                            &RtmpMessage {
                                csid: CSID_COMMAND,
                                timestamp_ms: 0,
                                msg_type: MSG_COMMAND_AMF0,
                                stream_id: 1,
                                payload: resp,
                            },
                        );
                    }
                    "deleteStream" => break,
                    _ => {}
                }
            }
            MSG_DATA_AMF0 => rec.metadata_count += 1,
            MSG_VIDEO => {
                rec.video_timestamps.push(msg.timestamp_ms);
                rec.video_tags.push(msg.payload);
                match behavior {
                    ServerBehavior::Normal => {}
                    ServerBehavior::DropAfterVideoTags { limit } => {
                        if rec.video_tags.len() >= limit {
                            break;
                        }
                    }
                    ServerBehavior::SendUnpublishAfterVideoTags { limit } => {
                        if rec.video_tags.len() >= limit {
                            let resp = encode_values(&[
                                Amf0Value::String("onStatus".into()),
                                Amf0Value::Number(0.0),
                                Amf0Value::Null,
                                Amf0Value::Object(vec![
                                    ("level".into(), Amf0Value::String("status".into())),
                                    (
                                        "code".into(),
                                        Amf0Value::String("NetStream.Unpublish.Success".into()),
                                    ),
                                ]),
                            ]);
                            let _ = writer.write_message(
                                &mut stream,
                                &RtmpMessage {
                                    csid: CSID_COMMAND,
                                    timestamp_ms: 0,
                                    msg_type: MSG_COMMAND_AMF0,
                                    stream_id: 1,
                                    payload: resp,
                                },
                            );
                            let _ = stream.flush();
                            // Keep TCP open briefly so client must react via `poll_inbound` onStatus parsing
                            thread::sleep(Duration::from_millis(250));
                            break;
                        }
                    }
                    ServerBehavior::ThrottlePerMessage { delay_ms, active } => {
                        if active.load(Ordering::Relaxed) {
                            thread::sleep(Duration::from_millis(delay_ms));
                        }
                    }
                }
            }
            MSG_AUDIO => rec.audio_tags.push(msg.payload),
            _ => {}
        }
    }
    records.lock().unwrap().push(rec);
}

#[allow(clippy::type_complexity)]
fn build_test_harness(width: u32, height: u32, initial_bitrate: u32) -> (
    MasterClock,
    AudioMixer,
    core_audio::mixer::AudioInputProducer,
    Arc<EngineControlBridge>,
    EmbeddedServer,
    GpuCompositor,
    SpyVideoEncoder,
    SwH264Encoder,
    MockAudioEncoder,
    Arc<Mutex<Vec<u32>>>,
    Arc<AtomicUsize>,
) {
    let clock = MasterClock::new();
    let mut mixer = AudioMixer::new(clock.clone());
    let (ch0, prod0) = mixer.add_channel(AudioInputConfig {
        name: "test-mic".into(),
        sample_rate: 48_000,
        channels: 2,
        initial_gain: 1.0,
        initial_muted: false,
        initial_delay_ms: 0,
    });
    let bridge = EngineControlBridge::new(
        vec![ch0],
        mixer.master_handle(),
        initial_bitrate,
        30.0,
        TlsRootConfig::System,
    );
    let server = EmbeddedServer::start(
        ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            meter_interval_ms: 25,
            preview_width: width / 2,
            preview_height: height / 2,
            preview_fps: 30,
        },
        bridge.clone(),
    )
    .expect("EmbeddedServer");
    let gpu = GpuContext::new_headless().expect("GpuContext");
    let comp = GpuCompositor::new(
        gpu,
        CompositorConfig {
            main_width: width,
            main_height: height,
            preview_width: width / 2,
            preview_height: height / 2,
            target_fps: 30,
        },
    );
    let (main_enc, bitrates, kf_reqs) = SpyVideoEncoder::new(width, height, initial_bitrate);
    let prev_enc = SwH264Encoder::new(width / 2, height / 2, initial_bitrate / 4);
    let aud_enc = MockAudioEncoder::new();
    (
        clock,
        mixer,
        prod0,
        bridge,
        server,
        comp,
        main_enc,
        prev_enc,
        aud_enc,
        bitrates,
        kf_reqs,
    )
}

#[allow(clippy::too_many_arguments)]
fn step_pipeline_frame(
    idx: u64,
    clock: &mut MasterClock,
    mixer: &mut AudioMixer,
    prod0: &mut core_audio::mixer::AudioInputProducer,
    bridge: &EngineControlBridge,
    comp: &mut GpuCompositor,
    main_enc: &mut SpyVideoEncoder,
    prev_enc: &mut SwH264Encoder,
    aud_enc: &mut MockAudioEncoder,
    server: &EmbeddedServer,
) {
    let pts = PtsUs(idx * 33_333);
    main_enc.encode_dummy(pts);
    let rgba_prev = vec![96u8; (main_enc.width / 2) as usize * (main_enc.height / 2) as usize * 4];
    prev_enc.encode_rgba(&rgba_prev, pts).unwrap();

    let pcm = vec![0.1f32; MIX_BLOCK_FRAMES * 2];
    let audio_blocks = if idx % 2 == 0 { 2 } else { 1 };
    for _ in 0..audio_blocks {
        prod0.push_f32(&pcm);
        let mut mix_buf = vec![0.0f32; MIX_BLOCK_FRAMES * 2];
        let st = mixer.mix_block(&mut mix_buf);
        aud_enc.push_pcm_f32(&mix_buf, st.to_pts_us()).unwrap();
    }

    let mut tick = PipelineTickContext {
        clock,
        bridge,
        compositor: comp,
        main_encoder: main_enc,
        preview_encoder: prev_enc,
        audio_encoder: aud_enc,
        server,
    };
    flush_encoders_to_outputs(&mut tick, pts).expect("flush");
}

#[test]
fn test_engine_late_stream_start_injects_cached_seq_headers_and_rebases_timestamps() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let records = Arc::new(Mutex::new(Vec::new()));
    let rec_clone = Arc::clone(&records);
    let stop_srv = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop_srv);
    listener.set_nonblocking(true).unwrap();

    let srv_thread = thread::spawn(move || {
        while !stop_clone.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((s, _)) => handle_rtmp_session(s, Arc::clone(&rec_clone), ServerBehavior::Normal),
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
    });

    let (
        mut clock,
        mut mixer,
        mut prod0,
        bridge,
        mut server,
        mut comp,
        mut main_enc,
        mut prev_enc,
        mut aud_enc,
        _,
        kf_reqs,
    ) = build_test_harness(64, 64, 1_500_000);

    // 1. Run 10 preview frames BEFORE start_stream -> initial SPS/PPS & AAC ASC already emitted here!
    for i in 0..10u64 {
        step_pipeline_frame(
            i,
            &mut clock,
            &mut mixer,
            &mut prod0,
            &bridge,
            &mut comp,
            &mut main_enc,
            &mut prev_enc,
            &mut aud_enc,
            &server,
        );
    }
    assert_eq!(bridge.snapshot().state, PipelineState::Idle);

    // 2. Start live stream late -> must replay cached SPS/PPS + AAC ASC + force IDR + rebase ts from 0ms
    let url = format!("rtmp://127.0.0.1:{}/live/late_start", port);
    bridge.start_stream(&url, Some(1_200_000)).unwrap();

    for i in 10..20u64 {
        step_pipeline_frame(
            i,
            &mut clock,
            &mut mixer,
            &mut prod0,
            &bridge,
            &mut comp,
            &mut main_enc,
            &mut prev_enc,
            &mut aud_enc,
            &server,
        );
        thread::sleep(Duration::from_millis(15));
    }

    // Wait for packets to flush to server
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some(st) = bridge.publisher_stats() {
            if st.packets_sent >= 10 {
                break;
            }
        }
        thread::sleep(Duration::from_millis(20));
    }

    assert!(kf_reqs.load(Ordering::Relaxed) >= 1, "expected keyframe request on late start");
    bridge.stop_stream().unwrap();
    assert_eq!(bridge.snapshot().state, PipelineState::Previewing);

    bridge.stop_all();
    server.stop();
    stop_srv.store(true, Ordering::Relaxed);
    srv_thread.join().unwrap();

    let recs = records.lock().unwrap();
    assert_eq!(recs.len(), 1);
    let r = &recs[0];
    assert_eq!(r.app, "live");
    assert_eq!(r.stream_key, "late_start");
    assert!(r.video_tags.len() >= 3, "video_tags={}", r.video_tags.len());
    assert!(r.audio_tags.len() >= 3, "audio_tags={}", r.audio_tags.len());
    // First video tag MUST be AVC sequence header (0x17, 0x00), second MUST be IDR keyframe (0x17, 0x01)
    assert_eq!(&r.video_tags[0][..2], &[0x17, 0x00]);
    assert_eq!(&r.video_tags[1][..2], &[0x17, 0x01]);
    // First audio tag MUST be AAC sequence header (0xAF, 0x00)
    assert_eq!(&r.audio_tags[0][..2], &[0xAF, 0x00]);
    // Timestamps rebased near 0ms (NOT 333ms from frame 10!)
    assert_eq!(r.video_timestamps[0], 0);
    assert!(
        r.video_timestamps[1] <= 50,
        "expected rebased first media ts near 0ms, got {}",
        r.video_timestamps[1]
    );
}

#[test]
fn test_engine_midstream_disconnect_auto_reconnects_and_requests_idr() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let records = Arc::new(Mutex::new(Vec::new()));
    let rec_clone = Arc::clone(&records);
    let stop_srv = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop_srv);
    let conn_num = Arc::new(AtomicUsize::new(0));
    let conn_clone = Arc::clone(&conn_num);
    listener.set_nonblocking(true).unwrap();

    let srv_thread = thread::spawn(move || {
        while !stop_clone.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((s, _)) => {
                    let n = conn_clone.fetch_add(1, Ordering::Relaxed);
                    // Drop the 1st TCP session mid-stream after 4 video tags, accept 2nd normally
                    let beh = if n == 0 {
                        ServerBehavior::DropAfterVideoTags { limit: 4 }
                    } else {
                        ServerBehavior::Normal
                    };
                    handle_rtmp_session(s, Arc::clone(&rec_clone), beh);
                }
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
    });

    let (
        mut clock,
        mut mixer,
        mut prod0,
        bridge,
        mut server,
        mut comp,
        mut main_enc,
        mut prev_enc,
        mut aud_enc,
        _,
        kf_reqs,
    ) = build_test_harness(64, 64, 1_500_000);
    bridge.set_reconnect_policy(80, 400, Some(5));

    bridge
        .start_stream(&format!("rtmp://127.0.0.1:{}/live/reconn", port), None)
        .unwrap();

    let mut frame = 0u64;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_reconnecting = false;
    let mut recovered = false;

    while Instant::now() < deadline {
        step_pipeline_frame(
            frame,
            &mut clock,
            &mut mixer,
            &mut prod0,
            &bridge,
            &mut comp,
            &mut main_enc,
            &mut prev_enc,
            &mut aud_enc,
            &server,
        );
        frame += 1;
        let snap = bridge.snapshot();
        if snap.state == PipelineState::Reconnecting {
            saw_reconnecting = true;
        }
        if let Some(st) = bridge.publisher_stats() {
            if st.reconnect_count >= 1 && snap.state == PipelineState::Streaming && conn_num.load(Ordering::Relaxed) >= 2 {
                if records.lock().unwrap().is_empty() {
                    // Continue pushing frames into session 2
                }
                if st.packets_sent > 12 {
                    recovered = true;
                    break;
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }

    assert!(saw_reconnecting || bridge.publisher_stats().unwrap().reconnect_count >= 1);
    assert!(recovered, "stream did not recover after server disconnect");
    assert!(kf_reqs.load(Ordering::Relaxed) >= 2, "expected keyframe request on initial + reconnect");

    bridge.stop_all();
    server.stop();
    stop_srv.store(true, Ordering::Relaxed);
    srv_thread.join().unwrap();

    let recs = records.lock().unwrap();
    assert!(recs.len() >= 2, "expected >=2 recorded connections, got {}", recs.len());
    // Session 2 MUST have re-sent AVC seq header & audio seq header & IDR keyframe
    assert_eq!(&recs[1].video_tags[0][..2], &[0x17, 0x00]);
    assert_eq!(&recs[1].audio_tags[0][..2], &[0xAF, 0x00]);
    assert_eq!(&recs[1].video_tags[1][..2], &[0x17, 0x01]);
}

#[test]
fn test_engine_server_onstatus_unpublish_triggers_immediate_reconnect() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let records = Arc::new(Mutex::new(Vec::new()));
    let rec_clone = Arc::clone(&records);
    let stop_srv = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop_srv);
    let conn_num = Arc::new(AtomicUsize::new(0));
    let conn_clone = Arc::clone(&conn_num);
    listener.set_nonblocking(true).unwrap();

    let srv_thread = thread::spawn(move || {
        while !stop_clone.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((s, _)) => {
                    let n = conn_clone.fetch_add(1, Ordering::Relaxed);
                    let beh = if n == 0 {
                        ServerBehavior::SendUnpublishAfterVideoTags { limit: 3 }
                    } else {
                        ServerBehavior::Normal
                    };
                    handle_rtmp_session(s, Arc::clone(&rec_clone), beh);
                }
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
    });

    let (
        mut clock,
        mut mixer,
        mut prod0,
        bridge,
        mut server,
        mut comp,
        mut main_enc,
        mut prev_enc,
        mut aud_enc,
        _,
        _,
    ) = build_test_harness(64, 64, 1_500_000);
    bridge.set_reconnect_policy(60, 300, Some(5));

    bridge
        .start_stream(&format!("rtmp://127.0.0.1:{}/live/unpub", port), None)
        .unwrap();

    let mut frame = 0u64;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut reconnected = false;
    while Instant::now() < deadline {
        step_pipeline_frame(
            frame,
            &mut clock,
            &mut mixer,
            &mut prod0,
            &bridge,
            &mut comp,
            &mut main_enc,
            &mut prev_enc,
            &mut aud_enc,
            &server,
        );
        frame += 1;
        if let Some(st) = bridge.publisher_stats() {
            if st.reconnect_count >= 1 && conn_num.load(Ordering::Relaxed) >= 2 && st.packets_sent >= 8 {
                reconnected = true;
                break;
            }
        }
        thread::sleep(Duration::from_millis(20));
    }

    assert!(reconnected, "expected reconnect on server NetStream.Unpublish.Success");
    bridge.stop_all();
    server.stop();
    stop_srv.store(true, Ordering::Relaxed);
    srv_thread.join().unwrap();
}

#[test]
fn test_engine_max_reconnect_exhaustion_transitions_to_error_and_recovers() {
    // Allocate a closed port
    let tmp = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = tmp.local_addr().unwrap().port();
    drop(tmp);

    let (
        mut clock,
        mut mixer,
        mut prod0,
        bridge,
        mut server,
        mut comp,
        mut main_enc,
        mut prev_enc,
        mut aud_enc,
        _,
        _,
    ) = build_test_harness(64, 64, 1_000_000);
    bridge.set_reconnect_policy(60, 120, Some(2));

    bridge
        .start_stream(&format!("rtmp://127.0.0.1:{}/live/dead", dead_port), None)
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(4);
    let mut reached_error = false;
    while Instant::now() < deadline {
        if bridge.snapshot().state == PipelineState::Error {
            reached_error = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(reached_error, "expected Error state after max reconnect attempts");
    assert!(bridge.last_error().is_some());

    // Now start a healthy server and retry start_stream directly from Error state!
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let healthy_port = listener.local_addr().unwrap().port();
    let records = Arc::new(Mutex::new(Vec::new()));
    let rec_clone = Arc::clone(&records);
    let stop_srv = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop_srv);
    listener.set_nonblocking(true).unwrap();

    let srv_thread = thread::spawn(move || {
        while !stop_clone.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((s, _)) => handle_rtmp_session(s, Arc::clone(&rec_clone), ServerBehavior::Normal),
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
    });

    bridge
        .start_stream(&format!("rtmp://127.0.0.1:{}/live/recovered", healthy_port), Some(800_000))
        .expect("recover start_stream from Error state");

    let recover_deadline = Instant::now() + Duration::from_secs(3);
    let mut rec_i = 0u64;
    while Instant::now() < recover_deadline {
        step_pipeline_frame(
            rec_i,
            &mut clock,
            &mut mixer,
            &mut prod0,
            &bridge,
            &mut comp,
            &mut main_enc,
            &mut prev_enc,
            &mut aud_enc,
            &server,
        );
        rec_i += 1;
        if bridge.snapshot().state == PipelineState::Streaming
            && bridge
                .publisher_stats()
                .is_some_and(|s| s.packets_sent >= 4)
        {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }

    assert_eq!(bridge.snapshot().state, PipelineState::Streaming);
    let sent = bridge.publisher_stats().unwrap().packets_sent;
    assert!(sent >= 4, "expected packets_sent >= 4 after recovery, got {}", sent);

    bridge.stop_all();
    server.stop();
    stop_srv.store(true, Ordering::Relaxed);
    srv_thread.join().unwrap();
}

#[test]
fn test_engine_abr_congestion_slowdown_and_encoder_bitrate_feedback() {
    static THROTTLE_ACTIVE: AtomicBool = AtomicBool::new(true);
    THROTTLE_ACTIVE.store(true, Ordering::Relaxed);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let records = Arc::new(Mutex::new(Vec::new()));
    let rec_clone = Arc::clone(&records);
    let stop_srv = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop_srv);
    listener.set_nonblocking(true).unwrap();

    let srv_thread = thread::spawn(move || {
        while !stop_clone.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((s, _)) => handle_rtmp_session(
                    s,
                    Arc::clone(&rec_clone),
                    ServerBehavior::ThrottlePerMessage {
                        delay_ms: 80,
                        active: &THROTTLE_ACTIVE,
                    },
                ),
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
    });

    let (
        mut clock,
        mut mixer,
        mut prod0,
        bridge,
        mut server,
        mut comp,
        mut main_enc,
        mut prev_enc,
        mut aud_enc,
        bitrates,
        _,
    ) = build_test_harness(64, 64, 2_000_000);

    // Aggressive ABR thresholds so congestion triggers deterministically inside a fast unit/E2E test
    bridge.set_abr_config(AbrConfig {
        initial_bitrate_bps: 2_000_000,
        min_bitrate_bps: 300_000,
        max_bitrate_bps: 4_000_000,
        decrease_dwell_threshold_ms: 250,
        decrease_factor: 0.70,
        decrease_cooldown_ms: 300,
        emergency_drop_dwell_ms: 700,
        increase_dwell_threshold_ms: 100,
        increase_hold_ms: 600,
        increase_step_bps: 200_000,
    });

    bridge
        .start_stream(&format!("rtmp://127.0.0.1:{}/live/abr", port), Some(2_000_000))
        .unwrap();

    // Wait briefly for RTMP connect to complete before flooding
    thread::sleep(Duration::from_millis(100));

    // Push large bursts while server reads slowly (80ms per video tag) so kernel TCP buffers fill and SharedQueue builds dwell!
    let big_payload = vec![0x88u8; 180_000];
    for i in 0..35u64 {
        let pts = PtsUs(i * 33_333);
        bridge.forward_packet_to_rtmp(&EncodedPacket {
            codec: CodecKind::H264,
            pts,
            dts: pts,
            is_keyframe: i % 5 == 0,
            is_sequence_header: false,
            data: big_payload.clone(),
        });
        step_pipeline_frame(
            i,
            &mut clock,
            &mut mixer,
            &mut prod0,
            &bridge,
            &mut comp,
            &mut main_enc,
            &mut prev_enc,
            &mut aud_enc,
            &server,
        );
        thread::sleep(Duration::from_millis(25));
    }

    let snap_congested = bridge.snapshot();
    assert!(
        snap_congested.bitrate_bps < 2_000_000,
        "expected ABR decrease below 2_000_000, got {}",
        snap_congested.bitrate_bps
    );
    assert!(
        bridge.abr_adjustment_count() >= 1,
        "expected >=1 ABR adjustment event"
    );
    let enc_bitrates = bitrates.lock().unwrap().clone();
    assert!(
        enc_bitrates.iter().any(|&b| b < 2_000_000),
        "expected SpyVideoEncoder::set_target_bitrate to receive decreased bitrate, got {:?}",
        enc_bitrates
    );

    // Lift throttle -> queue drains -> recovery step upward!
    THROTTLE_ACTIVE.store(false, Ordering::Relaxed);
    let low_br = snap_congested.bitrate_bps;
    let recovery_deadline = Instant::now() + Duration::from_secs(4);
    let mut frame_idx = 35u64;
    let mut recovered_br = low_br;
    while Instant::now() < recovery_deadline {
        step_pipeline_frame(
            frame_idx,
            &mut clock,
            &mut mixer,
            &mut prod0,
            &bridge,
            &mut comp,
            &mut main_enc,
            &mut prev_enc,
            &mut aud_enc,
            &server,
        );
        frame_idx += 1;
        recovered_br = bridge.snapshot().bitrate_bps;
        if recovered_br > low_br {
            break;
        }
        thread::sleep(Duration::from_millis(33));
    }
    assert!(
        recovered_br > low_br,
        "expected ABR additive increase after congestion cleared: low={} recovered={} decision={:?} stats={:?}",
        low_br,
        recovered_br,
        bridge.last_abr_decision(),
        bridge.publisher_stats()
    );

    bridge.stop_all();
    server.stop();
    stop_srv.store(true, Ordering::Relaxed);
    srv_thread.join().unwrap();
}

#[test]
fn test_engine_websocket_jsonrpc_stream_lifecycle_and_controls() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let rtmp_port = listener.local_addr().unwrap().port();
    let records = Arc::new(Mutex::new(Vec::new()));
    let rec_clone = Arc::clone(&records);
    let stop_srv = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop_srv);
    listener.set_nonblocking(true).unwrap();

    let srv_thread = thread::spawn(move || {
        while !stop_clone.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((s, _)) => handle_rtmp_session(s, Arc::clone(&rec_clone), ServerBehavior::Normal),
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
    });

    let (
        mut clock,
        mut mixer,
        mut prod0,
        bridge,
        mut server,
        mut comp,
        mut main_enc,
        mut prev_enc,
        mut aud_enc,
        _,
        _,
    ) = build_test_harness(64, 64, 1_800_000);
    let ui_addr = server.local_addr();

    // Minimal WebSocket handshake over TcpStream to exercise real JSON-RPC over WS
    let mut ws = TcpStream::connect(ui_addr).unwrap();
    ws.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let req = format!(
        "GET /ws HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        ui_addr
    );
    ws.write_all(req.as_bytes()).unwrap();
    let mut hdr_buf = [0u8; 1024];
    let n = ws.read(&mut hdr_buf).unwrap();
    let hdr_str = String::from_utf8_lossy(&hdr_buf[..n]);
    assert!(hdr_str.contains("101 Switching Protocols"));

    fn send_masked_ws_text(stream: &mut TcpStream, txt: &str) {
        let payload = txt.as_bytes();
        let mask = [0x12, 0x34, 0x56, 0x78];
        let mut frame = vec![0x81u8];
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        for (idx, b) in payload.iter().enumerate() {
            frame.push(b ^ mask[idx % 4]);
        }
        stream.write_all(&frame).unwrap();
    }

    // Issue JSON-RPC calls: mute, gain, delay, overlay transform, then stream.start
    send_masked_ws_text(
        &mut ws,
        r#"{"jsonrpc":"2.0","id":1,"method":"audio.set_gain","params":{"channel":0,"gain_db":-6.0}}"#,
    );
    send_masked_ws_text(
        &mut ws,
        r#"{"jsonrpc":"2.0","id":2,"method":"audio.set_mute","params":{"channel":0,"muted":true}}"#,
    );
    send_masked_ws_text(
        &mut ws,
        r#"{"jsonrpc":"2.0","id":3,"method":"overlay.set_transform","params":{"layer_id":0,"opacity":0.5,"x":0.1,"y":0.2,"scale_x":0.8,"scale_y":0.8,"rotation_rad":0.0,"visible":true}}"#,
    );
    let rpc_start = format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"stream.start","params":{{"rtmp_url":"rtmp://127.0.0.1:{}/live/ws_e2e","bitrate_bps":950000}}}}"#,
        rtmp_port
    );
    send_masked_ws_text(&mut ws, &rpc_start);

    for i in 0..12u64 {
        step_pipeline_frame(
            i,
            &mut clock,
            &mut mixer,
            &mut prod0,
            &bridge,
            &mut comp,
            &mut main_enc,
            &mut prev_enc,
            &mut aud_enc,
            &server,
        );
        thread::sleep(Duration::from_millis(20));
    }

    let snap = bridge.snapshot();
    assert_eq!(snap.state, PipelineState::Streaming);
    assert_eq!(snap.bitrate_bps, 950_000);
    assert!(snap.channels[0].muted);
    assert!((snap.channels[0].gain_db - (-6.0)).abs() < 0.1);
    assert!((snap.overlays[0].transform.opacity - 0.5).abs() < 0.01);

    // Stop stream via WS RPC
    send_masked_ws_text(&mut ws, r#"{"jsonrpc":"2.0","id":5,"method":"stream.stop"}"#);
    for _ in 0..30 {
        if bridge.snapshot().state == PipelineState::Previewing {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(bridge.snapshot().state, PipelineState::Previewing);

    bridge.stop_all();
    server.stop();
    stop_srv.store(true, Ordering::Relaxed);
    srv_thread.join().unwrap();
}

struct CaptureAudioEncoder {
    inner: MockAudioEncoder,
    captured_pcm: Arc<Mutex<Vec<f32>>>,
}

impl CaptureAudioEncoder {
    fn new() -> (Self, Arc<Mutex<Vec<f32>>>) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                inner: MockAudioEncoder::new(),
                captured_pcm: Arc::clone(&captured),
            },
            captured,
        )
    }
}

impl AudioEncoder for CaptureAudioEncoder {
    fn push_pcm_f32(&mut self, interleaved_pcm: &[f32], pts: PtsUs) -> CoreResult<()> {
        self.captured_pcm
            .lock()
            .unwrap()
            .extend_from_slice(interleaved_pcm);
        self.inner.push_pcm_f32(interleaved_pcm, pts)
    }

    fn pull_packet(&mut self) -> CoreResult<Option<EncodedPacket>> {
        self.inner.pull_packet()
    }
}

fn avcc_payload_to_annexb(avcc: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off + 4 <= avcc.len() {
        let nalu_len = u32::from_be_bytes([avcc[off], avcc[off + 1], avcc[off + 2], avcc[off + 3]])
            as usize;
        off += 4;
        if off + nalu_len > avcc.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&avcc[off..off + nalu_len]);
        off += nalu_len;
    }
    out
}

fn avc_config_record_to_annexb(rec: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if rec.len() < 7 {
        return out;
    }
    let num_sps = (rec[5] & 0x1F) as usize;
    let mut off = 6usize;
    for _ in 0..num_sps {
        if off + 2 > rec.len() {
            return out;
        }
        let len = u16::from_be_bytes([rec[off], rec[off + 1]]) as usize;
        off += 2;
        if off + len > rec.len() {
            return out;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&rec[off..off + len]);
        off += len;
    }
    if off >= rec.len() {
        return out;
    }
    let num_pps = rec[off] as usize;
    off += 1;
    for _ in 0..num_pps {
        if off + 2 > rec.len() {
            return out;
        }
        let len = u16::from_be_bytes([rec[off], rec[off + 1]]) as usize;
        off += 2;
        if off + len > rec.len() {
            return out;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&rec[off..off + len]);
        off += len;
    }
    out
}

fn decode_flv_avc_tags_to_frames(video_tags: &[Vec<u8>]) -> Vec<VideoFrame> {
    let mut dec = SwH264Decoder::new();
    let mut frames = Vec::new();
    for (idx, tag) in video_tags.iter().enumerate() {
        if tag.len() < 5 {
            continue;
        }
        let pts = PtsUs((idx as u64) * 33_333);
        let pkt = if tag[1] == 0x00 {
            let annexb = avc_config_record_to_annexb(&tag[5..]);
            if annexb.is_empty() {
                continue;
            }
            EncodedPacket {
                codec: CodecKind::H264,
                pts,
                dts: pts,
                is_keyframe: true,
                is_sequence_header: true,
                data: annexb,
            }
        } else if tag[1] == 0x01 {
            let annexb = avcc_payload_to_annexb(&tag[5..]);
            if annexb.is_empty() {
                continue;
            }
            EncodedPacket {
                codec: CodecKind::H264,
                pts,
                dts: pts,
                is_keyframe: (tag[0] & 0xF0) == 0x10,
                is_sequence_header: false,
                data: annexb,
            }
        } else {
            continue;
        };
        dec.push_packet(&pkt).unwrap();
        while let Some(frame) = dec.pull_frame().unwrap() {
            frames.push(frame);
        }
    }
    frames
}

fn nv12_pixel_rgb(frame: &VideoFrame, x: u32, y: u32) -> [u8; 3] {
    let NativeBufferHandle::CpuFallback { data, stride } = &frame.handle else {
        panic!("expected CpuFallback NV12 buffer");
    };
    let s = (*stride as usize).max(frame.width as usize);
    let y_size = s * (frame.height as usize);
    let xi = (x as usize).min((frame.width as usize).saturating_sub(1));
    let yi = (y as usize).min((frame.height as usize).saturating_sub(1));
    let y_val = data[yi * s + xi] as f32;
    let uv_row = y_size + (yi / 2) * s;
    let uv_col = (xi / 2) * 2;
    let cb = data[uv_row + uv_col] as f32;
    let cr = data[uv_row + uv_col + 1] as f32;

    let yn = (y_val - 16.0) * (255.0 / 219.0);
    let cbn = (cb - 128.0) * (255.0 / 224.0);
    let crn = (cr - 128.0) * (255.0 / 224.0);

    let r = (yn + 1.5748 * crn).clamp(0.0, 255.0) as u8;
    let g = (yn - 0.187324 * cbn - 0.468124 * crn).clamp(0.0, 255.0) as u8;
    let b = (yn + 1.8556 * cbn).clamp(0.0, 255.0) as u8;
    [r, g, b]
}

#[test]
fn test_engine_virtual_av_pipeline_compositor_and_mixer_verification() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let records = Arc::new(Mutex::new(Vec::new()));
    let rec_clone = Arc::clone(&records);
    let stop_srv = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop_srv);
    listener.set_nonblocking(true).unwrap();

    let srv_thread = thread::spawn(move || {
        while !stop_clone.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((s, _)) => handle_rtmp_session(s, Arc::clone(&rec_clone), ServerBehavior::Normal),
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
    });

    let width = 64u32;
    let height = 64u32;
    let mut clock = MasterClock::new();
    let mut mixer = AudioMixer::new(clock.clone());
    let (ch0, mut prod0) = mixer.add_channel(AudioInputConfig {
        name: "StereoTone".into(),
        sample_rate: 48_000,
        channels: 2,
        initial_gain: 1.0,
        initial_muted: false,
        initial_delay_ms: 0,
    });
    let (ch1, mut prod1) = mixer.add_channel(AudioInputConfig {
        name: "SubTone".into(),
        sample_rate: 48_000,
        channels: 2,
        initial_gain: 1.0,
        initial_muted: false,
        initial_delay_ms: 0,
    });
    let bridge = EngineControlBridge::new(
        vec![ch0, ch1],
        mixer.master_handle(),
        1_200_000,
        30.0,
        TlsRootConfig::System,
    );
    let mut server = EmbeddedServer::start(
        ServerConfig {
            bind_addr: "127.0.0.1:0".into(),
            meter_interval_ms: 25,
            preview_width: width,
            preview_height: height,
            preview_fps: 30,
        },
        bridge.clone(),
    )
    .expect("EmbeddedServer");
    let gpu = GpuContext::new_headless().expect("GpuContext");
    let mut comp = GpuCompositor::new(
        gpu,
        CompositorConfig {
            main_width: width,
            main_height: height,
            preview_width: width,
            preview_height: height,
            target_fps: 30,
        },
    );
    let mut main_enc = SwH264Encoder::new(width, height, 1_200_000);
    let mut prev_enc = SwH264Encoder::new(width, height, 400_000);
    let (mut aud_enc, captured_pcm) = CaptureAudioEncoder::new();

    let video_base = VirtualVideoSource::new(width, height, VirtualVideoPattern::default())
        .with_bar_speed(8);
    let video_overlay =
        VirtualVideoSource::new(width, height, VirtualVideoPattern::Solid([235, 30, 210, 255]));
    let overlay_rgba = video_overlay.render_rgba(0);

    let mut aud_src0 = VirtualAudioSource::new(
        48_000,
        VirtualAudioSignal::DualChannelTone {
            left_freq_hz: 440.0,
            right_freq_hz: 880.0,
            amplitude: 0.40,
        },
    );
    let mut aud_src1 = VirtualAudioSource::new(
        48_000,
        VirtualAudioSignal::SineTone {
            freq_hz: 1200.0,
            amplitude: 0.35,
        },
    );

    // Phase A: Base 4-quadrant NV12 + hidden overlay + ch0 dual-tone active (ch1 muted)
    bridge.set_audio_mute(1, true).unwrap();
    bridge
        .set_overlay_transform(
            1,
            LayerTransform {
                x: 0.5,
                y: 0.5,
                scale_x: 0.45,
                scale_y: 0.45,
                visible: false,
                ..Default::default()
            },
        )
        .unwrap();
    bridge
        .start_stream(
            &format!("rtmp://127.0.0.1:{}/live/virtual_av", port),
            Some(1_200_000),
        )
        .unwrap();

    let mut pump_av_frames = |start_idx: u64, count: u64| {
        for i in start_idx..(start_idx + count) {
            let pts = PtsUs(i * 33_333);
            let (y_plane, uv_plane) = video_base.render_nv12_bt709(i);
            let base_layer = comp
                .create_nv12_layer(
                    0,
                    true,
                    false,
                    ColorSpace::Bt709Limited,
                    width,
                    height,
                    &y_plane,
                    &uv_plane,
                    bridge.layer_transform(0),
                )
                .unwrap();
            let overlay_layer = comp
                .create_rgba_layer(
                    1,
                    false,
                    false,
                    width,
                    height,
                    &overlay_rgba,
                    bridge.layer_transform(1),
                )
                .unwrap();
            comp.render_dual(&[&base_layer, &overlay_layer], pts).unwrap();

            let composited_rgba =
                comp.readback_texture_rgba(comp.main_texture(), width, height).unwrap();
            main_enc.encode_rgba(&composited_rgba, pts).unwrap();
            prev_enc.encode_rgba(&composited_rgba, pts).unwrap();

            let blocks = VirtualAudioSource::mix_blocks_for_30fps_frame(i);
            for _ in 0..blocks {
                let pcm0 = aud_src0.next_stereo_f32(MIX_BLOCK_FRAMES);
                let pcm1 = aud_src1.next_stereo_f32(MIX_BLOCK_FRAMES);
                prod0.push_f32(&pcm0);
                prod1.push_f32(&pcm1);
                let mut mix_buf = vec![0.0f32; MIX_BLOCK_FRAMES * 2];
                let st = mixer.mix_block(&mut mix_buf);
                aud_enc.push_pcm_f32(&mix_buf, st.to_pts_us()).unwrap();
            }

            let mut tick = PipelineTickContext {
                clock: &mut clock,
                bridge: &bridge,
                compositor: &mut comp,
                main_encoder: &mut main_enc,
                preview_encoder: &mut prev_enc,
                audio_encoder: &mut aud_enc,
                server: &server,
            };
            flush_encoders_to_outputs(&mut tick, pts).unwrap();
            thread::sleep(Duration::from_millis(5));
        }
    };

    pump_av_frames(0, 6);
    let pcm_phase_a_end = captured_pcm.lock().unwrap().len();

    // Phase B: Reveal magenta overlay on Top-Right quadrant, attenuate ch0 (-12dB), unmute ch1 (1200Hz)
    bridge
        .set_overlay_transform(
            1,
            LayerTransform {
                x: 0.5,
                y: 0.5,
                scale_x: 0.45,
                scale_y: 0.45,
                visible: true,
                z_order: 1,
                ..Default::default()
            },
        )
        .unwrap();
    bridge.set_audio_gain(0, -12.0).unwrap();
    bridge.set_audio_mute(1, false).unwrap();
    pump_av_frames(6, 6);
    let pcm_phase_b_end = captured_pcm.lock().unwrap().len();

    // Phase C: Mute both audio channels & hide overlay again
    bridge.set_audio_mute(0, true).unwrap();
    bridge.set_audio_mute(1, true).unwrap();
    bridge
        .set_overlay_transform(
            1,
            LayerTransform {
                x: 0.5,
                y: 0.5,
                scale_x: 0.45,
                scale_y: 0.45,
                visible: false,
                ..Default::default()
            },
        )
        .unwrap();
    pump_av_frames(12, 4);

    bridge.stop_stream().unwrap();
    bridge.stop_all();
    server.stop();
    stop_srv.store(true, Ordering::Relaxed);
    srv_thread.join().unwrap();

    // 1. Decode post-RTMP delivered video frames back via SwH264Decoder and verify colors, overlay, and motion
    let recs = records.lock().unwrap();
    assert_eq!(recs.len(), 1);
    let frames = decode_flv_avc_tags_to_frames(&recs[0].video_tags);
    assert!(
        frames.len() >= 15,
        "expected >= 15 decoded RTMP frames, got {}",
        frames.len()
    );

    // Phase A frame 0: Red TL, Green TR, Blue BL, Yellow BR
    let f0 = &frames[0];
    let tl0 = nv12_pixel_rgb(f0, 12, 12);
    let tr0 = nv12_pixel_rgb(f0, 48, 12);
    let bl0 = nv12_pixel_rgb(f0, 12, 50);
    let br0 = nv12_pixel_rgb(f0, 48, 50);
    assert!(tl0[0] > 170 && tl0[1] < 80 && tl0[2] < 80, "TL expected Red, got {:?}", tl0);
    assert!(tr0[1] > 170 && tr0[0] < 80 && tr0[2] < 80, "TR expected Green, got {:?}", tr0);
    assert!(bl0[2] > 170 && bl0[0] < 80 && bl0[1] < 95, "BL expected Blue, got {:?}", bl0);
    assert!(br0[0] > 170 && br0[1] > 160 && br0[2] < 80, "BR expected Yellow, got {:?}", br0);

    // Moving bar verification between frame 0 (bar_x = 0..16) and frame 4 (bar_x = 32..48)
    // `y = height / 2 - 2` lies inside the moving bar vertically while strictly remaining in the Red/Green top half.
    let bar_y = height / 2 - 2;
    let bar0_left = nv12_pixel_rgb(&frames[0], 8, bar_y);
    let bar0_mid = nv12_pixel_rgb(&frames[0], 40, bar_y);
    let bar4_left = nv12_pixel_rgb(&frames[4], 8, bar_y);
    let bar4_mid = nv12_pixel_rgb(&frames[4], 40, bar_y);
    assert!(
        bar0_left[0] > 190 && bar0_left[1] > 190 && bar0_left[2] > 190,
        "frame 0 bar at x=8 should be bright white, got {:?}",
        bar0_left
    );
    assert!(
        bar0_mid[1] > 160 && bar0_mid[0] < 80,
        "frame 0 at x=40 should still be Green bg, got {:?}",
        bar0_mid
    );
    assert!(
        bar4_left[0] > 170 && bar4_left[1] < 80,
        "frame 4 at x=8 should return to Red bg, got {:?}",
        bar4_left
    );
    assert!(
        bar4_mid[0] > 190 && bar4_mid[1] > 190 && bar4_mid[2] > 190,
        "frame 4 bar at x=40 should be bright white, got {:?}",
        bar4_mid
    );

    // Phase B frame 7: Top-Right quadrant (48, 16) is now Magenta [235, 30, 210] while TL remains Red
    let f7 = &frames[7];
    let tr7 = nv12_pixel_rgb(f7, 48, 16);
    let tl7 = nv12_pixel_rgb(f7, 12, 12);
    assert!(
        tr7[0] > 175 && tr7[1] < 85 && tr7[2] > 155,
        "Phase B TR expected Magenta overlay, got {:?}",
        tr7
    );
    assert!(tl7[0] > 170 && tl7[1] < 80, "Phase B TL should remain Red, got {:?}", tl7);

    // Phase C frame 13: Top-Right returns to Green once overlay hidden
    let tr13 = nv12_pixel_rgb(&frames[13], 48, 16);
    assert!(
        tr13[1] > 170 && tr13[0] < 80 && tr13[2] < 80,
        "Phase C TR should revert to Green after visible=false, got {:?}",
        tr13
    );

    // 2. Verify post-mixer audio frequencies, stereo L/R isolation, -12dB gain, 2-ch mix, and mute
    let all_pcm = captured_pcm.lock().unwrap();
    let pcm_a = &all_pcm[..pcm_phase_a_end];
    let pcm_b = &all_pcm[pcm_phase_a_end..pcm_phase_b_end];
    let pcm_c = &all_pcm[pcm_phase_b_end..];

    let l_440_a = goertzel_magnitude(pcm_a, 2, 0, 48_000.0, 440.0);
    let l_880_a = goertzel_magnitude(pcm_a, 2, 0, 48_000.0, 880.0);
    let r_880_a = goertzel_magnitude(pcm_a, 2, 1, 48_000.0, 880.0);
    let r_440_a = goertzel_magnitude(pcm_a, 2, 1, 48_000.0, 440.0);
    let l_1200_a = goertzel_magnitude(pcm_a, 2, 0, 48_000.0, 1200.0);
    assert!(
        l_440_a > 0.25 && l_440_a > l_880_a * 4.0,
        "Phase A Left should dominate at 440Hz (440={}, 880={})",
        l_440_a,
        l_880_a
    );
    assert!(
        r_880_a > 0.25 && r_880_a > r_440_a * 4.0,
        "Phase A Right should dominate at 880Hz (880={}, 440={})",
        r_880_a,
        r_440_a
    );
    assert!(l_1200_a < 0.03, "Phase A ch1 (1200Hz) should be muted, got {}", l_1200_a);

    let l_440_b = goertzel_magnitude(pcm_b, 2, 0, 48_000.0, 440.0);
    let l_1200_b = goertzel_magnitude(pcm_b, 2, 0, 48_000.0, 1200.0);
    assert!(
        l_440_b < l_440_a * 0.45 && l_440_b > 0.05,
        "Phase B ch0 440Hz should be attenuated by ~-12dB (A={}, B={})",
        l_440_a,
        l_440_b
    );
    assert!(
        l_1200_b > 0.20,
        "Phase B unmuted ch1 1200Hz should mix in clearly, got {}",
        l_1200_b
    );

    let rms_c_l = channel_rms_dbfs(pcm_c, 2, 0);
    let rms_c_r = channel_rms_dbfs(pcm_c, 2, 1);
    assert!(
        rms_c_l < -60.0 && rms_c_r < -60.0,
        "Phase C muted channels should be silent (L={} dBFS, R={} dBFS)",
        rms_c_l,
        rms_c_r
    );
}
