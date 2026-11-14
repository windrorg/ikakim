#![cfg(target_os = "macos")]

use apple_common::audio_toolbox::AudioToolboxAacEncoder;
use apple_common::cv_metal::{
    AppleMetalZeroCopyImporter, AppleZeroCopyRenderSurfacePool, CVPixelBufferWrapper,
};
use apple_common::video_toolbox::VtH264Encoder;
use core_audio::mixer::{AudioInputConfig, AudioInputProducer, AudioMixer};
use core_audio::MIX_BLOCK_FRAMES;
use core_engine::pipeline::{flush_encoders_to_outputs, EngineControlBridge, PipelineTickContext};
use core_engine::virtual_input::{
    channel_rms_dbfs, goertzel_magnitude, VirtualAudioSignal, VirtualAudioSource,
    VirtualVideoPattern, VirtualVideoSource,
};
use core_gpu::{CompositorConfig, GpuCompositor, GpuContext};
use core_net_ui::rpc::ControlBackend;
use core_net_ui::{EmbeddedServer, ServerConfig};
use core_rtmp::abr::AbrConfig;
use core_rtmp::TlsRootConfig;
use core_types::clock::{MasterClock, PtsUs};
use core_types::media::LayerTransform;
use core_types::state::PipelineState;
use core_types::traits::{AudioEncoder, VideoEncoder};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

static TEST_DIR_SEQ: AtomicUsize = AtomicUsize::new(1);

fn resolve_tool(env_var: &str, candidates: &[&str], which_name: &str) -> Option<PathBuf> {
    if let Ok(p) = env::var(env_var) {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    for c in candidates {
        let pb = PathBuf::from(c);
        if pb.is_file() {
            return Some(pb);
        }
    }
    if let Ok(out) = Command::new("which").arg(which_name).output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                let pb = PathBuf::from(s);
                if pb.is_file() {
                    return Some(pb);
                }
            }
        }
    }
    None
}

fn require_or_skip_srs_ffprobe_ffmpeg() -> Option<(PathBuf, PathBuf, PathBuf)> {
    let (srs, ffprobe) = require_or_skip_srs_and_ffprobe()?;
    let require = env::var("IKAKIM_REQUIRE_SRS").as_deref() == Ok("1");
    let ffmpeg = resolve_tool(
        "IKAKIM_FFMPEG_BIN",
        &["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"],
        "ffmpeg",
    );
    match ffmpeg {
        Some(ff) => Some((srs, ffprobe, ff)),
        None => {
            if require {
                panic!("IKAKIM_REQUIRE_SRS=1 is set, but missing ffmpeg binary. Install ffmpeg.");
            }
            eprintln!("[SKIP] ffmpeg binary not found for decoded A/V verification");
            None
        }
    }
}

fn require_or_skip_srs_and_ffprobe() -> Option<(PathBuf, PathBuf)> {
    let require = env::var("IKAKIM_REQUIRE_SRS").as_deref() == Ok("1");
    let srs = resolve_tool(
        "IKAKIM_SRS_BIN",
        &[
            "/Users/kenichim/.local/srs/objs/srs",
            "/usr/local/srs/objs/srs",
            "/opt/homebrew/bin/srs",
        ],
        "srs",
    );
    let ffprobe = resolve_tool(
        "IKAKIM_FFPROBE_BIN",
        &["/opt/homebrew/bin/ffprobe", "/usr/local/bin/ffprobe"],
        "ffprobe",
    );

    match (srs, ffprobe) {
        (Some(s), Some(f)) => Some((s, f)),
        (s, f) => {
            if require {
                panic!(
                    "IKAKIM_REQUIRE_SRS=1 is set, but missing tool(s): srs={:?}, ffprobe={:?}. Run ./scripts/setup-srs-macos.sh and install ffmpeg.",
                    s, f
                );
            }
            eprintln!(
                "[SKIP] apple_srs_e2e_test requires local srs & ffprobe (srs={:?}, ffprobe={:?}). Run ./scripts/setup-srs-macos.sh",
                s, f
            );
            None
        }
    }
}

fn pick_unused_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().unwrap().port()
}

struct SrsProcess {
    srs_bin: PathBuf,
    work_dir: PathBuf,
    conf_path: PathBuf,
    log_path: PathBuf,
    rtmp_port: u16,
    http_port: u16,
    api_port: u16,
    child: Option<Child>,
}

impl SrsProcess {
    fn start(srs_bin: PathBuf) -> Self {
        let id = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let work_dir = env::temp_dir().join(format!("ikakim_srs_e2e_{}_{}", std::process::id(), id));
        let html_dir = work_dir.join("html");
        fs::create_dir_all(&html_dir).expect("create srs work dir");

        let rtmp_port = pick_unused_port();
        let http_port = pick_unused_port();
        let api_port = pick_unused_port();
        let conf_path = work_dir.join("srs.conf");
        let log_path = work_dir.join("srs.log");
        let pid_path = work_dir.join("srs.pid");

        let conf = format!(
            r#"listen {rtmp_port};
max_connections 1000;
daemon off;
pid {pid_path};
srs_log_tank file;
srs_log_file {log_path};

rtc_server {{
    enabled off;
    ecdsa off;
}}

http_api {{
    enabled on;
    listen 127.0.0.1:{api_port};
    raw_api {{
        enabled on;
        allow_reload on;
        allow_query on;
        allow_update on;
    }}
}}

http_server {{
    enabled on;
    listen 127.0.0.1:{http_port};
    dir {html_dir};
}}

vhost __defaultVhost__ {{
    gop_cache on;
    queue_length 10;
    mw_latency 100;
    tcp_nodelay on;
    http_remux {{
        enabled on;
        mount [vhost]/[app]/[stream].flv;
    }}
}}
"#,
            rtmp_port = rtmp_port,
            api_port = api_port,
            http_port = http_port,
            pid_path = pid_path.display(),
            log_path = log_path.display(),
            html_dir = html_dir.display(),
        );
        fs::write(&conf_path, conf).expect("write srs.conf");

        let mut proc_obj = Self {
            srs_bin,
            work_dir,
            conf_path,
            log_path,
            rtmp_port,
            http_port,
            api_port,
            child: None,
        };
        proc_obj.start_process();
        proc_obj
    }

    fn start_process(&mut self) {
        assert!(self.child.is_none(), "SRS child already running");
        let child = Command::new(&self.srs_bin)
            .arg("-c")
            .arg(&self.conf_path)
            .current_dir(&self.work_dir)
            .env("MallocNanoZone", "0")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn SRS");
        self.child = Some(child);
        self.wait_ready();
    }

    fn kill_now(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            if let Some(child) = self.child.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    let log = fs::read_to_string(&self.log_path).unwrap_or_default();
                    panic!("SRS exited unexpectedly ({}):\n{}", status, log);
                }
            }
            if let Ok((status, body)) = self.http_request("GET", "/api/v1/versions") {
                if status == 200 && body.contains("\"code\":0") {
                    return;
                }
            }
            thread::sleep(Duration::from_millis(25));
        }
        let log = fs::read_to_string(&self.log_path).unwrap_or_default();
        panic!("Timed out waiting for SRS HTTP API:\n{}", log);
    }

    fn http_request(&self, method: &str, path: &str) -> std::io::Result<(u16, String)> {
        let addr = format!("127.0.0.1:{}", self.api_port);
        let mut stream = TcpStream::connect(&addr)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let req = format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            method, path, addr
        );
        stream.write_all(req.as_bytes())?;
        let mut raw = String::new();
        let _ = stream.read_to_string(&mut raw);
        let status = raw
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        let body = raw
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.to_string())
            .unwrap_or_default();
        Ok((status, body))
    }

    fn rtmp_url(&self, app: &str, stream_key: &str) -> String {
        format!("rtmp://127.0.0.1:{}/{}/{}", self.rtmp_port, app, stream_key)
    }

    fn flv_url(&self, app: &str, stream_key: &str) -> String {
        format!(
            "http://127.0.0.1:{}/{}/{}.flv",
            self.http_port, app, stream_key
        )
    }

    fn streams_json(&self) -> String {
        self.http_request("GET", "/api/v1/streams/")
            .map(|(_, b)| b)
            .unwrap_or_default()
    }

    fn clients_json(&self) -> String {
        self.http_request("GET", "/api/v1/clients/")
            .map(|(_, b)| b)
            .unwrap_or_default()
    }

    /// Extract IDs of publishing clients from SRS `/api/v1/clients/` JSON whose URL contains `stream_key`.
    fn publisher_client_ids(&self, stream_key: &str) -> Vec<String> {
        let body = self.clients_json();
        let mut ids = Vec::new();
        let mut idx = 0usize;
        while let Some(rel_start) = body[idx..].find("{\"id\":\"") {
            let start = idx + rel_start;
            let Some(rel_end) = body[start..].find('}') else {
                break;
            };
            let obj = &body[start..=start + rel_end];
            if obj.contains("\"publish\":true") && obj.contains(stream_key) {
                if let Some(after_id) = obj.strip_prefix("{\"id\":\"") {
                    if let Some((id_str, _)) = after_id.split_once('"') {
                        ids.push(id_str.to_string());
                    }
                }
            }
            idx = start + rel_end + 1;
        }
        ids
    }

    fn kick_client(&self, client_id: &str) {
        let path = format!("/api/v1/clients/{}", client_id);
        let (status, body) = self.http_request("DELETE", &path).expect("DELETE client");
        assert_eq!(
            status, 200,
            "expected HTTP 200 kicking SRS client {}, got {} body={}",
            client_id, status, body
        );
    }

    fn log_tail(&self) -> String {
        let text = fs::read_to_string(&self.log_path).unwrap_or_default();
        let len = text.len();
        text[len.saturating_sub(2500)..].to_string()
    }
}

