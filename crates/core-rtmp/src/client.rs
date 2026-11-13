use crate::amf0::{decode_all, encode_values, Amf0Value};
use crate::chunk::{
    ChunkReader, ChunkWriter, RtmpMessage, CSID_AUDIO, CSID_COMMAND, CSID_DATA,
    CSID_PROTOCOL_CONTROL, CSID_VIDEO, MSG_ACK, MSG_AUDIO, MSG_COMMAND_AMF0, MSG_DATA_AMF0,
    MSG_SET_CHUNK_SIZE, MSG_SET_PEER_BANDWIDTH, MSG_USER_CONTROL, MSG_VIDEO, MSG_WINDOW_ACK_SIZE,
};
use crate::flv::{build_metadata_payload, pack_audio_tag, pack_video_tag};
use crate::handshake::perform_client_handshake;
use crate::transport::{RtmpTransport, RtmpUrl, TlsRootConfig};
use core_types::media::{CodecKind, EncodedPacket};
use core_types::{CoreError, CoreResult};
use std::io::{ErrorKind, Write};
use std::time::Duration;

const OUT_CHUNK_SIZE: u32 = 4096;

#[derive(Debug, Clone)]
pub struct StreamMetadata {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub video_bitrate_bps: u32,
    pub audio_bitrate_bps: u32,
}

impl Default for StreamMetadata {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60.0,
            video_bitrate_bps: 4_000_000,
            audio_bitrate_bps: 128_000,
        }
    }
}

pub struct RtmpClient {
    transport: RtmpTransport,
    chunk_writer: ChunkWriter,
    chunk_reader: ChunkReader,
    stream_id: u32,
    next_txn_id: f64,
    window_ack_size: u64,
    last_ack_sent: u64,
}

impl RtmpClient {
    pub fn connect_and_publish(
        url: &RtmpUrl,
        metadata: &StreamMetadata,
        tls_cfg: &TlsRootConfig,
    ) -> CoreResult<Self> {
        let mut transport = RtmpTransport::connect(
            url,
            tls_cfg,
            Duration::from_secs(10),
            Duration::from_secs(10),
        )?;

        perform_client_handshake(&mut transport)?;

        let mut client = Self {
            transport,
            chunk_writer: ChunkWriter::new(),
            chunk_reader: ChunkReader::new(),
            stream_id: 0,
            next_txn_id: 1.0,
            window_ack_size: 2_500_000,
            last_ack_sent: 0,
        };

        // SetChunkSize = 4096
        client.send_set_chunk_size(OUT_CHUNK_SIZE)?;
        client.chunk_writer.set_chunk_size(OUT_CHUNK_SIZE as usize);

        // 1. connect
        let connect_txn = client.alloc_txn_id();
        let connect_obj = Amf0Value::Object(vec![
            ("app".into(), Amf0Value::String(url.app.clone())),
            ("type".into(), Amf0Value::String("nonprivate".into())),
            ("flashVer".into(), Amf0Value::String("FMLE/3.0 (compatible; ikakim)".into())),
            ("tcUrl".into(), Amf0Value::String(url.tc_url.clone())),
            ("fpad".into(), Amf0Value::Boolean(false)),
            ("capabilities".into(), Amf0Value::Number(15.0)),
            ("audioCodecs".into(), Amf0Value::Number(0x0400 as f64)), // AAC
            ("videoCodecs".into(), Amf0Value::Number(0x0080 as f64)), // H.264
            ("videoFunction".into(), Amf0Value::Number(1.0)),
            ("objectEncoding".into(), Amf0Value::Number(0.0)),
        ]);
        client.send_command(
            CSID_COMMAND,
            0,
            &[
                Amf0Value::String("connect".into()),
                Amf0Value::Number(connect_txn),
                connect_obj,
            ],
        )?;
        client.wait_for_txn_result(connect_txn)?;

        // 2. releaseStream & FCPublish (best-effort; 一部サーバーのみ応答)
        let release_txn = client.alloc_txn_id();
        client.send_command(
            CSID_COMMAND,
            0,
            &[
                Amf0Value::String("releaseStream".into()),
                Amf0Value::Number(release_txn),
                Amf0Value::Null,
                Amf0Value::String(url.stream_key.clone()),
            ],
        )?;
        let fcpublish_txn = client.alloc_txn_id();
        client.send_command(
            CSID_COMMAND,
            0,
            &[
                Amf0Value::String("FCPublish".into()),
                Amf0Value::Number(fcpublish_txn),
                Amf0Value::Null,
                Amf0Value::String(url.stream_key.clone()),
            ],
        )?;

        // 3. createStream
        let create_txn = client.alloc_txn_id();
        client.send_command(
            CSID_COMMAND,
            0,
            &[
                Amf0Value::String("createStream".into()),
                Amf0Value::Number(create_txn),
                Amf0Value::Null,
            ],
        )?;
        let resp = client.wait_for_txn_result(create_txn)?;
        let stream_id = resp
            .get(3)
            .and_then(|v| v.as_f64())
            .map(|n| n as u32)
            .filter(|&id| id > 0)
            .unwrap_or(1);
        client.stream_id = stream_id;

        // 4. publish
        let publish_txn = client.alloc_txn_id();
        client.send_command(
            8,
            stream_id,
            &[
                Amf0Value::String("publish".into()),
                Amf0Value::Number(publish_txn),
                Amf0Value::Null,
                Amf0Value::String(url.stream_key.clone()),
                Amf0Value::String("live".into()),
            ],
        )?;
        client.wait_for_publish_start()?;

        // 5. @setDataFrame onMetaData
        let meta_payload = build_metadata_payload(metadata);
        client.chunk_writer.write_message(
            &mut client.transport,
            &RtmpMessage {
                csid: CSID_DATA,
                timestamp_ms: 0,
                msg_type: MSG_DATA_AMF0,
                stream_id,
                payload: meta_payload,
            },
        )?;
        client
            .transport
            .flush()
            .map_err(|e| CoreError::Network(format!("RTMP flush error: {}", e)))?;

        // 配信中のソケット受信ポーリングをノンブロッキング寄りの短タイムアウトに設定
        client
            .transport
            .set_read_timeout(Some(Duration::from_millis(5)));

        Ok(client)
    }

