//! macOS / iOS Zero-Copy `CVPixelBuffer` <-> `CVMetalTextureCache` <-> `wgpu::Texture` (`wgpu_hal::api::Metal`)

use core_gpu::{
    ColorSpace, ExternalRenderFormat, ExternalRenderTarget, GpuCompositor, GpuLayer, LayerInputKind,
};
use core_types::clock::PtsUs;
use core_types::media::{LayerTransform, NativeBufferHandle, PixelFormat, VideoFrame};
use core_types::{CoreError, CoreResult};
use metal::foreign_types::ForeignType;
use std::ffi::c_void;
use std::ptr;
use wgpu::hal::api::Metal as HalMetal;

pub type CFAllocatorRef = *const c_void;
pub type CFTypeRef = *const c_void;
pub type CFDictionaryRef = *const c_void;
pub type CFMutableDictionaryRef = *mut c_void;
pub type CFStringRef = *const c_void;
pub type CFBooleanRef = *const c_void;
pub type CFNumberRef = *const c_void;
pub type CVReturn = i32;
pub type CVPixelBufferRef = *mut c_void;
pub type CVMetalTextureCacheRef = *mut c_void;
pub type CVMetalTextureRef = *mut c_void;

pub const K_CV_RETURN_SUCCESS: CVReturn = 0;
pub const K_CV_PIXEL_FORMAT_BGRA: u32 = u32::from_be_bytes(*b"BGRA");
pub const K_CV_PIXEL_FORMAT_NV12_VIDEO: u32 = u32::from_be_bytes(*b"420v");
pub const K_CV_PIXEL_BUFFER_LOCK_READ_ONLY: u64 = 1;

const K_CF_NUMBER_SINT32_TYPE: isize = 3;
const MTL_PIXEL_FORMAT_R8_UNORM: usize = 10;
const MTL_PIXEL_FORMAT_RG8_UNORM: usize = 30;
const MTL_PIXEL_FORMAT_BGRA8_UNORM: usize = 80;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub static kCFAllocatorDefault: CFAllocatorRef;
    pub static kCFBooleanTrue: CFBooleanRef;
    pub static kCFTypeDictionaryKeyCallBacks: c_void;
    pub static kCFTypeDictionaryValueCallBacks: c_void;

    pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFGetRetainCount(cf: CFTypeRef) -> isize;
    pub fn CFDictionaryCreateMutable(
        allocator: CFAllocatorRef,
        capacity: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFMutableDictionaryRef;
    pub fn CFDictionarySetValue(
        the_dict: CFMutableDictionaryRef,
        key: *const c_void,
        value: *const c_void,
    );
    pub fn CFNumberCreate(
        allocator: CFAllocatorRef,
        the_type: isize,
        value_ptr: *const c_void,
    ) -> CFNumberRef;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    pub static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    pub static kCVPixelBufferWidthKey: CFStringRef;
    pub static kCVPixelBufferHeightKey: CFStringRef;
    pub static kCVPixelBufferMetalCompatibilityKey: CFStringRef;
    pub static kCVPixelBufferIOSurfacePropertiesKey: CFStringRef;

    fn CVPixelBufferCreate(
        allocator: CFAllocatorRef,
        width: usize,
        height: usize,
        pixel_format_type: u32,
        pixel_buffer_attributes: CFDictionaryRef,
        pixel_buffer_out: *mut CVPixelBufferRef,
    ) -> CVReturn;
    fn CVPixelBufferGetIOSurface(pixel_buffer: CVPixelBufferRef) -> *const c_void;
    pub fn CVPixelBufferLockBaseAddress(pixel_buffer: CVPixelBufferRef, lock_flags: u64) -> CVReturn;
    pub fn CVPixelBufferUnlockBaseAddress(
        pixel_buffer: CVPixelBufferRef,
        unlock_flags: u64,
    ) -> CVReturn;
    pub fn CVPixelBufferGetWidth(pixel_buffer: CVPixelBufferRef) -> usize;
    pub fn CVPixelBufferGetHeight(pixel_buffer: CVPixelBufferRef) -> usize;
    pub fn CVPixelBufferGetPixelFormatType(pixel_buffer: CVPixelBufferRef) -> u32;
    pub fn CVPixelBufferGetBaseAddress(pixel_buffer: CVPixelBufferRef) -> *mut u8;
    pub fn CVPixelBufferGetBytesPerRow(pixel_buffer: CVPixelBufferRef) -> usize;
    pub fn CVPixelBufferGetBaseAddressOfPlane(
        pixel_buffer: CVPixelBufferRef,
        plane_index: usize,
    ) -> *mut u8;
    pub fn CVPixelBufferGetBytesPerRowOfPlane(
        pixel_buffer: CVPixelBufferRef,
        plane_index: usize,
    ) -> usize;

    fn CVMetalTextureCacheCreate(
        allocator: CFAllocatorRef,
        cache_attributes: CFDictionaryRef,
        metal_device: *mut c_void,
        texture_attributes: CFDictionaryRef,
        cache_out: *mut CVMetalTextureCacheRef,
    ) -> CVReturn;
    fn CVMetalTextureCacheCreateTextureFromImage(
        allocator: CFAllocatorRef,
        texture_cache: CVMetalTextureCacheRef,
        source_image: CVPixelBufferRef,
        texture_attributes: CFDictionaryRef,
        pixel_format: usize,
        width: usize,
        height: usize,
        plane_index: usize,
        texture_out: *mut CVMetalTextureRef,
    ) -> CVReturn;
    fn CVMetalTextureGetTexture(image: CVMetalTextureRef) -> *mut c_void;
    fn CVMetalTextureCacheFlush(texture_cache: CVMetalTextureCacheRef, options: u64);
}

