//! Linux V4L2 Memory-to-Memory (M2M) ハードウェアコーデック統合
//! Amlogic S905X2 (`meson-vdec` / stateful H.264/MJPEG M2M) および
//! Linux カーネル `vicodec` (`FWHT` V4L2 M2M テストドライバ) に対応。

use core_types::clock::PtsUs;
use core_types::media::{CodecKind, EncodedPacket, NativeBufferHandle, PixelFormat, VideoFrame};
use core_types::traits::{VideoDecoder, VideoEncoder};
use core_types::{CoreError, CoreResult};
use std::collections::VecDeque;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::raw::{c_char, c_int, c_uint, c_ulong, c_void};
use std::ptr;

const O_RDWR: c_int = 2;
const O_RDONLY: c_int = 0;
const O_NONBLOCK: c_int = 0o4000;
const O_CLOEXEC: c_int = 0o2000000;
const F_DUPFD_CLOEXEC: c_int = 1030;
const PROT_READ: c_int = 1;
const PROT_WRITE: c_int = 2;
const MAP_SHARED: c_int = 1;
const MAP_FAILED: *mut c_void = !0usize as *mut c_void;
const POLLIN: i16 = 0x0001;

const VIDIOC_QUERYCAP: c_ulong = 0x8068_5600;
const VIDIOC_ENUM_FMT: c_ulong = 0xc040_5602;
const VIDIOC_S_FMT: c_ulong = 0xc0d0_5605;
const VIDIOC_REQBUFS: c_ulong = 0xc014_5608;
const VIDIOC_QUERYBUF: c_ulong = 0xc058_5609;
const VIDIOC_QBUF: c_ulong = 0xc058_560f;
const VIDIOC_EXPBUF: c_ulong = 0xc040_5610;
const VIDIOC_DQBUF: c_ulong = 0xc058_5611;
const VIDIOC_STREAMON: c_ulong = 0x4004_5612;
const VIDIOC_STREAMOFF: c_ulong = 0x4004_5613;
const VIDIOC_S_CTRL: c_ulong = 0xc008_561c;
const VIDIOC_ENCODER_CMD: c_ulong = 0xc028_564d;

const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const V4L2_BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
const V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE: u32 = 9;
const V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE: u32 = 10;

const V4L2_MEMORY_MMAP: u32 = 1;
const V4L2_MEMORY_DMABUF: u32 = 4;
const V4L2_FIELD_NONE: u32 = 1;

const V4L2_CAP_VIDEO_M2M_MPLANE: u32 = 0x0000_4000;
const V4L2_CAP_VIDEO_M2M: u32 = 0x0000_8000;
const V4L2_CAP_STREAMING: u32 = 0x0400_0000;
const V4L2_BUF_FLAG_KEYFRAME: u32 = 0x0000_0008;

// V4L2 MPEG/Video Controls (probe_m3 検証済み値)
pub const V4L2_CID_MPEG_VIDEO_GOP_SIZE: u32 = 0x0099_09cb;
pub const V4L2_CID_MPEG_VIDEO_BITRATE: u32 = 0x0099_09cf;
pub const V4L2_CID_MPEG_VIDEO_HEADER_MODE: u32 = 0x0099_09d8;
pub const V4L2_CID_MPEG_VIDEO_FORCE_KEY_FRAME: u32 = 0x0099_09e5;

pub const V4L2_FOURCC_H264: u32 = u32::from_le_bytes(*b"H264");
pub const V4L2_FOURCC_MJPEG: u32 = u32::from_le_bytes(*b"MJPG");
pub const V4L2_FOURCC_FWHT: u32 = u32::from_le_bytes(*b"FWHT");
pub const V4L2_FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");
pub const V4L2_FOURCC_YUYV: u32 = u32::from_le_bytes(*b"YUYV");
pub const V4L2_FOURCC_YU12: u32 = u32::from_le_bytes(*b"YU12");