    fn alloc_txn_id(&mut self) -> f64 {
        let id = self.next_txn_id;
        self.next_txn_id += 1.0;
        id
    }

    fn send_set_chunk_size(&mut self, size: u32) -> CoreResult<()> {
        let payload = (size & 0x7FFF_FFFF).to_be_bytes().to_vec();
        self.chunk_writer.write_message(
            &mut self.transport,
            &RtmpMessage {
                csid: CSID_PROTOCOL_CONTROL,
                timestamp_ms: 0,
                msg_type: MSG_SET_CHUNK_SIZE,
                stream_id: 0,
                payload,
            },
        )?;
        Ok(())
    }

    fn send_ack(&mut self, total_bytes: u32) -> CoreResult<()> {
        self.chunk_writer.write_message(
            &mut self.transport,
            &RtmpMessage {
                csid: CSID_PROTOCOL_CONTROL,
                timestamp_ms: 0,
                msg_type: MSG_ACK,
                stream_id: 0,
                payload: total_bytes.to_be_bytes().to_vec(),
            },
        )?;
        Ok(())
    }

    fn send_command(&mut self, csid: u32, stream_id: u32, args: &[Amf0Value]) -> CoreResult<()> {
        let payload = encode_values(args);
        self.chunk_writer.write_message(
            &mut self.transport,
            &RtmpMessage {
                csid,
                timestamp_ms: 0,
                msg_type: MSG_COMMAND_AMF0,
                stream_id,
                payload,
            },
        )?;
        self.transport
            .flush()
            .map_err(|e| CoreError::Network(format!("RTMP flush command error: {}", e)))
    }

    fn handle_control_message(&mut self, msg: &RtmpMessage) -> CoreResult<()> {
        match msg.msg_type {
            MSG_WINDOW_ACK_SIZE if msg.payload.len() >= 4 => {
                self.window_ack_size =
                    u32::from_be_bytes([msg.payload[0], msg.payload[1], msg.payload[2], msg.payload[3]])
                        as u64;
            }
            MSG_SET_PEER_BANDWIDTH if msg.payload.len() >= 4 => {
                let win =
                    u32::from_be_bytes([msg.payload[0], msg.payload[1], msg.payload[2], msg.payload[3]]);
                self.chunk_writer.write_message(
                    &mut self.transport,
                    &RtmpMessage {
                        csid: CSID_PROTOCOL_CONTROL,
                        timestamp_ms: 0,
                        msg_type: MSG_WINDOW_ACK_SIZE,
                        stream_id: 0,
                        payload: win.to_be_bytes().to_vec(),
                    },
                )?;
                let _ = self.transport.flush();
            }
            MSG_USER_CONTROL if msg.payload.len() >= 6 => {
                let event_type = u16::from_be_bytes([msg.payload[0], msg.payload[1]]);
                if event_type == 6 {
                    // PingRequest -> PingResponse (event_type = 7)
                    let mut pong = msg.payload[..6].to_vec();
                    pong[0] = 0;
                    pong[1] = 7;
                    self.chunk_writer.write_message(
                        &mut self.transport,
                        &RtmpMessage {
                            csid: CSID_PROTOCOL_CONTROL,
                            timestamp_ms: 0,
                            msg_type: MSG_USER_CONTROL,
                            stream_id: 0,
                            payload: pong,
                        },
                    )?;
                    let _ = self.transport.flush();
                }
            }
            _ => {}
        }

        let rx = self.chunk_reader.bytes_received();
        if self.window_ack_size > 0 && rx.saturating_sub(self.last_ack_sent) >= self.window_ack_size {
            self.last_ack_sent = rx;
            self.send_ack(rx as u32)?;
            let _ = self.transport.flush();
        }
        Ok(())
    }