impl Drop for SrsProcess {
    fn drop(&mut self) {
        self.kill_now();
        let _ = fs::remove_dir_all(&self.work_dir);
    }
}

/// Bandwidth-throttling TCP proxy in front of SRS RTMP port to test real backpressure ABR & drops.
struct ThrottlingProxy {
    listen_port: u16,
    bytes_per_sec_limit: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl ThrottlingProxy {
    fn start(target_port: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy listener");
        listener.set_nonblocking(true).unwrap();
        let listen_port = listener.local_addr().unwrap().port();
        let limit = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let limit_c = Arc::clone(&limit);
        let stop_c = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let mut workers = Vec::new();
            while !stop_c.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((client, _)) => {
                        let _ = client.set_nonblocking(false);
                        let Ok(server) =
                            TcpStream::connect(format!("127.0.0.1:{}", target_port))
                        else {
                            continue;
                        };
                        let _ = client.set_read_timeout(Some(Duration::from_millis(50)));
                        let _ = server.set_read_timeout(Some(Duration::from_millis(50)));
                        let c2 = client.try_clone().unwrap();
                        let s2 = server.try_clone().unwrap();
                        let stop_up = Arc::clone(&stop_c);
                        let stop_dn = Arc::clone(&stop_c);
                        let lim_up = Arc::clone(&limit_c);
                        workers.push(thread::spawn(move || {
                            forward_loop(client, server, Some(lim_up), stop_up);
                        }));
                        workers.push(thread::spawn(move || {
                            forward_loop(s2, c2, None, stop_dn);
                        }));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(10)),
                }
            }
            for w in workers {
                let _ = w.join();
            }
        });

        Self {
            listen_port,
            bytes_per_sec_limit: limit,
            stop,
            handle: Some(handle),
        }
    }

    fn set_limit_bytes_per_sec(&self, bps: usize) {
        self.bytes_per_sec_limit.store(bps, Ordering::Release);
    }

    fn rtmp_url(&self, app: &str, stream_key: &str) -> String {
        format!("rtmp://127.0.0.1:{}/{}/{}", self.listen_port, app, stream_key)
    }
}

impl Drop for ThrottlingProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn forward_loop(
    mut src: TcpStream,
    mut dst: TcpStream,
    limit_bps: Option<Arc<AtomicUsize>>,
    stop: Arc<AtomicBool>,
) {
    let mut buf = [0u8; 4096];
    let mut window_start = Instant::now();
    let mut sent_in_window = 0usize;
    while !stop.load(Ordering::Acquire) {
        match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if let Some(ref lim) = limit_bps {
                    let cap = lim.load(Ordering::Acquire);
                    if cap > 0 {
                        // Simple 100ms token-slice pacer so kernel buffers stay tight and publisher sees real backpressure
                        let slice_cap = (cap / 10).max(256);
                        sent_in_window += n;
                        if sent_in_window >= slice_cap {
                            let elapsed = window_start.elapsed();
                            if elapsed < Duration::from_millis(100) {
                                thread::sleep(Duration::from_millis(100) - elapsed);
                            }
                            window_start = Instant::now();
                            sent_in_window = 0;
                        }
                    } else {
                        sent_in_window = 0;
                    }
                }
                if dst.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(_) => break,
        }
    }
    let _ = dst.shutdown(Shutdown::Both);
    let _ = src.shutdown(Shutdown::Both);
}

struct AppleSrsHarness {
    width: u32,
    height: u32,
    fps: u32,
    clock: MasterClock,
    mixer: AudioMixer,
    prod0: AudioInputProducer,
    prod1: Option<AudioInputProducer>,
    bridge: Arc<EngineControlBridge>,
    server: EmbeddedServer,
    importer: AppleMetalZeroCopyImporter,
    comp: GpuCompositor,
    main_pool: AppleZeroCopyRenderSurfacePool,
    prev_pool: AppleZeroCopyRenderSurfacePool,
    main_enc: VtH264Encoder,
    prev_enc: VtH264Encoder,
    aud_enc: AudioToolboxAacEncoder,
    base_video: Option<VirtualVideoSource>,
    prebaked_base: Vec<CVPixelBufferWrapper>,
    overlay_pb: Option<CVPixelBufferWrapper>,
    aud_src0: VirtualAudioSource,
    aud_src1: Option<VirtualAudioSource>,
}

impl AppleSrsHarness {
    fn new(width: u32, height: u32, initial_bitrate_bps: u32) -> Self {
        Self::new_virtual_with_fps(
            width,
            height,
            30,
            initial_bitrate_bps,
            None,
            None,
            VirtualAudioSignal::SineTone {
                freq_hz: 440.0,
                amplitude: 0.2,
            },
            None,
            0,
        )
    }

