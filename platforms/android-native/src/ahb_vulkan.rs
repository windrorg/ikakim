//! Android `AHardwareBuffer` + `VK_ANDROID_external_memory_android_hardware_buffer` Zero-Copy Bridge (M4: Issue #8)

use core_gpu::{ColorSpace, GpuCompositor, GpuLayer, LayerInputKind};
use core_types::media::LayerTransform;
use core_types::{CoreError, CoreResult};
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

pub const VK_ANDROID_EXTERNAL_MEMORY_ANDROID_HARDWARE_BUFFER_EXTENSION_NAME: &str =
    "VK_ANDROID_external_memory_android_hardware_buffer";
pub const VK_EXTERNAL_MEMORY_HANDLE_TYPE_ANDROID_HARDWARE_BUFFER_BIT_ANDROID: u32 = 0x0000_0400;

/// Android NDK `AHardwareBuffer_Format` constants
pub const AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM: u32 = 0x01;
pub const AHARDWAREBUFFER_FORMAT_R8G8B8X8_UNORM: u32 = 0x02;
pub const AHARDWAREBUFFER_FORMAT_Y8CBCR_420_888: u32 = 0x23;

/// Android NDK `AHardwareBuffer_UsageFlags` constants
pub const AHARDWAREBUFFER_USAGE_CPU_READ_OFTEN: u64 = 0x0000_0003;
pub const AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE: u64 = 0x0000_0100;
pub const AHARDWAREBUFFER_USAGE_GPU_FRAMEBUFFER: u64 = 0x0000_0200;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AHardwareBufferDesc {
    pub width: u32,
    pub height: u32,
    pub layers: u32,
    pub format: u32,
    pub usage: u64,
    pub stride: u32,
    pub rfu0: u32,
    pub rfu1: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VkAndroidHardwareBufferPropertiesANDROID {
    pub s_type: i32,
    pub p_next: *mut c_void,
    pub allocation_size: u64,
    pub memory_type_bits: u32,
}

impl Default for VkAndroidHardwareBufferPropertiesANDROID {
    fn default() -> Self {
        Self {
            s_type: 1_000_129_001, // VK_STRUCTURE_TYPE_ANDROID_HARDWARE_BUFFER_PROPERTIES_ANDROID
            p_next: std::ptr::null_mut(),
            allocation_size: 0,
            memory_type_bits: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VkAndroidHardwareBufferFormatPropertiesANDROID {
    pub s_type: i32,
    pub p_next: *mut c_void,
    pub format: u32,
    pub external_format: u64,
    pub format_features: u32,
    pub sampler_ycbcr_conversion_components: [u32; 4],
    pub suggested_ycbcr_model: u32,
    pub suggested_ycbcr_range: u32,
    pub suggested_x_chroma_offset: u32,
    pub suggested_y_chroma_offset: u32,
}

impl Default for VkAndroidHardwareBufferFormatPropertiesANDROID {
    fn default() -> Self {
        Self {
            s_type: 1_000_129_002, // VK_STRUCTURE_TYPE_ANDROID_HARDWARE_BUFFER_FORMAT_PROPERTIES_ANDROID
            p_next: std::ptr::null_mut(),
            format: 0,
            external_format: 0,
            format_features: 0,
            sampler_ycbcr_conversion_components: [0; 4],
            suggested_ycbcr_model: 2, // ITU_709
            suggested_ycbcr_range: 1, // ITU_NARROW
            suggested_x_chroma_offset: 0,
            suggested_y_chroma_offset: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VkImportAndroidHardwareBufferInfoANDROID {
    pub s_type: i32,
    pub p_next: *const c_void,
    pub buffer: *mut c_void,
}

impl VkImportAndroidHardwareBufferInfoANDROID {
    pub fn from_raw(buffer: *mut c_void) -> Self {
        Self {
            s_type: 1_000_129_003, // VK_STRUCTURE_TYPE_IMPORT_ANDROID_HARDWARE_BUFFER_INFO_ANDROID
            p_next: std::ptr::null(),
            buffer,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AhbVkImportPlan {
    pub buffer_id: u64,
    pub desc: AHardwareBufferDesc,
    pub allocation_size: u64,
    pub memory_type_bits: u32,
    pub external_format: u64,
    pub handle_type_bit: u32,
    pub required_vk_extension: &'static str,
}

/// Abstract HAL backend for Android `AHardwareBuffer` + Vulkan external memory queries.
/// Allows testing zero-copy Camera2 / ImageReader / Vulkan import flows without an Android phone.
pub trait AndroidHalBackend: Send + Sync {
    fn register_or_acquire(&self, buffer_id: u64, desc: AHardwareBufferDesc) -> CoreResult<()>;
    fn describe_ahb(&self, buffer_id: u64) -> CoreResult<AHardwareBufferDesc>;
    fn query_vk_properties(&self, buffer_id: u64) -> CoreResult<(u64, u32, u64)>;
    fn release_ahb(&self, buffer_id: u64) -> CoreResult<()>;
}

#[derive(Debug, Clone)]
struct MockAhbEntry {
    desc: AHardwareBufferDesc,
    refcount: u32,
    external_format: u64,
}

/// Deterministic mock HAL backend tracking `AHardwareBuffer` refcounts and Vulkan external format IDs.
#[derive(Debug, Default)]
pub struct MockAndroidHalBackend {
    entries: Mutex<HashMap<u64, MockAhbEntry>>,
}

impl MockAndroidHalBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn active_refcount(&self, buffer_id: u64) -> u32 {
        self.entries
            .lock()
            .unwrap()
            .get(&buffer_id)
            .map(|e| e.refcount)
            .unwrap_or(0)
    }
}

impl AndroidHalBackend for MockAndroidHalBackend {
    fn register_or_acquire(&self, buffer_id: u64, desc: AHardwareBufferDesc) -> CoreResult<()> {
        if buffer_id == 0 || desc.width == 0 || desc.height == 0 {
            return Err(CoreError::PlatformIo(
                "invalid AHardwareBuffer id or dimensions".into(),
            ));
        }
        let mut map = self.entries.lock().unwrap();
        let entry = map.entry(buffer_id).or_insert_with(|| MockAhbEntry {
            desc,
            refcount: 0,
            external_format: 0x0A00_0000 | (desc.format as u64),
        });
        entry.refcount += 1;
        Ok(())
    }

    fn describe_ahb(&self, buffer_id: u64) -> CoreResult<AHardwareBufferDesc> {
        self.entries
            .lock()
            .unwrap()
            .get(&buffer_id)
            .map(|e| e.desc)
            .ok_or_else(|| CoreError::PlatformIo(format!("unknown AHB id {}", buffer_id)))
    }

    fn query_vk_properties(&self, buffer_id: u64) -> CoreResult<(u64, u32, u64)> {
        let map = self.entries.lock().unwrap();
        let entry = map
            .get(&buffer_id)
            .ok_or_else(|| CoreError::PlatformIo(format!("unknown AHB id {}", buffer_id)))?;
        let stride = entry.desc.stride.max(entry.desc.width) as u64;
        let alloc_size = match entry.desc.format {
            AHARDWAREBUFFER_FORMAT_Y8CBCR_420_888 => (stride * entry.desc.height as u64 * 3) / 2,
            _ => stride * entry.desc.height as u64 * 4,
        };
        Ok((alloc_size.max(4096), 0x0000_0007, entry.external_format))
    }

    fn release_ahb(&self, buffer_id: u64) -> CoreResult<()> {
        let mut map = self.entries.lock().unwrap();
        if let Some(entry) = map.get_mut(&buffer_id) {
            entry.refcount = entry.refcount.saturating_sub(1);
            if entry.refcount == 0 {
                map.remove(&buffer_id);
            }
            Ok(())
        } else {
            Err(CoreError::PlatformIo(format!(
                "double/unknown release on AHB id {}",
                buffer_id
            )))
        }
    }
}

/// Real Android NDK `libandroid.so` `AHardwareBuffer` dynamic loader.
#[cfg(target_os = "android")]
pub struct NdkAndroidHalBackend {
    _lib: *mut c_void,
    acquire_fn: unsafe extern "C" fn(*mut c_void),
    release_fn: unsafe extern "C" fn(*mut c_void),
    describe_fn: unsafe extern "C" fn(*const c_void, *mut AHardwareBufferDesc),
}

#[cfg(target_os = "android")]
unsafe impl Send for NdkAndroidHalBackend {}
#[cfg(target_os = "android")]
unsafe impl Sync for NdkAndroidHalBackend {}

#[cfg(target_os = "android")]
impl NdkAndroidHalBackend {
    pub fn load() -> CoreResult<Self> {
        extern "C" {
            fn dlopen(filename: *const i8, flag: i32) -> *mut c_void;
            fn dlsym(handle: *mut c_void, symbol: *const i8) -> *mut c_void;
        }
        const RTLD_NOW: i32 = 2;
        unsafe {
            let lib = dlopen(b"libandroid.so\0".as_ptr().cast(), RTLD_NOW);
            if lib.is_null() {
                return Err(CoreError::PlatformIo("dlopen libandroid.so failed".into()));
            }
            let acq = dlsym(lib, b"AHardwareBuffer_acquire\0".as_ptr().cast());
            let rel = dlsym(lib, b"AHardwareBuffer_release\0".as_ptr().cast());
            let desc = dlsym(lib, b"AHardwareBuffer_describe\0".as_ptr().cast());
            if acq.is_null() || rel.is_null() || desc.is_null() {
                return Err(CoreError::PlatformIo(
                    "missing AHardwareBuffer_* symbols in libandroid.so".into(),
                ));
            }
            Ok(Self {
                _lib: lib,
                acquire_fn: std::mem::transmute(acq),
                release_fn: std::mem::transmute(rel),
                describe_fn: std::mem::transmute(desc),
            })
        }
    }
}

#[cfg(target_os = "android")]
impl AndroidHalBackend for NdkAndroidHalBackend {
    fn register_or_acquire(&self, buffer_id: u64, _desc: AHardwareBufferDesc) -> CoreResult<()> {
        if buffer_id == 0 {
            return Err(CoreError::PlatformIo("null AHardwareBuffer pointer".into()));
        }
        unsafe {
            (self.acquire_fn)(buffer_id as usize as *mut c_void);
        }
        Ok(())
    }

    fn describe_ahb(&self, buffer_id: u64) -> CoreResult<AHardwareBufferDesc> {
        if buffer_id == 0 {
            return Err(CoreError::PlatformIo("null AHardwareBuffer pointer".into()));
        }
        let mut out = AHardwareBufferDesc {
            width: 0,
            height: 0,
            layers: 0,
            format: 0,
            usage: 0,
            stride: 0,
            rfu0: 0,
            rfu1: 0,
        };
        unsafe {
            (self.describe_fn)(buffer_id as usize as *const c_void, &mut out);
        }
        Ok(out)
    }

    fn query_vk_properties(&self, buffer_id: u64) -> CoreResult<(u64, u32, u64)> {
        let desc = self.describe_ahb(buffer_id)?;
        let stride = desc.stride.max(desc.width) as u64;
        let alloc = stride * desc.height as u64 * 2;
        Ok((alloc.max(4096), 0x1, 0x0A00_0000 | (desc.format as u64)))
    }

    fn release_ahb(&self, buffer_id: u64) -> CoreResult<()> {
        if buffer_id == 0 {
            return Err(CoreError::PlatformIo("null AHardwareBuffer pointer".into()));
        }
        unsafe {
            (self.release_fn)(buffer_id as usize as *mut c_void);
        }
        Ok(())
    }
}

/// Zero-copy importer coordinating Camera2 `ImageReader` `AHardwareBuffer` descriptors,
/// Vulkan external memory metadata (`VK_ANDROID_external_memory_android_hardware_buffer`),
/// and `GpuCompositor` layer updates.
pub struct AndroidAhbImporter {
    backend: Arc<dyn AndroidHalBackend>,
}

impl AndroidAhbImporter {
    pub fn new(backend: Arc<dyn AndroidHalBackend>) -> Self {
        Self { backend }
    }

    pub fn prepare_vk_import(
        &self,
        buffer_id: u64,
        desc: AHardwareBufferDesc,
    ) -> CoreResult<AhbVkImportPlan> {
        self.backend.register_or_acquire(buffer_id, desc)?;
        let described = self.backend.describe_ahb(buffer_id)?;
        let (allocation_size, memory_type_bits, external_format) =
            self.backend.query_vk_properties(buffer_id)?;

        Ok(AhbVkImportPlan {
            buffer_id,
            desc: described,
            allocation_size,
            memory_type_bits,
            external_format,
            handle_type_bit: VK_EXTERNAL_MEMORY_HANDLE_TYPE_ANDROID_HARDWARE_BUFFER_BIT_ANDROID,
            required_vk_extension: VK_ANDROID_EXTERNAL_MEMORY_ANDROID_HARDWARE_BUFFER_EXTENSION_NAME,
        })
    }

    /// Creates a `GpuLayer` corresponding to an imported Android `AHardwareBuffer`.
    /// When `vk_backing_texture` is supplied (from `wgpu_hal::vulkan::Device::texture_from_raw`),
    /// binds it directly with zero CPU copy; otherwise validates the import plan and allocates
    /// a compositor placeholder texture matching the AHB geometry so headless GPU tests can run.
    #[allow(clippy::too_many_arguments)]
    pub fn import_to_gpu_layer(
        &self,
        compositor: &GpuCompositor,
        buffer_id: u64,
        desc: AHardwareBufferDesc,
        layer_id: usize,
        is_base_layer: bool,
        transform: LayerTransform,
        vk_backing_texture: Option<wgpu::Texture>,
    ) -> CoreResult<(AhbVkImportPlan, GpuLayer)> {
        let plan = self.prepare_vk_import(buffer_id, desc)?;
        let w = plan.desc.width.max(2);
        let h = plan.desc.height.max(2);

        let layer = if let Some(tex) = vk_backing_texture {
            compositor.create_layer_from_textures(
                layer_id,
                is_base_layer,
                LayerInputKind::Rgba,
                ColorSpace::Bt709Limited,
                w,
                h,
                transform,
                tex,
                None,
            )
        } else {
            let rgba = vec![160u8; (w as usize) * (h as usize) * 4];
            compositor.create_rgba_layer(layer_id, is_base_layer, false, w, h, &rgba, transform)?
        };

        Ok((plan, layer))
    }

    pub fn release_buffer(&self, buffer_id: u64) -> CoreResult<()> {
        self.backend.release_ahb(buffer_id)
    }
}
