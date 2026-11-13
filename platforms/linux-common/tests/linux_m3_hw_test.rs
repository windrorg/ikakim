//! Milestone 3 (Issue #7) Linux arm64 ハードウェアコーデック・Surfaceless EGL 3.2・V4L2 M2M・ABR 統合テスト

#![cfg(target_os = "linux")]

use core_audio::mixer::{AudioInputConfig, AudioMixer};
use core_engine::pipeline::EngineControlBridge;
use core_engine::SwH264Encoder;
use core_rtmp::{AbrConfig, AimdAbrController, TlsRootConfig};
use core_types::clock::{MasterClock, PtsUs, MASTER_SAMPLE_RATE};
use core_types::media::{CodecKind, EncodedPacket, NativeBufferHandle, PixelFormat};
use core_types::traits::{VideoDecoder, VideoEncoder};
use core_types::CoreResult;
use linux_common::codec_factory::{sync_abr_step, HardwareProbeReport};
use linux_common::dmabuf_gpu::SurfacelessEglDmaBuf;
use linux_common::drm_gbm::list_drm_nodes;
use linux_common::rockchip_mpp::{
    MppApi, MppBuffer, MppBufferGroup, MppCapability, MppCtx, MppDecoder, MppEncCfg, MppFrame,
    MppH264Encoder, MppPacket, MppSysBackend, RockchipDualEncoders, MPP_CTX_ENC,
    MPP_DEC_SET_EXT_BUF_GROUP, MPP_DEC_SET_INFO_CHANGE_READY, MPP_ENC_GET_HDR_SYNC,
    MPP_ENC_SET_CFG, MPP_ENC_SET_IDR_FRAME, MPP_OK, MPP_VIDEO_CODING_AVC,
};
use linux_common::v4l2::{enumerate_capture_formats, V4l2CaptureDevice};
use linux_common::v4l2_m2m::{
    fourcc_to_string, list_m2m_codecs, V4l2M2mDecoder, V4l2M2mEncoder, V4L2_FOURCC_FWHT,
};
use std::collections::HashMap;
use std::ffi::{c_void, CString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::raw::{c_char, c_int, c_uint};
use std::path::Path;
use std::ptr;
use std::sync::{Arc, Mutex};

const LOCK_EX: c_int = 2;
const O_CREAT_RDWR: c_int = 0o100 | 0o2;

extern "C" {
    fn open(path: *const c_char, oflag: c_int, ...) -> c_int;
    fn flock(fd: c_int, operation: c_int) -> c_int;
    fn memfd_create(name: *const c_char, flags: c_uint) -> c_int;
}

/// 複数の cargo integration test バイナリ間で `/dev/video0` を直列化するプロセス間ファイルロック
#[allow(dead_code)]
struct CrossProcessVideo0Lock(OwnedFd);

impl CrossProcessVideo0Lock {
    fn acquire() -> Self {
        let path = CString::new("/tmp/ikakim_vivid_video0.lock").unwrap();
        let fd = unsafe { open(path.as_ptr(), O_CREAT_RDWR, 0o666) };
        assert!(fd >= 0, "open lockfile failed");
        let rc = unsafe { flock(fd, LOCK_EX) };
        assert_eq!(rc, 0, "flock lockfile failed");
        Self(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

fn make_test_bridge() -> Arc<EngineControlBridge> {
    let clock = MasterClock::new();
    let mut mixer = AudioMixer::new(clock);
    let (ch, _prod) = mixer.add_channel(AudioInputConfig {
        name: "test-ch".into(),
        sample_rate: MASTER_SAMPLE_RATE as u32,
        channels: 2,
        initial_gain: 1.0,
        initial_muted: false,
        initial_delay_ms: 0,
    });
    EngineControlBridge::new(
        vec![ch],
        mixer.master_handle(),
        4_000_000,
        60.0,
        TlsRootConfig::System,
    )
}

#[test]
fn m3_probe_capabilities_and_drm_nodes() {
    let mpp = MppCapability::detect();
    let probe = HardwareProbeReport::detect();
    let drm = list_drm_nodes();
    assert_eq!(mpp.is_available(), probe.mpp.is_available());
    if Path::new("/dev/dri/card0").exists() {
        assert!(!drm.is_empty(), "DRM node should be enumerated");
    }
}

// --- RK3588 MPP インプロセスモック ---

struct MockCtx {
    is_enc: bool,
    width: u32,
    height: u32,
    bps_target: i32,
    idr_pending: bool,
    info_change_pending: bool,
    last_pts: i64,
    out_fd: OwnedFd,
}

struct MockCfg {
    params: HashMap<String, i32>,
}

struct MockBuf {
    fd: RawFd,
    _owned: Option<OwnedFd>,
}

struct MockFrame {
    w: u32,
    h: u32,
    stride: u32,
    fmt: i32,
    pts: i64,
    buf: MppBuffer,
    info_chg: bool,
}

struct MockPacket {
    data: Vec<u8>,
    pts: i64,
}

struct MockMppSys {
    api: MppApi,
    live_ctx: Mutex<Vec<usize>>,
    bitrate_updates: Mutex<Vec<i32>>,
    imported_dmabuf_fds: Mutex<Vec<(RawFd, usize)>>,
    idr_requests: Mutex<usize>,
}

impl MockMppSys {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            api: MppApi {
                size: std::mem::size_of::<MppApi>() as u32,
                version: 1,
                decode: None,
                decode_put_packet: None,
                decode_get_frame: None,
                encode: None,
                encode_put_frame: None,
                encode_get_packet: None,
                poll: None,
                control: None,
                reset: None,
            },
            live_ctx: Mutex::new(Vec::new()),
            bitrate_updates: Mutex::new(Vec::new()),
            imported_dmabuf_fds: Mutex::new(Vec::new()),
            idr_requests: Mutex::new(0),
        })
    }
}

unsafe impl Send for MockMppSys {}
unsafe impl Sync for MockMppSys {}

#[allow(clippy::missing_safety_doc)]
unsafe impl MppSysBackend for MockMppSys {
    unsafe fn create(&self, ctx: *mut MppCtx, mpi: *mut *const MppApi) -> i32 {
        let fd = memfd_create(c"mock_mpp".as_ptr(), 0);
        let c = Box::into_raw(Box::new(MockCtx {
            is_enc: true,
            width: 0,
            height: 0,
            bps_target: 0,
            idr_pending: false,
            info_change_pending: true,
            last_pts: 0,
            out_fd: OwnedFd::from_raw_fd(fd),
        }));
        self.live_ctx.lock().unwrap().push(c as usize);
        *ctx = c as MppCtx;
        *mpi = &self.api as *const MppApi;
        MPP_OK
    }

    unsafe fn init(&self, ctx: MppCtx, ctx_type: i32, _coding: i32) -> i32 {
        let c = &mut *(ctx as *mut MockCtx);
        c.is_enc = ctx_type == MPP_CTX_ENC;
        MPP_OK
    }

    unsafe fn destroy(&self, ctx: MppCtx) -> i32 {
        if !ctx.is_null() {
            self.live_ctx.lock().unwrap().retain(|&p| p != ctx as usize);
            drop(Box::from_raw(ctx as *mut MockCtx));
        }
        MPP_OK
    }

    unsafe fn control(&self, ctx: MppCtx, _mpi: *const MppApi, cmd: u32, param: *mut c_void) -> i32 {
        let c = &mut *(ctx as *mut MockCtx);
        match cmd {
            MPP_ENC_SET_CFG => {
                let cfg = &*(param as *const MockCfg);
                if let Some(&w) = cfg.params.get("prep:width") {
                    c.width = w as u32;
                }
                if let Some(&h) = cfg.params.get("prep:height") {
                    c.height = h as u32;
                }
                if let Some(&bps) = cfg.params.get("rc:bps_target") {
                    c.bps_target = bps;
                    self.bitrate_updates.lock().unwrap().push(bps);
                }
                MPP_OK
            }
            MPP_ENC_GET_HDR_SYNC => {
                let pkt = Box::into_raw(Box::new(MockPacket {
                    // Annex-B SPS (NAL type 7) + PPS (NAL type 8)
                    data: vec![0, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0, 0, 0, 1, 0x68, 0xee, 0x3c],
                    pts: 0,
                }));
                *(param as *mut MppPacket) = pkt as MppPacket;
                MPP_OK
            }
            MPP_ENC_SET_IDR_FRAME => {
                c.idr_pending = true;
                *self.idr_requests.lock().unwrap() += 1;
                MPP_OK
            }
            MPP_DEC_SET_EXT_BUF_GROUP => MPP_OK,
            MPP_DEC_SET_INFO_CHANGE_READY => {
                c.info_change_pending = false;
                MPP_OK
            }
            _ => MPP_OK,
        }
    }

    unsafe fn enc_cfg_init(&self, cfg: *mut MppEncCfg) -> i32 {
        let b = Box::into_raw(Box::new(MockCfg {
            params: HashMap::new(),
        }));
        *cfg = b as MppEncCfg;
        MPP_OK
    }

    unsafe fn enc_cfg_deinit(&self, cfg: MppEncCfg) -> i32 {
        if !cfg.is_null() {
            drop(Box::from_raw(cfg as *mut MockCfg));
        }
        MPP_OK
    }

    unsafe fn enc_cfg_set_s32(&self, cfg: MppEncCfg, name: &str, val: i32) -> i32 {
        let c = &mut *(cfg as *mut MockCfg);
        c.params.insert(name.to_string(), val);
        MPP_OK
    }

    unsafe fn buffer_group_get_internal(&self, group: *mut MppBufferGroup, _buf_type: i32) -> i32 {
        *group = Box::into_raw(Box::new(0u8)) as MppBufferGroup;
        MPP_OK
    }

    unsafe fn buffer_group_put(&self, group: MppBufferGroup) -> i32 {
        if !group.is_null() {
            drop(Box::from_raw(group as *mut u8));
        }
        MPP_OK
    }

    unsafe fn buffer_import_fd(
        &self,
        _group: MppBufferGroup,
        fd: RawFd,
        size: usize,
    ) -> CoreResult<MppBuffer> {
        self.imported_dmabuf_fds.lock().unwrap().push((fd, size));
        let b = Box::into_raw(Box::new(MockBuf { fd, _owned: None }));
        Ok(b as MppBuffer)
    }

    unsafe fn buffer_get_fd(&self, buf: MppBuffer) -> RawFd {
        (*(buf as *const MockBuf)).fd
    }

    unsafe fn buffer_get_ptr(&self, _buf: MppBuffer) -> *mut c_void {
        ptr::null_mut()
    }

    unsafe fn buffer_put(&self, buf: MppBuffer) {
        if !buf.is_null() {
            drop(Box::from_raw(buf as *mut MockBuf));
        }
    }

    unsafe fn frame_init(&self, frame: *mut MppFrame) -> i32 {
        let f = Box::into_raw(Box::new(MockFrame {
            w: 0,
            h: 0,
            stride: 0,
            fmt: 0,
            pts: 0,
            buf: ptr::null_mut(),
            info_chg: false,
        }));
        *frame = f as MppFrame;
        MPP_OK
    }

    unsafe fn frame_deinit(&self, frame: *mut MppFrame) {
        if !frame.is_null() && !(*frame).is_null() {
            let f = Box::from_raw(*frame as *mut MockFrame);
            if !f.buf.is_null() && !f.w == 0 {
                // デコーダ側が生成した MockBuf のみ解放する
                let b = &*(f.buf as *const MockBuf);
                if b._owned.is_some() {
                    drop(Box::from_raw(f.buf as *mut MockBuf));
                }
            }
            *frame = ptr::null_mut();
        }
    }

    unsafe fn frame_set_geometry(
        &self,
        frame: MppFrame,
        w: u32,
        h: u32,
        hor: u32,
        _ver: u32,
        fmt: i32,
    ) {
        let f = &mut *(frame as *mut MockFrame);
        f.w = w;
        f.h = h;
        f.stride = hor;
        f.fmt = fmt;
    }

    unsafe fn frame_set_buffer(&self, frame: MppFrame, buf: MppBuffer) {
        (*(frame as *mut MockFrame)).buf = buf;
    }

    unsafe fn frame_set_pts(&self, frame: MppFrame, pts: i64) {
        (*(frame as *mut MockFrame)).pts = pts;
    }

    unsafe fn frame_get_meta(&self, frame: MppFrame) -> (u32, u32, u32, i32, i64, MppBuffer, bool) {
        let f = &*(frame as *const MockFrame);
        (f.w, f.h, f.stride, f.fmt, f.pts, f.buf, f.info_chg)
    }

    unsafe fn packet_init(&self, packet: *mut MppPacket, data: *const u8, size: usize) -> i32 {
        let slice = if data.is_null() || size == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data, size).to_vec()
        };
        let p = Box::into_raw(Box::new(MockPacket { data: slice, pts: 0 }));
        *packet = p as MppPacket;
        MPP_OK
    }

    unsafe fn packet_init_with_buffer(&self, packet: *mut MppPacket, _buf: MppBuffer) -> i32 {
        let p = Box::into_raw(Box::new(MockPacket {
            data: Vec::new(),
            pts: 0,
        }));
        *packet = p as MppPacket;
        MPP_OK
    }

    unsafe fn packet_deinit(&self, packet: *mut MppPacket) {
        if !packet.is_null() && !(*packet).is_null() {
            drop(Box::from_raw(*packet as *mut MockPacket));
            *packet = ptr::null_mut();
        }
    }

    unsafe fn packet_set_pts(&self, packet: MppPacket, pts: i64) {
        (*(packet as *mut MockPacket)).pts = pts;
    }

    unsafe fn packet_data_slice(&self, packet: MppPacket) -> (&[u8], i64) {
        let p = &*(packet as *const MockPacket);
        (&p.data, p.pts)
    }

    unsafe fn encode_put_frame(&self, ctx: MppCtx, _mpi: *const MppApi, frame: MppFrame) -> i32 {
        let c = &mut *(ctx as *mut MockCtx);
        let f = &*(frame as *const MockFrame);
        c.last_pts = f.pts;
        MPP_OK
    }

    unsafe fn encode_get_packet(
        &self,
        ctx: MppCtx,
        _mpi: *const MppApi,
        packet: *mut MppPacket,
    ) -> i32 {
        let c = &mut *(ctx as *mut MockCtx);
        let data = if c.idr_pending {
            c.idr_pending = false;
            vec![0, 0, 0, 1, 0x65, 0x88, 0x84, 0x21] // IDR NAL (5)
        } else {
            vec![0, 0, 0, 1, 0x41, 0x9a, 0x22] // P-frame NAL (1)
        };
        *packet = Box::into_raw(Box::new(MockPacket {
            data,
            pts: c.last_pts,
        })) as MppPacket;
        MPP_OK
    }

    unsafe fn decode_put_packet(&self, ctx: MppCtx, _mpi: *const MppApi, packet: MppPacket) -> i32 {
        let c = &mut *(ctx as *mut MockCtx);
        c.last_pts = (*(packet as *const MockPacket)).pts;
        MPP_OK
    }

    unsafe fn decode_get_frame(
        &self,
        ctx: MppCtx,
        _mpi: *const MppApi,
        frame: *mut MppFrame,
    ) -> i32 {
        let c = &mut *(ctx as *mut MockCtx);
        let dec_fd = memfd_create(c"mock_dec_dmabuf".as_ptr(), 0);
        let owned_buf_fd = OwnedFd::from_raw_fd(dec_fd);
        let raw_buf_fd = owned_buf_fd.as_raw_fd();
        let buf = Box::into_raw(Box::new(MockBuf {
            fd: raw_buf_fd,
            _owned: Some(owned_buf_fd),
        })) as MppBuffer;
        let f = Box::into_raw(Box::new(MockFrame {
            w: 1920,
            h: 1080,
            stride: 1920,
            fmt: 0,
            pts: c.last_pts,
            buf,
            info_chg: false,
        }));
        let _ = c.out_fd.as_raw_fd();
        *frame = f as MppFrame;
        MPP_OK
    }
}

