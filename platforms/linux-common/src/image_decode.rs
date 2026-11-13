//! Linux 動的 FFI 静止画デコーダ (`libpng16.so.16` + `libjpeg.so.8` + 純 Rust PPM フォールバック)

use core_types::{CoreError, CoreResult};
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::ptr;

const RTLD_LAZY: c_int = 1;
const PNG_IMAGE_VERSION: u32 = 1;
const PNG_FORMAT_RGBA: u32 = 3; // PNG_FORMAT_FLAG_ALPHA (1) | PNG_FORMAT_FLAG_COLOR (2)

const JPEG_LIB_VERSION: c_int = 80;
const JCS_RGB: c_int = 2;
const JCS_EXT_RGBA: c_int = 12;

extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
}

#[repr(C)]
struct PngImage {
    opaque: *mut c_void,
    version: u32,
    width: u32,
    height: u32,
    format: u32,
    flags: u32,
    colormap_entries: u32,
    warning_or_error: u32,
    message: [u8; 64],
}

#[derive(Debug, Clone)]
pub struct DecodedImageRgba {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// PNG (`libpng16.so.16`), JPEG (`libjpeg.so.8`), または PPM (`P6`) を RGBA8 へデコードする
pub fn decode_image_rgba(bytes: &[u8]) -> CoreResult<DecodedImageRgba> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return decode_png_dlopen(bytes);
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return decode_jpeg_dlopen(bytes);
    }
    if bytes.starts_with(b"P6") {
        return decode_ppm_p6(bytes);
    }
    Err(CoreError::UnsupportedFormat(
        "unrecognized image magic (expected PNG, JPEG, or PPM P6)".into(),
    ))
}

fn decode_png_dlopen(bytes: &[u8]) -> CoreResult<DecodedImageRgba> {
    type FnBeginMem = unsafe extern "C" fn(*mut PngImage, *const c_void, usize) -> c_int;
    type FnFinishRead =
        unsafe extern "C" fn(*mut PngImage, *const c_void, *mut c_void, i32, *mut c_void) -> c_int;
    type FnFree = unsafe extern "C" fn(*mut PngImage);

    unsafe {
        let lib = dlopen(c"libpng16.so.16".as_ptr(), RTLD_LAZY);
        if lib.is_null() {
            return Err(CoreError::PlatformIo("dlopen libpng16.so.16 failed".into()));
        }
        let p_begin = dlsym(lib, c"png_image_begin_read_from_memory".as_ptr());
        let p_finish = dlsym(lib, c"png_image_finish_read".as_ptr());
        let p_free = dlsym(lib, c"png_image_free".as_ptr());
        if p_begin.is_null() || p_finish.is_null() || p_free.is_null() {
            dlclose(lib);
            return Err(CoreError::PlatformIo("libpng16 symbols missing".into()));
        }
        let begin_mem: FnBeginMem = std::mem::transmute(p_begin);
        let finish_read: FnFinishRead = std::mem::transmute(p_finish);
        let image_free: FnFree = std::mem::transmute(p_free);

        let mut img: PngImage = std::mem::zeroed();
        img.version = PNG_IMAGE_VERSION;
        if begin_mem(&mut img, bytes.as_ptr() as *const c_void, bytes.len()) == 0 {
            let msg_len = img.message.iter().position(|&b| b == 0).unwrap_or(64);
            let msg = String::from_utf8_lossy(&img.message[..msg_len]).into_owned();
            image_free(&mut img);
            dlclose(lib);
            return Err(CoreError::UnsupportedFormat(format!("libpng begin failed: {}", msg)));
        }
        img.format = PNG_FORMAT_RGBA;
        let w = img.width;
        let h = img.height;
        if w == 0 || h == 0 {
            image_free(&mut img);
            dlclose(lib);
            return Err(CoreError::UnsupportedFormat("PNG has 0 dimension".into()));
        }
        let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
        let ok = finish_read(
            &mut img,
            ptr::null(),
            rgba.as_mut_ptr() as *mut c_void,
            0,
            ptr::null_mut(),
        );
        if ok == 0 {
            let msg_len = img.message.iter().position(|&b| b == 0).unwrap_or(64);
            let msg = String::from_utf8_lossy(&img.message[..msg_len]).into_owned();
            image_free(&mut img);
            dlclose(lib);
            return Err(CoreError::UnsupportedFormat(format!(
                "libpng finish failed: {}",
                msg
            )));
        }
        image_free(&mut img);
        dlclose(lib);
        Ok(DecodedImageRgba {
            width: w,
            height: h,
            rgba,
        })
    }
}