/// CoreVideo `CVPixelBuffer` の RAII ラッパー（`Send` 対応）
pub struct CVPixelBufferWrapper {
    raw: CVPixelBufferRef,
}

unsafe impl Send for CVPixelBufferWrapper {}

unsafe fn create_metal_iosurface_pixel_buffer_attrs() -> CFMutableDictionaryRef {
    let attrs = CFDictionaryCreateMutable(
        kCFAllocatorDefault,
        2,
        &kCFTypeDictionaryKeyCallBacks,
        &kCFTypeDictionaryValueCallBacks,
    );
    CFDictionarySetValue(attrs, kCVPixelBufferMetalCompatibilityKey, kCFBooleanTrue);
    let io_props = CFDictionaryCreateMutable(
        kCFAllocatorDefault,
        0,
        &kCFTypeDictionaryKeyCallBacks,
        &kCFTypeDictionaryValueCallBacks,
    );
    CFDictionarySetValue(attrs, kCVPixelBufferIOSurfacePropertiesKey, io_props as *const c_void);
    CFRelease(io_props as CFTypeRef);
    attrs
}

impl CVPixelBufferWrapper {
    /// 既存の `CVPixelBufferRef` を `CFRetain` して所有権保持する
    ///
    /// # Safety
    /// `raw` は有効な `CVPixelBufferRef` でなければならない。
    pub unsafe fn from_retained(raw: CVPixelBufferRef) -> CoreResult<Self> {
        if raw.is_null() {
            return Err(CoreError::PlatformIo("null CVPixelBufferRef".into()));
        }
        CFRetain(raw);
        Ok(Self { raw })
    }

    /// 呼び出し元が既に retain 済みの `CVPixelBufferRef` の所有権を受け取る
    ///
    /// # Safety
    /// `raw` は有効な `CVPixelBufferRef` であり、所有権を本構造体に移譲できる必要がある。
    pub unsafe fn from_owned(raw: CVPixelBufferRef) -> CoreResult<Self> {
        if raw.is_null() {
            return Err(CoreError::PlatformIo("null CVPixelBufferRef".into()));
        }
        Ok(Self { raw })
    }

    /// `IOSurface` + Metal 互換属性付きの空 BGRA8 `CVPixelBuffer` を確保する（GPU レンダーターゲット用・CPU 書き込みなし）
    pub fn new_bgra_surface(width: u32, height: u32) -> CoreResult<Self> {
        let mut pb: CVPixelBufferRef = ptr::null_mut();
        unsafe {
            let attrs = create_metal_iosurface_pixel_buffer_attrs();
            let status = CVPixelBufferCreate(
                kCFAllocatorDefault,
                width as usize,
                height as usize,
                K_CV_PIXEL_FORMAT_BGRA,
                attrs as CFDictionaryRef,
                &mut pb,
            );
            CFRelease(attrs as CFTypeRef);
            if status != K_CV_RETURN_SUCCESS || pb.is_null() {
                return Err(CoreError::PlatformIo(format!(
                    "CVPixelBufferCreate BGRA failed: {}",
                    status
                )));
            }
            Ok(Self { raw: pb })
        }
    }

