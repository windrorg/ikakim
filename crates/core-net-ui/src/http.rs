//! ゼロ依存 HTTP/1.1 パーサ、埋め込み静的アセット配信、および WebSocket アップグレード応答

use crate::rpc::{snapshot_to_json, ControlBackend};
use crate::sha1_b64::compute_websocket_accept;
use core_types::{CoreError, CoreResult};
use std::io::{BufRead, BufReader, Write};

const INDEX_HTML: &[u8] = include_bytes!("../../../web-ui/index.html");
const APP_JS: &[u8] = include_bytes!("../../../web-ui/app.js");
const STYLE_CSS: &[u8] = include_bytes!("../../../web-ui/style.css");

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
}

impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn is_websocket_upgrade(&self) -> bool {
        let has_upgrade = self
            .header("upgrade")
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
        let has_conn = self
            .header("connection")
            .is_some_and(|v| contains_ignore_ascii_case(v, b"upgrade"));
        has_upgrade && has_conn && self.header("sec-websocket-key").is_some()
    }
}

#[inline]
fn contains_ignore_ascii_case(haystack: &str, needle: &[u8]) -> bool {
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

/// HTTP/1.1 リクエスト行とヘッダ部を読み取る
pub fn read_http_request<R: std::io::Read>(reader: &mut BufReader<R>) -> CoreResult<HttpRequest> {
    let mut request_line = String::new();
    let n = reader
        .read_line(&mut request_line)
        .map_err(|e| CoreError::Network(format!("http read request line: {}", e)))?;
    if n == 0 {
        return Err(CoreError::Network("http connection closed".into()));
    }

    let parts: Vec<&str> = request_line.trim_end_matches(['\r', '\n']).split_whitespace().collect();
    if parts.len() < 2 {
        return Err(CoreError::Network(format!(
            "malformed HTTP request line: {}",
            request_line.trim()
        )));
    }
    let method = parts[0].to_string();
    let raw_target = parts[1];
    let path = raw_target.split('?').next().unwrap_or(raw_target).to_string();

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .map_err(|e| CoreError::Network(format!("http read header: {}", e)))?;
        if bytes == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }

    Ok(HttpRequest {
        method,
        path,
        headers,
    })
}

/// WebSocket 101 Switching Protocols 応答を書き込む
pub fn write_websocket_upgrade<W: Write>(writer: &mut W, key: &str) -> CoreResult<()> {
    let accept = compute_websocket_accept(key);
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        accept
    );
    writer
        .write_all(resp.as_bytes())
        .map_err(|e| CoreError::Network(format!("ws upgrade write: {}", e)))?;
    writer
        .flush()
        .map_err(|e| CoreError::Network(format!("ws upgrade flush: {}", e)))?;
    Ok(())
}

/// 静的アセット・ヘルスチェック・ステータス JSON を返す
pub fn handle_http_request<W: Write>(
    writer: &mut W,
    req: &HttpRequest,
    backend: &dyn ControlBackend,
) -> CoreResult<()> {
    if req.method != "GET" && req.method != "HEAD" {
        return write_response(writer, 405, "Method Not Allowed", "text/plain; charset=utf-8", b"Method Not Allowed");
    }

    let is_head = req.method == "HEAD";
    match req.path.as_str() {
        "/" | "/index.html" => write_static(writer, "text/html; charset=utf-8", INDEX_HTML, is_head),
        "/app.js" => write_static(writer, "application/javascript; charset=utf-8", APP_JS, is_head),
        "/style.css" => write_static(writer, "text/css; charset=utf-8", STYLE_CSS, is_head),
        "/healthz" => write_static(writer, "text/plain; charset=utf-8", b"ok\n", is_head),
        "/api/status" => {
            let body = snapshot_to_json(&backend.snapshot()).to_json_string();
            write_static(writer, "application/json; charset=utf-8", body.as_bytes(), is_head)
        }
        _ => write_response(writer, 404, "Not Found", "text/plain; charset=utf-8", b"Not Found"),
    }
}

fn write_static<W: Write>(
    writer: &mut W,
    content_type: &str,
    body: &[u8],
    head_only: bool,
) -> CoreResult<()> {
    let hdr = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-cache\r\n\
         Connection: close\r\n\r\n",
        content_type,
        body.len()
    );
    writer
        .write_all(hdr.as_bytes())
        .map_err(|e| CoreError::Network(format!("http write hdr: {}", e)))?;
    if !head_only {
        writer
            .write_all(body)
            .map_err(|e| CoreError::Network(format!("http write body: {}", e)))?;
    }
    writer
        .flush()
        .map_err(|e| CoreError::Network(format!("http flush: {}", e)))?;
    Ok(())
}

fn write_response<W: Write>(
    writer: &mut W,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> CoreResult<()> {
    let hdr = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        status,
        reason,
        content_type,
        body.len()
    );
    writer
        .write_all(hdr.as_bytes())
        .map_err(|e| CoreError::Network(format!("http write err hdr: {}", e)))?;
    writer
        .write_all(body)
        .map_err(|e| CoreError::Network(format!("http write err body: {}", e)))?;
    writer
        .flush()
        .map_err(|e| CoreError::Network(format!("http flush: {}", e)))?;
    Ok(())
}