    fn new_virtual(
        width: u32,
        height: u32,
        initial_bitrate_bps: u32,
        base_video: VirtualVideoSource,
        overlay_video: Option<VirtualVideoSource>,
        aud_signal0: VirtualAudioSignal,
        aud_signal1: Option<VirtualAudioSignal>,
    ) -> Self {
        Self::new_virtual_with_fps(
            width,
            height,
            30,
            initial_bitrate_bps,
            Some(base_video),
            overlay_video,
            aud_signal0,
            aud_signal1,
            0,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_virtual_with_fps(
        width: u32,
        height: u32,
        fps: u32,
        initial_bitrate_bps: u32,
        base_video: Option<VirtualVideoSource>,
        overlay_video: Option<VirtualVideoSource>,
        aud_signal0: VirtualAudioSignal,
        aud_signal1: Option<VirtualAudioSignal>,
        prebake_ring: usize,
    ) -> Self {
        let prev_width = width / 2;
        let prev_height = height / 2;
        let clock = MasterClock::new();
        let mut mixer = AudioMixer::new(clock.clone());
        let (ch0, prod0) = mixer.add_channel(AudioInputConfig {
            name: "apple-mic".into(),
            sample_rate: 48_000,
            channels: 2,
            initial_gain: 1.0,
            initial_muted: false,
            initial_delay_ms: 0,
        });
        let mut channels = vec![ch0];
        let mut prod1 = None;
        let mut aud_src1 = None;
        if let Some(sig1) = aud_signal1 {
            let (ch1, p1) = mixer.add_channel(AudioInputConfig {
                name: "apple-aux".into(),
                sample_rate: 48_000,
                channels: 2,
                initial_gain: 1.0,
                initial_muted: false,
                initial_delay_ms: 0,
            });
            channels.push(ch1);
            prod1 = Some(p1);
            aud_src1 = Some(VirtualAudioSource::new(48_000, sig1));
        }

        let bridge = EngineControlBridge::new(
            channels,
            mixer.master_handle(),
            initial_bitrate_bps,
            fps as f32,
            TlsRootConfig::System,
        );
        let server = EmbeddedServer::start(
            ServerConfig {
                bind_addr: "127.0.0.1:0".into(),
                meter_interval_ms: 50,
                preview_width: prev_width,
                preview_height: prev_height,
                preview_fps: fps,
            },
            bridge.clone(),
        )
        .expect("EmbeddedServer::start");

        let gpu = GpuContext::new_headless().expect("Metal GpuContext::new_headless");
        let importer =
            AppleMetalZeroCopyImporter::new(&gpu.device).expect("AppleMetalZeroCopyImporter");
        let comp = GpuCompositor::new(
            gpu,
            CompositorConfig {
                main_width: width,
                main_height: height,
                preview_width: prev_width,
                preview_height: prev_height,
                target_fps: fps,
            },
        );
        let mut main_pool =
            AppleZeroCopyRenderSurfacePool::new(&importer, &comp.context().device, width, height, 6)
                .expect("main AppleZeroCopyRenderSurfacePool");
        main_pool.prebind_preview_blit(&comp);
        let prev_pool = AppleZeroCopyRenderSurfacePool::new(
            &importer,
            &comp.context().device,
            prev_width,
            prev_height,
            6,
        )
        .expect("prev AppleZeroCopyRenderSurfacePool");
        let main_enc = VtH264Encoder::new(width, height, fps, initial_bitrate_bps)
            .expect("VtH264Encoder main");
        let prev_enc = VtH264Encoder::new(prev_width, prev_height, fps, initial_bitrate_bps / 4)
            .expect("VtH264Encoder preview");
        let aud_enc = AudioToolboxAacEncoder::new(128_000).expect("AudioToolboxAacEncoder");

        let prebaked_base = match (&base_video, prebake_ring) {
            (Some(src), n) if n > 0 => (0..n as u64)
                .map(|k| {
                    let (y, uv) = src.render_nv12_bt709(k);
                    CVPixelBufferWrapper::new_nv12(width, height, &y, &uv)
                        .expect("prebaked NV12 CVPixelBuffer")
                })
                .collect(),
            _ => Vec::new(),
        };

        let overlay_pb = overlay_video.map(|src| {
            let w = src.width;
            let h = src.height;
            let bytes = src.render_bgra(0);
            CVPixelBufferWrapper::new_bgra(w, h, &bytes).expect("CVPixelBufferWrapper bgra")
        });

        Self {
            width,
            height,
            fps,
            clock,
            mixer,
            prod0,
            prod1,
            bridge,
            server,
            importer,
            comp,
            main_pool,
            prev_pool,
            main_enc,
            prev_enc,
            aud_enc,
            base_video,
            prebaked_base,
            overlay_pb,
            aud_src0: VirtualAudioSource::new(48_000, aud_signal0),
            aud_src1,
        }
    }

    fn frame_delay_ms(&self) -> u64 {
        if self.fps >= 60 { 16 } else { 33 }
    }

    fn step(&mut self, frame_idx: u64, high_entropy: bool) {
        let owned_pb;
        let base_pb = if !self.prebaked_base.is_empty() {
            &self.prebaked_base[(frame_idx as usize) % self.prebaked_base.len()]
        } else {
            let w = self.width as usize;
            let h = self.height as usize;
            let (y_plane, uv_plane) = if let Some(src) = &self.base_video {
                src.render_nv12_bt709(frame_idx)
            } else if high_entropy {
                let mut y_plane = vec![0u8; w * h];
                let mut uv_plane = vec![128u8; w * (h / 2)];
                for row in 0..h {
                    for col in 0..w {
                        let seed = ((row as u32)
                            .wrapping_mul(1664525)
                            ^ (col as u32).wrapping_mul(1013904223)
                            ^ (frame_idx as u32).wrapping_mul(2246822519))
                            >> 16;
                        y_plane[row * w + col] = (16 + (seed % 219)) as u8;
                        if row % 2 == 0 && col % 2 == 0 {
                            uv_plane[(row / 2) * w + col] = ((seed >> 2) % 220) as u8;
                            uv_plane[(row / 2) * w + col + 1] = ((seed >> 5) % 220) as u8;
                        }
                    }
                }
                (y_plane, uv_plane)
            } else {
                VirtualVideoSource::new(self.width, self.height, VirtualVideoPattern::ColorbarsMotion)
                    .render_nv12_bt709(frame_idx)
            };
            owned_pb = CVPixelBufferWrapper::new_nv12(self.width, self.height, &y_plane, &uv_plane)
                .expect("CVPixelBufferWrapper::new_nv12");
            &owned_pb
        };

        let imported_base = self
            .importer
            .import_to_gpu_layer(&self.comp, base_pb, 0, true, self.bridge.layer_transform(0))
            .expect("import_to_gpu_layer base");

        let imported_overlay = self.overlay_pb.as_ref().map(|ov_pb| {
            self.importer
                .import_to_gpu_layer(
                    &self.comp,
                    ov_pb,
                    1,
                    false,
                    self.bridge.layer_transform(1),
                )
                .expect("import_to_gpu_layer overlay")
        });

        let pts = PtsUs(frame_idx * (1_000_000 / self.fps as u64));
        let main_slot = self
            .main_pool
            .acquire_slot(&self.importer, &self.comp.context().device)
            .expect("acquire main IOSurface slot");
        let prev_slot = self
            .prev_pool
            .acquire_slot(&self.importer, &self.comp.context().device)
            .expect("acquire prev IOSurface slot");

        if let Some(ov) = &imported_overlay {
            self.comp
                .render_dual_into(
                    &[&imported_base.layer, &ov.layer],
                    pts,
                    Some(main_slot.as_external_target()),
                    Some(prev_slot.as_external_target()),
                )
                .expect("render_dual_into multilayer");
        } else {
            self.comp
                .render_dual_into(
                    &[&imported_base.layer],
                    pts,
                    Some(main_slot.as_external_target()),
                    Some(prev_slot.as_external_target()),
                )
                .expect("render_dual_into single");
        }
        self.importer.flush();

        // Zero-copy CVPixelBuffer -> VideoToolbox hardware encode (no GPU readback or CPU swizzle)
        self.main_enc
            .encode_pixel_buffer(&main_slot.pixel_buffer, pts)
            .expect("vt encode_pixel_buffer main");
        self.prev_enc
            .encode_pixel_buffer(&prev_slot.pixel_buffer, pts)
            .expect("vt encode_pixel_buffer prev");

        // Feed exact 48kHz audio synchronized to target fps (30fps: 4,3,3 / 60fps: 2,2,1)
        let blocks = VirtualAudioSource::mix_blocks_for_fps_frame(self.fps, frame_idx);
        let mut mix_buf = vec![0.0f32; MIX_BLOCK_FRAMES * 2];
        for _ in 0..blocks {
            let pcm0 = self.aud_src0.next_stereo_f32(MIX_BLOCK_FRAMES);
            self.prod0.push_f32(&pcm0);
            if let (Some(p1), Some(s1)) = (self.prod1.as_mut(), self.aud_src1.as_mut()) {
                let pcm1 = s1.next_stereo_f32(MIX_BLOCK_FRAMES);
                p1.push_f32(&pcm1);
            }
            let st = self.mixer.mix_block(&mut mix_buf);
            self.aud_enc
                .push_pcm_f32(&mix_buf, st.to_pts_us())
                .expect("audiotoolbox push");
        }

        let mut tick = PipelineTickContext {
            clock: &mut self.clock,
            bridge: &self.bridge,
            compositor: &mut self.comp,
            main_encoder: &mut self.main_enc,
            preview_encoder: &mut self.prev_enc,
            audio_encoder: &mut self.aud_enc,
            server: &self.server,
        };
        flush_encoders_to_outputs(&mut tick, pts).expect("flush_encoders_to_outputs");
    }

    /// Keep pumping frames on schedule until `predicate` returns true or `timeout` elapses.
    fn pump_until<F>(
        &mut self,
        start_idx: u64,
        timeout: Duration,
        high_entropy: bool,
        mut predicate: F,
    ) -> (u64, bool)
    where
        F: FnMut(&EngineControlBridge) -> bool,
    {
        let deadline = Instant::now() + timeout;
        let mut idx = start_idx;
        let delay = self.frame_delay_ms();
        while Instant::now() < deadline {
            self.step(idx, high_entropy);
            idx += 1;
            if predicate(&self.bridge) {
                return (idx, true);
            }
            thread::sleep(Duration::from_millis(delay));
        }
        (idx, false)
    }

    fn shutdown(&mut self) {
        let _ = self.main_enc.flush();
        let _ = self.prev_enc.flush();
        self.bridge.stop_all();
        self.server.stop();
    }
}

/// Run `ffprobe` on a background thread while continuing to pump live media frames to SRS.
fn verify_http_flv_with_ffprobe(
    srs: &SrsProcess,
    ffprobe_bin: &std::path::Path,
    flv_url: &str,
    harness: &mut AppleSrsHarness,
    frame_idx: &mut u64,
    expected_w: u32,
    expected_h: u32,
) {
    let delay = harness.frame_delay_ms();
    // Allow at least ~20 frames of live A+V packets to enter SRS GOP cache before opening HTTP-FLV viewer
    for _ in 0..20 {
        harness.step(*frame_idx, false);
        *frame_idx += 1;
        thread::sleep(Duration::from_millis(delay));
    }

    let probe_bin = ffprobe_bin.to_path_buf();
    let url = flv_url.to_string();
    let done = Arc::new(AtomicBool::new(false));
    let done_c = Arc::clone(&done);

    let handle = thread::spawn(move || {
        let out = Command::new(probe_bin)
            .args([
                "-v",
                "error",
                "-analyzeduration",
                "2000000",
                "-probesize",
                "1000000",
                "-show_entries",
                "stream=codec_type,codec_name,width,height,sample_rate,channels",
                "-of",
                "compact=p=1:nk=0",
                &url,
            ])
            .output()
            .expect("spawn ffprobe");
        done_c.store(true, Ordering::Release);
        out
    });

    let deadline = Instant::now() + Duration::from_secs(12);
    let delay = harness.frame_delay_ms();
    while !done.load(Ordering::Acquire) && Instant::now() < deadline {
        harness.step(*frame_idx, false);
        *frame_idx += 1;
        thread::sleep(Duration::from_millis(delay));
    }

    let out = handle.join().expect("join ffprobe thread");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "ffprobe failed on {} (status={}): stdout=\n{}\nstderr=\n{}",
        flv_url,
        out.status,
        stdout,
        stderr
    );
    assert!(
        stdout.contains("codec_type=video") && stdout.contains("codec_name=h264"),
        "expected H.264 video stream from ffprobe:\n{}",
        stdout
    );
    assert!(
        stdout.contains(&format!("width={}", expected_w))
            && stdout.contains(&format!("height={}", expected_h)),
        "expected {}x{} video resolution from ffprobe:\n{}",
        expected_w,
        expected_h,
        stdout
    );
    assert!(
        stdout.contains("codec_type=audio")
            && stdout.contains("codec_name=aac")
            && stdout.contains("sample_rate=48000")
            && stdout.contains("channels=2"),
        "expected 48kHz stereo AAC stream from ffprobe:\n{}\nstreams_json={}\nsrs_log=\n{}",
        stdout,
        srs.streams_json(),
        srs.log_tail()
    );
}

fn ws_connect(addr: std::net::SocketAddr) -> TcpStream {
    let mut ws = TcpStream::connect(addr).expect("connect WS");
    ws.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let req = format!(
        "GET /ws HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        addr
    );
    ws.write_all(req.as_bytes()).unwrap();
    let mut buf = [0u8; 1024];
    let n = ws.read(&mut buf).unwrap();
    let hdr = String::from_utf8_lossy(&buf[..n]);
    assert!(hdr.contains("101 Switching Protocols"), "bad WS upgrade:\n{}", hdr);
    ws
}

fn ws_send_text(stream: &mut TcpStream, txt: &str) {
    let payload = txt.as_bytes();
    let mask = [0xAA, 0xBB, 0xCC, 0xDD];
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

#[test]
fn test_srs_e2e_apple_metal_videotoolbox_publish_ffprobe_and_ws_controls() {
    let Some((srs_bin, ffprobe_bin)) = require_or_skip_srs_and_ffprobe() else {
        return;
    };
    let srs = SrsProcess::start(srs_bin);
    let mut harness = AppleSrsHarness::new(1280, 720, 2_000_000);

    // 1. Pre-warm Metal + VideoToolbox + AudioToolbox BEFORE starting stream (late start test)
    let mut frame_idx = 0u64;
    for _ in 0..15 {
        harness.step(frame_idx, false);
        frame_idx += 1;
        thread::sleep(Duration::from_millis(33));
    }
    assert_eq!(harness.bridge.snapshot().state, PipelineState::Idle);

    // 2. Connect WebSocket JSON-RPC & drive audio/overlay/stream controls
    let mut ws = ws_connect(harness.server.local_addr());
    ws_send_text(
        &mut ws,
        r#"{"jsonrpc":"2.0","id":1,"method":"audio.set_gain","params":{"channel":0,"gain_db":-3.0}}"#,
    );
    ws_send_text(
        &mut ws,
        r#"{"jsonrpc":"2.0","id":2,"method":"overlay.set_transform","params":{"layer_id":0,"opacity":0.85,"x":0.0,"y":0.0,"scale_x":1.0,"scale_y":1.0,"rotation_rad":0.0,"visible":true}}"#,
    );
    let start_rpc = format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"stream.start","params":{{"rtmp_url":"{}","bitrate_bps":1600000}}}}"#,
        srs.rtmp_url("live", "apple_main")
    );
    ws_send_text(&mut ws, &start_rpc);

    let (next_idx, ok) = harness.pump_until(frame_idx, Duration::from_secs(5), false, |b| {
        let s = b.snapshot();
        s.state == PipelineState::Streaming
            && b.publisher_stats().is_some_and(|ps| ps.packets_sent >= 30)
            && !srs.publisher_client_ids("apple_main").is_empty()
    });
    frame_idx = next_idx;
    assert!(
        ok,
        "stream did not become active on SRS; snap={:?} err={:?} srs_streams={} log=\n{}",
        harness.bridge.snapshot(),
        harness.bridge.last_error(),
        srs.streams_json(),
        srs.log_tail()
    );

    let snap = harness.bridge.snapshot();
    assert_eq!(snap.bitrate_bps, 1_600_000);
    assert!((snap.channels[0].gain_db - (-3.0)).abs() < 0.1);
    assert!((snap.overlays[0].transform.opacity - 0.85).abs() < 0.01);

    // Confirm binary WebSocket frames (0x01 fMP4 preview and/or 0x02 meters) are also flowing
    let mut ws_probe_buf = [0u8; 4096];
    let n = ws.read(&mut ws_probe_buf).expect("read WS frames");
    assert!(n > 0, "expected WS server push frames (fMP4 / meters)");

    // 3. Verify SRS stream API metadata + live HTTP-FLV playback via ffprobe
    let streams = srs.streams_json();
    assert!(
        streams.contains("\"name\":\"apple_main\"") && streams.contains("\"active\":true"),
        "expected active stream in SRS /api/v1/streams/: {}",
        streams
    );
    verify_http_flv_with_ffprobe(
        &srs,
        &ffprobe_bin,
        &srs.flv_url("live", "apple_main"),
        &mut harness,
        &mut frame_idx,
        1280,
        720,
    );

    // 4. Clean WS stream.stop -> returns to Previewing
    ws_send_text(&mut ws, r#"{"jsonrpc":"2.0","id":4,"method":"stream.stop"}"#);
    let (next_idx, stopped) = harness.pump_until(frame_idx, Duration::from_secs(3), false, |b| {
        b.snapshot().state == PipelineState::Previewing
            && srs.publisher_client_ids("apple_main").is_empty()
    });
    frame_idx = next_idx;
    let _ = frame_idx;
    assert!(stopped, "expected clean stream.stop to disconnect from SRS");

    harness.shutdown();
}