    /// Metal 互換属性付きの BGRA8 `CVPixelBuffer` を確保し RGBA/BGRA データを書き込む
    pub fn new_bgra(width: u32, height: u32, bgra_data: &[u8]) -> CoreResult<Self> {
        let wrapper = Self::new_bgra_surface(width, height)?;
        unsafe {
            CVPixelBufferLockBaseAddress(wrapper.raw, 0);
            let dst_base = CVPixelBufferGetBaseAddress(wrapper.raw);
            let dst_stride = CVPixelBufferGetBytesPerRow(wrapper.raw);
            let src_stride = (width as usize) * 4;
            for row in 0..height as usize {
                let src_row = &bgra_data[row * src_stride..(row + 1) * src_stride];
                ptr::copy_nonoverlapping(
                    src_row.as_ptr(),
                    dst_base.add(row * dst_stride),
                    src_stride,
                );
            }
            CVPixelBufferUnlockBaseAddress(wrapper.raw, 0);
            Ok(wrapper)
        }
    }

    /// Metal 互換属性付きの NV12 (`420v`) `CVPixelBuffer` を確保し Y/UV プレーンを書き込む
    pub fn new_nv12(width: u32, height: u32, y_plane: &[u8], uv_plane: &[u8]) -> CoreResult<Self> {
        let mut pb: CVPixelBufferRef = ptr::null_mut();
        unsafe {
            let attrs = create_metal_iosurface_pixel_buffer_attrs();
            let status = CVPixelBufferCreate(
                kCFAllocatorDefault,
                width as usize,
                height as usize,
                K_CV_PIXEL_FORMAT_NV12_VIDEO,
                attrs as CFDictionaryRef,
                &mut pb,
            );
            CFRelease(attrs as CFTypeRef);
            if status != K_CV_RETURN_SUCCESS || pb.is_null() {
                return Err(CoreError::PlatformIo(format!(
                    "CVPixelBufferCreate NV12 failed: {}",
                    status
                )));
            }

            let wrapper = Self { raw: pb };
            CVPixelBufferLockBaseAddress(wrapper.raw, 0);
            let y_dst = CVPixelBufferGetBaseAddressOfPlane(wrapper.raw, 0);
            let y_dst_stride = CVPixelBufferGetBytesPerRowOfPlane(wrapper.raw, 0);
            let y_src_stride = width as usize;
            for row in 0..height as usize {
                ptr::copy_nonoverlapping(
                    y_plane[row * y_src_stride..(row + 1) * y_src_stride].as_ptr(),
                    y_dst.add(row * y_dst_stride),
                    y_src_stride,
                );
            }

            let uv_dst = CVPixelBufferGetBaseAddressOfPlane(wrapper.raw, 1);
            let uv_dst_stride = CVPixelBufferGetBytesPerRowOfPlane(wrapper.raw, 1);
            let uv_src_stride = width as usize;
            for row in 0..(height as usize / 2) {
                ptr::copy_nonoverlapping(
                    uv_plane[row * uv_src_stride..(row + 1) * uv_src_stride].as_ptr(),
                    uv_dst.add(row * uv_dst_stride),
                    uv_src_stride,
                );
            }
            CVPixelBufferUnlockBaseAddress(wrapper.raw, 0);
            Ok(wrapper)
        }
    }

    pub fn raw(&self) -> CVPixelBufferRef {
        self.raw
    }

    pub fn width(&self) -> u32 {
        unsafe { CVPixelBufferGetWidth(self.raw) as u32 }
    }

    pub fn height(&self) -> u32 {
        unsafe { CVPixelBufferGetHeight(self.raw) as u32 }
    }

    pub fn pixel_format(&self) -> u32 {
        unsafe { CVPixelBufferGetPixelFormatType(self.raw) }
    }