extern "C" {
    fn open(path: *const c_char, oflag: c_int, ...) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    fn poll(fds: *mut PollFd, nfds: c_ulong, timeout: c_int) -> c_int;
    fn mmap(
        addr: *mut c_void,
        length: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(addr: *mut c_void, length: usize) -> c_int;
}

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

#[repr(C)]
struct V4l2Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct V4l2FmtDesc {
    index: u32,
    buf_type: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2PlanePixFormat {
    sizeimage: u32,
    bytesperline: u32,
    reserved: [u16; 6],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2PixFormatMplane {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    colorspace: u32,
    plane_fmt: [V4l2PlanePixFormat; 8],
    num_planes: u8,
    flags: u8,
    ycbcr_enc: u8,
    quantization: u8,
    xfer_func: u8,
    reserved: [u8; 7],
}

#[repr(C)]
union V4l2FormatUnion {
    pix: V4l2PixFormat,
    pix_mp: V4l2PixFormatMplane,
    raw_data: [u8; 200],
}

#[repr(C)]
struct V4l2Format {
    buf_type: u32,
    _pad: u32,
    fmt: V4l2FormatUnion,
}

#[repr(C)]
struct V4l2RequestBuffers {
    count: u32,
    buf_type: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2Plane {
    bytesused: u32,
    length: u32,
    m_mem_offset_or_fd: usize,
    data_offset: u32,
    reserved: [u32; 11],
}

#[repr(C)]
struct V4l2Buffer {
    index: u32,
    buf_type: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    _pad0: u32,
    timestamp_sec: i64,
    timestamp_usec: i64,
    timecode: [u32; 4],
    sequence: u32,
    memory: u32,
    m_offset_or_planes_ptr: usize,
    length: u32,
    reserved2: u32,
    reserved: u32,
    _tail_pad: u32,
}

#[repr(C)]
struct V4l2ExportBuffer {
    buf_type: u32,
    index: u32,
    plane: u32,
    flags: u32,
    fd: c_int,
    reserved: [u32; 11],
}

#[repr(C)]
struct V4l2Control {
    id: u32,
    value: i32,
}

#[derive(Debug, Clone)]
pub struct V4l2M2mCodecInfo {
    pub path: String,
    pub driver: String,
    pub card: String,
    pub is_mplane: bool,
    pub output_fourccs: Vec<u32>,
    pub capture_fourccs: Vec<u32>,
}

impl V4l2M2mCodecInfo {
    pub fn supports_encode_fourcc(&self, coded: u32) -> bool {
        self.capture_fourccs.contains(&coded)
    }

    pub fn supports_decode_fourcc(&self, coded: u32) -> bool {
        self.output_fourccs.contains(&coded)
    }
}

pub fn fourcc_to_string(fourcc: u32) -> String {
    let b = fourcc.to_le_bytes();
    b.iter()
        .map(|&c| if c.is_ascii_graphic() || c == b' ' { c as char } else { '?' })
        .collect()
}

/// `/dev/video0`..`/dev/video23` を走査して V4L2 M2M エンコーダ／デコーダを列挙する
pub fn list_m2m_codecs() -> Vec<V4l2M2mCodecInfo> {
    let mut out = Vec::new();
    for idx in 0..24 {
        let path = format!("/dev/video{}", idx);
        let Ok(cpath) = CString::new(path.as_str()) else {
            continue;
        };
        let fd = unsafe { open(cpath.as_ptr(), O_RDWR | O_CLOEXEC) };
        if fd < 0 {
            continue;
        }
        let mut cap: V4l2Capability = unsafe { std::mem::zeroed() };
        let ok = unsafe { ioctl(fd, VIDIOC_QUERYCAP, &mut cap) } == 0;
        if ok {
            let eff_caps = if cap.device_caps != 0 {
                cap.device_caps
            } else {
                cap.capabilities
            };
            let is_mplane = (eff_caps & V4L2_CAP_VIDEO_M2M_MPLANE) != 0;
            let is_single = (eff_caps & V4L2_CAP_VIDEO_M2M) != 0;
            if (is_mplane || is_single) && (eff_caps & V4L2_CAP_STREAMING) != 0 {
                let out_type = if is_mplane {
                    V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE
                } else {
                    V4L2_BUF_TYPE_VIDEO_OUTPUT
                };
                let cap_type = if is_mplane {
                    V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE
                } else {
                    V4L2_BUF_TYPE_VIDEO_CAPTURE
                };
                out.push(V4l2M2mCodecInfo {
                    path,
                    driver: c_bytes_to_string(&cap.driver),
                    card: c_bytes_to_string(&cap.card),
                    is_mplane,
                    output_fourccs: enum_fourccs(fd, out_type),
                    capture_fourccs: enum_fourccs(fd, cap_type),
                });
            }
        }
        unsafe { close(fd) };
    }
    out
}

fn enum_fourccs(fd: RawFd, buf_type: u32) -> Vec<u32> {
    let mut list = Vec::new();
    for idx in 0..64 {
        let mut desc: V4l2FmtDesc = unsafe { std::mem::zeroed() };
        desc.index = idx;
        desc.buf_type = buf_type;
        if unsafe { ioctl(fd, VIDIOC_ENUM_FMT, &mut desc) } < 0 {
            break;
        }
        list.push(desc.pixelformat);
    }
    list
}

fn c_bytes_to_string(buf: &[u8]) -> String {
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

struct MmapSlot {
    ptr: *mut c_void,
    len: usize,
}

impl Drop for MmapSlot {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.ptr != MAP_FAILED && self.len > 0 {
            unsafe { munmap(self.ptr, self.len) };
            self.ptr = ptr::null_mut();
        }
    }
}

/// V4L2 M2M ハードウェアエンコーダ（Amlogic S905X2 `H264` & カーネル `vicodec` `FWHT` 対応）
pub struct V4l2M2mEncoder {
    fd: OwnedFd,
    is_mplane: bool,
    out_buf_type: u32,
    cap_buf_type: u32,
    out_memory: u32,
    width: u32,
    height: u32,
    coded_fourcc: u32,
    out_sizeimage: u32,
    bitrate_bps: u32,
    out_mmaps: Vec<MmapSlot>,
    cap_mmaps: Vec<MmapSlot>,
    next_out_idx: usize,
    force_keyframe: bool,
    streaming: bool,
    queue: VecDeque<EncodedPacket>,
}

unsafe impl Send for V4l2M2mEncoder {}

impl V4l2M2mEncoder {
    /// Amlogic `H264` または指定 `coded_fourcc` を出力する M2M エンコーダを開く
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        device_path: &str,
        width: u32,
        height: u32,
        raw_format: PixelFormat,
        coded_fourcc: u32,
        bitrate_bps: u32,
        fps: u32,
    ) -> CoreResult<Self> {
        let raw_fourcc = match raw_format {
            PixelFormat::Nv12 => V4L2_FOURCC_NV12,
            PixelFormat::Yuyv => V4L2_FOURCC_YUYV,
            other => {
                return Err(CoreError::UnsupportedFormat(format!(
                    "V4L2 M2M encoder expects NV12 or YUYV raw input, got {:?}",
                    other
                )))
            }
        };

        let cpath = CString::new(device_path)
            .map_err(|e| CoreError::PlatformIo(format!("invalid path: {}", e)))?;
        let raw_fd = unsafe { open(cpath.as_ptr(), O_RDWR | O_CLOEXEC | O_NONBLOCK) };
        if raw_fd < 0 {
            return Err(CoreError::PlatformIo(format!(
                "open({}) failed: {}",
                device_path,
                std::io::Error::last_os_error()
            )));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

        let mut cap: V4l2Capability = unsafe { std::mem::zeroed() };
        if unsafe { ioctl(fd.as_raw_fd(), VIDIOC_QUERYCAP, &mut cap) } < 0 {
            return Err(CoreError::PlatformIo("VIDIOC_QUERYCAP failed".into()));
        }
        let eff_caps = if cap.device_caps != 0 {
            cap.device_caps
        } else {
            cap.capabilities
        };
        let is_mplane = (eff_caps & V4L2_CAP_VIDEO_M2M_MPLANE) != 0;
        let out_buf_type = if is_mplane {
            V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE
        } else {
            V4L2_BUF_TYPE_VIDEO_OUTPUT
        };
        let cap_buf_type = if is_mplane {
            V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE
        } else {
            V4L2_BUF_TYPE_VIDEO_CAPTURE
        };

        // 1. OUTPUT (raw frame input) フォーマット設定
        let out_sizeimage = set_m2m_fmt(fd.as_raw_fd(), out_buf_type, is_mplane, width, height, raw_fourcc)?;
        // 2. CAPTURE (compressed H264/FWHT output) フォーマット設定
        let _cap_size = set_m2m_fmt(fd.as_raw_fd(), cap_buf_type, is_mplane, width, height, coded_fourcc)?;

        // 3. Bitrate / GOP 初期制御 (S905X2 meson / v4l2-ctrls)
        let _ = set_v4l2_ctrl(fd.as_raw_fd(), V4L2_CID_MPEG_VIDEO_BITRATE, bitrate_bps as i32);
        let _ = set_v4l2_ctrl(fd.as_raw_fd(), V4L2_CID_MPEG_VIDEO_GOP_SIZE, (fps.max(1) * 2) as i32);

        // 4. OUTPUT バッファ: DMA-BUF を試行し、非対応なら MMAP へフォールバック
        let mut out_memory = V4L2_MEMORY_DMABUF;
        let mut out_req = V4l2RequestBuffers {
            count: 2,
            buf_type: out_buf_type,
            memory: out_memory,
            capabilities: 0,
            flags: 0,
            reserved: [0; 3],
        };
        let mut out_mmaps = Vec::new();
        if unsafe { ioctl(fd.as_raw_fd(), VIDIOC_REQBUFS, &mut out_req) } < 0 || out_req.count == 0 {
            out_memory = V4L2_MEMORY_MMAP;
            out_req.count = 2;
            out_req.memory = out_memory;
            if unsafe { ioctl(fd.as_raw_fd(), VIDIOC_REQBUFS, &mut out_req) } < 0 || out_req.count == 0 {
                return Err(CoreError::Codec(format!(
                    "V4L2 M2M OUTPUT REQBUFS failed: {}",
                    std::io::Error::last_os_error()
                )));
            }
            for idx in 0..out_req.count {
                out_mmaps.push(query_and_mmap(fd.as_raw_fd(), out_buf_type, is_mplane, idx)?);
            }
        }

        // 5. CAPTURE バッファ: 圧縮ストリーム受け取り用 MMAP
        let mut cap_req = V4l2RequestBuffers {
            count: 2,
            buf_type: cap_buf_type,
            memory: V4L2_MEMORY_MMAP,
            capabilities: 0,
            flags: 0,
            reserved: [0; 3],
        };
        if unsafe { ioctl(fd.as_raw_fd(), VIDIOC_REQBUFS, &mut cap_req) } < 0 || cap_req.count == 0 {
            return Err(CoreError::Codec(format!(
                "V4L2 M2M CAPTURE REQBUFS failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        let mut cap_mmaps = Vec::with_capacity(cap_req.count as usize);
        for idx in 0..cap_req.count {
            let slot = query_and_mmap(fd.as_raw_fd(), cap_buf_type, is_mplane, idx)?;
            queue_mmap_buf(fd.as_raw_fd(), cap_buf_type, is_mplane, idx, 0)?;
            cap_mmaps.push(slot);
        }

        let mut enc = Self {
            fd,
            is_mplane,
            out_buf_type,
            cap_buf_type,
            out_memory,
            width,
            height,
            coded_fourcc,
            out_sizeimage,
            bitrate_bps,
            out_mmaps,
            cap_mmaps,
            next_out_idx: 0,
            force_keyframe: true,
            streaming: false,
            queue: VecDeque::new(),
        };
        enc.start_streaming()?;
        Ok(enc)
    }

    pub fn out_memory_mode(&self) -> &'static str {
        if self.out_memory == V4L2_MEMORY_DMABUF {
            "DMABUF"
        } else {
            "MMAP"
        }
    }

    pub fn coded_fourcc(&self) -> u32 {
        self.coded_fourcc
    }

    pub fn bitrate_bps(&self) -> u32 {
        self.bitrate_bps
    }

    pub fn start_streaming(&mut self) -> CoreResult<()> {
        if self.streaming {
            return Ok(());
        }
        let mut out_t: c_uint = self.out_buf_type;
        let mut cap_t: c_uint = self.cap_buf_type;
        if unsafe { ioctl(self.fd.as_raw_fd(), VIDIOC_STREAMON, &mut out_t) } < 0 {
            return Err(CoreError::Codec(format!(
                "M2M OUTPUT STREAMON failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        if unsafe { ioctl(self.fd.as_raw_fd(), VIDIOC_STREAMON, &mut cap_t) } < 0 {
            return Err(CoreError::Codec(format!(
                "M2M CAPTURE STREAMON failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        self.streaming = true;
        Ok(())
    }

    /// `DMA-BUF` FD を V4L2 M2M エンコーダへ投入（ドライバが MMAP のみなら DMA-BUF mmap フォールバック）
    pub fn encode_dmabuf(&mut self, dmabuf_fd: RawFd, payload_len: usize, pts: PtsUs) -> CoreResult<()> {
        if self.force_keyframe {
            let _ = set_v4l2_ctrl(self.fd.as_raw_fd(), V4L2_CID_MPEG_VIDEO_FORCE_KEY_FRAME, 1);
            self.force_keyframe = false;
        }

        let len_to_use = payload_len.min(self.out_sizeimage as usize).max(1);
        let idx = self.next_out_idx;
        self.next_out_idx = (self.next_out_idx + 1) % 2;

        if self.out_memory == V4L2_MEMORY_DMABUF {
            if queue_dmabuf_buf(
                self.fd.as_raw_fd(),
                self.out_buf_type,
                self.is_mplane,
                idx as u32,
                dmabuf_fd,
                len_to_use as u32,
                self.out_sizeimage,
            )
            .is_ok()
            {
                return self.dequeue_exchange(pts);
            }
            // ドライバ / foreign DMA-BUF 間でゼロコピーインポート不可時は MMAP へ安全にフォールバック
            self.switch_output_to_mmap()?;
        }
        {
            let mapped = unsafe {
                mmap(
                    ptr::null_mut(),
                    len_to_use,
                    PROT_READ,
                    MAP_SHARED,
                    dmabuf_fd,
                    0,
                )
            };
            if mapped == MAP_FAILED {
                return Err(CoreError::PlatformIo(format!(
                    "mmap dmabuf for M2M failed: {}",
                    std::io::Error::last_os_error()
                )));
            }
            let copy_len = len_to_use.min(self.out_mmaps[idx].len);
            unsafe {
                ptr::copy_nonoverlapping(
                    mapped as *const u8,
                    self.out_mmaps[idx].ptr as *mut u8,
                    copy_len,
                );
                munmap(mapped, len_to_use);
            }
            queue_mmap_buf(
                self.fd.as_raw_fd(),
                self.out_buf_type,
                self.is_mplane,
                idx as u32,
                copy_len as u32,
            )?;
        }

        self.dequeue_exchange(pts)
    }

    fn switch_output_to_mmap(&mut self) -> CoreResult<()> {
        let mut out_t: c_uint = self.out_buf_type;
        unsafe {
            ioctl(self.fd.as_raw_fd(), VIDIOC_STREAMOFF, &mut out_t);
        }
        let mut free_req = V4l2RequestBuffers {
            count: 0,
            buf_type: self.out_buf_type,
            memory: V4L2_MEMORY_DMABUF,
            capabilities: 0,
            flags: 0,
            reserved: [0; 3],
        };
        unsafe {
            let _ = ioctl(self.fd.as_raw_fd(), VIDIOC_REQBUFS, &mut free_req);
        }
        let mut mmap_req = V4l2RequestBuffers {
            count: 2,
            buf_type: self.out_buf_type,
            memory: V4L2_MEMORY_MMAP,
            capabilities: 0,
            flags: 0,
            reserved: [0; 3],
        };
        if unsafe { ioctl(self.fd.as_raw_fd(), VIDIOC_REQBUFS, &mut mmap_req) } < 0
            || mmap_req.count == 0
        {
            return Err(CoreError::Codec(format!(
                "V4L2 M2M OUTPUT fallback MMAP REQBUFS failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        self.out_mmaps.clear();
        for idx in 0..mmap_req.count {
            self.out_mmaps.push(query_and_mmap(
                self.fd.as_raw_fd(),
                self.out_buf_type,
                self.is_mplane,
                idx,
            )?);
        }
        if unsafe { ioctl(self.fd.as_raw_fd(), VIDIOC_STREAMON, &mut out_t) } < 0 {
            return Err(CoreError::Codec("M2M OUTPUT fallback STREAMON failed".into()));
        }
        self.out_memory = V4L2_MEMORY_MMAP;
        Ok(())
    }

    /// 生ピクセルバッファ (`NV12` または `YUYV`) をエンコード投入する
    pub fn encode_raw_slice(&mut self, raw: &[u8], pts: PtsUs) -> CoreResult<()> {
        if self.out_memory != V4L2_MEMORY_MMAP || self.out_mmaps.is_empty() {
            return Err(CoreError::Codec(
                "encoder initialized in DMABUF mode; pass DMA-BUF FD via encode_dmabuf".into(),
            ));
        }
        if self.force_keyframe {
            let _ = set_v4l2_ctrl(self.fd.as_raw_fd(), V4L2_CID_MPEG_VIDEO_FORCE_KEY_FRAME, 1);
            self.force_keyframe = false;
        }
        let idx = self.next_out_idx;
        self.next_out_idx = (self.next_out_idx + 1) % self.out_mmaps.len();
        let copy_len = raw.len().min(self.out_mmaps[idx].len);
        unsafe {
            ptr::copy_nonoverlapping(raw.as_ptr(), self.out_mmaps[idx].ptr as *mut u8, copy_len);
        }
        queue_mmap_buf(
            self.fd.as_raw_fd(),
            self.out_buf_type,
            self.is_mplane,
            idx as u32,
            copy_len as u32,
        )?;
        self.dequeue_exchange(pts)
    }

    fn dequeue_exchange(&mut self, pts: PtsUs) -> CoreResult<()> {
        let mut pfd = PollFd {
            fd: self.fd.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        };
        let _ = unsafe { poll(&mut pfd, 1, 1000) };

        // 1. Reclaim OUTPUT buffer
        let _ = dequeue_buf(
            self.fd.as_raw_fd(),
            self.out_buf_type,
            self.out_memory,
            self.is_mplane,
        );

        // 2. Dequeue CAPTURE compressed packet & re-queue
        if let Ok((cap_idx, bytesused, flags)) = dequeue_buf(
            self.fd.as_raw_fd(),
            self.cap_buf_type,
            V4L2_MEMORY_MMAP,
            self.is_mplane,
        ) {
            if bytesused > 0 {
                let slot = &self.cap_mmaps[cap_idx as usize];
                let slice_len = (bytesused as usize).min(slot.len);
                let data =
                    unsafe { std::slice::from_raw_parts(slot.ptr as *const u8, slice_len) }.to_vec();
                self.queue.push_back(EncodedPacket {
                    codec: CodecKind::H264,
                    pts,
                    dts: pts,
                    is_keyframe: (flags & V4L2_BUF_FLAG_KEYFRAME) != 0 || self.coded_fourcc == V4L2_FOURCC_FWHT,
                    is_sequence_header: false,
                    data,
                });
            }
            let _ = queue_mmap_buf(self.fd.as_raw_fd(), self.cap_buf_type, self.is_mplane, cap_idx, 0);
        }
        Ok(())
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

impl VideoEncoder for V4l2M2mEncoder {
    /// ABR からの動的ビットレート変更 (`V4L2_CID_MPEG_VIDEO_BITRATE`)
    fn set_target_bitrate(&mut self, bitrate_bps: u32) -> CoreResult<()> {
        let clamped = bitrate_bps.max(100_000);
        let rc = set_v4l2_ctrl(
            self.fd.as_raw_fd(),
            V4L2_CID_MPEG_VIDEO_BITRATE,
            clamped as i32,
        );
        // 実機 H264 エンコーダが ENOTTY/EINVAL 以外で拒否した場合はエラー化
        if rc.is_err() && self.coded_fourcc == V4L2_FOURCC_H264 {
            return rc;
        }
        self.bitrate_bps = clamped;
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn pull_packet(&mut self) -> CoreResult<Option<EncodedPacket>> {
        Ok(self.queue.pop_front())
    }
}

impl Drop for V4l2M2mEncoder {
    fn drop(&mut self) {
        if self.streaming {
            let mut out_t: c_uint = self.out_buf_type;
            let mut cap_t: c_uint = self.cap_buf_type;
            let mut cmd: [u32; 10] = [0; 10];
            cmd[0] = 1; // V4L2_ENC_CMD_STOP
            unsafe {
                let _ = ioctl(self.fd.as_raw_fd(), VIDIOC_ENCODER_CMD, cmd.as_mut_ptr());
                ioctl(self.fd.as_raw_fd(), VIDIOC_STREAMOFF, &mut out_t);
                ioctl(self.fd.as_raw_fd(), VIDIOC_STREAMOFF, &mut cap_t);
            }
            self.streaming = false;
        }
    }
}

/// V4L2 M2M ハードウェアデコーダ（H.264 / MJPEG / FWHT → `DMA-BUF` ゼロコピー出力）
pub struct V4l2M2mDecoder {
    fd: OwnedFd,
    is_mplane: bool,
    out_buf_type: u32,
    cap_buf_type: u32,
    raw_fourcc: u32,
    width: u32,
    height: u32,
    out_mmaps: Vec<MmapSlot>,
    cap_dmabuf_fds: Vec<OwnedFd>,
    cap_stride: u32,
    next_out_idx: usize,
    decoded_frames: VecDeque<VideoFrame>,
    streaming: bool,
    cap_started: bool,
}

unsafe impl Send for V4l2M2mDecoder {}

impl V4l2M2mDecoder {
    pub fn open(
        device_path: &str,
        width: u32,
        height: u32,
        coded_fourcc: u32,
        out_raw_format: PixelFormat,
    ) -> CoreResult<Self> {
        let raw_fourcc = match out_raw_format {
            PixelFormat::Nv12 => V4L2_FOURCC_NV12,
            PixelFormat::Yuyv => V4L2_FOURCC_YUYV,
            _ => V4L2_FOURCC_NV12,
        };
        let cpath = CString::new(device_path)
            .map_err(|e| CoreError::PlatformIo(format!("invalid path: {}", e)))?;
        let raw_fd = unsafe { open(cpath.as_ptr(), O_RDWR | O_CLOEXEC | O_NONBLOCK) };
        if raw_fd < 0 {
            return Err(CoreError::PlatformIo(format!(
                "open({}) failed: {}",
                device_path,
                std::io::Error::last_os_error()
            )));
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

        let mut cap: V4l2Capability = unsafe { std::mem::zeroed() };
        if unsafe { ioctl(fd.as_raw_fd(), VIDIOC_QUERYCAP, &mut cap) } < 0 {
            return Err(CoreError::PlatformIo("VIDIOC_QUERYCAP failed".into()));
        }
        let eff_caps = if cap.device_caps != 0 {
            cap.device_caps
        } else {
            cap.capabilities
        };
        let is_mplane = (eff_caps & V4L2_CAP_VIDEO_M2M_MPLANE) != 0;
        let out_buf_type = if is_mplane {
            V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE
        } else {
            V4L2_BUF_TYPE_VIDEO_OUTPUT
        };
        let cap_buf_type = if is_mplane {
            V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE
        } else {
            V4L2_BUF_TYPE_VIDEO_CAPTURE
        };

        let _ = set_m2m_fmt(fd.as_raw_fd(), out_buf_type, is_mplane, width, height, coded_fourcc)?;

        let mut out_req = V4l2RequestBuffers {
            count: 2,
            buf_type: out_buf_type,
            memory: V4L2_MEMORY_MMAP,
            capabilities: 0,
            flags: 0,
            reserved: [0; 3],
        };
        if unsafe { ioctl(fd.as_raw_fd(), VIDIOC_REQBUFS, &mut out_req) } < 0 || out_req.count == 0 {
            return Err(CoreError::Codec("V4L2 M2M DEC OUTPUT REQBUFS failed".into()));
        }
        let mut out_mmaps = Vec::with_capacity(out_req.count as usize);
        for idx in 0..out_req.count {
            out_mmaps.push(query_and_mmap(fd.as_raw_fd(), out_buf_type, is_mplane, idx)?);
        }

        let mut out_t: c_uint = out_buf_type;
        if unsafe { ioctl(fd.as_raw_fd(), VIDIOC_STREAMON, &mut out_t) } < 0 {
            return Err(CoreError::Codec("DEC OUTPUT STREAMON failed".into()));
        }

        Ok(Self {
            fd,
            is_mplane,
            out_buf_type,
            cap_buf_type,
            raw_fourcc,
            width,
            height,
            out_mmaps,
            cap_dmabuf_fds: Vec::new(),
            cap_stride: width,
            next_out_idx: 0,
            decoded_frames: VecDeque::new(),
            streaming: true,
            cap_started: false,
        })
    }

    fn ensure_capture_started(&mut self) -> CoreResult<()> {
        if self.cap_started {
            return Ok(());
        }
        let _ = set_m2m_fmt(
            self.fd.as_raw_fd(),
            self.cap_buf_type,
            self.is_mplane,
            self.width,
            self.height,
            self.raw_fourcc,
        )?;
        let mut cap_req = V4l2RequestBuffers {
            count: 2,
            buf_type: self.cap_buf_type,
            memory: V4L2_MEMORY_MMAP,
            capabilities: 0,
            flags: 0,
            reserved: [0; 3],
        };
        if unsafe { ioctl(self.fd.as_raw_fd(), VIDIOC_REQBUFS, &mut cap_req) } < 0
            || cap_req.count == 0
        {
            return Err(CoreError::Codec("V4L2 M2M DEC CAPTURE REQBUFS failed".into()));
        }
        self.cap_dmabuf_fds.clear();
        for idx in 0..cap_req.count {
            let mut exp = V4l2ExportBuffer {
                buf_type: self.cap_buf_type,
                index: idx,
                plane: 0,
                flags: (O_CLOEXEC | O_RDONLY) as u32,
                fd: -1,
                reserved: [0; 11],
            };
            if unsafe { ioctl(self.fd.as_raw_fd(), VIDIOC_EXPBUF, &mut exp) } < 0 || exp.fd < 0 {
                return Err(CoreError::Codec("V4L2 M2M DEC EXPBUF failed".into()));
            }
            self.cap_dmabuf_fds
                .push(unsafe { OwnedFd::from_raw_fd(exp.fd) });
            queue_mmap_buf(self.fd.as_raw_fd(), self.cap_buf_type, self.is_mplane, idx, 0)?;
        }
        let mut cap_t: c_uint = self.cap_buf_type;
        if unsafe { ioctl(self.fd.as_raw_fd(), VIDIOC_STREAMON, &mut cap_t) } < 0 {
            return Err(CoreError::Codec("DEC CAPTURE STREAMON failed".into()));
        }
        self.cap_started = true;
        Ok(())
    }
}

impl VideoDecoder for V4l2M2mDecoder {
    fn push_packet(&mut self, packet: &EncodedPacket) -> CoreResult<()> {
        let idx = self.next_out_idx;
        self.next_out_idx = (self.next_out_idx + 1) % self.out_mmaps.len();
        let copy_len = packet.data.len().min(self.out_mmaps[idx].len);
        unsafe {
            ptr::copy_nonoverlapping(
                packet.data.as_ptr(),
                self.out_mmaps[idx].ptr as *mut u8,
                copy_len,
            );
        }
        queue_mmap_buf(
            self.fd.as_raw_fd(),
            self.out_buf_type,
            self.is_mplane,
            idx as u32,
            copy_len as u32,
        )?;
        self.ensure_capture_started()?;

        let mut pfd = PollFd {
            fd: self.fd.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        };
        let _ = unsafe { poll(&mut pfd, 1, 1000) };
        let _ = dequeue_buf(
            self.fd.as_raw_fd(),
            self.out_buf_type,
            V4L2_MEMORY_MMAP,
            self.is_mplane,
        );

        if let Ok((cap_idx, bytesused, _flags)) = dequeue_buf(
            self.fd.as_raw_fd(),
            self.cap_buf_type,
            V4L2_MEMORY_MMAP,
            self.is_mplane,
        ) {
            if bytesused > 0 {
                let dup = unsafe {
                    fcntl(
                        self.cap_dmabuf_fds[cap_idx as usize].as_raw_fd(),
                        F_DUPFD_CLOEXEC,
                        0,
                    )
                };
                if dup >= 0 {
                    let owned = unsafe { OwnedFd::from_raw_fd(dup) };
                    self.decoded_frames.push_back(VideoFrame {
                        width: self.width,
                        height: self.height,
                        format: PixelFormat::Nv12,
                        pts: packet.pts,
                        handle: NativeBufferHandle::DmaBuf {
                            fd: owned,
                            offset: 0,
                            stride: self.cap_stride,
                            modifier: 0,
                        },
                    });
                }
            }
            let _ = queue_mmap_buf(
                self.fd.as_raw_fd(),
                self.cap_buf_type,
                self.is_mplane,
                cap_idx,
                0,
            );
        }
        Ok(())
    }

    fn pull_frame(&mut self) -> CoreResult<Option<VideoFrame>> {
        Ok(self.decoded_frames.pop_front())
    }
}

impl Drop for V4l2M2mDecoder {
    fn drop(&mut self) {
        if self.streaming {
            let mut out_t: c_uint = self.out_buf_type;
            let mut cap_t: c_uint = self.cap_buf_type;
            unsafe {
                ioctl(self.fd.as_raw_fd(), VIDIOC_STREAMOFF, &mut out_t);
                if self.cap_started {
                    ioctl(self.fd.as_raw_fd(), VIDIOC_STREAMOFF, &mut cap_t);
                }
            }
            self.streaming = false;
            self.cap_started = false;
        }
    }
}

pub fn set_v4l2_ctrl(fd: RawFd, id: u32, value: i32) -> CoreResult<()> {
    let mut ctrl = V4l2Control { id, value };
    if unsafe { ioctl(fd, VIDIOC_S_CTRL, &mut ctrl) } < 0 {
        return Err(CoreError::PlatformIo(format!(
            "VIDIOC_S_CTRL(0x{:x}={}) failed: {}",
            id,
            value,
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

fn set_m2m_fmt(
    fd: RawFd,
    buf_type: u32,
    is_mplane: bool,
    width: u32,
    height: u32,
    fourcc: u32,
) -> CoreResult<u32> {
    let mut fmt: V4l2Format = unsafe { std::mem::zeroed() };
    fmt.buf_type = buf_type;
    if is_mplane {
        let mut mp: V4l2PixFormatMplane = unsafe { std::mem::zeroed() };
        mp.width = width;
        mp.height = height;
        mp.pixelformat = fourcc;
        mp.field = V4L2_FIELD_NONE;
        mp.num_planes = 1;
        fmt.fmt.pix_mp = mp;
        if unsafe { ioctl(fd, VIDIOC_S_FMT, &mut fmt) } < 0 {
            return Err(CoreError::Codec(format!(
                "VIDIOC_S_FMT mplane(0x{:08x}) failed: {}",
                fourcc,
                std::io::Error::last_os_error()
            )));
        }
        let res = unsafe { fmt.fmt.pix_mp };
        if res.pixelformat != fourcc {
            return Err(CoreError::UnsupportedFormat(format!(
                "M2M negotiated FourCC 0x{:08x} instead of 0x{:08x}",
                res.pixelformat, fourcc
            )));
        }
        Ok(res.plane_fmt[0].sizeimage)
    } else {
        fmt.fmt.pix = V4l2PixFormat {
            width,
            height,
            pixelformat: fourcc,
            field: V4L2_FIELD_NONE,
            bytesperline: 0,
            sizeimage: 0,
            colorspace: 0,
            priv_: 0,
            flags: 0,
            ycbcr_enc: 0,
            quantization: 0,
            xfer_func: 0,
        };
        if unsafe { ioctl(fd, VIDIOC_S_FMT, &mut fmt) } < 0 {
            return Err(CoreError::Codec(format!(
                "VIDIOC_S_FMT single(0x{:08x}) failed: {}",
                fourcc,
                std::io::Error::last_os_error()
            )));
        }
        let res = unsafe { fmt.fmt.pix };
        if res.pixelformat != fourcc {
            return Err(CoreError::UnsupportedFormat(format!(
                "M2M negotiated FourCC 0x{:08x} instead of 0x{:08x}",
                res.pixelformat, fourcc
            )));
        }
        Ok(res.sizeimage)
    }
}

fn query_and_mmap(fd: RawFd, buf_type: u32, is_mplane: bool, idx: u32) -> CoreResult<MmapSlot> {
    let mut planes = [V4l2Plane::default(); 1];
    let mut qb: V4l2Buffer = unsafe { std::mem::zeroed() };
    qb.index = idx;
    qb.buf_type = buf_type;
    qb.memory = V4L2_MEMORY_MMAP;
    if is_mplane {
        qb.m_offset_or_planes_ptr = planes.as_mut_ptr() as usize;
        qb.length = 1;
    }
    if unsafe { ioctl(fd, VIDIOC_QUERYBUF, &mut qb) } < 0 {
        return Err(CoreError::PlatformIo(format!(
            "VIDIOC_QUERYBUF failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    let (len, offset) = if is_mplane {
        (planes[0].length as usize, planes[0].m_mem_offset_or_fd as i64)
    } else {
        (qb.length as usize, qb.m_offset_or_planes_ptr as i64)
    };
    let ptr_mapped = unsafe { mmap(ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, offset) };
    if ptr_mapped == MAP_FAILED {
        return Err(CoreError::PlatformIo(format!(
            "M2M mmap failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(MmapSlot {
        ptr: ptr_mapped,
        len,
    })
}

fn queue_mmap_buf(
    fd: RawFd,
    buf_type: u32,
    is_mplane: bool,
    idx: u32,
    bytesused: u32,
) -> CoreResult<()> {
    let mut planes = [V4l2Plane {
        bytesused,
        ..Default::default()
    }; 1];
    let mut qb: V4l2Buffer = unsafe { std::mem::zeroed() };
    qb.index = idx;
    qb.buf_type = buf_type;
    qb.memory = V4L2_MEMORY_MMAP;
    qb.bytesused = bytesused;
    if is_mplane {
        qb.m_offset_or_planes_ptr = planes.as_mut_ptr() as usize;
        qb.length = 1;
    }
    if unsafe { ioctl(fd, VIDIOC_QBUF, &mut qb) } < 0 {
        return Err(CoreError::PlatformIo(format!(
            "VIDIOC_QBUF mmap failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

fn queue_dmabuf_buf(
    fd: RawFd,
    buf_type: u32,
    is_mplane: bool,
    idx: u32,
    dmabuf_fd: RawFd,
    bytesused: u32,
    buf_length: u32,
) -> CoreResult<()> {
    let total_len = buf_length.max(bytesused);
    let mut planes = [V4l2Plane {
        bytesused,
        length: total_len,
        m_mem_offset_or_fd: dmabuf_fd as usize,
        ..Default::default()
    }; 1];
    let mut qb: V4l2Buffer = unsafe { std::mem::zeroed() };
    qb.index = idx;
    qb.buf_type = buf_type;
    qb.memory = V4L2_MEMORY_DMABUF;
    qb.bytesused = bytesused;
    if is_mplane {
        qb.m_offset_or_planes_ptr = planes.as_mut_ptr() as usize;
        qb.length = 1;
    } else {
        qb.m_offset_or_planes_ptr = dmabuf_fd as usize;
        qb.length = total_len;
    }
    if unsafe { ioctl(fd, VIDIOC_QBUF, &mut qb) } < 0 {
        return Err(CoreError::PlatformIo(format!(
            "VIDIOC_QBUF dmabuf failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

fn dequeue_buf(fd: RawFd, buf_type: u32, memory: u32, is_mplane: bool) -> CoreResult<(u32, u32, u32)> {
    let mut planes = [V4l2Plane::default(); 1];
    let mut dq: V4l2Buffer = unsafe { std::mem::zeroed() };
    dq.buf_type = buf_type;
    dq.memory = memory;
    if is_mplane {
        dq.m_offset_or_planes_ptr = planes.as_mut_ptr() as usize;
        dq.length = 1;
    }
    if unsafe { ioctl(fd, VIDIOC_DQBUF, &mut dq) } < 0 {
        return Err(CoreError::PlatformIo(format!(
            "VIDIOC_DQBUF failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    let bytesused = if is_mplane {
        planes[0].bytesused
    } else {
        dq.bytesused
    };
    Ok((dq.index, bytesused, dq.flags))
}