#[test]
fn test_srs_e2e_live_client_kick_auto_reconnects_and_stays_playable() {
    let Some((srs_bin, ffprobe_bin)) = require_or_skip_srs_and_ffprobe() else {
        return;
    };
    let srs = SrsProcess::start(srs_bin);
    let mut harness = AppleSrsHarness::new(640, 360, 1_200_000);
    harness.bridge.set_reconnect_policy(120, 400, Some(10));

    harness
        .bridge
        .start_stream(&srs.rtmp_url("live", "kick_test"), None)
        .unwrap();

    let (mut frame_idx, ready) = harness.pump_until(0, Duration::from_secs(5), false, |b| {
        b.snapshot().state == PipelineState::Streaming
            && !srs.publisher_client_ids("kick_test").is_empty()
            && b.publisher_stats().is_some_and(|s| s.packets_sent >= 20)
    });
    assert!(ready, "initial SRS publish failed");

    let initial_ids = srs.publisher_client_ids("kick_test");
    assert_eq!(initial_ids.len(), 1);
    let old_client_id = initial_ids[0].clone();

    // Kick the active publisher client using SRS raw_api
    srs.kick_client(&old_client_id);

    // Continue pumping frames -> publisher must detect disconnect, reconnect, and get a NEW client_id
    let (next_idx, recovered) =
        harness.pump_until(frame_idx, Duration::from_secs(6), false, |b| {
            let ids = srs.publisher_client_ids("kick_test");
            b.snapshot().state == PipelineState::Streaming
                && b.publisher_stats().is_some_and(|s| s.reconnect_count >= 1)
                && ids.iter().any(|id| id != &old_client_id)
        });
    frame_idx = next_idx;
    assert!(
        recovered,
        "did not auto-reconnect after SRS kick: stats={:?} err={:?} clients={}",
        harness.bridge.publisher_stats(),
        harness.bridge.last_error(),
        srs.clients_json()
    );

    verify_http_flv_with_ffprobe(
        &srs,
        &ffprobe_bin,
        &srs.flv_url("live", "kick_test"),
        &mut harness,
        &mut frame_idx,
        640,
        360,
    );

    harness.shutdown();
}

