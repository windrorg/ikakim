//! Linux `DMA-BUF` -> Surfaceless EGL (`EGL_EXT_image_dma_buf_import`) /
//! Vulkan (`VK_EXT_external_memory_dma_buf`) ゼロコピー GPU インポータ & `mmap` フォールバック

use crate::v4l2::read_dmabuf_via_mmap;
use core_gpu::{ColorSpace, GpuCompositor, GpuLayer};
use core_types::media::{LayerTransform, NativeBufferHandle, PixelFormat, VideoFrame};
use core_types::{CoreError, CoreResult};
use std::ffi::CStr;
use std::os::fd::{AsRawFd, RawFd};
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::ptr;
use wgpu::hal::api::Vulkan as HalVulkan;

const RTLD_LAZY: c_int = 1;

const EGL_NONE: i32 = 0x3038;
const EGL_TRUE: u32 = 1;
const EGL_OPENGL_ES_API: u32 = 0x30a0;
const EGL_PLATFORM_SURFACELESS_MESA: u32 = 0x31dd;
const EGL_SURFACE_TYPE: i32 = 0x3033;
const EGL_RENDERABLE_TYPE: i32 = 0x3040;
const EGL_OPENGL_ES3_BIT: i32 = 0x0040;
const EGL_CONTEXT_CLIENT_VERSION: i32 = 0x3098;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: i32 = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: i32 = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: i32 = 0x3274;
const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8d65;
const GL_NO_ERROR: u32 = 0;

// DRM FourCC
const DRM_FORMAT_YUYV: i32 = u32::from_le_bytes(*b"YUYV") as i32;
const DRM_FORMAT_NV12: i32 = u32::from_le_bytes(*b"NV12") as i32;
const DRM_FORMAT_ABGR8888: i32 = u32::from_le_bytes(*b"AB24") as i32;

extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
}

type EglDisplay = *mut c_void;
type EglContext = *mut c_void;
type EglConfig = *mut c_void;
type EglImageKHR = *mut c_void;

type FnEglGetProcAddress = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type FnEglGetError = unsafe extern "C" fn() -> i32;
type FnEglInitialize = unsafe extern "C" fn(EglDisplay, *mut i32, *mut i32) -> u32;
type FnEglTerminate = unsafe extern "C" fn(EglDisplay) -> u32;
type FnEglBindApi = unsafe extern "C" fn(u32) -> u32;
type FnEglChooseConfig =
    unsafe extern "C" fn(EglDisplay, *const i32, *mut EglConfig, i32, *mut i32) -> u32;
type FnEglCreateContext =
    unsafe extern "C" fn(EglDisplay, EglConfig, EglContext, *const i32) -> EglContext;
type FnEglDestroyContext = unsafe extern "C" fn(EglDisplay, EglContext) -> u32;
type FnEglMakeCurrent =
    unsafe extern "C" fn(EglDisplay, *mut c_void, *mut c_void, EglContext) -> u32;
type FnEglGetPlatformDisplayExt =
    unsafe extern "C" fn(u32, *mut c_void, *const i32) -> EglDisplay;
type FnEglCreateImageKhr =
    unsafe extern "C" fn(EglDisplay, EglContext, u32, *mut c_void, *const i32) -> EglImageKHR;
type FnEglDestroyImageKhr = unsafe extern "C" fn(EglDisplay, EglImageKHR) -> u32;

type FnGlGenTextures = unsafe extern "C" fn(i32, *mut c_uint);
type FnGlDeleteTextures = unsafe extern "C" fn(i32, *const c_uint);
type FnGlBindTexture = unsafe extern "C" fn(u32, c_uint);
type FnGlGetError = unsafe extern "C" fn() -> u32;
type FnGlEglImageTargetTexture2DOes = unsafe extern "C" fn(u32, EglImageKHR);

/// Mesa `EGL_PLATFORM_SURFACELESS_MESA` + GLES3 による `EGL_EXT_image_dma_buf_import` コンテキスト
pub struct SurfacelessEglDmaBuf {
    lib_egl: *mut c_void,
    lib_gles: *mut c_void,
    dpy: EglDisplay,
    ctx: EglContext,
    egl_get_error: FnEglGetError,
    egl_terminate: FnEglTerminate,
    egl_destroy_context: FnEglDestroyContext,
    egl_make_current: FnEglMakeCurrent,
    egl_create_image: FnEglCreateImageKhr,
    egl_destroy_image: FnEglDestroyImageKhr,
    gl_gen_textures: FnGlGenTextures,
    gl_delete_textures: FnGlDeleteTextures,
    gl_bind_texture: FnGlBindTexture,
    gl_get_error: FnGlGetError,
    gl_image_target_tex: FnGlEglImageTargetTexture2DOes,
}

