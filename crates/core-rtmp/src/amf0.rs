use core_types::{CoreError, CoreResult};

const MARKER_NUMBER: u8 = 0x00;
const MARKER_BOOLEAN: u8 = 0x01;
const MARKER_STRING: u8 = 0x02;
const MARKER_OBJECT: u8 = 0x03;
const MARKER_NULL: u8 = 0x05;
const MARKER_UNDEFINED: u8 = 0x06;
const MARKER_ECMA_ARRAY: u8 = 0x08;
const MARKER_OBJECT_END: u8 = 0x09;
const MARKER_STRICT_ARRAY: u8 = 0x0A;
const MARKER_LONG_STRING: u8 = 0x0C;

#[derive(Debug, Clone, PartialEq)]
pub enum Amf0Value {
    Number(f64),
    Boolean(bool),
    String(String),
    Object(Vec<(String, Amf0Value)>),
    Null,
    Undefined,
    EcmaArray(Vec<(String, Amf0Value)>),
    StrictArray(Vec<Amf0Value>),
}

impl Amf0Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Boolean(b) => Some(*b),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Amf0Value> {
        match self {
            Self::Object(pairs) | Self::EcmaArray(pairs) => {
                pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
            }
            _ => None,
        }
    }
}

pub fn encode_values(values: &[Amf0Value]) -> Vec<u8> {
    let mut out = Vec::with_capacity(128);
    for v in values {
        encode_value(v, &mut out);
    }
    out
}

pub fn encode_value(val: &Amf0Value, out: &mut Vec<u8>) {
    match val {
        Amf0Value::Number(n) => {
            out.push(MARKER_NUMBER);
            out.extend_from_slice(&n.to_be_bytes());
        }
        Amf0Value::Boolean(b) => {
            out.push(MARKER_BOOLEAN);
            out.push(if *b { 1 } else { 0 });
        }
        Amf0Value::String(s) => {
            let bytes = s.as_bytes();
            if bytes.len() <= u16::MAX as usize {
                out.push(MARKER_STRING);
                out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
            } else {
                out.push(MARKER_LONG_STRING);
                out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            }
            out.extend_from_slice(bytes);
        }
        Amf0Value::Object(pairs) => {
            out.push(MARKER_OBJECT);
            encode_properties(pairs, out);
        }
        Amf0Value::Null => out.push(MARKER_NULL),
        Amf0Value::Undefined => out.push(MARKER_UNDEFINED),
        Amf0Value::EcmaArray(pairs) => {
            out.push(MARKER_ECMA_ARRAY);
            out.extend_from_slice(&(pairs.len() as u32).to_be_bytes());
            encode_properties(pairs, out);
        }
        Amf0Value::StrictArray(items) => {
            out.push(MARKER_STRICT_ARRAY);
            out.extend_from_slice(&(items.len() as u32).to_be_bytes());
            for item in items {
                encode_value(item, out);
            }
        }
    }
}

fn encode_properties(pairs: &[(String, Amf0Value)], out: &mut Vec<u8>) {
    for (k, v) in pairs {
        let kb = k.as_bytes();
        let len = kb.len().min(u16::MAX as usize) as u16;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&kb[..len as usize]);
        encode_value(v, out);
    }
    out.extend_from_slice(&[0x00, 0x00, MARKER_OBJECT_END]);
}

pub fn decode_all(data: &[u8]) -> CoreResult<Vec<Amf0Value>> {
    let mut pos = 0;
    let mut values = Vec::new();
    while pos < data.len() {
        values.push(decode_one(data, &mut pos)?);
    }
    Ok(values)
}

pub fn decode_one(data: &[u8], pos: &mut usize) -> CoreResult<Amf0Value> {
    let marker = *data
        .get(*pos)
        .ok_or_else(|| CoreError::Network("AMF0 unexpected EOF".into()))?;
    *pos += 1;

    match marker {
        MARKER_NUMBER => {
            let bytes = read_bytes::<8>(data, pos)?;
            Ok(Amf0Value::Number(f64::from_be_bytes(bytes)))
        }
        MARKER_BOOLEAN => {
            let b = read_bytes::<1>(data, pos)?[0];
            Ok(Amf0Value::Boolean(b != 0))
        }
        MARKER_STRING => {
            let len = u16::from_be_bytes(read_bytes::<2>(data, pos)?) as usize;
            Ok(Amf0Value::String(read_utf8(data, pos, len)?))
        }
        MARKER_LONG_STRING => {
            let len = u32::from_be_bytes(read_bytes::<4>(data, pos)?) as usize;
            Ok(Amf0Value::String(read_utf8(data, pos, len)?))
        }
        MARKER_OBJECT => Ok(Amf0Value::Object(decode_properties(data, pos)?)),
        MARKER_NULL => Ok(Amf0Value::Null),
        MARKER_UNDEFINED => Ok(Amf0Value::Undefined),
        MARKER_ECMA_ARRAY => {
            let _count = u32::from_be_bytes(read_bytes::<4>(data, pos)?);
            Ok(Amf0Value::EcmaArray(decode_properties(data, pos)?))
        }
        MARKER_STRICT_ARRAY => {
            let count = u32::from_be_bytes(read_bytes::<4>(data, pos)?) as usize;
            let mut items = Vec::with_capacity(count.min(1024));
            for _ in 0..count {
                items.push(decode_one(data, pos)?);
            }
            Ok(Amf0Value::StrictArray(items))
        }
        other => Err(CoreError::Network(format!(
            "AMF0 unsupported marker: 0x{:02x}",
            other
        ))),
    }
}

fn decode_properties(data: &[u8], pos: &mut usize) -> CoreResult<Vec<(String, Amf0Value)>> {
    let mut pairs = Vec::new();
    loop {
        let len = u16::from_be_bytes(read_bytes::<2>(data, pos)?) as usize;
        if len == 0 {
            let end_marker = read_bytes::<1>(data, pos)?[0];
            if end_marker == MARKER_OBJECT_END {
                break;
            }
            return Err(CoreError::Network(format!(
                "AMF0 invalid object end marker: 0x{:02x}",
                end_marker
            )));
        }
        let key = read_utf8(data, pos, len)?;
        let val = decode_one(data, pos)?;
        pairs.push((key, val));
    }
    Ok(pairs)
}

fn read_bytes<const N: usize>(data: &[u8], pos: &mut usize) -> CoreResult<[u8; N]> {
    if *pos + N > data.len() {
        return Err(CoreError::Network("AMF0 truncated payload".into()));
    }
    let mut buf = [0u8; N];
    buf.copy_from_slice(&data[*pos..*pos + N]);
    *pos += N;
    Ok(buf)
}

fn read_utf8(data: &[u8], pos: &mut usize, len: usize) -> CoreResult<String> {
    if *pos + len > data.len() {
        return Err(CoreError::Network("AMF0 truncated string".into()));
    }
    let s = String::from_utf8_lossy(&data[*pos..*pos + len]).into_owned();
    *pos += len;
    Ok(s)
}