#[test]
fn m3_rk3588_mpp_dual_encoder_dmabuf_abr_and_decoder() {
    let sys = MockMppSys::new();
    let main_enc = MppH264Encoder::with_backend(sys.clone(), 1920, 1080, 60, 6_000_000)
        .expect("MPP main encoder init");
    let preview_enc = MppH264Encoder::with_backend(sys.clone(), 1280, 720, 30, 1_500_000)
        .expect("MPP preview encoder init");
    assert_eq!(main_enc.fps(), 60);
    assert_eq!(preview_enc.dimensions(), (1280, 720));

    let mut dual = RockchipDualEncoders {
        main: main_enc,
        preview: preview_enc,
    };

    // 1. Sequence header が生成されていること
    let seq_main = dual.main.pull_packet().unwrap().expect("main seq header");
    assert!(seq_main.is_sequence_header);
    assert!(seq_main.is_keyframe);
    let _seq_prev = dual
        .preview
        .pull_packet()
        .unwrap()
        .expect("preview seq header");

    // 2. DMA-BUF FD を直接ゼロコピーエンコード投入
    let raw_dmabuf = unsafe { memfd_create(c"nv12_dmabuf".as_ptr(), 0) };
    let owned_dmabuf = unsafe { OwnedFd::from_raw_fd(raw_dmabuf) };
    let dmabuf_size = 1920 * 1080 * 3 / 2;

    dual.main
        .encode_dmabuf(
            owned_dmabuf.as_raw_fd(),
            dmabuf_size,
            1920,
            1080,
            PixelFormat::Nv12,
            PtsUs(16_666),
        )
        .unwrap();
    dual.preview
        .encode_dmabuf(
            owned_dmabuf.as_raw_fd(),
            1280 * 720 * 3 / 2,
            1280,
            720,
            PixelFormat::Nv12,
            PtsUs(16_666),
        )
        .unwrap();

    let idr = dual.main.pull_packet().unwrap().expect("IDR packet");
    assert!(idr.is_keyframe);
    assert_eq!(idr.pts, PtsUs(16_666));
    assert_eq!(sys.imported_dmabuf_fds.lock().unwrap().len(), 2);

    // 3. AIMD ABR -> EngineControlBridge -> MppH264Encoder の動的ビットレート更新 + 復帰時 IDR 要求
    let mut abr = AimdAbrController::new(AbrConfig::default());
    let bridge = make_test_bridge();
    // 高キュー滞留 (>1000ms) でビットレート削減 & 緊急ドロップ突入
    let updated = sync_abr_step(&mut abr, &bridge, &mut dual.main, 900_000, 1200)
        .expect("ABR step succeeds");
    assert!(updated.is_some(), "congestion must lower bitrate");
    assert!(dual.main.bitrate_bps() < 6_000_000);
    // 滞留解消後に IDR キーフレーム要求が出ることを検証する
    let _ = sync_abr_step(&mut abr, &bridge, &mut dual.main, 10_000, 200)
        .expect("ABR recovery step succeeds");

    dual.main
        .encode_dmabuf(
            owned_dmabuf.as_raw_fd(),
            dmabuf_size,
            1920,
            1080,
            PixelFormat::Nv12,
            PtsUs(33_333),
        )
        .unwrap();
    let recovered_idr = dual.main.pull_packet().unwrap().expect("IDR after ABR");
    assert!(recovered_idr.is_keyframe);
    assert!(*sys.idr_requests.lock().unwrap() >= 2);

    // 4. MPP デコーダが DMA-BUF VideoFrame を出力すること
    let mut dec =
        MppDecoder::with_backend(sys.clone(), MPP_VIDEO_CODING_AVC).expect("MPP decoder init");
    dec.push_packet(&EncodedPacket {
        codec: CodecKind::H264,
        pts: PtsUs(50_000),
        dts: PtsUs(50_000),
        is_keyframe: true,
        is_sequence_header: false,
        data: vec![0, 0, 0, 1, 0x65, 0x88],
    })
    .unwrap();
    let decoded = dec.pull_frame().unwrap().expect("decoded DMA-BUF frame");
    assert_eq!(decoded.width, 1920);
    assert_eq!(decoded.height, 1080);
    match decoded.handle {
        NativeBufferHandle::DmaBuf { fd, stride, .. } => {
            assert!(fd.as_raw_fd() >= 0);
            assert_eq!(stride, 1920);
        }
        other => panic!("expected DmaBuf, got {:?}", other),
    }

    drop(dual);
    drop(dec);
    assert!(
        sys.live_ctx.lock().unwrap().is_empty(),
        "all MPP contexts must be destroyed cleanly"
    );
}

