//! Linux V4L2 キャプチャ（`DMA-BUF` ゼロコピーエクスポート `VIDIOC_EXPBUF` 対応）
//! `vivid` 仮想ドライバ・UVC HDMI キャプチャ・組み込み ISP 共通

use core_types::clock::PtsUs;
use core_types::media::{NativeBufferHandle, PixelFormat, VideoFrame};
use core_types::{CoreError, CoreResult};
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::raw::{c_char, c_int, c_uint, c_ulong, c_void};
use std::ptr;

const O_RDWR: c_int = 2;
const O_RDONLY: c_int = 0;
const O_CLOEXEC: c_int = 0o2000000;
const F_DUPFD_CLOEXEC: c_int = 1030;
const POLLIN: i16 = 0x0001;

// Linux aarch64/x86_64 共通 <linux/videodev2.h> ioctl 定数
const VIDIOC_QUERYCAP: c_ulong = 0x8068_5600;
const VIDIOC_S_FMT: c_ulong = 0xc0d0_5605;
const VIDIOC_REQBUFS: c_ulong = 0xc014_5608;
const VIDIOC_QUERYBUF: c_ulong = 0xc058_5609;
const VIDIOC_QBUF: c_ulong = 0xc058_560f;
const VIDIOC_EXPBUF: c_ulong = 0xc040_5610;
const VIDIOC_DQBUF: c_ulong = 0xc058_5611;
const VIDIOC_STREAMON: c_ulong = 0x4004_5612;
const VIDIOC_STREAMOFF: c_ulong = 0x4004_5613;

const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const V4L2_MEMORY_MMAP: u32 = 1;
const V4L2_FIELD_NONE: u32 = 1;
const V4L2_CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const V4L2_CAP_STREAMING: u32 = 0x0400_0000;

pub const V4L2_PIX_FMT_YUYV: u32 = u32::from_le_bytes(*b"YUYV");
pub const V4L2_PIX_FMT_NV12: u32 = u32::from_le_bytes(*b"NV12");
pub const V4L2_PIX_FMT_MJPEG: u32 = u32::from_le_bytes(*b"MJPG");

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

