use core_types::{CoreError, CoreResult};
use std::collections::HashMap;
use std::io::{Read, Write};

pub const DEFAULT_CHUNK_SIZE: usize = 128;

pub const MSG_SET_CHUNK_SIZE: u8 = 1;
pub const MSG_ABORT: u8 = 2;
pub const MSG_ACK: u8 = 3;
pub const MSG_USER_CONTROL: u8 = 4;
pub const MSG_WINDOW_ACK_SIZE: u8 = 5;
pub const MSG_SET_PEER_BANDWIDTH: u8 = 6;
pub const MSG_AUDIO: u8 = 8;
pub const MSG_VIDEO: u8 = 9;
pub const MSG_DATA_AMF0: u8 = 18;
pub const MSG_COMMAND_AMF0: u8 = 20;

pub const CSID_PROTOCOL_CONTROL: u32 = 2;
pub const CSID_COMMAND: u32 = 3;
pub const CSID_AUDIO: u32 = 4;
pub const CSID_VIDEO: u32 = 6;
pub const CSID_DATA: u32 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtmpMessage {
    pub csid: u32,
    pub timestamp_ms: u32,
    pub msg_type: u8,
    pub stream_id: u32,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone)]
struct OutPrevHeader {
    timestamp_ms: u32,
    stream_id: u32,
}

pub struct ChunkWriter {
    chunk_size: usize,
    prev_headers: HashMap<u32, OutPrevHeader>,
}

impl ChunkWriter {
    pub fn new() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            prev_headers: HashMap::new(),
        }
    }

    pub fn set_chunk_size(&mut self, size: usize) {
        self.chunk_size = size.max(1);
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    pub fn write_message<W: Write>(&mut self, writer: &mut W, msg: &RtmpMessage) -> CoreResult<usize> {
        let payload_len = msg.payload.len() as u32;
        let mut total_written = 0usize;
        let mut offset = 0usize;
        let mut msg_ext_ts: Option<u32> = None;

        while offset < msg.payload.len() || (msg.payload.is_empty() && offset == 0) {
            let chunk_len = (msg.payload.len() - offset).min(self.chunk_size);
            let mut hdr = [0u8; 18];
            let mut hdr_len: usize;

            if offset == 0 {
                // Fmt 0 または Fmt 1 で送信
                let use_fmt1 = if let Some(prev) = self.prev_headers.get(&msg.csid) {
                    prev.stream_id == msg.stream_id && msg.timestamp_ms >= prev.timestamp_ms
                } else {
                    false
                };

                if use_fmt1 {
                    let prev = &self.prev_headers[&msg.csid];
                    let delta = msg.timestamp_ms.wrapping_sub(prev.timestamp_ms);
                    hdr_len = encode_basic_header_buf(1, msg.csid, &mut hdr);
                    let ts_field = if delta >= 0x00FF_FFFF { 0x00FF_FFFF } else { delta };
                    hdr[hdr_len..hdr_len + 3].copy_from_slice(&ts_field.to_be_bytes()[1..4]);
                    hdr[hdr_len + 3..hdr_len + 6].copy_from_slice(&payload_len.to_be_bytes()[1..4]);
                    hdr[hdr_len + 6] = msg.msg_type;
                    hdr_len += 7;
                    if delta >= 0x00FF_FFFF {
                        hdr[hdr_len..hdr_len + 4].copy_from_slice(&delta.to_be_bytes());
                        hdr_len += 4;
                        msg_ext_ts = Some(delta);
                    }
                    self.prev_headers.insert(
                        msg.csid,
                        OutPrevHeader {
                            timestamp_ms: msg.timestamp_ms,
                            stream_id: msg.stream_id,
                        },
                    );
                } else {
                    hdr_len = encode_basic_header_buf(0, msg.csid, &mut hdr);
                    let ts_field = if msg.timestamp_ms >= 0x00FF_FFFF {
                        0x00FF_FFFF
                    } else {
                        msg.timestamp_ms
                    };
                    hdr[hdr_len..hdr_len + 3].copy_from_slice(&ts_field.to_be_bytes()[1..4]);
                    hdr[hdr_len + 3..hdr_len + 6].copy_from_slice(&payload_len.to_be_bytes()[1..4]);
                    hdr[hdr_len + 6] = msg.msg_type;
                    hdr[hdr_len + 7..hdr_len + 11].copy_from_slice(&msg.stream_id.to_le_bytes());
                    hdr_len += 11;
                    if msg.timestamp_ms >= 0x00FF_FFFF {
                        hdr[hdr_len..hdr_len + 4].copy_from_slice(&msg.timestamp_ms.to_be_bytes());
                        hdr_len += 4;
                        msg_ext_ts = Some(msg.timestamp_ms);
                    }
                    self.prev_headers.insert(
                        msg.csid,
                        OutPrevHeader {
                            timestamp_ms: msg.timestamp_ms,
                            stream_id: msg.stream_id,
                        },
                    );
                }
            } else {
                // Fmt 3 continuation: 先頭チャンク (offset == 0) が拡張タイムスタンプを伴った場合のみ再送
                hdr_len = encode_basic_header_buf(3, msg.csid, &mut hdr);
                if let Some(ext_ts) = msg_ext_ts {
                    hdr[hdr_len..hdr_len + 4].copy_from_slice(&ext_ts.to_be_bytes());
                    hdr_len += 4;
                }
            }

            writer
                .write_all(&hdr[..hdr_len])
                .map_err(|e| CoreError::Network(format!("RTMP write header error: {}", e)))?;
            total_written += hdr_len;

            if chunk_len > 0 {
                writer
                    .write_all(&msg.payload[offset..offset + chunk_len])
                    .map_err(|e| CoreError::Network(format!("RTMP write payload error: {}", e)))?;
                total_written += chunk_len;
            }
            offset += chunk_len;
            if msg.payload.is_empty() {
                break;
            }
        }

        Ok(total_written)
    }
}