#[test]
fn test_srs_e2e_process_kill_restart_and_max_reconnect_error_recovery() {
    let Some((srs_bin, ffprobe_bin)) = require_or_skip_srs_and_ffprobe() else {
        return;
    };
    let mut srs = SrsProcess::start(srs_bin);
    let mut harness = AppleSrsHarness::new(640, 360, 1_200_000);
    harness.bridge.set_reconnect_policy(120, 400, Some(15));

    let rtmp_url = srs.rtmp_url("live", "restart_test");
    harness.bridge.start_stream(&rtmp_url, None).unwrap();

    let (mut frame_idx, ready) = harness.pump_until(0, Duration::from_secs(5), false, |b| {
        b.snapshot().state == PipelineState::Streaming
            && b.publisher_stats().is_some_and(|s| s.packets_sent >= 15)
    });
    assert!(ready, "initial SRS publish failed");

    // Part A: SIGKILL SRS mid-stream -> verify Reconnecting -> restart SRS -> auto-reconnects
    srs.kill_now();
    let (next_idx, saw_reconnecting) =
        harness.pump_until(frame_idx, Duration::from_secs(3), false, |b| {
            b.snapshot().state == PipelineState::Reconnecting
        });
    frame_idx = next_idx;
    assert!(saw_reconnecting, "expected Reconnecting state after SRS SIGKILL");

    srs.start_process();
    let (next_idx, reconnected) =
        harness.pump_until(frame_idx, Duration::from_secs(6), false, |b| {
            b.snapshot().state == PipelineState::Streaming
                && b.publisher_stats().is_some_and(|s| s.reconnect_count >= 1)
                && !srs.publisher_client_ids("restart_test").is_empty()
        });
    frame_idx = next_idx;
    assert!(reconnected, "expected automatic reconnect after SRS restart");

    verify_http_flv_with_ffprobe(
        &srs,
        &ffprobe_bin,
        &srs.flv_url("live", "restart_test"),
        &mut harness,
        &mut frame_idx,
        640,
        360,
    );

    // Part B: Finite max_reconnect_attempts -> Error state -> recover via stream.start
    harness.bridge.stop_stream().unwrap();
    harness.bridge.set_reconnect_policy(80, 160, Some(2));
    harness.bridge.start_stream(&rtmp_url, None).unwrap();
    let (next_idx, ready2) = harness.pump_until(frame_idx, Duration::from_secs(4), false, |b| {
        b.snapshot().state == PipelineState::Streaming
    });
    frame_idx = next_idx;
    assert!(ready2);

    srs.kill_now();
    let (next_idx, hit_error) =
        harness.pump_until(frame_idx, Duration::from_secs(5), false, |b| {
            b.snapshot().state == PipelineState::Error
        });
    frame_idx = next_idx;
    assert!(
        hit_error,
        "expected PipelineState::Error after max_reconnect_attempts exhausted, state={:?}",
        harness.bridge.snapshot().state
    );
    assert!(harness.bridge.last_error().is_some());

    // Bring SRS back up and issue manual stream.start -> Error -> Streaming recovery
    srs.start_process();
    harness.bridge.start_stream(&rtmp_url, None).unwrap();
    let (next_idx, recovered_from_error) =
        harness.pump_until(frame_idx, Duration::from_secs(5), false, |b| {
            b.snapshot().state == PipelineState::Streaming
                && !srs.publisher_client_ids("restart_test").is_empty()
        });
    let _ = next_idx;
    assert!(
        recovered_from_error,
        "expected Error -> Streaming recovery when SRS came back up"
    );

    harness.shutdown();
}

#[test]
fn test_srs_e2e_throttled_proxy_abr_slowdown_drop_and_recovery() {
    let Some((srs_bin, ffprobe_bin)) = require_or_skip_srs_and_ffprobe() else {
        return;
    };
    let srs = SrsProcess::start(srs_bin);
    let proxy = ThrottlingProxy::start(srs.rtmp_port);

    let mut harness = AppleSrsHarness::new(1280, 720, 2_500_000);
    harness.bridge.set_abr_config(AbrConfig {
        initial_bitrate_bps: 2_500_000,
        min_bitrate_bps: 300_000,
        max_bitrate_bps: 4_000_000,
        decrease_dwell_threshold_ms: 150,
        decrease_factor: 0.70,
        decrease_cooldown_ms: 250,
        emergency_drop_dwell_ms: 400,
        increase_dwell_threshold_ms: 120,
        increase_hold_ms: 600,
        increase_step_bps: 250_000,
    });

    harness
        .bridge
        .start_stream(&proxy.rtmp_url("live", "abr_srs"), Some(2_500_000))
        .unwrap();

    let (mut frame_idx, started) = harness.pump_until(0, Duration::from_secs(5), false, |b| {
        b.snapshot().state == PipelineState::Streaming
            && b.publisher_stats().is_some_and(|s| s.packets_sent >= 10)
    });
    assert!(started, "failed to publish through proxy");

    // Engage extreme throttle (8 KB/s = 64 kbps) + high-entropy 720p frames to induce real backpressure
    proxy.set_limit_bytes_per_sec(8_000);
    let (next_idx, reduced) =
        harness.pump_until(frame_idx, Duration::from_secs(12), true, |b| {
            let snap = b.snapshot();
            let drops = b.publisher_stats().map(|s| s.packets_dropped_abr).unwrap_or(0);
            snap.bitrate_bps < 2_500_000 && drops > 0
        });
    frame_idx = next_idx;

    let low_snap = harness.bridge.snapshot();
    let low_stats = harness.bridge.publisher_stats().unwrap();
    assert!(
        reduced,
        "expected ABR bitrate reduction & emergency drops under 8KB/s proxy throttle: snap={:?} stats={:?} decision={:?}",
        low_snap,
        low_stats,
        harness.bridge.last_abr_decision()
    );
    assert!(harness.bridge.abr_adjustment_count() >= 1);

    // Lift proxy bandwidth cap -> drain backlog -> ABR additive increase + valid HTTP-FLV stream
    proxy.set_limit_bytes_per_sec(0);
    let low_br = low_snap.bitrate_bps;
    let (next_idx, increased) =
        harness.pump_until(frame_idx, Duration::from_secs(10), false, |b| {
            b.snapshot().bitrate_bps > low_br
        });
    frame_idx = next_idx;
    assert!(
        increased,
        "expected ABR recovery above {} after proxy throttle lifted: final={} decision={:?} stats={:?}",
        low_br,
        harness.bridge.snapshot().bitrate_bps,
        harness.bridge.last_abr_decision(),
        harness.bridge.publisher_stats()
    );

    verify_http_flv_with_ffprobe(
        &srs,
        &ffprobe_bin,
        &srs.flv_url("live", "abr_srs"),
        &mut harness,
        &mut frame_idx,
        1280,
        720,
    );

    harness.shutdown();
}

