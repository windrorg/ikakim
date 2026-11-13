//! macOS / iOS ImageIO + CoreGraphics 静止画（PNG / JPEG）デコーダ

use crate::cv_metal::{kCFAllocatorDefault, CFRelease, CFTypeRef};
use core_types::{CoreError, CoreResult};
use std::ffi::c_void;
use std::ptr;

type CFDataRef = *const c_void;
type CGImageSourceRef = *mut c_void;
type CGImageRef = *mut c_void;
type CGColorSpaceRef = *mut c_void;
type CGContextRef = *mut c_void;

const K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST: u32 = 1;
const K_CG_BITMAP_BYTE_ORDER_32_BIG: u32 = 4 << 12;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin_x: f64,
    origin_y: f64,
    width: f64,
    height: f64,
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFDataCreate(allocator: *const c_void, bytes: *const u8, length: isize) -> CFDataRef;
}

#[link(name = "ImageIO", kind = "framework")]
extern "C" {
    fn CGImageSourceCreateWithData(data: CFDataRef, options: *const c_void) -> CGImageSourceRef;
    fn CGImageSourceCreateImageAtIndex(
        isrc: CGImageSourceRef,
        index: usize,
        options: *const c_void,
    ) -> CGImageRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGImageGetWidth(image: CGImageRef) -> usize;
    fn CGImageGetHeight(image: CGImageRef) -> usize;
    fn CGColorSpaceCreateDeviceRGB() -> CGColorSpaceRef;
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: CGColorSpaceRef,
        bitmap_info: u32,
    ) -> CGContextRef;
    fn CGContextDrawImage(c: CGContextRef, rect: CGRect, image: CGImageRef);
}

#[derive(Debug, Clone)]
pub struct DecodedImageRgba {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// PNG / JPEG / GIF 等の画像バイト列を ImageIO 経由で RGBA8 へデコードする
pub fn decode_image_rgba(bytes: &[u8]) -> CoreResult<DecodedImageRgba> {
    if bytes.is_empty() {
        return Err(CoreError::UnsupportedFormat("empty image buffer".into()));
    }
    unsafe {
        let cf_data = CFDataCreate(kCFAllocatorDefault, bytes.as_ptr(), bytes.len() as isize);
        if cf_data.is_null() {
            return Err(CoreError::PlatformIo("CFDataCreate failed".into()));
        }
        let src = CGImageSourceCreateWithData(cf_data, ptr::null());
        CFRelease(cf_data as CFTypeRef);
        if src.is_null() {
            return Err(CoreError::UnsupportedFormat(
                "CGImageSourceCreateWithData failed".into(),
            ));
        }

        let img = CGImageSourceCreateImageAtIndex(src, 0, ptr::null());
        CFRelease(src as CFTypeRef);
        if img.is_null() {
            return Err(CoreError::UnsupportedFormat(
                "CGImageSourceCreateImageAtIndex failed".into(),
            ));
        }

        let w = CGImageGetWidth(img);
        let h = CGImageGetHeight(img);
        if w == 0 || h == 0 {
            CFRelease(img as CFTypeRef);
            return Err(CoreError::UnsupportedFormat("decoded image has 0 size".into()));
        }

        let mut rgba = vec![0u8; w * h * 4];
        let cs = CGColorSpaceCreateDeviceRGB();
        let ctx = CGBitmapContextCreate(
            rgba.as_mut_ptr() as *mut c_void,
            w,
            h,
            8,
            w * 4,
            cs,
            K_CG_IMAGE_ALPHA_PREMULTIPLIED_LAST | K_CG_BITMAP_BYTE_ORDER_32_BIG,
        );
        CFRelease(cs as CFTypeRef);
        if ctx.is_null() {
            CFRelease(img as CFTypeRef);
            return Err(CoreError::PlatformIo("CGBitmapContextCreate failed".into()));
        }

        CGContextDrawImage(
            ctx,
            CGRect {
                origin_x: 0.0,
                origin_y: 0.0,
                width: w as f64,
                height: h as f64,
            },
            img,
        );
        CFRelease(ctx as CFTypeRef);
        CFRelease(img as CFTypeRef);

        Ok(DecodedImageRgba {
            width: w as u32,
            height: h as u32,
            rgba,
        })
    }
}