    /// 背後のハードウェア共有 `IOSurfaceRef` ポインタ（ゼロコピー検証用）
    pub fn iosurface_ptr(&self) -> *const c_void {
        unsafe { CVPixelBufferGetIOSurface(self.raw) }
    }

    /// CoreFoundation リファレンスカウント（プール再利用判定用）
    pub fn retain_count(&self) -> isize {
        unsafe { CFGetRetainCount(self.raw as CFTypeRef) }
    }

    /// ゼロコピー用の `core_types::media::VideoFrame` を生成する（ポインタ所有権は `self` が保持）
    pub fn as_video_frame(&self, pts: PtsUs) -> VideoFrame {
        let fmt = if self.pixel_format() == K_CV_PIXEL_FORMAT_NV12_VIDEO {
            PixelFormat::Nv12
        } else {
            PixelFormat::Bgra8
        };
        let stride = unsafe { CVPixelBufferGetBytesPerRowOfPlane(self.raw, 0) as u32 };
        VideoFrame {
            width: self.width(),
            height: self.height(),
            format: fmt,
            pts,
            handle: NativeBufferHandle::OpaqueHandle {
                ptr: self.raw as usize,
                stride,
            },
        }
    }
}

impl Drop for CVPixelBufferWrapper {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { CFRelease(self.raw as CFTypeRef) };
            self.raw = ptr::null_mut();
        }
    }
}

/// ゼロコピーで `wgpu::Texture` へ取り込んだ際の CoreVideo Metal テクスチャ保持オブジェクト
pub struct ImportedMetalTextures {
    pub kind: LayerInputKind,
    pub width: u32,
    pub height: u32,
    pub tex0: wgpu::Texture,
    pub tex1: Option<wgpu::Texture>,
    cv_tex0: CVMetalTextureRef,
    cv_tex1: CVMetalTextureRef,
}

impl Drop for ImportedMetalTextures {
    fn drop(&mut self) {
        unsafe {
            if !self.cv_tex0.is_null() {
                CFRelease(self.cv_tex0 as CFTypeRef);
            }
            if !self.cv_tex1.is_null() {
                CFRelease(self.cv_tex1 as CFTypeRef);
            }
        }
    }
}

/// `CVPixelBuffer` からゼロコピーで構築された `GpuLayer` と CoreVideo 参照保持ラッパー
pub struct ImportedGpuLayer {
    pub layer: GpuLayer,
    cv_tex0: CVMetalTextureRef,
    cv_tex1: CVMetalTextureRef,
}

impl ImportedGpuLayer {
    /// 取り込み元 `MTLTexture` が参照している `IOSurfaceRef` ポインタを返す
    pub fn metal_iosurface_ptr(&self) -> *const c_void {
        cv_metal_tex_iosurface_ptr(self.cv_tex0)
    }
}

/// GPU レンダーパス (`GpuCompositor`) の出力先兼 VideoToolbox 入力となるゼロコピー `IOSurface` サーフェス
pub struct AppleZeroCopyRenderSlot {
    pub view: wgpu::TextureView,
    pub texture: wgpu::Texture,
    cv_tex: CVMetalTextureRef,
    pub pixel_buffer: CVPixelBufferWrapper,
    iosurface: *const c_void,
    mtl_iosurface: *const c_void,
}

unsafe impl Send for AppleZeroCopyRenderSlot {}

impl AppleZeroCopyRenderSlot {
    /// `GpuCompositor::render_dual_into` に渡す外部ターゲット記述子を生成する
    pub fn as_external_target(&self) -> ExternalRenderTarget<'_> {
        ExternalRenderTarget {
            view: &self.view,
            format: ExternalRenderFormat::Bgra8Unorm,
            readable_for_preview_blit: true,
        }
    }

    pub fn iosurface_ptr(&self) -> *const c_void {
        self.iosurface
    }

    pub fn metal_iosurface_ptr(&self) -> *const c_void {
        self.mtl_iosurface
    }
}

impl Drop for AppleZeroCopyRenderSlot {
    fn drop(&mut self) {
        if !self.cv_tex.is_null() {
            unsafe { CFRelease(self.cv_tex as CFTypeRef) };
            self.cv_tex = ptr::null_mut();
        }
    }
}