unsafe impl Send for SurfacelessEglDmaBuf {}

impl SurfacelessEglDmaBuf {
    pub fn new() -> CoreResult<Self> {
        unsafe {
            let lib_egl = dlopen(c"libEGL.so.1".as_ptr(), RTLD_LAZY);
            let lib_gles = dlopen(c"libGLESv2.so.2".as_ptr(), RTLD_LAZY);
            if lib_egl.is_null() || lib_gles.is_null() {
                if !lib_egl.is_null() {
                    dlclose(lib_egl);
                }
                if !lib_gles.is_null() {
                    dlclose(lib_gles);
                }
                return Err(CoreError::PlatformIo(
                    "failed to dlopen libEGL.so.1 / libGLESv2.so.2".into(),
                ));
            }

            macro_rules! load_egl {
                ($sym:literal, $ty:ty) => {{
                    let p = dlsym(lib_egl, $sym.as_ptr());
                    if p.is_null() {
                        dlclose(lib_egl);
                        dlclose(lib_gles);
                        return Err(CoreError::PlatformIo(format!(
                            "dlsym({}) failed",
                            $sym.to_string_lossy()
                        )));
                    }
                    std::mem::transmute::<*mut c_void, $ty>(p)
                }};
            }
            macro_rules! load_gl {
                ($sym:literal, $ty:ty) => {{
                    let p = dlsym(lib_gles, $sym.as_ptr());
                    if p.is_null() {
                        dlclose(lib_egl);
                        dlclose(lib_gles);
                        return Err(CoreError::PlatformIo(format!(
                            "dlsym({}) failed",
                            $sym.to_string_lossy()
                        )));
                    }
                    std::mem::transmute::<*mut c_void, $ty>(p)
                }};
            }

            let egl_get_proc = load_egl!(c"eglGetProcAddress", FnEglGetProcAddress);
            let egl_get_error = load_egl!(c"eglGetError", FnEglGetError);
            let egl_initialize = load_egl!(c"eglInitialize", FnEglInitialize);
            let egl_terminate = load_egl!(c"eglTerminate", FnEglTerminate);
            let egl_bind_api = load_egl!(c"eglBindAPI", FnEglBindApi);
            let egl_choose_config = load_egl!(c"eglChooseConfig", FnEglChooseConfig);
            let egl_create_context = load_egl!(c"eglCreateContext", FnEglCreateContext);
            let egl_destroy_context = load_egl!(c"eglDestroyContext", FnEglDestroyContext);
            let egl_make_current = load_egl!(c"eglMakeCurrent", FnEglMakeCurrent);

            let gl_gen_textures = load_gl!(c"glGenTextures", FnGlGenTextures);
            let gl_delete_textures = load_gl!(c"glDeleteTextures", FnGlDeleteTextures);
            let gl_bind_texture = load_gl!(c"glBindTexture", FnGlBindTexture);
            let gl_get_error = load_gl!(c"glGetError", FnGlGetError);

            let p_get_plat = egl_get_proc(c"eglGetPlatformDisplayEXT".as_ptr());
            let p_create_img = egl_get_proc(c"eglCreateImageKHR".as_ptr());
            let p_destroy_img = egl_get_proc(c"eglDestroyImageKHR".as_ptr());
            let p_img_target = egl_get_proc(c"glEGLImageTargetTexture2DOES".as_ptr());
            if p_get_plat.is_null()
                || p_create_img.is_null()
                || p_destroy_img.is_null()
                || p_img_target.is_null()
            {
                dlclose(lib_egl);
                dlclose(lib_gles);
                return Err(CoreError::PlatformIo(
                    "EGL DMA-BUF / Surfaceless extensions not available".into(),
                ));
            }
            let egl_get_platform_display: FnEglGetPlatformDisplayExt =
                std::mem::transmute(p_get_plat);
            let egl_create_image: FnEglCreateImageKhr = std::mem::transmute(p_create_img);
            let egl_destroy_image: FnEglDestroyImageKhr = std::mem::transmute(p_destroy_img);
            let gl_image_target_tex: FnGlEglImageTargetTexture2DOes =
                std::mem::transmute(p_img_target);

            let dpy = egl_get_platform_display(
                EGL_PLATFORM_SURFACELESS_MESA,
                ptr::null_mut(),
                ptr::null(),
            );
            if dpy.is_null() || egl_initialize(dpy, ptr::null_mut(), ptr::null_mut()) != EGL_TRUE {
                dlclose(lib_egl);
                dlclose(lib_gles);
                return Err(CoreError::PlatformIo("eglInitialize(SURFACELESS_MESA) failed".into()));
            }
            egl_bind_api(EGL_OPENGL_ES_API);
            let cfg_attrs = [
                EGL_SURFACE_TYPE,
                0,
                EGL_RENDERABLE_TYPE,
                EGL_OPENGL_ES3_BIT,
                EGL_NONE,
            ];
            let mut cfg: EglConfig = ptr::null_mut();
            let mut n = 0;
            if egl_choose_config(dpy, cfg_attrs.as_ptr(), &mut cfg, 1, &mut n) != EGL_TRUE || n == 0
            {
                egl_terminate(dpy);
                dlclose(lib_egl);
                dlclose(lib_gles);
                return Err(CoreError::PlatformIo("eglChooseConfig failed".into()));
            }
            let ctx_attrs = [EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE];
            let ctx = egl_create_context(dpy, cfg, ptr::null_mut(), ctx_attrs.as_ptr());
            if ctx.is_null()
                || egl_make_current(dpy, ptr::null_mut(), ptr::null_mut(), ctx) != EGL_TRUE
            {
                if !ctx.is_null() {
                    egl_destroy_context(dpy, ctx);
                }
                egl_terminate(dpy);
                dlclose(lib_egl);
                dlclose(lib_gles);
                return Err(CoreError::PlatformIo("eglCreateContext/MakeCurrent failed".into()));
            }

            Ok(Self {
                lib_egl,
                lib_gles,
                dpy,
                ctx,
                egl_get_error,
                egl_terminate,
                egl_destroy_context,
                egl_make_current,
                egl_create_image,
                egl_destroy_image,
                gl_gen_textures,
                gl_delete_textures,
                gl_bind_texture,
                gl_get_error,
                gl_image_target_tex,
            })
        }
    }