    fn wait_for_txn_result(&mut self, expected_txn: f64) -> CoreResult<Vec<Amf0Value>> {
        for _ in 0..64 {
            let msg = self.chunk_reader.read_message(&mut self.transport)?;
            self.handle_control_message(&msg)?;
            if msg.msg_type == MSG_COMMAND_AMF0 {
                let vals = decode_all(&msg.payload)?;
                let cmd = vals.first().and_then(|v| v.as_str()).unwrap_or("");
                let txn = vals.get(1).and_then(|v| v.as_f64()).unwrap_or(-1.0);
                if cmd == "_result" && (txn - expected_txn).abs() < 0.01 {
                    return Ok(vals);
                }
                if cmd == "_error" && (txn - expected_txn).abs() < 0.01 {
                    return Err(CoreError::Network(format!("RTMP _error: {:?}", vals)));
                }
            }
        }
        Err(CoreError::Network(format!(
            "RTMP timeout waiting for _result txn={}",
            expected_txn
        )))
    }

    fn wait_for_publish_start(&mut self) -> CoreResult<()> {
        for _ in 0..64 {
            let msg = self.chunk_reader.read_message(&mut self.transport)?;
            self.handle_control_message(&msg)?;
            if msg.msg_type == MSG_COMMAND_AMF0 {
                let vals = decode_all(&msg.payload)?;
                let cmd = vals.first().and_then(|v| v.as_str()).unwrap_or("");
                if cmd == "onStatus" {
                    let info = vals.get(3);
                    let code = info
                        .and_then(|o| o.get("code"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let level = info
                        .and_then(|o| o.get("level"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if code == "NetStream.Publish.Start" {
                        return Ok(());
                    }
                    if level == "error" {
                        return Err(CoreError::Network(format!("RTMP publish failed: {}", code)));
                    }
                } else if cmd == "_error" {
                    return Err(CoreError::Network(format!("RTMP publish _error: {:?}", vals)));
                }
            }
        }
        Err(CoreError::Network(
            "RTMP timeout waiting for NetStream.Publish.Start".into(),
        ))
    }

    /// 映像（H.264）または音声（AAC-LC）パケットを RTMP チャンクとして送信する。
    /// 送信したワイヤバイト数を返す。
    pub fn send_packet(&mut self, packet: &EncodedPacket) -> CoreResult<usize> {
        let ts_ms = packet.dts.to_timestamp_ms().0;
        let (csid, msg_type, payload) = match packet.codec {
            CodecKind::H264 => (CSID_VIDEO, MSG_VIDEO, pack_video_tag(packet)?),
            CodecKind::AacLc => (CSID_AUDIO, MSG_AUDIO, pack_audio_tag(packet)?),
            other => {
                return Err(CoreError::UnsupportedFormat(format!(
                    "RTMP does not support codec {:?}",
                    other
                )));
            }
        };

        let written = self.chunk_writer.write_message(
            &mut self.transport,
            &RtmpMessage {
                csid,
                timestamp_ms: ts_ms,
                msg_type,
                stream_id: self.stream_id,
                payload,
            },
        )?;
        self.transport
            .flush()
            .map_err(|e| CoreError::Network(format!("RTMP flush media error: {}", e)))?;
        Ok(written)
    }

    /// ノンブロッキング/短タイムアウトでサーバーからの制御メッセージ（Ping/Ack/切断通知等）を処理する
    pub fn poll_inbound(&mut self) -> CoreResult<()> {
        match self.chunk_reader.read_message(&mut self.transport) {
            Ok(msg) => {
                self.handle_control_message(&msg)?;
                if msg.msg_type == MSG_COMMAND_AMF0 {
                    if let Ok(vals) = decode_all(&msg.payload) {
                        let cmd = vals.first().and_then(|v| v.as_str()).unwrap_or("");
                        if cmd == "onStatus" {
                            let info = vals.get(3);
                            let level = info
                                .and_then(|o| o.get("level"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let code = info
                                .and_then(|o| o.get("code"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            if level == "error"
                                || code == "NetStream.Unpublish.Success"
                                || code == "NetConnection.Connect.Closed"
                            {
                                return Err(CoreError::Network(format!(
                                    "RTMP server closed stream: {} ({})",
                                    code, level
                                )));
                            }
                        } else if cmd == "_error" || cmd == "close" {
                            return Err(CoreError::Network(format!(
                                "RTMP server command {}: {:?}",
                                cmd, vals
                            )));
                        }
                    }
                }
                Ok(())
            }
            Err(CoreError::Network(ref s))
                if s.contains("TimedOut")
                    || s.contains("WouldBlock")
                    || s.contains("timed out")
                    || s.contains("Resource temporarily unavailable") =>
            {
                Ok(())
            }
            Err(e) => {
                // io::ErrorKind::WouldBlock / TimedOut が文字列表現以外の場合の安全策
                let _ = ErrorKind::TimedOut;
                Err(e)
            }
        }
    }

    pub fn close(&mut self) {
        let _ = self.send_command(
            CSID_COMMAND,
            0,
            &[
                Amf0Value::String("deleteStream".into()),
                Amf0Value::Number(0.0),
                Amf0Value::Null,
                Amf0Value::Number(self.stream_id as f64),
            ],
        );
    }
}