/// Metal レンダーターゲットと VideoToolbox 間で `IOSurface` バッファを使い回すゼロコピーリングプール
pub struct AppleZeroCopyRenderSurfacePool {
    width: u32,
    height: u32,
    slots: Vec<AppleZeroCopyRenderSlot>,
    cursor: usize,
    max_slots: usize,
    allocations_total: u64,
    reuses_total: u64,
}

unsafe impl Send for AppleZeroCopyRenderSurfacePool {}

impl AppleZeroCopyRenderSurfacePool {
    pub fn new(
        importer: &AppleMetalZeroCopyImporter,
        device: &wgpu::Device,
        width: u32,
        height: u32,
        initial_slots: usize,
    ) -> CoreResult<Self> {
        let count = initial_slots.max(2);
        let max_slots = count.max(16);
        let mut pool = Self {
            width,
            height,
            slots: Vec::with_capacity(max_slots),
            cursor: 0,
            max_slots,
            allocations_total: 0,
            reuses_total: 0,
        };
        for _ in 0..count {
            let slot = pool.create_slot(importer, device)?;
            pool.slots.push(slot);
        }
        Ok(pool)
    }

    fn create_slot(
        &mut self,
        importer: &AppleMetalZeroCopyImporter,
        device: &wgpu::Device,
    ) -> CoreResult<AppleZeroCopyRenderSlot> {
        let pb = CVPixelBufferWrapper::new_bgra_surface(self.width, self.height)?;
        let iosurface = pb.iosurface_ptr();
        let usage = wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC;
        let (cv_tex, texture) = importer.wrap_plane_with_usage(
            device,
            pb.raw(),
            0,
            MTL_PIXEL_FORMAT_BGRA8_UNORM,
            wgpu::TextureFormat::Bgra8Unorm,
            self.width,
            self.height,
            usage,
            "ikakim-apple-rt-bgra",
        )?;
        let mtl_iosurface = cv_metal_tex_iosurface_ptr(cv_tex);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.allocations_total += 1;
        Ok(AppleZeroCopyRenderSlot {
            view,
            texture,
            cv_tex,
            pixel_buffer: pb,
            iosurface,
            mtl_iosurface,
        })
    }

    /// VideoToolbox 側で未参照 (`retain_count == 1`) のスロットをゼロコピー再利用する
    pub fn acquire_slot(
        &mut self,
        importer: &AppleMetalZeroCopyImporter,
        device: &wgpu::Device,
    ) -> CoreResult<&AppleZeroCopyRenderSlot> {
        for attempt in 0..200 {
            let n = self.slots.len();
            for offset in 0..n {
                let idx = (self.cursor + offset) % n;
                if self.slots[idx].pixel_buffer.retain_count() == 1 {
                    self.cursor = (idx + 1) % n;
                    self.reuses_total += 1;
                    return Ok(&self.slots[idx]);
                }
            }
            if self.slots.len() < self.max_slots {
                let slot = self.create_slot(importer, device)?;
                self.slots.push(slot);
                let idx = self.slots.len() - 1;
                self.cursor = 0;
                return Ok(&self.slots[idx]);
            }
            if attempt < 199 {
                std::thread::sleep(std::time::Duration::from_micros(250));
            }
        }
        Err(CoreError::PlatformIo(
            "AppleZeroCopyRenderSurfacePool: all slots busy in VideoToolbox".into(),
        ))
    }

    pub fn allocations_total(&self) -> u64 {
        self.allocations_total
    }

    pub fn reuses_total(&self) -> u64 {
        self.reuses_total
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }
}

fn cv_metal_tex_iosurface_ptr(cv_tex: CVMetalTextureRef) -> *const c_void {
    extern "C" {
        fn objc_msgSend();
        fn sel_registerName(name: *const std::ffi::c_char) -> *mut c_void;
    }
    if cv_tex.is_null() {
        return ptr::null();
    }
    unsafe {
        let mtl_ptr = CVMetalTextureGetTexture(cv_tex);
        if mtl_ptr.is_null() {
            return ptr::null();
        }
        let msg_send: extern "C" fn(*mut c_void, *mut c_void) -> *const c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let sel = sel_registerName(c"iosurface".as_ptr());
        msg_send(mtl_ptr, sel)
    }
}