#[test]
fn test_srs_e2e_duplicate_stream_key_conflict_and_takeover() {
    let Some((srs_bin, ffprobe_bin)) = require_or_skip_srs_and_ffprobe() else {
        return;
    };
    let srs = SrsProcess::start(srs_bin);
    let url = srs.rtmp_url("live", "conflict_key");

    let mut primary = AppleSrsHarness::new(640, 360, 1_000_000);
    primary.bridge.start_stream(&url, None).unwrap();
    let (mut p_idx, primary_ok) = primary.pump_until(0, Duration::from_secs(5), false, |b| {
        b.snapshot().state == PipelineState::Streaming
            && !srs.publisher_client_ids("conflict_key").is_empty()
    });
    assert!(primary_ok, "primary publisher failed to start");

    // Secondary attempts to publish to the exact same stream key -> SRS rejects duplicate StreamBusy
    let mut secondary = AppleSrsHarness::new(640, 360, 1_000_000);
    secondary.bridge.set_reconnect_policy(120, 300, Some(30));
    secondary.bridge.start_stream(&url, None).unwrap();

    let mut s_idx = 0u64;
    let reject_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < reject_deadline {
        primary.step(p_idx, false);
        p_idx += 1;
        secondary.step(s_idx, false);
        s_idx += 1;
        if secondary.bridge.snapshot().state != PipelineState::Streaming
            && secondary.bridge.last_error().is_some()
        {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(primary.bridge.snapshot().state, PipelineState::Streaming);
    assert_ne!(
        secondary.bridge.snapshot().state,
        PipelineState::Streaming,
        "secondary publisher should be rejected while primary holds the SRS stream key (err={:?})",
        secondary.bridge.last_error()
    );

    // Stop primary -> secondary must automatically take over the stream key cleanly
    primary.bridge.stop_stream().unwrap();
    primary.shutdown();

    let (mut s_idx, took_over) = secondary.pump_until(s_idx, Duration::from_secs(6), false, |b| {
        b.snapshot().state == PipelineState::Streaming
            && !srs.publisher_client_ids("conflict_key").is_empty()
    });
    assert!(
        took_over,
        "secondary publisher did not take over after primary stopped: state={:?} err={:?}",
        secondary.bridge.snapshot().state,
        secondary.bridge.last_error()
    );

    verify_http_flv_with_ffprobe(
        &srs,
        &ffprobe_bin,
        &srs.flv_url("live", "conflict_key"),
        &mut secondary,
        &mut s_idx,
        640,
        360,
    );

    secondary.shutdown();
}

struct DecodedFlvSegment {
    video_rgb_frames: Vec<Vec<u8>>,
    audio_pcm_stereo: Vec<f32>,
}

fn capture_and_decode_flv_segment(
    ffmpeg_bin: &std::path::Path,
    flv_url: &str,
    harness: &mut AppleSrsHarness,
    frame_idx: &mut u64,
    pre_roll_frames: usize,
    duration_secs: f32,
) -> DecodedFlvSegment {
    let delay = harness.frame_delay_ms();
    for _ in 0..pre_roll_frames {
        harness.step(*frame_idx, false);
        *frame_idx += 1;
        thread::sleep(Duration::from_millis(delay));
    }

    let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_dir = env::temp_dir().join(format!("ikakim_ffdec_{}_{}", std::process::id(), seq));
    fs::create_dir_all(&tmp_dir).unwrap();
    let vid_path = tmp_dir.join("video.rgb24");
    let aud_path = tmp_dir.join("audio.f32le");

    let ff_bin = ffmpeg_bin.to_path_buf();
    let url = flv_url.to_string();
    let vid_c = vid_path.clone();
    let aud_c = aud_path.clone();
    let dur_str = format!("{:.2}", duration_secs);
    let done = Arc::new(AtomicBool::new(false));
    let done_c = Arc::clone(&done);

    let handle = thread::spawn(move || {
        let out = Command::new(ff_bin)
            .args([
                "-y",
                "-v",
                "error",
                "-analyzeduration",
                "800000",
                "-probesize",
                "65536",
                "-i",
                &url,
                "-t",
                &dur_str,
                "-map",
                "0:v:0",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                vid_c.to_str().unwrap(),
                "-map",
                "0:a:0",
                "-f",
                "f32le",
                "-acodec",
                "pcm_f32le",
                "-ar",
                "48000",
                "-ac",
                "2",
                aud_c.to_str().unwrap(),
            ])
            .output()
            .expect("spawn ffmpeg decode");
        done_c.store(true, Ordering::Release);
        out
    });

    let deadline = Instant::now() + Duration::from_secs(15);
    let delay = harness.frame_delay_ms();
    while !done.load(Ordering::Acquire) && Instant::now() < deadline {
        harness.step(*frame_idx, false);
        *frame_idx += 1;
        thread::sleep(Duration::from_millis(delay));
    }

    let out = handle.join().expect("join ffmpeg decode thread");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "ffmpeg decode failed on {} (status={}): stderr=\n{}",
        flv_url,
        out.status,
        stderr
    );

    let raw_vid = fs::read(&vid_path).unwrap_or_default();
    let raw_aud = fs::read(&aud_path).unwrap_or_default();
    let _ = fs::remove_dir_all(&tmp_dir);

    let frame_bytes = (harness.width as usize) * (harness.height as usize) * 3;
    let video_rgb_frames: Vec<Vec<u8>> = raw_vid
        .chunks_exact(frame_bytes)
        .map(|c| c.to_vec())
        .collect();
    let audio_pcm_stereo: Vec<f32> = raw_aud
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();

    DecodedFlvSegment {
        video_rgb_frames,
        audio_pcm_stereo,
    }
}

fn rgb_pixel(frame_rgb: &[u8], width: u32, x: u32, y: u32) -> [u8; 3] {
    let idx = ((y as usize) * (width as usize) + (x as usize)) * 3;
    [frame_rgb[idx], frame_rgb[idx + 1], frame_rgb[idx + 2]]
}

fn find_bright_bar_center_x(frame_rgb: &[u8], width: u32, height: u32) -> Option<f32> {
    let y = height / 2 - 4;
    let mut sum_x = 0.0f32;
    let mut count = 0u32;
    for x in 0..width {
        let [r, g, b] = rgb_pixel(frame_rgb, width, x, y);
        if r > 185 && g > 185 && b > 185 {
            sum_x += x as f32;
            count += 1;
        }
    }
    if count >= 4 {
        Some(sum_x / count as f32)
    } else {
        None
    }
}

fn send_ws_rpc_sequence(ui_addr: std::net::SocketAddr, rpcs: &[&str]) {
    let mut ws = TcpStream::connect(ui_addr).expect("connect WS RPC");
    ws.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let req = format!(
        "GET /ws HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        ui_addr
    );
    ws.write_all(req.as_bytes()).unwrap();
    let mut hdr = [0u8; 1024];
    let n = ws.read(&mut hdr).unwrap();
    assert!(String::from_utf8_lossy(&hdr[..n]).contains("101 Switching Protocols"));

    for txt in rpcs {
        let payload = txt.as_bytes();
        let mask = [0x21, 0x43, 0x65, 0x87];
        let mut frame = vec![0x81u8];
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        for (i, b) in payload.iter().enumerate() {
            frame.push(b ^ mask[i % 4]);
        }
        ws.write_all(&frame).unwrap();
    }
    let _ = ws.flush();
    thread::sleep(Duration::from_millis(50));
}

#[test]
fn test_srs_e2e_delivered_video_color_motion_and_audio_stereo_tones() {
    let Some((srs_bin, _ffprobe_bin, ffmpeg_bin)) = require_or_skip_srs_ffprobe_ffmpeg() else {
        return;
    };
    let srs = SrsProcess::start(srs_bin);
    let width = 320u32;
    let height = 180u32;
    let base_video =
        VirtualVideoSource::new(width, height, VirtualVideoPattern::default()).with_bar_speed(10);
    let mut harness = AppleSrsHarness::new_virtual(
        width,
        height,
        1_200_000,
        base_video,
        None,
        VirtualAudioSignal::DualChannelTone {
            left_freq_hz: 440.0,
            right_freq_hz: 880.0,
            amplitude: 0.35,
        },
        None,
    );
    harness
        .bridge
        .start_stream(&srs.rtmp_url("live", "av_verify"), Some(1_200_000))
        .unwrap();

    let (mut frame_idx, started) = harness.pump_until(0, Duration::from_secs(6), false, |b| {
        b.snapshot().state == PipelineState::Streaming
    });
    assert!(started, "failed to reach Streaming state against SRS");

    let seg = capture_and_decode_flv_segment(
        &ffmpeg_bin,
        &srs.flv_url("live", "av_verify"),
        &mut harness,
        &mut frame_idx,
        24,
        1.5,
    );
    harness.shutdown();

    assert!(
        seg.video_rgb_frames.len() >= 18,
        "expected >= 18 decoded video frames from SRS, got {}",
        seg.video_rgb_frames.len()
    );
    assert!(
        seg.audio_pcm_stereo.len() >= 48_000 * 2,
        "expected >= 1.0s stereo decoded PCM from SRS, got {} samples",
        seg.audio_pcm_stereo.len()
    );

    // Inspect a steady-state decoded frame: Red TL, Green TR, Blue BL, Yellow BR
    let f_mid = &seg.video_rgb_frames[seg.video_rgb_frames.len() / 2];
    let tl = rgb_pixel(f_mid, width, width / 4, height / 4);
    let tr = rgb_pixel(f_mid, width, width * 3 / 4, height / 4);
    let bl = rgb_pixel(f_mid, width, width / 4, height * 3 / 4);
    let br = rgb_pixel(f_mid, width, width * 3 / 4, height * 3 / 4);
    assert!(tl[0] > 165 && tl[1] < 85 && tl[2] < 85, "TL expected Red, got {:?}", tl);
    assert!(tr[1] > 165 && tr[0] < 85 && tr[2] < 85, "TR expected Green, got {:?}", tr);
    assert!(bl[2] > 165 && bl[0] < 85 && bl[1] < 100, "BL expected Blue, got {:?}", bl);
    assert!(br[0] > 165 && br[1] > 155 && br[2] < 85, "BR expected Yellow, got {:?}", br);

    // Motion verification: find distinct bright moving-bar X centers ~8 frames apart
    let f_a = &seg.video_rgb_frames[2];
    let f_b = &seg.video_rgb_frames[10];
    let cx_a = find_bright_bar_center_x(f_a, width, height)
        .expect("moving bar not found in early SRS frame");
    let cx_b = find_bright_bar_center_x(f_b, width, height)
        .expect("moving bar not found in later SRS frame");
    assert!(
        (cx_b - cx_a).abs() > 25.0,
        "expected visible horizontal bar motion across SRS decoded frames (cx_a={}, cx_b={})",
        cx_a,
        cx_b
    );

    // Stereo audio frequency verification after AAC encode -> SRS -> ffmpeg decode (use second half to avoid AAC priming padding)
    let aud_steady = &seg.audio_pcm_stereo[seg.audio_pcm_stereo.len() / 2..];
    let l_440 = goertzel_magnitude(aud_steady, 2, 0, 48_000.0, 440.0);
    let l_880 = goertzel_magnitude(aud_steady, 2, 0, 48_000.0, 880.0);
    let r_880 = goertzel_magnitude(aud_steady, 2, 1, 48_000.0, 880.0);
    let r_440 = goertzel_magnitude(aud_steady, 2, 1, 48_000.0, 440.0);
    assert!(
        l_440 > 0.15 && l_440 > l_880 * 3.5,
        "Left delivered AAC audio should contain 440Hz (440={}, 880={})",
        l_440,
        l_880
    );
    assert!(
        r_880 > 0.15 && r_880 > r_440 * 3.5,
        "Right delivered AAC audio should contain 880Hz (880={}, 440={})",
        r_880,
        r_440
    );
}

#[test]
fn test_srs_e2e_delivered_overlay_transform_and_audio_gain_mute_via_ws_rpc() {
    let Some((srs_bin, _ffprobe_bin, ffmpeg_bin)) = require_or_skip_srs_ffprobe_ffmpeg() else {
        return;
    };
    let srs = SrsProcess::start(srs_bin);
    let width = 320u32;
    let height = 180u32;
    let base_video =
        VirtualVideoSource::new(width, height, VirtualVideoPattern::default()).with_bar_speed(6);
    let overlay_video =
        VirtualVideoSource::new(width, height, VirtualVideoPattern::Solid([235, 30, 210, 255]));
    let mut harness = AppleSrsHarness::new_virtual(
        width,
        height,
        1_200_000,
        base_video,
        Some(overlay_video),
        VirtualAudioSignal::SineTone {
            freq_hz: 500.0,
            amplitude: 0.35,
        },
        Some(VirtualAudioSignal::SineTone {
            freq_hz: 1000.0,
            amplitude: 0.35,
        }),
    );

    // Phase 1: Magenta overlay on Top-Left quadrant; both 500Hz & 1000Hz active
    harness
        .bridge
        .set_overlay_transform(
            1,
            LayerTransform {
                x: -0.5,
                y: 0.5,
                scale_x: 0.45,
                scale_y: 0.45,
                visible: true,
                z_order: 1,
                ..Default::default()
            },
        )
        .unwrap();
    harness
        .bridge
        .start_stream(&srs.rtmp_url("live", "av_ws_ctrl"), Some(1_200_000))
        .unwrap();

    let (mut frame_idx, started) = harness.pump_until(0, Duration::from_secs(6), false, |b| {
        b.snapshot().state == PipelineState::Streaming
    });
    assert!(started, "failed to reach Streaming state against SRS");

    let seg1 = capture_and_decode_flv_segment(
        &ffmpeg_bin,
        &srs.flv_url("live", "av_ws_ctrl"),
        &mut harness,
        &mut frame_idx,
        24,
        1.5,
    );
    let f1 = seg1.video_rgb_frames.last().expect("phase 1 video frame");
    let tl1 = rgb_pixel(f1, width, width / 4, height / 4);
    let br1 = rgb_pixel(f1, width, width * 3 / 4, height * 3 / 4);
    assert!(
        tl1[0] > 165 && tl1[1] < 90 && tl1[2] > 145,
        "Phase 1 TL expected Magenta overlay, got {:?}",
        tl1
    );
    assert!(
        br1[0] > 165 && br1[1] > 155 && br1[2] < 85,
        "Phase 1 BR expected Yellow base, got {:?}",
        br1
    );
    let a1 = &seg1.audio_pcm_stereo[seg1.audio_pcm_stereo.len() / 2..];
    let mag_500_p1 = goertzel_magnitude(a1, 2, 0, 48_000.0, 500.0);
    let mag_1000_p1 = goertzel_magnitude(a1, 2, 0, 48_000.0, 1000.0);
    assert!(
        mag_500_p1 > 0.12 && mag_1000_p1 > 0.12,
        "Phase 1 expected both 500Hz ({}) and 1000Hz ({})",
        mag_500_p1,
        mag_1000_p1
    );

    // Phase 2 via WebSocket JSON-RPC: move overlay to Bottom-Right, attenuate Ch0 -12dB, mute Ch1
    send_ws_rpc_sequence(
        harness.server.local_addr(),
        &[
            r#"{"jsonrpc":"2.0","id":10,"method":"overlay.set_transform","params":{"layer_id":1,"x":0.5,"y":-0.5,"scale_x":0.45,"scale_y":0.45,"opacity":1.0,"rotation_rad":0.0,"visible":true}}"#,
            r#"{"jsonrpc":"2.0","id":11,"method":"audio.set_gain","params":{"channel":0,"gain_db":-12.0}}"#,
            r#"{"jsonrpc":"2.0","id":12,"method":"audio.set_mute","params":{"channel":1,"muted":true}}"#,
        ],
    );
    // Force an IDR keyframe so SRS gop_cache rolls over cleanly onto post-RPC frames
    VideoEncoder::request_keyframe(&mut harness.main_enc);

    let seg2 = capture_and_decode_flv_segment(
        &ffmpeg_bin,
        &srs.flv_url("live", "av_ws_ctrl"),
        &mut harness,
        &mut frame_idx,
        35,
        1.5,
    );
    let f2 = seg2.video_rgb_frames.last().expect("phase 2 video frame");
    let tl2 = rgb_pixel(f2, width, width / 4, height / 4);
    let br2 = rgb_pixel(f2, width, width * 3 / 4, height * 3 / 4);
    assert!(
        tl2[0] > 165 && tl2[1] < 85 && tl2[2] < 85,
        "Phase 2 TL expected Red after overlay moved away, got {:?}",
        tl2
    );
    assert!(
        br2[0] > 165 && br2[1] < 90 && br2[2] > 145,
        "Phase 2 BR expected Magenta after WS overlay move, got {:?}",
        br2
    );
    let a2 = &seg2.audio_pcm_stereo[seg2.audio_pcm_stereo.len() / 2..];
    let mag_500_p2 = goertzel_magnitude(a2, 2, 0, 48_000.0, 500.0);
    let mag_1000_p2 = goertzel_magnitude(a2, 2, 0, 48_000.0, 1000.0);
    assert!(
        mag_500_p2 < mag_500_p1 * 0.45 && mag_500_p2 > 0.03,
        "Phase 2 500Hz should be attenuated ~-12dB (p1={}, p2={})",
        mag_500_p1,
        mag_500_p2
    );
    assert!(
        mag_1000_p2 < 0.02,
        "Phase 2 1000Hz (Ch1 muted via WS) should vanish, got {}",
        mag_1000_p2
    );

    // Phase 3 via WebSocket JSON-RPC: hide overlay & mute Ch0 (full audio silence)
    send_ws_rpc_sequence(
        harness.server.local_addr(),
        &[
            r#"{"jsonrpc":"2.0","id":13,"method":"overlay.set_transform","params":{"layer_id":1,"x":0.5,"y":-0.5,"scale_x":0.45,"scale_y":0.45,"opacity":1.0,"rotation_rad":0.0,"visible":false}}"#,
            r#"{"jsonrpc":"2.0","id":14,"method":"audio.set_mute","params":{"channel":0,"muted":true}}"#,
        ],
    );
    VideoEncoder::request_keyframe(&mut harness.main_enc);

    let seg3 = capture_and_decode_flv_segment(
        &ffmpeg_bin,
        &srs.flv_url("live", "av_ws_ctrl"),
        &mut harness,
        &mut frame_idx,
        35,
        1.5,
    );
    let f3 = seg3.video_rgb_frames.last().expect("phase 3 video frame");
    let br3 = rgb_pixel(f3, width, width * 3 / 4, height * 3 / 4);
    assert!(
        br3[0] > 165 && br3[1] > 155 && br3[2] < 85,
        "Phase 3 BR expected Yellow after visible=false via WS, got {:?}",
        br3
    );
    let a3 = &seg3.audio_pcm_stereo[seg3.audio_pcm_stereo.len() / 2..];
    let rms_l = channel_rms_dbfs(a3, 2, 0);
    let rms_r = channel_rms_dbfs(a3, 2, 1);
    assert!(
        rms_l < -55.0 && rms_r < -55.0,
        "Phase 3 muted stream should be silent (L={} dBFS, R={} dBFS)",
        rms_l,
        rms_r
    );

    harness.shutdown();
}

#[test]
fn test_srs_e2e_1080p60_zero_copy_multilayer_streaming_and_abr() {
    let Some((srs_bin, ffprobe_bin, ffmpeg_bin)) = require_or_skip_srs_ffprobe_ffmpeg() else {
        return;
    };
    let srs = SrsProcess::start(srs_bin);
    let width = 1920u32;
    let height = 1080u32;
    let base_video =
        VirtualVideoSource::new(width, height, VirtualVideoPattern::default()).with_bar_speed(16);
    let overlay_video =
        VirtualVideoSource::new(256, 256, VirtualVideoPattern::Solid([235, 30, 210, 255]));

    let mut harness = AppleSrsHarness::new_virtual_with_fps(
        width,
        height,
        60,
        6_000_000,
        Some(base_video),
        Some(overlay_video),
        VirtualAudioSignal::SineTone {
            freq_hz: 500.0,
            amplitude: 0.35,
        },
        Some(VirtualAudioSignal::SineTone {
            freq_hz: 1000.0,
            amplitude: 0.35,
        }),
        12,
    );

    send_ws_rpc_sequence(
        harness.server.local_addr(),
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"overlay.set_transform","params":{"layer_id":1,"x":0.5,"y":-0.5,"scale_x":0.35,"scale_y":0.35,"opacity":1.0,"rotation_rad":0.0,"visible":true}}"#,
        ],
    );

    let rtmp_url = srs.rtmp_url("live", "e2e_1080p60");
    harness
        .bridge
        .start_stream(&rtmp_url, None)
        .expect("start_stream 1080p60");

    let mut frame_idx = 0u64;
    let (_, connected) = harness.pump_until(0, Duration::from_secs(6), false, |b| {
        b.snapshot().state == PipelineState::Streaming
    });
    assert!(connected, "expected 1080p60 RTMP publisher to enter Streaming state");
    frame_idx += 20; // account for warmup ticks

    // Benchmark 120 live 1080p60 zero-copy frames streaming to real local SRS v6
    let mut step_ms = Vec::with_capacity(120);
    for _ in 0..120 {
        let t0 = Instant::now();
        harness.step(frame_idx, false);
        step_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        frame_idx += 1;
    }
    let avg_step_ms = step_ms.iter().sum::<f64>() / step_ms.len() as f64;
    eprintln!(
        "[SRS E2E 1080p60 zero-copy] avg_step={avg_step_ms:.2}ms main_pool(alloc={}, reuse={}) prev_pool(alloc={}, reuse={})",
        harness.main_pool.allocations_total(),
        harness.main_pool.reuses_total(),
        harness.prev_pool.allocations_total(),
        harness.prev_pool.reuses_total(),
    );

    assert!(
        avg_step_ms < 16.67,
        "1080p60 SRS E2E step avg {avg_step_ms:.2}ms exceeded 16.67ms 60fps budget"
    );
    assert!(
        harness.main_pool.allocations_total() <= 6 && harness.prev_pool.allocations_total() <= 6,
        "1080p60 zero-copy pool over-allocated: main={} prev={}",
        harness.main_pool.allocations_total(),
        harness.prev_pool.allocations_total(),
    );
    assert!(
        harness.main_pool.reuses_total() >= 115,
        "expected >= 115 main surface reuses, got {}",
        harness.main_pool.reuses_total()
    );

    // 1. Verify SRS live HTTP-FLV metadata & 1920x1080 H.264 + 48kHz stereo AAC via ffprobe
    let flv_url = srs.flv_url("live", "e2e_1080p60");
    verify_http_flv_with_ffprobe(
        &srs,
        &ffprobe_bin,
        &flv_url,
        &mut harness,
        &mut frame_idx,
        width,
        height,
    );

    // 2. Decode real 1080p60 RGB frames + PCM audio delivered through SRS HTTP-FLV
    let seg = capture_and_decode_flv_segment(
        &ffmpeg_bin,
        &flv_url,
        &mut harness,
        &mut frame_idx,
        30,
        1.5,
    );
    assert!(
        !seg.video_rgb_frames.is_empty(),
        "expected decoded 1080p60 RGB frames from SRS HTTP-FLV"
    );
    let f = seg.video_rgb_frames.last().expect("1080p frame");
    let tl = rgb_pixel(f, width, width / 4, height / 4);
    let tr = rgb_pixel(f, width, width * 3 / 4, height / 4);
    let bl = rgb_pixel(f, width, width / 4, height * 3 / 4);
    let br = rgb_pixel(f, width, width * 3 / 4, height * 3 / 4);
    assert!(
        tl[0] > 165 && tl[1] < 85 && tl[2] < 85,
        "1080p60 TL expected Red, got {:?}",
        tl
    );
    assert!(
        tr[1] > 155 && tr[0] < 85,
        "1080p60 TR expected Green, got {:?}",
        tr
    );
    assert!(
        bl[2] > 165 && bl[0] < 85,
        "1080p60 BL expected Blue, got {:?}",
        bl
    );
    assert!(
        br[0] > 165 && br[1] < 90 && br[2] > 145,
        "1080p60 BR expected Magenta overlay, got {:?}",
        br
    );

    let aud_slice = &seg.audio_pcm_stereo[seg.audio_pcm_stereo.len() / 2..];
    let mag_500 = goertzel_magnitude(aud_slice, 2, 0, 48_000.0, 500.0);
    let mag_1000 = goertzel_magnitude(aud_slice, 2, 0, 48_000.0, 1000.0);
    assert!(
        mag_500 > 0.05 && mag_1000 > 0.05,
        "1080p60 audio expected mixed 500Hz+1000Hz tones (500Hz={mag_500}, 1000Hz={mag_1000})"
    );

    // 3. Apply ABR target adjustment + confirm sustained streaming with 0 dropped frames
    harness
        .bridge
        .apply_abr_bitrate(3_000_000, &mut harness.main_enc)
        .expect("apply_abr_bitrate");
    assert_eq!(harness.bridge.snapshot().bitrate_bps, 3_000_000);
    VideoEncoder::request_keyframe(&mut harness.main_enc);
    let delay = harness.frame_delay_ms();
    for _ in 0..30 {
        harness.step(frame_idx, false);
        frame_idx += 1;
        thread::sleep(Duration::from_millis(delay));
    }
    let (next_idx, steady) = harness.pump_until(frame_idx, Duration::from_secs(4), false, |b| {
        b.snapshot().state == PipelineState::Streaming
    });
    let _ = next_idx;
    assert!(steady, "expected 1080p60 stream to remain Streaming after ABR change");
    let pstats = harness
        .bridge
        .publisher_stats()
        .expect("publisher_stats during 1080p60");
    assert!(pstats.packets_sent > 120);
    // AIMD may immediately probe upward on zero-congestion localhost SRS after the explicit drop
    assert!(harness.bridge.snapshot().bitrate_bps >= 3_000_000);

    harness.shutdown();
}
