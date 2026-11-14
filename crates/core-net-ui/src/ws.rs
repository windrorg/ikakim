//! ゼロ依存 RFC 6455 WebSocket フレームコーデック

use core_types::{CoreError, CoreResult};
use std::io::{Read, Write};

pub const MAX_WS_FRAME_SIZE: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsOpcode {
    Continuation = 0x0,
    Text = 0x1,
    Binary = 0x2,
    Close = 0x8,
    Ping = 0x9,
    Pong = 0xA,
}

impl WsOpcode {
    pub fn from_u8(val: u8) -> CoreResult<Self> {
        match val {
            0x0 => Ok(Self::Continuation),
            0x1 => Ok(Self::Text),
            0x2 => Ok(Self::Binary),
            0x8 => Ok(Self::Close),
            0x9 => Ok(Self::Ping),
            0xA => Ok(Self::Pong),
            other => Err(CoreError::Network(format!("unknown WebSocket opcode {}", other))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsMessage {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(u16, String),
}

#[derive(Debug, Clone)]
pub struct RawWsFrame {
    pub fin: bool,
    pub opcode: WsOpcode,
    pub masked: bool,
    pub payload: Vec<u8>,
}

/// 64-bit ワード一括処理で RFC 6455 WebSocket マスク/マスク解除（XOR）をインプレース適用する
#[inline]
pub fn apply_ws_mask(payload: &mut [u8], key: [u8; 4]) {
    let key64 = u64::from_ne_bytes([
        key[0], key[1], key[2], key[3], key[0], key[1], key[2], key[3],
    ]);
    let mut chunks = payload.chunks_exact_mut(8);
    for chunk in &mut chunks {
        let word = u64::from_ne_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
        chunk.copy_from_slice(&(word ^ key64).to_ne_bytes());
    }
    for (i, b) in chunks.into_remainder().iter_mut().enumerate() {
        *b ^= key[i & 3];
    }
}

/// ストリームから 1 つの WebSocket フレームを読み取る（マスク解除含む）
pub fn read_raw_frame<R: Read>(reader: &mut R) -> CoreResult<RawWsFrame> {
    let mut header = [0u8; 2];
    reader.read_exact(&mut header).map_err(|e| {
        if matches!(
            e.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ) {
            CoreError::Network(format!("TimedOut: {}", e))
        } else {
            CoreError::Network(format!("ws read header: {}", e))
        }
    })?;

    let fin = (header[0] & 0x80) != 0;
    let opcode = WsOpcode::from_u8(header[0] & 0x0F)?;
    let masked = (header[1] & 0x80) != 0;
    let len7 = (header[1] & 0x7F) as u64;

    let payload_len: usize = if len7 < 126 {
        len7 as usize
    } else if len7 == 126 {
        let mut ext = [0u8; 2];
        reader
            .read_exact(&mut ext)
            .map_err(|e| CoreError::Network(format!("ws read len16: {}", e)))?;
        u16::from_be_bytes(ext) as usize
    } else {
        let mut ext = [0u8; 8];
        reader
            .read_exact(&mut ext)
            .map_err(|e| CoreError::Network(format!("ws read len64: {}", e)))?;
        let n = u64::from_be_bytes(ext);
        if n > MAX_WS_FRAME_SIZE as u64 {
            return Err(CoreError::Network("ws payload exceeds max size".into()));
        }
        n as usize
    };

    if payload_len > MAX_WS_FRAME_SIZE {
        return Err(CoreError::Network("ws payload exceeds max size".into()));
    }

    let mask_key = if masked {
        let mut k = [0u8; 4];
        reader
            .read_exact(&mut k)
            .map_err(|e| CoreError::Network(format!("ws read mask: {}", e)))?;
        Some(k)
    } else {
        None
    };

    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        reader
            .read_exact(&mut payload)
            .map_err(|e| CoreError::Network(format!("ws read payload: {}", e)))?;
    }

    if let Some(k) = mask_key {
        apply_ws_mask(&mut payload, k);
    }

    Ok(RawWsFrame {
        fin,
        opcode,
        masked,
        payload,
    })
}

/// RFC 6455 フラグメンテーションを結合し、`WsMessage` を 1 つ読み取る
pub fn read_ws_message<R: Read>(reader: &mut R) -> CoreResult<WsMessage> {
    let first = read_raw_frame(reader)?;
    let opcode = first.opcode;
    let mut payload = first.payload;
    let mut fin = first.fin;

    while !fin {
        let cont = read_raw_frame(reader)?;
        if payload.len() + cont.payload.len() > MAX_WS_FRAME_SIZE {
            return Err(CoreError::Network("ws fragmented message too large".into()));
        }
        payload.extend_from_slice(&cont.payload);
        fin = cont.fin;
    }

    match opcode {
        WsOpcode::Text => {
            let s = String::from_utf8(payload)
                .map_err(|_| CoreError::Network("invalid UTF-8 in ws text frame".into()))?;
            Ok(WsMessage::Text(s))
        }
        WsOpcode::Binary => Ok(WsMessage::Binary(payload)),
        WsOpcode::Ping => Ok(WsMessage::Ping(payload)),
        WsOpcode::Pong => Ok(WsMessage::Pong(payload)),
        WsOpcode::Close => {
            if payload.len() >= 2 {
                let code = u16::from_be_bytes([payload[0], payload[1]]);
                let reason = String::from_utf8_lossy(&payload[2..]).to_string();
                Ok(WsMessage::Close(code, reason))
            } else {
                Ok(WsMessage::Close(1000, String::new()))
            }
        }
        WsOpcode::Continuation => Err(CoreError::Network(
            "unexpected standalone continuation frame".into(),
        )),
    }
}

/// サーバー送信（マスクなし）フレームを書き出す
pub fn write_server_frame<W: Write>(
    writer: &mut W,
    opcode: WsOpcode,
    payload: &[u8],
) -> CoreResult<()> {
    write_frame_internal(writer, opcode, payload, None)
}

/// クライアント送信（マスク付き、テストやクライアント用）フレームを書き出す
pub fn write_masked_frame<W: Write>(
    writer: &mut W,
    opcode: WsOpcode,
    payload: &[u8],
    mask_key: [u8; 4],
) -> CoreResult<()> {
    write_frame_internal(writer, opcode, payload, Some(mask_key))
}

fn write_frame_internal<W: Write>(
    writer: &mut W,
    opcode: WsOpcode,
    payload: &[u8],
    mask_key: Option<[u8; 4]>,
) -> CoreResult<()> {
    let mut hdr = [0u8; 14];
    hdr[0] = 0x80 | (opcode as u8);
    let mask_bit = if mask_key.is_some() { 0x80u8 } else { 0x00u8 };
    let len = payload.len();
    let mut hdr_len = if len < 126 {
        hdr[1] = mask_bit | (len as u8);
        2
    } else if len <= 0xFFFF {
        hdr[1] = mask_bit | 126;
        hdr[2..4].copy_from_slice(&(len as u16).to_be_bytes());
        4
    } else {
        hdr[1] = mask_bit | 127;
        hdr[2..10].copy_from_slice(&(len as u64).to_be_bytes());
        10
    };

    if let Some(k) = mask_key {
        hdr[hdr_len..hdr_len + 4].copy_from_slice(&k);
        hdr_len += 4;
    }

    // 30Hz メーター通知や JSON-RPC 応答などの小サイズ（<= 2048B）非マスクフレームは
    // スタック上でヘッダとペイロードを結合し 1 回の write_all に集約する
    if mask_key.is_none() && len <= 2048 {
        let mut combined = [0u8; 14 + 2048];
        combined[..hdr_len].copy_from_slice(&hdr[..hdr_len]);
        combined[hdr_len..hdr_len + len].copy_from_slice(payload);
        writer
            .write_all(&combined[..hdr_len + len])
            .map_err(|e| CoreError::Network(format!("ws write combined frame: {}", e)))?;
        writer
            .flush()
            .map_err(|e| CoreError::Network(format!("ws flush: {}", e)))?;
        return Ok(());
    }

    writer
        .write_all(&hdr[..hdr_len])
        .map_err(|e| CoreError::Network(format!("ws write header: {}", e)))?;

    if !payload.is_empty() {
        if let Some(k) = mask_key {
            let mut masked = payload.to_vec();
            apply_ws_mask(&mut masked, k);
            writer
                .write_all(&masked)
                .map_err(|e| CoreError::Network(format!("ws write masked payload: {}", e)))?;
        } else {
            writer
                .write_all(payload)
                .map_err(|e| CoreError::Network(format!("ws write payload: {}", e)))?;
        }
    }
    writer
        .flush()
        .map_err(|e| CoreError::Network(format!("ws flush: {}", e)))?;
    Ok(())
}
