use core_types::{CoreError, CoreResult};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};

pub const DEFAULT_CHUNK_SIZE: usize = 128;
const FAST_CSID_LIMIT: usize = 16;

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

#[derive(Debug, Clone, Copy)]
struct OutPrevHeader {
    timestamp_ms: u32,
    stream_id: u32,
}

pub struct ChunkWriter {
    chunk_size: usize,
    fast_prev: [Option<OutPrevHeader>; FAST_CSID_LIMIT],
    prev_headers: HashMap<u32, OutPrevHeader>,
    staging_buf: Vec<u8>,
}

impl ChunkWriter {
    pub fn new() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            fast_prev: [None; FAST_CSID_LIMIT],
            prev_headers: HashMap::new(),
            staging_buf: Vec::with_capacity(16 * 1024),
        }
    }

    pub fn set_chunk_size(&mut self, size: usize) {
        self.chunk_size = size.max(1);
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    #[inline]
    fn get_prev(&self, csid: u32) -> Option<OutPrevHeader> {
        if (csid as usize) < FAST_CSID_LIMIT {
            self.fast_prev[csid as usize]
        } else {
            self.prev_headers.get(&csid).copied()
        }
    }

    #[inline]
    fn set_prev(&mut self, csid: u32, prev: OutPrevHeader) {
        if (csid as usize) < FAST_CSID_LIMIT {
            self.fast_prev[csid as usize] = Some(prev);
        } else {
            self.prev_headers.insert(csid, prev);
        }
    }

    pub fn write_message<W: Write>(&mut self, writer: &mut W, msg: &RtmpMessage) -> CoreResult<usize> {
        self.write_message_payload(
            writer,
            msg.csid,
            msg.timestamp_ms,
            msg.msg_type,
            msg.stream_id,
            &msg.payload,
        )
    }

    /// 借用スライス `payload` をチャンク分割して内部ステージングバッファへ一括構築し、
    /// 単一の `write_all` システムコール（および単一の TLS レコード）で送信する
    pub fn write_message_payload<W: Write>(
        &mut self,
        writer: &mut W,
        csid: u32,
        timestamp_ms: u32,
        msg_type: u8,
        stream_id: u32,
        payload: &[u8],
    ) -> CoreResult<usize> {
        let payload_len = payload.len() as u32;
        let num_chunks = payload.len().div_ceil(self.chunk_size).max(1);
        self.staging_buf.clear();
        self.staging_buf.reserve(payload.len() + num_chunks * 18);

        let mut offset = 0usize;
        let mut msg_ext_ts: Option<u32> = None;

        while offset < payload.len() || (payload.is_empty() && offset == 0) {
            let chunk_len = (payload.len() - offset).min(self.chunk_size);
            let mut hdr = [0u8; 18];
            let mut hdr_len: usize;

            if offset == 0 {
                // Fmt 0 または Fmt 1 で送信
                let prev_opt = self.get_prev(csid);
                let use_fmt1 = prev_opt
                    .is_some_and(|prev| prev.stream_id == stream_id && timestamp_ms >= prev.timestamp_ms);

                if let (true, Some(prev)) = (use_fmt1, prev_opt) {
                    let delta = timestamp_ms.wrapping_sub(prev.timestamp_ms);
                    hdr_len = encode_basic_header_buf(1, csid, &mut hdr);
                    let ts_field = if delta >= 0x00FF_FFFF { 0x00FF_FFFF } else { delta };
                    hdr[hdr_len..hdr_len + 3].copy_from_slice(&ts_field.to_be_bytes()[1..4]);
                    hdr[hdr_len + 3..hdr_len + 6].copy_from_slice(&payload_len.to_be_bytes()[1..4]);
                    hdr[hdr_len + 6] = msg_type;
                    hdr_len += 7;
                    if delta >= 0x00FF_FFFF {
                        hdr[hdr_len..hdr_len + 4].copy_from_slice(&delta.to_be_bytes());
                        hdr_len += 4;
                        msg_ext_ts = Some(delta);
                    }
                    self.set_prev(
                        csid,
                        OutPrevHeader {
                            timestamp_ms,
                            stream_id,
                        },
                    );
                } else {
                    hdr_len = encode_basic_header_buf(0, csid, &mut hdr);
                    let ts_field = if timestamp_ms >= 0x00FF_FFFF {
                        0x00FF_FFFF
                    } else {
                        timestamp_ms
                    };
                    hdr[hdr_len..hdr_len + 3].copy_from_slice(&ts_field.to_be_bytes()[1..4]);
                    hdr[hdr_len + 3..hdr_len + 6].copy_from_slice(&payload_len.to_be_bytes()[1..4]);
                    hdr[hdr_len + 6] = msg_type;
                    hdr[hdr_len + 7..hdr_len + 11].copy_from_slice(&stream_id.to_le_bytes());
                    hdr_len += 11;
                    if timestamp_ms >= 0x00FF_FFFF {
                        hdr[hdr_len..hdr_len + 4].copy_from_slice(&timestamp_ms.to_be_bytes());
                        hdr_len += 4;
                        msg_ext_ts = Some(timestamp_ms);
                    }
                    self.set_prev(
                        csid,
                        OutPrevHeader {
                            timestamp_ms,
                            stream_id,
                        },
                    );
                }
            } else {
                // Fmt 3 continuation: 先頭チャンク (offset == 0) が拡張タイムスタンプを伴った場合のみ再送
                hdr_len = encode_basic_header_buf(3, csid, &mut hdr);
                if let Some(ext_ts) = msg_ext_ts {
                    hdr[hdr_len..hdr_len + 4].copy_from_slice(&ext_ts.to_be_bytes());
                    hdr_len += 4;
                }
            }

            self.staging_buf.extend_from_slice(&hdr[..hdr_len]);
            if chunk_len > 0 {
                self.staging_buf
                    .extend_from_slice(&payload[offset..offset + chunk_len]);
            }
            offset += chunk_len;
            if payload.is_empty() {
                break;
            }
        }

        writer
            .write_all(&self.staging_buf)
            .map_err(|e| CoreError::Network(format!("RTMP write message error: {}", e)))?;
        Ok(self.staging_buf.len())
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
    fast_streams: [InChunkState; FAST_CSID_LIMIT],
    streams: HashMap<u32, InChunkState>,
    bytes_received: u64,
}

impl ChunkReader {
    pub fn new() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            fast_streams: std::array::from_fn(|_| InChunkState::default()),
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

    /// ノンブロッキング/短タイムアウト受信用: 先頭バイトが `WouldBlock`/`TimedOut` ならヒープ確保なしで `Ok(None)` を返す
    pub fn try_read_message<R: Read>(&mut self, reader: &mut R) -> CoreResult<Option<RtmpMessage>> {
        let mut b0 = [0u8; 1];
        match Self::read_exact_counted(&mut self.bytes_received, reader, &mut b0) {
            Ok(()) => self.read_message_after_b0(reader, b0[0]).map(Some),
            Err(ref e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                Ok(None)
            }
            Err(e) => Err(CoreError::Network(format!(
                "RTMP read chunk header error: {}",
                e
            ))),
        }
    }

    pub fn read_message<R: Read>(&mut self, reader: &mut R) -> CoreResult<RtmpMessage> {
        let mut b0 = [0u8; 1];
        Self::read_exact_counted(&mut self.bytes_received, reader, &mut b0)
            .map_err(|e| CoreError::Network(format!("RTMP read chunk header error: {}", e)))?;
        self.read_message_after_b0(reader, b0[0])
    }

    fn read_message_after_b0<R: Read>(
        &mut self,
        reader: &mut R,
        mut first_byte: u8,
    ) -> CoreResult<RtmpMessage> {
        loop {
            let fmt = (first_byte >> 6) & 0x03;
            let csid_low = (first_byte & 0x3F) as u32;
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
            let state = if (csid as usize) < FAST_CSID_LIMIT {
                &mut self.fast_streams[csid as usize]
            } else {
                self.streams.entry(csid).or_default()
            };

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

            let mut next_b0 = [0u8; 1];
            Self::read_exact_counted(&mut self.bytes_received, reader, &mut next_b0)
                .map_err(|e| CoreError::Network(format!("RTMP read continuation header error: {}", e)))?;
            first_byte = next_b0[0];
        }
    }
}

impl Default for ChunkReader {
    fn default() -> Self {
        Self::new()
    }
}
