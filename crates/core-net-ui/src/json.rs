//! ゼロ依存 JSON パーサ・シリアライザ

use core_types::{CoreError, CoreResult};
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
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

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Number(n) => Some(*n as i64),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(n) if *n >= 0.0 => Some(*n as u64),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        match self {
            Self::Object(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            Self::Array(items) => Some(items.as_slice()),
            _ => None,
        }
    }

    /// コンパクトな JSON 文字列へシリアライズする
    pub fn to_json_string(&self) -> String {
        let mut out = String::with_capacity(128);
        self.write_json(&mut out);
        out
    }

    fn write_json(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Self::Number(n) => {
                if !n.is_finite() {
                    out.push_str("null");
                } else if (n.fract() == 0.0) && (*n >= (i64::MIN as f64)) && (*n <= (i64::MAX as f64))
                {
                    let _ = write!(out, "{}", *n as i64);
                } else {
                    let _ = write!(out, "{}", n);
                }
            }
            Self::String(s) => write_escaped_string(s, out),
            Self::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_json(out);
                }
                out.push(']');
            }
            Self::Object(pairs) => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_escaped_string(k, out);
                    out.push(':');
                    v.write_json(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_escaped_string(s: &str, out: &mut String) {
    out.push('"');
    let bytes = s.as_bytes();
    let mut last = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        let escaped = match b {
            b'"' => Some("\\\""),
            b'\\' => Some("\\\\"),
            b'\n' => Some("\\n"),
            b'\r' => Some("\\r"),
            b'\t' => Some("\\t"),
            c if c < 0x20 => {
                if i > last {
                    out.push_str(&s[last..i]);
                }
                let _ = write!(out, "\\u{:04x}", c as u32);
                last = i + 1;
                None
            }
            _ => None,
        };
        if let Some(esc) = escaped {
            if i > last {
                out.push_str(&s[last..i]);
            }
            out.push_str(esc);
            last = i + 1;
        }
    }
    if last < s.len() {
        out.push_str(&s[last..]);
    }
    out.push('"');
}