#[test]
fn m3_surfaceless_egl_gles32_uvc_formats_and_dmabuf_import() {
    if !Path::new("/dev/video0").exists() {
        return;
    }
    let _lock = CrossProcessVideo0Lock::acquire();

    let fmts = enumerate_capture_formats("/dev/video0").expect("VIDIOC_ENUM_FMT /dev/video0");
    assert!(!fmts.is_empty(), "capture device must expose formats");
    assert!(
        fmts.iter().any(|f| f.fourcc_str == "YUYV"),
        "expected YUYV in {:?}",
        fmts
    );

    let egl = SurfacelessEglDmaBuf::new().expect("Surfaceless EGL init");
    let ver = egl.gles_version();
    assert!(
        ver.contains("OpenGL ES 3."),
        "expected OpenGL ES 3.x, got {}",
        ver
    );
    assert!(
        egl.verify_gles32_external_shader()
            .expect("shader verify"),
        "GL_OES_EGL_image_external_essl3 shader compile must succeed"
    );

    let mut cap = V4l2CaptureDevice::open("/dev/video0", 640, 360, PixelFormat::Yuyv, 4)
        .expect("open vivid");
    cap.start_streaming().expect("streamon");
    let frame = cap
        .capture_frame(PtsUs(1000), 2000)
        .expect("capture YUYV dmabuf");
    let NativeBufferHandle::DmaBuf {
        ref fd,
        offset,
        stride,
        ..
    } = frame.handle
    else {
        panic!("expected DmaBuf from V4L2");
    };
    let tex = egl
        .bind_dmabuf_external_texture(
            fd.as_raw_fd(),
            640,
            360,
            PixelFormat::Yuyv,
            offset,
            stride,
        )
        .expect("EGLImage DMA-BUF import");
    assert!(tex.gl_tex != 0);
}