const PROT_READ: c_int = 1;
const MAP_SHARED: c_int = 1;
const MAP_FAILED: *mut c_void = !0usize as *mut c_void;

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
union V4l2FormatUnion {
    pix: V4l2PixFormat,
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
    m_offset_or_userptr: usize,
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

#[derive(Debug, Clone)]
pub struct V4l2DeviceInfo {
    pub path: String,
    pub driver: String,
    pub card: String,
    pub capabilities: u32,
}

/// `/dev/video0`..`/dev/video15` の V4L2 キャプチャデバイスを列挙する
pub fn list_capture_devices() -> Vec<V4l2DeviceInfo> {
    let mut out = Vec::new();
    for idx in 0..16 {
        let path = format!("/dev/video{}", idx);
        let Ok(cpath) = CString::new(path.as_str()) else {
            continue;
        };
        let fd = unsafe { open(cpath.as_ptr(), O_RDWR | O_CLOEXEC) };
        if fd < 0 {
            continue;
        }
        let mut cap: V4l2Capability = unsafe { std::mem::zeroed() };
        let rc = unsafe { ioctl(fd, VIDIOC_QUERYCAP, &mut cap as *mut V4l2Capability) };
        unsafe { close(fd) };
        if rc == 0 && (cap.capabilities & V4L2_CAP_VIDEO_CAPTURE) != 0 {
            let driver = c_bytes_to_string(&cap.driver);
            let card = c_bytes_to_string(&cap.card);
            out.push(V4l2DeviceInfo {
                path,
                driver,
                card,
                capabilities: cap.capabilities,
            });
        }
    }
    out
}

fn c_bytes_to_string(buf: &[u8]) -> String {
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

/// V4L2 ゼロコピー (`DMA-BUF`) キャプチャデバイス
pub struct V4l2CaptureDevice {
    fd: OwnedFd,
    width: u32,
    height: u32,
    format: PixelFormat,
    bytes_per_line: u32,
    size_image: u32,
    dmabuf_fds: Vec<OwnedFd>,
    streaming: bool,
    last_dequeued: Option<u32>,
}

impl V4l2CaptureDevice {
    pub fn open(
        device_path: &str,
        width: u32,
        height: u32,
        format: PixelFormat,
        buffer_count: u32,
    ) -> CoreResult<Self> {
        let fourcc = match format {
            PixelFormat::Yuyv => V4L2_PIX_FMT_YUYV,
            PixelFormat::Nv12 => V4L2_PIX_FMT_NV12,
            PixelFormat::Mjpeg => V4L2_PIX_FMT_MJPEG,
            other => {
                return Err(CoreError::UnsupportedFormat(format!(
                    "V4L2 does not support capture format {:?}",
                    other
                )))
            }
        };

        let cpath = CString::new(device_path)
            .map_err(|e| CoreError::PlatformIo(format!("invalid path: {}", e)))?;
        let raw_fd = unsafe { open(cpath.as_ptr(), O_RDWR | O_CLOEXEC) };
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
            return Err(CoreError::PlatformIo(format!(
                "VIDIOC_QUERYCAP failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        if (cap.capabilities & V4L2_CAP_VIDEO_CAPTURE) == 0
            || (cap.capabilities & V4L2_CAP_STREAMING) == 0
        {
            return Err(CoreError::PlatformIo(format!(
                "{} does not support VIDEO_CAPTURE + STREAMING (caps=0x{:x})",
                device_path, cap.capabilities
            )));
        }

        let mut fmt: V4l2Format = unsafe { std::mem::zeroed() };
        fmt.buf_type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
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
        if unsafe { ioctl(fd.as_raw_fd(), VIDIOC_S_FMT, &mut fmt) } < 0 {
            return Err(CoreError::PlatformIo(format!(
                "VIDIOC_S_FMT failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        let pix = unsafe { fmt.fmt.pix };
        if pix.pixelformat != fourcc {
            return Err(CoreError::UnsupportedFormat(format!(
                "V4L2 driver {} negotiated different FourCC 0x{:08x} instead of 0x{:08x}",
                device_path, pix.pixelformat, fourcc
            )));
        }

        let count = buffer_count.clamp(2, 8);
        let mut req: V4l2RequestBuffers = unsafe { std::mem::zeroed() };
        req.count = count;
        req.buf_type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        req.memory = V4L2_MEMORY_MMAP;
        if unsafe { ioctl(fd.as_raw_fd(), VIDIOC_REQBUFS, &mut req) } < 0 || req.count == 0 {
            return Err(CoreError::PlatformIo(format!(
                "VIDIOC_REQBUFS failed: {}",
                std::io::Error::last_os_error()
            )));
        }

        let mut dmabuf_fds = Vec::with_capacity(req.count as usize);
        for idx in 0..req.count {
            let mut qbuf: V4l2Buffer = unsafe { std::mem::zeroed() };
            qbuf.index = idx;
            qbuf.buf_type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
            qbuf.memory = V4L2_MEMORY_MMAP;
            if unsafe { ioctl(fd.as_raw_fd(), VIDIOC_QUERYBUF, &mut qbuf) } < 0 {
                return Err(CoreError::PlatformIo(format!(
                    "VIDIOC_QUERYBUF failed: {}",
                    std::io::Error::last_os_error()
                )));
            }

            let mut exp: V4l2ExportBuffer = unsafe { std::mem::zeroed() };
            exp.buf_type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
            exp.index = idx;
            exp.plane = 0;
            exp.flags = (O_CLOEXEC | O_RDONLY) as u32;
            if unsafe { ioctl(fd.as_raw_fd(), VIDIOC_EXPBUF, &mut exp) } < 0 || exp.fd < 0 {
                return Err(CoreError::PlatformIo(format!(
                    "VIDIOC_EXPBUF failed on buf {}: {}",
                    idx,
                    std::io::Error::last_os_error()
                )));
            }
            dmabuf_fds.push(unsafe { OwnedFd::from_raw_fd(exp.fd) });

            let mut enqueue: V4l2Buffer = unsafe { std::mem::zeroed() };
            enqueue.index = idx;
            enqueue.buf_type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
            enqueue.memory = V4L2_MEMORY_MMAP;
            if unsafe { ioctl(fd.as_raw_fd(), VIDIOC_QBUF, &mut enqueue) } < 0 {
                return Err(CoreError::PlatformIo(format!(
                    "initial VIDIOC_QBUF failed on buf {}: {}",
                    idx,
                    std::io::Error::last_os_error()
                )));
            }
        }

        let mut dev = Self {
            fd,
            width: pix.width,
            height: pix.height,
            format,
            bytes_per_line: pix.bytesperline,
            size_image: pix.sizeimage,
            dmabuf_fds,
            streaming: false,
            last_dequeued: None,
        };
        dev.start_streaming()?;
        Ok(dev)
    }

    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn format(&self) -> PixelFormat {
        self.format
    }

    pub fn bytes_per_line(&self) -> u32 {
        self.bytes_per_line
    }

    pub fn size_image(&self) -> u32 {
        self.size_image
    }

    pub fn start_streaming(&mut self) -> CoreResult<()> {
        if self.streaming {
            return Ok(());
        }
        let mut buf_type: c_uint = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        if unsafe { ioctl(self.fd.as_raw_fd(), VIDIOC_STREAMON, &mut buf_type) } < 0 {
            return Err(CoreError::PlatformIo(format!(
                "VIDIOC_STREAMON failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        self.streaming = true;
        Ok(())
    }

    pub fn stop_streaming(&mut self) {
        if self.streaming {
            let mut buf_type: c_uint = V4L2_BUF_TYPE_VIDEO_CAPTURE;
            unsafe {
                ioctl(self.fd.as_raw_fd(), VIDIOC_STREAMOFF, &mut buf_type);
            }
            self.streaming = false;
        }
    }

    /// 次の映像フレームを `DMA-BUF` (`NativeBufferHandle::DmaBuf`) としてゼロコピー取得する
    pub fn capture_frame(&mut self, pts: PtsUs, timeout_ms: i32) -> CoreResult<VideoFrame> {
        if let Some(prev_idx) = self.last_dequeued.take() {
            let mut re_q: V4l2Buffer = unsafe { std::mem::zeroed() };
            re_q.index = prev_idx;
            re_q.buf_type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
            re_q.memory = V4L2_MEMORY_MMAP;
            if unsafe { ioctl(self.fd.as_raw_fd(), VIDIOC_QBUF, &mut re_q) } < 0 {
                return Err(CoreError::PlatformIo(format!(
                    "re-queue VIDIOC_QBUF failed: {}",
                    std::io::Error::last_os_error()
                )));
            }
        }

        let mut pfd = PollFd {
            fd: self.fd.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        };
        let pr = unsafe { poll(&mut pfd, 1, timeout_ms) };
        if pr <= 0 {
            return Err(CoreError::PlatformIo(format!(
                "V4L2 poll timeout/error (rc={})",
                pr
            )));
        }

        let mut dq: V4l2Buffer = unsafe { std::mem::zeroed() };
        dq.buf_type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        dq.memory = V4L2_MEMORY_MMAP;
        if unsafe { ioctl(self.fd.as_raw_fd(), VIDIOC_DQBUF, &mut dq) } < 0 {
            return Err(CoreError::PlatformIo(format!(
                "VIDIOC_DQBUF failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        self.last_dequeued = Some(dq.index);

        let src_fd = self
            .dmabuf_fds
            .get(dq.index as usize)
            .ok_or_else(|| CoreError::PlatformIo("dequeued index out of range".into()))?;
        let dup_raw = unsafe { fcntl(src_fd.as_raw_fd(), F_DUPFD_CLOEXEC, 0) };
        if dup_raw < 0 {
            return Err(CoreError::PlatformIo(format!(
                "fcntl(F_DUPFD_CLOEXEC) on dmabuf failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        let owned_dmabuf = unsafe { OwnedFd::from_raw_fd(dup_raw) };

        Ok(VideoFrame {
            width: self.width,
            height: self.height,
            format: self.format,
            pts,
            handle: NativeBufferHandle::DmaBuf {
                fd: owned_dmabuf,
                offset: 0,
                stride: self.bytes_per_line,
                modifier: 0,
            },
        })
    }
}

impl Drop for V4l2CaptureDevice {
    fn drop(&mut self) {
        self.stop_streaming();
    }
}

/// `DMA-BUF` ファイルディスクリプタを `mmap` して先頭 `len` バイトをコピー読み出しする（Vulkan仮想GPUフォールバック用）
pub fn read_dmabuf_via_mmap(fd: RawFd, offset: usize, len: usize) -> CoreResult<Vec<u8>> {
    let map_len = offset + len;
    let ptr_mapped = unsafe { mmap(ptr::null_mut(), map_len, PROT_READ, MAP_SHARED, fd, 0) };
    if ptr_mapped == MAP_FAILED {
        return Err(CoreError::PlatformIo(format!(
            "dmabuf mmap failed (len={}): {}",
            map_len,
            std::io::Error::last_os_error()
        )));
    }
    let mut out = vec![0u8; len];
    unsafe {
        ptr::copy_nonoverlapping((ptr_mapped as *const u8).add(offset), out.as_mut_ptr(), len);
        munmap(ptr_mapped, map_len);
    }
    Ok(out)
}