/// JSON 文字列をパースする
pub fn parse_json(input: &str) -> CoreResult<JsonValue> {
    let mut parser = JsonParser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    let val = parser.parse_value(0)?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(CoreError::Network(format!(
            "unexpected trailing JSON bytes at pos {}",
            parser.pos
        )));
    }
    Ok(val)
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && matches!(self.bytes[self.pos], b' ' | b'\t' | b'\r' | b'\n')
        {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn next(&mut self) -> CoreResult<u8> {
        if self.pos >= self.bytes.len() {
            return Err(CoreError::Network("unexpected EOF in JSON".into()));
        }
        let b = self.bytes[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn parse_value(&mut self, depth: usize) -> CoreResult<JsonValue> {
        if depth > 64 {
            return Err(CoreError::Network("JSON nesting too deep".into()));
        }
        self.skip_ws();
        match self.peek() {
            Some(b'n') => {
                self.expect_literal(b"null")?;
                Ok(JsonValue::Null)
            }
            Some(b't') => {
                self.expect_literal(b"true")?;
                Ok(JsonValue::Bool(true))
            }
            Some(b'f') => {
                self.expect_literal(b"false")?;
                Ok(JsonValue::Bool(false))
            }
            Some(b'"') => self.parse_string().map(JsonValue::String),
            Some(b'[') => self.parse_array(depth + 1),
            Some(b'{') => self.parse_object(depth + 1),
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            Some(other) => Err(CoreError::Network(format!(
                "unexpected JSON char '{}' at {}",
                other as char, self.pos
            ))),
            None => Err(CoreError::Network("unexpected EOF in JSON".into())),
        }
    }

    fn expect_literal(&mut self, lit: &[u8]) -> CoreResult<()> {
        if self.bytes[self.pos..].starts_with(lit) {
            self.pos += lit.len();
            Ok(())
        } else {
            Err(CoreError::Network(format!(
                "expected '{}' at {}",
                String::from_utf8_lossy(lit),
                self.pos
            )))
        }
    }

    fn parse_string(&mut self) -> CoreResult<String> {
        if self.next()? != b'"' {
            return Err(CoreError::Network("expected '\"'".into()));
        }
        let start = self.pos;
        let mut fast = true;
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'"' {
                let s = std::str::from_utf8(&self.bytes[start..self.pos])
                    .map_err(|_| CoreError::Network("invalid UTF-8 in JSON string".into()))?;
                self.pos += 1;
                if fast {
                    return Ok(s.to_string());
                }
                break;
            }
            if b == b'\\' {
                fast = false;
                break;
            }
            self.pos += 1;
        }
        if fast {
            return Err(CoreError::Network("unterminated JSON string".into()));
        }

        // エスケープを含む文字列用の低速パス
        self.pos = start;
        let mut out = String::new();
        loop {
            if self.pos >= self.bytes.len() {
                return Err(CoreError::Network("unterminated JSON string".into()));
            }
            let b = self.bytes[self.pos];
            if b == b'"' {
                self.pos += 1;
                return Ok(out);
            }
            if b == b'\\' {
                self.pos += 1;
                let esc = self.next()?;
                match esc {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{0008}'),
                    b'f' => out.push('\u{000C}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let code = self.parse_hex4()?;
                        if (0xD800..=0xDBFF).contains(&code) && self.bytes[self.pos..].starts_with(b"\\u")
                        {
                            self.pos += 2;
                            let low = self.parse_hex4()?;
                            let cp = 0x10000 + (((code as u32 - 0xD800) << 10) | (low as u32 - 0xDC00));
                            let ch = char::from_u32(cp)
                                .ok_or_else(|| CoreError::Network("invalid surrogate pair".into()))?;
                            out.push(ch);
                        } else if let Some(ch) = char::from_u32(code as u32) {
                            out.push(ch);
                        } else {
                            return Err(CoreError::Network("invalid \\uXXXX codepoint".into()));
                        }
                    }
                    other => {
                        return Err(CoreError::Network(format!(
                            "unknown escape \\{}",
                            other as char
                        )))
                    }
                }
            } else {
                let s = std::str::from_utf8(&self.bytes[self.pos..])
                    .map_err(|_| CoreError::Network("invalid UTF-8 in JSON string".into()))?;
                let ch = s.chars().next().unwrap();
                out.push(ch);
                self.pos += ch.len_utf8();
            }
        }
    }

    fn parse_hex4(&mut self) -> CoreResult<u16> {
        if self.pos + 4 > self.bytes.len() {
            return Err(CoreError::Network("truncated \\uXXXX escape".into()));
        }
        let hex = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
            .map_err(|_| CoreError::Network("invalid \\uXXXX UTF-8".into()))?;
        self.pos += 4;
        u16::from_str_radix(hex, 16)
            .map_err(|_| CoreError::Network("invalid \\uXXXX hex".into()))
    }

    fn parse_number(&mut self) -> CoreResult<JsonValue> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-') {
                self.pos += 1;
            } else {
                break;
            }
        }
        let slice = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| CoreError::Network("invalid number UTF-8".into()))?;
        let val: f64 = slice
            .parse()
            .map_err(|_| CoreError::Network(format!("invalid JSON number: {}", slice)))?;
        Ok(JsonValue::Number(val))
    }

    fn parse_array(&mut self, depth: usize) -> CoreResult<JsonValue> {
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonValue::Array(items));
        }
        loop {
            items.push(self.parse_value(depth)?);
            self.skip_ws();
            match self.next()? {
                b',' => continue,
                b']' => return Ok(JsonValue::Array(items)),
                other => {
                    return Err(CoreError::Network(format!(
                        "expected ',' or ']' in array, got '{}'",
                        other as char
                    )))
                }
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> CoreResult<JsonValue> {
        self.pos += 1; // '{'
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonValue::Object(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            if self.next()? != b':' {
                return Err(CoreError::Network("expected ':' in JSON object".into()));
            }
            let val = self.parse_value(depth)?;
            pairs.push((key, val));
            self.skip_ws();
            match self.next()? {
                b',' => continue,
                b'}' => return Ok(JsonValue::Object(pairs)),
                other => {
                    return Err(CoreError::Network(format!(
                        "expected ',' or '}}' in object, got '{}'",
                        other as char
                    )))
                }
            }
        }
    }
}