#[test]
fn m3_v4l2_m2m_vicodec_dmabuf_encode_decode_roundtrip() {
    let codecs = list_m2m_codecs();
    let Some(enc_dev) = codecs.iter().find(|c| c.supports_encode_fourcc(V4L2_FOURCC_FWHT)) else {
        return;
    };
    let Some(dec_dev) = codecs.iter().find(|c| c.supports_decode_fourcc(V4L2_FOURCC_FWHT)) else {
        return;
    };

    assert_eq!(fourcc_to_string(V4L2_FOURCC_FWHT), "FWHT");
    // 16x16 マクロブロック整列された 640x480 で vivid -> vicodec DMA-BUF ゼロコピー経路を検証
    let mut m2m_enc = V4l2M2mEncoder::open(
        &enc_dev.path,
        640,
        480,
        PixelFormat::Nv12,
        V4L2_FOURCC_FWHT,
        2_000_000,
        30,
    )
    .expect("open vicodec encoder");

    // DMA-BUF または NV12 生フレームでエンコードし FWHT パケットを得る
    if Path::new("/dev/video0").exists() {
        let _lock = CrossProcessVideo0Lock::acquire();
        let mut cap = V4l2CaptureDevice::open("/dev/video0", 640, 480, PixelFormat::Nv12, 4)
            .expect("open vivid NV12");
        cap.start_streaming().expect("vivid streamon");
        let cap_frame = cap
            .capture_frame(PtsUs(16_666), 2000)
            .expect("vivid dmabuf");
        let NativeBufferHandle::DmaBuf { fd, .. } = cap_frame.handle else {
            panic!("expected DMA-BUF handle");
        };
        m2m_enc
            .encode_dmabuf(fd.as_raw_fd(), 640 * 480 * 3 / 2, PtsUs(16_666))
            .expect("vicodec encode from DMA-BUF");
        assert_eq!(m2m_enc.out_memory_mode(), "DMABUF");
    } else {
        let nv12 = vec![128u8; 640 * 480 * 3 / 2];
        m2m_enc
            .encode_raw_slice(&nv12, PtsUs(16_666))
            .expect("vicodec encode raw");
    }

    let pkt = m2m_enc
        .pull_packet()
        .expect("pull encoded packet")
        .expect("vicodec produced FWHT packet");
    assert!(!pkt.data.is_empty(), "encoded FWHT payload must be non-empty");
    m2m_enc.set_target_bitrate(1_200_000).expect("set bitrate");
    assert_eq!(m2m_enc.bitrate_bps(), 1_200_000);

    let mut m2m_dec = V4l2M2mDecoder::open(
        &dec_dev.path,
        640,
        480,
        V4L2_FOURCC_FWHT,
        PixelFormat::Nv12,
    )
    .expect("open vicodec decoder");
    m2m_dec.push_packet(&pkt).expect("vicodec decode packet");
    let dec_frame = m2m_dec
        .pull_frame()
        .expect("pull decoded frame")
        .expect("vicodec produced DMA-BUF frame");
    assert_eq!(dec_frame.width, 640);
    assert_eq!(dec_frame.height, 480);
    assert!(matches!(
        dec_frame.handle,
        NativeBufferHandle::DmaBuf { .. }
    ));
}

#[test]
fn m3_abr_bridge_with_sw_h264_encoder() {
    let mut abr = AimdAbrController::new(AbrConfig::default());
    let bridge = make_test_bridge();
    let mut enc = SwH264Encoder::new(640, 360, 4_000_000);

    // 過負荷を発生させてビットレート低下＆正常動作を確認
    let new_bps = sync_abr_step(&mut abr, &bridge, &mut enc, 700_000, 1200).unwrap();
    assert!(new_bps.is_some());
    assert!(new_bps.unwrap() < 4_000_000);
}