impl Drop for ImportedGpuLayer {
    fn drop(&mut self) {
        unsafe {
            if !self.cv_tex0.is_null() {
                CFRelease(self.cv_tex0 as CFTypeRef);
            }
            if !self.cv_tex1.is_null() {
                CFRelease(self.cv_tex1 as CFTypeRef);
            }
        }
    }
}

/// `CVMetalTextureCache` と `wgpu_hal::api::Metal` を直結するゼロコピーインポータ
pub struct AppleMetalZeroCopyImporter {
    cache: CVMetalTextureCacheRef,
}

unsafe impl Send for AppleMetalZeroCopyImporter {}

impl AppleMetalZeroCopyImporter {
    pub fn new(device: &wgpu::Device) -> CoreResult<Self> {
        let mtl_dev_ptr = unsafe {
            device.as_hal::<HalMetal, _, _>(|hal_dev| {
                hal_dev.map(|d| d.raw_device().lock().as_ptr() as *mut c_void)
            })
        }
        .flatten()
        .ok_or_else(|| CoreError::PlatformIo("active wgpu backend is not Metal".into()))?;

        let mut cache: CVMetalTextureCacheRef = ptr::null_mut();
        let status = unsafe {
            CVMetalTextureCacheCreate(
                kCFAllocatorDefault,
                ptr::null(),
                mtl_dev_ptr,
                ptr::null(),
                &mut cache,
            )
        };
        if status != K_CV_RETURN_SUCCESS || cache.is_null() {
            return Err(CoreError::PlatformIo(format!(
                "CVMetalTextureCacheCreate failed: {}",
                status
            )));
        }

        Ok(Self { cache })
    }

    pub fn flush(&self) {
        unsafe { CVMetalTextureCacheFlush(self.cache, 0) };
    }

    #[allow(clippy::type_complexity)]
    fn import_raw_planes(
        &self,
        device: &wgpu::Device,
        pb: CVPixelBufferRef,
    ) -> CoreResult<(
        LayerInputKind,
        u32,
        u32,
        wgpu::Texture,
        Option<wgpu::Texture>,
        CVMetalTextureRef,
        CVMetalTextureRef,
    )> {
        if pb.is_null() {
            return Err(CoreError::PlatformIo("null CVPixelBuffer".into()));
        }
        let w = unsafe { CVPixelBufferGetWidth(pb) as u32 };
        let h = unsafe { CVPixelBufferGetHeight(pb) as u32 };
        let fmt = unsafe { CVPixelBufferGetPixelFormatType(pb) };

        if fmt == K_CV_PIXEL_FORMAT_NV12_VIDEO {
            let (cv0, tex0) = self.wrap_plane(
                device,
                pb,
                0,
                MTL_PIXEL_FORMAT_R8_UNORM,
                wgpu::TextureFormat::R8Unorm,
                w,
                h,
                "ikakim-apple-nv12-y",
            )?;
            let (cv1, tex1) = self.wrap_plane(
                device,
                pb,
                1,
                MTL_PIXEL_FORMAT_RG8_UNORM,
                wgpu::TextureFormat::Rg8Unorm,
                w / 2,
                h / 2,
                "ikakim-apple-nv12-uv",
            )?;
            Ok((LayerInputKind::Nv12, w, h, tex0, Some(tex1), cv0, cv1))
        } else {
            let (cv0, tex0) = self.wrap_plane(
                device,
                pb,
                0,
                MTL_PIXEL_FORMAT_BGRA8_UNORM,
                wgpu::TextureFormat::Bgra8Unorm,
                w,
                h,
                "ikakim-apple-bgra",
            )?;
            Ok((LayerInputKind::Rgba, w, h, tex0, None, cv0, ptr::null_mut()))
        }
    }

    /// `CVPixelBuffer` から CPU コピーなしで直接 `wgpu::Texture`（1または2プレーン）を生成する
    pub fn import_cv_pixel_buffer(
        &self,
        device: &wgpu::Device,
        pb: CVPixelBufferRef,
    ) -> CoreResult<ImportedMetalTextures> {
        let (kind, width, height, tex0, tex1, cv_tex0, cv_tex1) =
            self.import_raw_planes(device, pb)?;
        Ok(ImportedMetalTextures {
            kind,
            width,
            height,
            tex0,
            tex1,
            cv_tex0,
            cv_tex1,
        })
    }