    /// `DMA-BUF` FD を `EGLImageKHR` 経由で外部 GL テクスチャ (`GL_TEXTURE_EXTERNAL_OES`) へゼロコピー取り込みする
    pub fn bind_dmabuf_external_texture(
        &self,
        fd: RawFd,
        width: u32,
        height: u32,
        format: PixelFormat,
        offset: u32,
        stride: u32,
    ) -> CoreResult<EglExternalTexture<'_>> {
        let fourcc = match format {
            PixelFormat::Yuyv => DRM_FORMAT_YUYV,
            PixelFormat::Nv12 => DRM_FORMAT_NV12,
            PixelFormat::Rgba8 | PixelFormat::Bgra8 => DRM_FORMAT_ABGR8888,
            PixelFormat::Mjpeg => {
                return Err(CoreError::UnsupportedFormat(
                    "MJPEG must be decoded before DMA-BUF EGL import".into(),
                ))
            }
        };

        let attrs = [
            EGL_WIDTH,
            width as i32,
            EGL_HEIGHT,
            height as i32,
            EGL_LINUX_DRM_FOURCC_EXT,
            fourcc,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            fd,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            offset as i32,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            stride as i32,
            EGL_NONE,
        ];

        unsafe {
            let img = (self.egl_create_image)(
                self.dpy,
                ptr::null_mut(),
                EGL_LINUX_DMA_BUF_EXT,
                ptr::null_mut(),
                attrs.as_ptr(),
            );
            if img.is_null() {
                let err = (self.egl_get_error)();
                return Err(CoreError::PlatformIo(format!(
                    "eglCreateImageKHR(DMA-BUF fourcc=0x{:08x}) failed: 0x{:x}",
                    fourcc, err
                )));
            }
            let mut tex: c_uint = 0;
            (self.gl_gen_textures)(1, &mut tex);
            (self.gl_bind_texture)(GL_TEXTURE_EXTERNAL_OES, tex);
            (self.gl_image_target_tex)(GL_TEXTURE_EXTERNAL_OES, img);
            let gl_err = (self.gl_get_error)();
            if gl_err != GL_NO_ERROR {
                (self.gl_delete_textures)(1, &tex);
                (self.egl_destroy_image)(self.dpy, img);
                return Err(CoreError::PlatformIo(format!(
                    "glEGLImageTargetTexture2DOES failed: 0x{:x}",
                    gl_err
                )));
            }
            Ok(EglExternalTexture {
                owner: self,
                image: img,
                gl_tex: tex,
            })
        }
    }
}

