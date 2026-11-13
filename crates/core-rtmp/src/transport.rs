use core_types::{CoreError, CoreResult};
use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtmpUrl {
    pub use_tls: bool,
    pub host: String,
    pub port: u16,
    pub app: String,
    pub stream_key: String,
    pub tc_url: String,
}

impl RtmpUrl {
    /// `rtmp://host[:port]/app/stream_key` または `rtmps://host[:port]/app/stream_key` をパースする
    pub fn parse(raw: &str) -> CoreResult<Self> {
        let (use_tls, default_port, rest) = if let Some(r) = raw.strip_prefix("rtmps://") {
            (true, 443u16, r)
        } else if let Some(r) = raw.strip_prefix("rtmp://") {
            (false, 1935u16, r)
        } else {
            return Err(CoreError::Network(format!(
                "URL must start with rtmp:// or rtmps://: {}",
                raw
            )));
        };

        let (authority, path_raw) = rest
            .split_once('/')
            .ok_or_else(|| CoreError::Network(format!("RTMP URL missing /app/key: {}", raw)))?;

        if authority.is_empty() {
            return Err(CoreError::Network("RTMP URL missing host".into()));
        }

        let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
            // IPv6 `[::1]:1935`
            let (addr, after) = stripped
                .split_once(']')
                .ok_or_else(|| CoreError::Network(format!("invalid IPv6 host: {}", authority)))?;
            let port = if let Some(p) = after.strip_prefix(':') {
                p.parse::<u16>()
                    .map_err(|_| CoreError::Network(format!("invalid port: {}", p)))?
            } else {
                default_port
            };
            (addr.to_string(), port)
        } else if let Some((h, p)) = authority.rsplit_once(':') {
            let port = p
                .parse::<u16>()
                .map_err(|_| CoreError::Network(format!("invalid port: {}", p)))?;
            (h.to_string(), port)
        } else {
            (authority.to_string(), default_port)
        };

        let (app, stream_key) = path_raw.split_once('/').unwrap_or((path_raw, ""));
        if app.is_empty() {
            return Err(CoreError::Network(format!("RTMP URL missing app name: {}", raw)));
        }
        if stream_key.is_empty() {
            return Err(CoreError::Network(format!(
                "RTMP URL missing stream key: {}",
                raw
            )));
        }

        let scheme = if use_tls { "rtmps" } else { "rtmp" };
        let tc_url = if port == default_port {
            format!("{}://{}/{}", scheme, host, app)
        } else {
            format!("{}://{}:{}/{}", scheme, host, port, app)
        };

        Ok(Self {
            use_tls,
            host,
            port,
            app: app.to_string(),
            stream_key: stream_key.to_string(),
            tc_url,
        })
    }
}

/// RTMPS (TLS) ルート証明書設定
#[derive(Debug, Clone, Default)]
pub enum TlsRootConfig {
    /// OS 標準 CA バンドル（Linux `/etc/ssl/certs/ca-certificates.crt`、macOS `/etc/ssl/cert.pem` 等）を使用
    #[default]
    System,
    /// 指定された PEM バイト列のみを信頼（ローカルテスト / プライベートCA向け）
    CustomPem(Vec<u8>),
    /// OS 標準 CA に加えて追加の PEM を信頼
    SystemPlusCustomPem(Vec<u8>),
}