fn decode_jpeg_dlopen(bytes: &[u8]) -> CoreResult<DecodedImageRgba> {
    // aarch64/x86_64 libjpeg-turbo 8 (`JPEG_LIB_VERSION=80`) ABI バッファサイズと既知オフセット
    #[repr(C, align(16))]
    struct JpegDecompressBuf {
        raw: [u8; 768],
    }
    #[repr(C, align(16))]
    struct JpegErrMgrBuf {
        raw: [u8; 256],
    }

    type FnStdError = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
    type FnCreateDecompress = unsafe extern "C" fn(*mut c_void, c_int, usize);
    type FnDestroyDecompress = unsafe extern "C" fn(*mut c_void);
    type FnMemSrc = unsafe extern "C" fn(*mut c_void, *const u8, usize);
    type FnReadHeader = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
    type FnStartDecompress = unsafe extern "C" fn(*mut c_void) -> c_int;
    type FnReadScanlines = unsafe extern "C" fn(*mut c_void, *mut *mut u8, c_uint) -> c_uint;
    type FnFinishDecompress = unsafe extern "C" fn(*mut c_void) -> c_int;

    unsafe {
        let mut lib = dlopen(c"libjpeg.so.8".as_ptr(), RTLD_LAZY);
        if lib.is_null() {
            lib = dlopen(c"libjpeg.so.62".as_ptr(), RTLD_LAZY);
        }
        if lib.is_null() {
            return Err(CoreError::PlatformIo("dlopen libjpeg failed".into()));
        }

        macro_rules! load_j {
            ($sym:literal, $ty:ty) => {{
                let p = dlsym(lib, $sym.as_ptr());
                if p.is_null() {
                    dlclose(lib);
                    return Err(CoreError::PlatformIo(format!(
                        "libjpeg dlsym({}) missing",
                        $sym.to_string_lossy()
                    )));
                }
                std::mem::transmute::<*mut c_void, $ty>(p)
            }};
        }

        let jpeg_std_error = load_j!(c"jpeg_std_error", FnStdError);
        let jpeg_create_decompress = load_j!(c"jpeg_CreateDecompress", FnCreateDecompress);
        let jpeg_destroy_decompress = load_j!(c"jpeg_destroy_decompress", FnDestroyDecompress);
        let jpeg_mem_src = load_j!(c"jpeg_mem_src", FnMemSrc);
        let jpeg_read_header = load_j!(c"jpeg_read_header", FnReadHeader);
        let jpeg_start_decompress = load_j!(c"jpeg_start_decompress", FnStartDecompress);
        let jpeg_read_scanlines = load_j!(c"jpeg_read_scanlines", FnReadScanlines);
        let jpeg_finish_decompress = load_j!(c"jpeg_finish_decompress", FnFinishDecompress);

        let mut cinfo: JpegDecompressBuf = std::mem::zeroed();
        let mut jerr: JpegErrMgrBuf = std::mem::zeroed();
        let cinfo_ptr = cinfo.raw.as_mut_ptr() as *mut c_void;
        let err_ptr = jpeg_std_error(jerr.raw.as_mut_ptr() as *mut c_void);
        ptr::write_unaligned(cinfo.raw.as_mut_ptr() as *mut *mut c_void, err_ptr);

        // sizeof(struct jpeg_decompress_struct) = 656 on 64-bit libjpeg8
        jpeg_create_decompress(cinfo_ptr, JPEG_LIB_VERSION, 656);
        jpeg_mem_src(cinfo_ptr, bytes.as_ptr(), bytes.len());
        if jpeg_read_header(cinfo_ptr, 1) != 1 {
            jpeg_destroy_decompress(cinfo_ptr);
            dlclose(lib);
            return Err(CoreError::UnsupportedFormat("jpeg_read_header failed".into()));
        }
        // out_color_space at offset 64 -> request JCS_EXT_RGBA
        ptr::write_unaligned(cinfo.raw.as_mut_ptr().add(64) as *mut c_int, JCS_EXT_RGBA);
        if jpeg_start_decompress(cinfo_ptr) == 0 {
            jpeg_destroy_decompress(cinfo_ptr);
            dlclose(lib);
            return Err(CoreError::UnsupportedFormat("jpeg_start_decompress failed".into()));
        }

        let w = ptr::read_unaligned(cinfo.raw.as_ptr().add(136) as *const u32);
        let h = ptr::read_unaligned(cinfo.raw.as_ptr().add(140) as *const u32);
        let comp = ptr::read_unaligned(cinfo.raw.as_ptr().add(148) as *const c_int);
        let cs = ptr::read_unaligned(cinfo.raw.as_ptr().add(64) as *const c_int);

        let stride = (w as usize) * (comp as usize);
        let mut raw = vec![0u8; stride * (h as usize)];
        for y in 0..h as usize {
            let mut row_ptr = raw.as_mut_ptr().add(y * stride);
            jpeg_read_scanlines(cinfo_ptr, &mut row_ptr, 1);
        }
        jpeg_finish_decompress(cinfo_ptr);
        jpeg_destroy_decompress(cinfo_ptr);
        dlclose(lib);

        let rgba = if comp == 4 {
            raw
        } else if comp == 3 || cs == JCS_RGB {
            let mut out = Vec::with_capacity((w as usize) * (h as usize) * 4);
            for rgb in raw.chunks_exact(3) {
                out.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
            }
            out
        } else {
            return Err(CoreError::UnsupportedFormat(format!(
                "unsupported jpeg output components: {}",
                comp
            )));
        };

        Ok(DecodedImageRgba {
            width: w,
            height: h,
            rgba,
        })
    }
}