impl Default for ChunkWriter {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn encode_basic_header_buf(fmt: u8, csid: u32, out: &mut [u8; 18]) -> usize {
    let f = (fmt & 0x03) << 6;
    if csid <= 63 {
        out[0] = f | (csid as u8);
        1
    } else if csid <= 319 {
        out[0] = f;
        out[1] = (csid - 64) as u8;
        2
    } else {
        out[0] = f | 1;
        let val = (csid - 64) as u16;
        out[1] = (val & 0xFF) as u8;
        out[2] = ((val >> 8) & 0xFF) as u8;
        3
    }
}

#[derive(Debug, Clone, Default)]
struct InChunkState {
    timestamp_ms: u32,
    timestamp_delta: u32,
    has_ext_ts: bool,
    msg_len: usize,
    msg_type: u8,
    stream_id: u32,
    buf: Vec<u8>,
}

pub struct ChunkReader {
    chunk_size: usize,
    streams: HashMap<u32, InChunkState>,
    bytes_received: u64,
}

impl ChunkReader {
    pub fn new() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            streams: HashMap::new(),
            bytes_received: 0,
        }
    }

    pub fn set_chunk_size(&mut self, size: usize) {
        self.chunk_size = size.max(1);
    }

    pub fn bytes_received(&self) -> u64 {
        self.bytes_received
    }

    fn read_exact_counted<R: Read>(
        bytes_received: &mut u64,
        reader: &mut R,
        buf: &mut [u8],
    ) -> std::io::Result<()> {
        reader.read_exact(buf)?;
        *bytes_received += buf.len() as u64;
        Ok(())
    }

    pub fn read_message<R: Read>(&mut self, reader: &mut R) -> CoreResult<RtmpMessage> {
        loop {
            let mut b0 = [0u8; 1];
            Self::read_exact_counted(&mut self.bytes_received, reader, &mut b0)
                .map_err(|e| CoreError::Network(format!("RTMP read chunk header error: {}", e)))?;

            let fmt = (b0[0] >> 6) & 0x03;
            let csid_low = (b0[0] & 0x3F) as u32;
            let csid = match csid_low {
                0 => {
                    let mut b1 = [0u8; 1];
                    Self::read_exact_counted(&mut self.bytes_received, reader, &mut b1)
                        .map_err(|e| CoreError::Network(format!("RTMP read csid2 error: {}", e)))?;
                    64 + b1[0] as u32
                }
                1 => {
                    let mut b12 = [0u8; 2];
                    Self::read_exact_counted(&mut self.bytes_received, reader, &mut b12)
                        .map_err(|e| CoreError::Network(format!("RTMP read csid3 error: {}", e)))?;
                    64 + b12[0] as u32 + ((b12[1] as u32) << 8)
                }
                id => id,
            };

            let bytes_received = &mut self.bytes_received;
            let state = self.streams.entry(csid).or_default();

            match fmt {
                0 => {
                    let mut h = [0u8; 11];
                    Self::read_exact_counted(bytes_received, reader, &mut h)
                        .map_err(|e| CoreError::Network(format!("RTMP read fmt0 error: {}", e)))?;
                    let ts = u32::from_be_bytes([0, h[0], h[1], h[2]]);
                    state.msg_len = u32::from_be_bytes([0, h[3], h[4], h[5]]) as usize;
                    state.msg_type = h[6];
                    state.stream_id = u32::from_le_bytes([h[7], h[8], h[9], h[10]]);
                    if ts == 0x00FF_FFFF {
                        let mut ext = [0u8; 4];
                        Self::read_exact_counted(bytes_received, reader, &mut ext).map_err(|e| {
                            CoreError::Network(format!("RTMP read ext ts error: {}", e))
                        })?;
                        state.timestamp_ms = u32::from_be_bytes(ext);
                        state.has_ext_ts = true;
                    } else {
                        state.timestamp_ms = ts;
                        state.has_ext_ts = false;
                    }
                    state.timestamp_delta = 0;
                    state.buf.clear();
                }
                1 => {
                    let mut h = [0u8; 7];
                    Self::read_exact_counted(bytes_received, reader, &mut h)
                        .map_err(|e| CoreError::Network(format!("RTMP read fmt1 error: {}", e)))?;
                    let delta = u32::from_be_bytes([0, h[0], h[1], h[2]]);
                    state.msg_len = u32::from_be_bytes([0, h[3], h[4], h[5]]) as usize;
                    state.msg_type = h[6];
                    if delta == 0x00FF_FFFF {
                        let mut ext = [0u8; 4];
                        Self::read_exact_counted(bytes_received, reader, &mut ext).map_err(|e| {
                            CoreError::Network(format!("RTMP read ext ts error: {}", e))
                        })?;
                        state.timestamp_delta = u32::from_be_bytes(ext);
                        state.has_ext_ts = true;
                    } else {
                        state.timestamp_delta = delta;
                        state.has_ext_ts = false;
                    }
                    state.timestamp_ms = state.timestamp_ms.wrapping_add(state.timestamp_delta);
                    state.buf.clear();
                }
                2 => {
                    let mut h = [0u8; 3];
                    Self::read_exact_counted(bytes_received, reader, &mut h)
                        .map_err(|e| CoreError::Network(format!("RTMP read fmt2 error: {}", e)))?;
                    let delta = u32::from_be_bytes([0, h[0], h[1], h[2]]);
                    if delta == 0x00FF_FFFF {
                        let mut ext = [0u8; 4];
                        Self::read_exact_counted(bytes_received, reader, &mut ext).map_err(|e| {
                            CoreError::Network(format!("RTMP read ext ts error: {}", e))
                        })?;
                        state.timestamp_delta = u32::from_be_bytes(ext);
                        state.has_ext_ts = true;
                    } else {
                        state.timestamp_delta = delta;
                        state.has_ext_ts = false;
                    }
                    state.timestamp_ms = state.timestamp_ms.wrapping_add(state.timestamp_delta);
                    state.buf.clear();
                }
                3 => {
                    if state.buf.is_empty() {
                        // Fmt3 で新メッセージを開始するケース
                        if state.has_ext_ts {
                            let mut ext = [0u8; 4];
                            Self::read_exact_counted(bytes_received, reader, &mut ext).map_err(|e| {
                                CoreError::Network(format!("RTMP read fmt3 ext ts error: {}", e))
                            })?;
                            state.timestamp_delta = u32::from_be_bytes(ext);
                        }
                        state.timestamp_ms = state.timestamp_ms.wrapping_add(state.timestamp_delta);
                    } else if state.has_ext_ts {
                        let mut ext = [0u8; 4];
                        Self::read_exact_counted(bytes_received, reader, &mut ext).map_err(|e| {
                            CoreError::Network(format!("RTMP read fmt3 cont ext ts error: {}", e))
                        })?;
                    }
                }
                _ => unreachable!(),
            }

            let remaining = state.msg_len.saturating_sub(state.buf.len());
            let to_read = remaining.min(self.chunk_size);
            if to_read > 0 {
                let cur_len = state.buf.len();
                state.buf.resize(cur_len + to_read, 0);
                Self::read_exact_counted(bytes_received, reader, &mut state.buf[cur_len..])
                    .map_err(|e| CoreError::Network(format!("RTMP read payload error: {}", e)))?;
            }

            if state.buf.len() >= state.msg_len {
                let payload = std::mem::take(&mut state.buf);
                let msg = RtmpMessage {
                    csid,
                    timestamp_ms: state.timestamp_ms,
                    msg_type: state.msg_type,
                    stream_id: state.stream_id,
                    payload,
                };
                if msg.msg_type == MSG_SET_CHUNK_SIZE && msg.payload.len() >= 4 {
                    let sz = u32::from_be_bytes([
                        msg.payload[0] & 0x7F,
                        msg.payload[1],
                        msg.payload[2],
                        msg.payload[3],
                    ]) as usize;
                    self.set_chunk_size(sz);
                }
                return Ok(msg);
            }
        }
    }
}

impl Default for ChunkReader {
    fn default() -> Self {
        Self::new()
    }
}