impl Drop for SurfacelessEglDmaBuf {
    fn drop(&mut self) {
        unsafe {
            (self.egl_make_current)(self.dpy, ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
            (self.egl_destroy_context)(self.dpy, self.ctx);
            (self.egl_terminate)(self.dpy);
            dlclose(self.lib_egl);
            dlclose(self.lib_gles);
        }
    }
}

pub struct EglExternalTexture<'a> {
    owner: &'a SurfacelessEglDmaBuf,
    pub image: EglImageKHR,
    pub gl_tex: c_uint,
}

impl Drop for EglExternalTexture<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.owner.gl_delete_textures)(1, &self.gl_tex);
            (self.owner.egl_destroy_image)(self.owner.dpy, self.image);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxImportMode {
    VulkanDmaBufZeroCopy,
    SurfacelessEglAndMmap,
    DmaBufMmapFallback,
    CpuBuffer,
}

pub struct ImportedLinuxLayer {
    pub layer: GpuLayer,
    pub mode: LinuxImportMode,
}

/// Linux `DMA-BUF` (`Vulkan VK_EXT_external_memory_dma_buf` / `Surfaceless EGL` / `mmap`) インポータ
pub struct LinuxDmaBufImporter {
    egl: Option<SurfacelessEglDmaBuf>,
    vulkan_dmabuf_ext: bool,
}