/// 純 Rust PPM (P6 Binary RGB) フォールバックデコーダ
pub fn decode_ppm_p6(bytes: &[u8]) -> CoreResult<DecodedImageRgba> {
    let mut i = 0usize;
    let mut tokens: Vec<&[u8]> = Vec::with_capacity(4);
    while tokens.len() < 4 && i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'#' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if start < i {
            tokens.push(&bytes[start..i]);
        }
    }
    if tokens.len() < 4 || tokens[0] != b"P6" {
        return Err(CoreError::UnsupportedFormat("invalid PPM P6 header".into()));
    }
    let w: u32 = std::str::from_utf8(tokens[1])
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| CoreError::UnsupportedFormat("invalid PPM width".into()))?;
    let h: u32 = std::str::from_utf8(tokens[2])
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| CoreError::UnsupportedFormat("invalid PPM height".into()))?;
    if i >= bytes.len() {
        return Err(CoreError::UnsupportedFormat("truncated PPM".into()));
    }
    i += 1; // single whitespace separator before binary payload
    let rgb_len = (w as usize) * (h as usize) * 3;
    if bytes.len() < i + rgb_len {
        return Err(CoreError::UnsupportedFormat("truncated PPM pixel payload".into()));
    }
    let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for px in bytes[i..i + rgb_len].chunks_exact(3) {
        rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
    }
    Ok(DecodedImageRgba {
        width: w,
        height: h,
        rgba,
    })
}