impl TlsRootConfig {
    pub fn build_client_config(&self) -> CoreResult<Arc<ClientConfig>> {
        let mut store = RootCertStore::empty();

        let load_system = matches!(self, Self::System | Self::SystemPlusCustomPem(_));
        if load_system {
            let candidates = [
                "/etc/ssl/certs/ca-certificates.crt",
                "/etc/pki/tls/certs/ca-bundle.crt",
                "/etc/ssl/cert.pem",
            ];
            let mut loaded_any = false;
            for path in candidates {
                if let Ok(pem_bytes) = fs::read(path) {
                    if add_pem_certs(&mut store, &pem_bytes)? > 0 {
                        loaded_any = true;
                        break;
                    }
                }
            }
            if !loaded_any {
                return Err(CoreError::Network(
                    "no system CA certificate bundle found for RTMPS".into(),
                ));
            }
        }

        match self {
            Self::CustomPem(pem) | Self::SystemPlusCustomPem(pem) => {
                let added = add_pem_certs(&mut store, pem)?;
                if added == 0 {
                    return Err(CoreError::Network(
                        "custom PEM contained zero valid certificates".into(),
                    ));
                }
            }
            Self::System => {}
        }

        let cfg = ClientConfig::builder()
            .with_root_certificates(store)
            .with_no_client_auth();
        Ok(Arc::new(cfg))
    }
}

fn add_pem_certs(store: &mut RootCertStore, pem_bytes: &[u8]) -> CoreResult<usize> {
    let mut added = 0usize;
    for item in CertificateDer::pem_slice_iter(pem_bytes) {
        let cert = item.map_err(|e| CoreError::Network(format!("invalid PEM certificate: {}", e)))?;
        if store.add(cert).is_ok() {
            added += 1;
        }
    }
    Ok(added)
}

pub enum RtmpTransport {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl RtmpTransport {
    pub fn connect(
        url: &RtmpUrl,
        tls_cfg: &TlsRootConfig,
        connect_timeout: Duration,
        io_timeout: Duration,
    ) -> CoreResult<Self> {
        let addr_str = format!("{}:{}", url.host, url.port);
        let addrs: Vec<_> = addr_str
            .to_socket_addrs()
            .map_err(|e| CoreError::Network(format!("DNS resolve failed ({}): {}", addr_str, e)))?
            .collect();
        if addrs.is_empty() {
            return Err(CoreError::Network(format!(
                "no socket addresses resolved for {}",
                addr_str
            )));
        }

        let mut last_err = None;
        let mut tcp_opt = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, connect_timeout) {
                Ok(s) => {
                    tcp_opt = Some(s);
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        let tcp = tcp_opt.ok_or_else(|| {
            CoreError::Network(format!(
                "TCP connect to {} failed: {}",
                addr_str,
                last_err
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "unknown error".into())
            ))
        })?;

        let _ = tcp.set_nodelay(true);
        let _ = tcp.set_read_timeout(Some(io_timeout));
        let _ = tcp.set_write_timeout(Some(io_timeout));

        if url.use_tls {
            let client_cfg = tls_cfg.build_client_config()?;
            let server_name = ServerName::try_from(url.host.clone()).map_err(|e| {
                CoreError::Network(format!("invalid TLS server name '{}': {}", url.host, e))
            })?;
            let conn = ClientConnection::new(client_cfg, server_name)
                .map_err(|e| CoreError::Network(format!("rustls ClientConnection failed: {}", e)))?;
            let mut tls_stream = StreamOwned::new(conn, tcp);
            // TLS ハンドシェイクを即時完了させて証明書検証エラーを接続時点で検知する
            while tls_stream.conn.is_handshaking() {
                tls_stream.conn.complete_io(&mut tls_stream.sock).map_err(|e| {
                    CoreError::Network(format!("RTMPS TLS handshake failed: {}", e))
                })?;
            }
            Ok(Self::Tls(Box::new(tls_stream)))
        } else {
            Ok(Self::Plain(tcp))
        }
    }

    pub fn set_read_timeout(&self, dur: Option<Duration>) {
        match self {
            Self::Plain(s) => {
                let _ = s.set_read_timeout(dur);
            }
            Self::Tls(s) => {
                let _ = s.sock.set_read_timeout(dur);
            }
        }
    }
}

impl Read for RtmpTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(buf),
            Self::Tls(s) => s.read(buf),
        }
    }
}

impl Write for RtmpTransport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(buf),
            Self::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(s) => s.flush(),
            Self::Tls(s) => s.flush(),
        }
    }
}