impl LinuxDmaBufImporter {
    pub fn new(device: &wgpu::Device) -> Self {
        let vulkan_dmabuf_ext = unsafe {
            device.as_hal::<HalVulkan, _, _>(|hal_dev| {
                hal_dev
                    .map(|d| {
                        let exts: &[&'static CStr] = d.enabled_device_extensions();
                        exts.iter()
                            .any(|e| e.to_bytes() == b"VK_EXT_external_memory_dma_buf")
                    })
                    .unwrap_or(false)
            })
        }
        .unwrap_or(false);

        let egl = SurfacelessEglDmaBuf::new().ok();
        Self {
            egl,
            vulkan_dmabuf_ext,
        }
    }

    pub fn has_surfaceless_egl(&self) -> bool {
        self.egl.is_some()
    }

    pub fn has_vulkan_dmabuf_extension(&self) -> bool {
        self.vulkan_dmabuf_ext
    }

    /// `VideoFrame`（`DMA-BUF` / `CpuFallback`）を `GpuCompositor` の `GpuLayer` として取り込む
    pub fn import_frame_to_layer(
        &self,
        comp: &GpuCompositor,
        frame: &VideoFrame,
        layer_id: usize,
        is_base_layer: bool,
        transform: LayerTransform,
    ) -> CoreResult<ImportedLinuxLayer> {
        match &frame.handle {
            NativeBufferHandle::DmaBuf {
                fd,
                offset,
                stride,
                modifier: _,
            } => {
                // 1. Surfaceless EGL での zero-copy DMA-BUF bind 可否を確認（GLES/実機・VM EGL テスト用）
                let mut egl_verified = false;
                if let Some(ref egl) = self.egl {
                    if frame.format == PixelFormat::Yuyv
                        || frame.format == PixelFormat::Rgba8
                        || frame.format == PixelFormat::Bgra8
                    {
                        egl_verified = egl
                            .bind_dmabuf_external_texture(
                                fd.as_raw_fd(),
                                frame.width,
                                frame.height,
                                frame.format,
                                *offset,
                                *stride,
                            )
                            .is_ok();
                    }
                }

                // 2. Vulkan バックエンドが wgpu 側で VK_EXT_external_memory_dma_buf を有効化していれば直接 VkImage インポート
                if self.vulkan_dmabuf_ext && frame.format == PixelFormat::Rgba8 {
                    if let Ok(layer) = self.try_import_vulkan_rgba(
                        comp,
                        fd.as_raw_fd(),
                        frame.width,
                        frame.height,
                        layer_id,
                        is_base_layer,
                        transform,
                    ) {
                        return Ok(ImportedLinuxLayer {
                            layer,
                            mode: LinuxImportMode::VulkanDmaBufZeroCopy,
                        });
                    }
                }

                // 3. 仮想GPU (`llvmpipe`) 等で VkImage インポート不可の場合: DMA-BUF FD を mmap して `wgpu` へ投入
                let byte_len = match frame.format {
                    PixelFormat::Yuyv => (*stride as usize) * (frame.height as usize),
                    PixelFormat::Nv12 => {
                        let y_bytes = (*stride as usize) * (frame.height as usize);
                        let uv_bytes = (*stride as usize) * ((frame.height / 2) as usize);
                        y_bytes + uv_bytes
                    }
                    PixelFormat::Rgba8 | PixelFormat::Bgra8 => {
                        (*stride as usize) * (frame.height as usize)
                    }
                    PixelFormat::Mjpeg => {
                        return Err(CoreError::UnsupportedFormat(
                            "MJPEG frame cannot be uploaded to GPU without decode".into(),
                        ))
                    }
                };
                let raw = read_dmabuf_via_mmap(fd.as_raw_fd(), *offset as usize, byte_len)?;
                let layer = self.upload_cpu_pixels_to_layer(
                    comp,
                    frame.width,
                    frame.height,
                    frame.format,
                    *stride,
                    &raw,
                    layer_id,
                    is_base_layer,
                    transform,
                )?;
                Ok(ImportedLinuxLayer {
                    layer,
                    mode: if egl_verified {
                        LinuxImportMode::SurfacelessEglAndMmap
                    } else {
                        LinuxImportMode::DmaBufMmapFallback
                    },
                })
            }
            NativeBufferHandle::CpuFallback { data, stride } => {
                let layer = self.upload_cpu_pixels_to_layer(
                    comp,
                    frame.width,
                    frame.height,
                    frame.format,
                    *stride,
                    data,
                    layer_id,
                    is_base_layer,
                    transform,
                )?;
                Ok(ImportedLinuxLayer {
                    layer,
                    mode: LinuxImportMode::CpuBuffer,
                })
            }
            NativeBufferHandle::OpaqueHandle { .. } => Err(CoreError::UnsupportedFormat(
                "OpaqueHandle is not supported on Linux".into(),
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn try_import_vulkan_rgba(
        &self,
        comp: &GpuCompositor,
        dmabuf_fd: RawFd,
        width: u32,
        height: u32,
        layer_id: usize,
        is_base_layer: bool,
        transform: LayerTransform,
    ) -> CoreResult<GpuLayer> {
        use ash::vk;
        const DUPFD_CLOEXEC: c_int = 1030;
        extern "C" {
            fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
            fn close(fd: c_int) -> c_int;
        }
        let dup_fd = unsafe { fcntl(dmabuf_fd, DUPFD_CLOEXEC, 0) };
        if dup_fd < 0 {
            return Err(CoreError::PlatformIo("fcntl dup failed".into()));
        }

        let res = unsafe {
            comp.context().device.as_hal::<HalVulkan, _, _>(|hal_dev_opt| {
                let Some(hal_dev) = hal_dev_opt else {
                    close(dup_fd);
                    return Err(CoreError::PlatformIo("not vulkan".into()));
                };
                let vk_dev = hal_dev.raw_device();
                let mut ext_img_info = vk::ExternalMemoryImageCreateInfo::default()
                    .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
                let img_info = vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(vk::Format::R8G8B8A8_UNORM)
                    .extent(vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::LINEAR)
                    .usage(vk::ImageUsageFlags::SAMPLED)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .initial_layout(vk::ImageLayout::UNDEFINED)
                    .push_next(&mut ext_img_info);

                let Ok(vk_image) = vk_dev.create_image(&img_info, None) else {
                    close(dup_fd);
                    return Err(CoreError::PlatformIo("vkCreateImage failed".into()));
                };
                let req = vk_dev.get_image_memory_requirements(vk_image);
                let mut import_info = vk::ImportMemoryFdInfoKHR::default()
                    .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
                    .fd(dup_fd);
                let alloc_info = vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(req.memory_type_bits.trailing_zeros())
                    .push_next(&mut import_info);

                let Ok(vk_mem) = vk_dev.allocate_memory(&alloc_info, None) else {
                    vk_dev.destroy_image(vk_image, None);
                    close(dup_fd);
                    return Err(CoreError::PlatformIo("vkAllocateMemory(DMA_BUF) failed".into()));
                };
                if vk_dev.bind_image_memory(vk_image, vk_mem, 0).is_err() {
                    vk_dev.free_memory(vk_mem, None);
                    vk_dev.destroy_image(vk_image, None);
                    return Err(CoreError::PlatformIo("vkBindImageMemory failed".into()));
                }

                let hal_desc = wgpu::hal::TextureDescriptor {
                    label: Some("ikakim-vk-dmabuf"),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::hal::TextureUses::RESOURCE,
                    memory_flags: wgpu::hal::MemoryFlags::empty(),
                    view_formats: vec![],
                };
                let vk_dev_clone = vk_dev.clone();
                let drop_cb: wgpu::hal::DropCallback = Box::new(move || {
                    vk_dev_clone.destroy_image(vk_image, None);
                    vk_dev_clone.free_memory(vk_mem, None);
                });
                let hal_tex =
                    wgpu::hal::vulkan::Device::texture_from_raw(vk_image, &hal_desc, Some(drop_cb));
                let wgpu_tex = comp.context().device.create_texture_from_hal::<HalVulkan>(
                    hal_tex,
                    &wgpu::TextureDescriptor {
                        label: Some("ikakim-vk-dmabuf"),
                        size: wgpu::Extent3d {
                            width,
                            height,
                            depth_or_array_layers: 1,
                        },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        usage: wgpu::TextureUsages::TEXTURE_BINDING,
                        view_formats: &[],
                    },
                );
                Ok(comp.create_layer_from_textures(
                    layer_id,
                    is_base_layer,
                    core_gpu::LayerInputKind::Rgba,
                    ColorSpace::Bt709Full,
                    width,
                    height,
                    transform,
                    wgpu_tex,
                    None,
                ))
            })
        };

        match res {
            Some(r) => r,
            None => {
                unsafe { close(dup_fd) };
                Err(CoreError::PlatformIo("active wgpu backend is not Vulkan".into()))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn upload_cpu_pixels_to_layer(
        &self,
        comp: &GpuCompositor,
        width: u32,
        height: u32,
        format: PixelFormat,
        stride: u32,
        data: &[u8],
        layer_id: usize,
        is_base_layer: bool,
        transform: LayerTransform,
    ) -> CoreResult<GpuLayer> {
        match format {
            PixelFormat::Yuyv => {
                let row_bytes = (width as usize) * 2;
                let tight = repack_rows(data, stride as usize, row_bytes, height as usize);
                comp.create_yuyv_layer(
                    layer_id,
                    is_base_layer,
                    ColorSpace::Bt601Limited,
                    width,
                    height,
                    &tight,
                    transform,
                )
            }
            PixelFormat::Nv12 => {
                let y_len = (stride as usize) * (height as usize);
                let y_tight = repack_rows(&data[..y_len], stride as usize, width as usize, height as usize);
                let uv_tight = repack_rows(
                    &data[y_len..],
                    stride as usize,
                    width as usize,
                    (height / 2) as usize,
                );
                comp.create_nv12_layer(
                    layer_id,
                    is_base_layer,
                    false,
                    ColorSpace::Bt709Limited,
                    width,
                    height,
                    &y_tight,
                    &uv_tight,
                    transform,
                )
            }
            PixelFormat::Rgba8 => {
                let tight = repack_rows(data, stride as usize, (width as usize) * 4, height as usize);
                comp.create_rgba_layer(
                    layer_id,
                    is_base_layer,
                    false,
                    width,
                    height,
                    &tight,
                    transform,
                )
            }
            PixelFormat::Bgra8 => {
                let tight = repack_rows(data, stride as usize, (width as usize) * 4, height as usize);
                let mut rgba = vec![0u8; tight.len()];
                for (s, d) in tight.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                    d[3] = s[3];
                }
                comp.create_rgba_layer(
                    layer_id,
                    is_base_layer,
                    false,
                    width,
                    height,
                    &rgba,
                    transform,
                )
            }
            PixelFormat::Mjpeg => Err(CoreError::UnsupportedFormat(
                "MJPEG cannot be uploaded directly as GPU pixels".into(),
            )),
        }
    }
}

fn repack_rows(src: &[u8], src_stride: usize, dst_row_bytes: usize, rows: usize) -> Vec<u8> {
    if src_stride <= dst_row_bytes && src.len() >= dst_row_bytes * rows {
        return src[..dst_row_bytes * rows].to_vec();
    }
    let mut out = Vec::with_capacity(dst_row_bytes * rows);
    for r in 0..rows {
        let start = r * src_stride;
        let end = (start + dst_row_bytes).min(src.len());
        out.extend_from_slice(&src[start..end]);
        while out.len() < (r + 1) * dst_row_bytes {
            out.push(0);
        }
    }
    out
}