    /// `CVPixelBuffer` をそのまま `GpuLayer` として `GpuCompositor` へ渡せる形で生成する
    pub fn import_to_gpu_layer(
        &self,
        comp: &GpuCompositor,
        pb: &CVPixelBufferWrapper,
        layer_id: usize,
        is_base_layer: bool,
        transform: LayerTransform,
    ) -> CoreResult<ImportedGpuLayer> {
        let (kind, width, height, tex0, tex1, cv_tex0, cv_tex1) =
            self.import_raw_planes(&comp.context().device, pb.raw())?;

        let layer = comp.create_layer_from_textures(
            layer_id,
            is_base_layer,
            kind,
            ColorSpace::Bt709Limited,
            width,
            height,
            transform,
            tex0,
            tex1,
        );
        Ok(ImportedGpuLayer {
            layer,
            cv_tex0,
            cv_tex1,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn wrap_plane(
        &self,
        device: &wgpu::Device,
        pb: CVPixelBufferRef,
        plane_idx: usize,
        mtl_fmt: usize,
        wgpu_fmt: wgpu::TextureFormat,
        width: u32,
        height: u32,
        label: &str,
    ) -> CoreResult<(CVMetalTextureRef, wgpu::Texture)> {
        self.wrap_plane_with_usage(
            device,
            pb,
            plane_idx,
            mtl_fmt,
            wgpu_fmt,
            width,
            height,
            wgpu::TextureUsages::TEXTURE_BINDING,
            label,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn wrap_plane_with_usage(
        &self,
        device: &wgpu::Device,
        pb: CVPixelBufferRef,
        plane_idx: usize,
        mtl_fmt: usize,
        wgpu_fmt: wgpu::TextureFormat,
        width: u32,
        height: u32,
        usage: wgpu::TextureUsages,
        label: &str,
    ) -> CoreResult<(CVMetalTextureRef, wgpu::Texture)> {
        let mut cv_tex: CVMetalTextureRef = ptr::null_mut();
        let status = unsafe {
            CVMetalTextureCacheCreateTextureFromImage(
                kCFAllocatorDefault,
                self.cache,
                pb,
                ptr::null(),
                mtl_fmt,
                width as usize,
                height as usize,
                plane_idx,
                &mut cv_tex,
            )
        };
        if status != K_CV_RETURN_SUCCESS || cv_tex.is_null() {
            return Err(CoreError::PlatformIo(format!(
                "CVMetalTextureCacheCreateTextureFromImage failed: {} (fmt={})",
                status, mtl_fmt
            )));
        }

        let mtl_ptr = unsafe { CVMetalTextureGetTexture(cv_tex) };
        if mtl_ptr.is_null() {
            unsafe { CFRelease(cv_tex as CFTypeRef) };
            return Err(CoreError::PlatformIo("CVMetalTextureGetTexture returned null".into()));
        }

        let owned_mtl_tex = unsafe {
            let tex_ref = &*(mtl_ptr as *const metal::TextureRef);
            tex_ref.to_owned()
        };

        let hal_tex = unsafe {
            wgpu::hal::metal::Device::texture_from_raw(
                owned_mtl_tex,
                wgpu_fmt,
                metal::MTLTextureType::D2,
                1,
                1,
                wgpu::hal::CopyExtent {
                    width,
                    height,
                    depth: 1,
                },
            )
        };

        let wgpu_tex = unsafe {
            device.create_texture_from_hal::<HalMetal>(
                hal_tex,
                &wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu_fmt,
                    usage,
                    view_formats: &[],
                },
            )
        };

        Ok((cv_tex, wgpu_tex))
    }
}

impl Drop for AppleMetalZeroCopyImporter {
    fn drop(&mut self) {
        if !self.cache.is_null() {
            unsafe { CFRelease(self.cache as CFTypeRef) };
            self.cache = ptr::null_mut();
        }
    }
}

/// CoreFoundation i32 数値ラッパー生成補助
pub(crate) unsafe fn cf_number_i32(val: i32) -> CFNumberRef {
    CFNumberCreate(
        kCFAllocatorDefault,
        K_CF_NUMBER_SINT32_TYPE,
        (&val as *const i32) as *const c_void,
    )
}
