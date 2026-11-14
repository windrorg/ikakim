//! Android JNI ブリッジ定義・ポータブル検証ヘルパー（M4: Issue #8）
//!
//! Android 実機/NDK ビルド時（`target_os = "android"`）は
//! `Java_com_ikakim_live_IkakimBridge_*` JNI エクスポートを公開し、
//! Kotlin 側 (`Camera2AhbCapture.kt`, `IkakimHostActivity.kt`) と `IkakimEngineHandle` を接続する。

use crate::{
    ikakim_engine_create, ikakim_engine_default_config,
    ikakim_engine_submit_ahardware_buffer_meta, IkakimEngineHandle,
};

#[cfg(target_os = "android")]
use crate::{
    ikakim_engine_destroy, ikakim_engine_push_audio_f32, ikakim_engine_request_keyframe,
    ikakim_engine_step_tick, ikakim_engine_submit_rgba_layer, ikakim_engine_ui_port,
};

/// Android `AHardwareBuffer` 外部メモリ記述子（C-API / JNI 共通メタ表現）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AhbDescriptor {
    pub buffer_id: u64,
    pub width: u32,
    pub height: u32,
    pub android_format: u32,
    pub usage_flags: u64,
}

impl AhbDescriptor {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.buffer_id == 0 {
            return Err("AHardwareBuffer buffer_id must be non-zero");
        }
        if self.width == 0 || self.height == 0 {
            return Err("AHardwareBuffer dimensions must be non-zero");
        }
        Ok(())
    }
}

/// JNI の `jlong` ネイティブポインタ値から生ポインタを安全に復元する
///
/// # Safety
/// `ptr_val` は `handle_to_jlong` が返した有効かつ未解放の `IkakimEngineHandle` ポインタか 0 である必要がある。
#[inline]
pub unsafe fn jlong_to_handle(ptr_val: i64) -> *mut IkakimEngineHandle {
    if ptr_val == 0 {
        std::ptr::null_mut()
    } else {
        ptr_val as u64 as usize as *mut IkakimEngineHandle
    }
}

#[inline]
pub fn handle_to_jlong(handle: *mut IkakimEngineHandle) -> i64 {
    handle as usize as i64
}

/// C-API の全プラットフォーム共通ラッパー呼び出し（JNI 未有効環境でも単体テスト可能）
///
/// # Safety
/// `handle` must be null or a valid pointer to `IkakimEngineHandle`.
pub unsafe fn portable_submit_ahb_meta(
    handle: *mut IkakimEngineHandle,
    layer_id: u32,
    is_base_layer: bool,
    desc: AhbDescriptor,
) -> i32 {
    if desc.validate().is_err() {
        return -1;
    }
    unsafe {
        ikakim_engine_submit_ahardware_buffer_meta(
            handle,
            layer_id,
            is_base_layer,
            desc.buffer_id,
            desc.width,
            desc.height,
            desc.android_format,
            desc.usage_flags,
        )
    }
}

/// Kotlin/JVM の `nativeCreate(mainWidth, mainHeight, previewWidth, previewHeight, fps, bitrateBps, httpPort)` 実装
pub fn portable_create(
    main_width: u32,
    main_height: u32,
    preview_width: u32,
    preview_height: u32,
    target_fps: u32,
    initial_bitrate_bps: u32,
    http_port: u16,
) -> i64 {
    let mut cfg = ikakim_engine_default_config();
    if main_width > 0 {
        cfg.main_width = main_width;
    }
    if main_height > 0 {
        cfg.main_height = main_height;
    }
    if preview_width > 0 {
        cfg.preview_width = preview_width;
    }
    if preview_height > 0 {
        cfg.preview_height = preview_height;
    }
    if target_fps > 0 {
        cfg.target_fps = target_fps;
    }
    if initial_bitrate_bps > 0 {
        cfg.initial_bitrate_bps = initial_bitrate_bps;
    }
    cfg.http_port = http_port;
    handle_to_jlong(unsafe { ikakim_engine_create(&cfg) })
}

#[cfg(target_os = "android")]
mod android_exports {
    use super::*;
    use std::ffi::c_void;

    pub type JniEnvPtr = *mut c_void;
    pub type JObject = *mut c_void;
    pub type JLong = i64;
    pub type JInt = i32;
    pub type JBoolean = u8;

    #[no_mangle]
    pub extern "system" fn Java_com_ikakim_live_IkakimBridge_nativeCreate(
        _env: JniEnvPtr,
        _cls: JObject,
        main_width: JInt,
        main_height: JInt,
        preview_width: JInt,
        preview_height: JInt,
        target_fps: JInt,
        initial_bitrate_bps: JInt,
        http_port: JInt,
    ) -> JLong {
        portable_create(
            main_width.max(0) as u32,
            main_height.max(0) as u32,
            preview_width.max(0) as u32,
            preview_height.max(0) as u32,
            target_fps.max(0) as u32,
            initial_bitrate_bps.max(0) as u32,
            http_port.clamp(0, u16::MAX as i32) as u16,
        )
    }

    #[no_mangle]
    pub extern "system" fn Java_com_ikakim_live_IkakimBridge_nativeDestroy(
        _env: JniEnvPtr,
        _cls: JObject,
        handle: JLong,
    ) {
        unsafe {
            ikakim_engine_destroy(jlong_to_handle(handle));
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_ikakim_live_IkakimBridge_nativeUiPort(
        _env: JniEnvPtr,
        _cls: JObject,
        handle: JLong,
    ) -> JInt {
        unsafe { ikakim_engine_ui_port(jlong_to_handle(handle)) as JInt }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_ikakim_live_IkakimBridge_nativeSubmitAhbMeta(
        _env: JniEnvPtr,
        _cls: JObject,
        handle: JLong,
        layer_id: JInt,
        is_base_layer: JBoolean,
        buffer_id: JLong,
        width: JInt,
        height: JInt,
        android_format: JInt,
        usage_flags: JLong,
    ) -> JInt {
        unsafe {
            portable_submit_ahb_meta(
                jlong_to_handle(handle),
                layer_id.max(0) as u32,
                is_base_layer != 0,
                AhbDescriptor {
                    buffer_id: buffer_id.max(0) as u64,
                    width: width.max(0) as u32,
                    height: height.max(0) as u32,
                    android_format: android_format.max(0) as u32,
                    usage_flags: usage_flags.max(0) as u64,
                },
            )
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_ikakim_live_IkakimBridge_nativeSubmitRgba(
        _env: JniEnvPtr,
        _cls: JObject,
        handle: JLong,
        layer_id: JInt,
        is_base_layer: JBoolean,
        top_bottom_alpha: JBoolean,
        width: JInt,
        height: JInt,
        rgba_ptr: JLong,
        rgba_len: JInt,
    ) -> JInt {
        unsafe {
            ikakim_engine_submit_rgba_layer(
                jlong_to_handle(handle),
                layer_id.max(0) as u32,
                is_base_layer != 0,
                top_bottom_alpha != 0,
                width.max(0) as u32,
                height.max(0) as u32,
                rgba_ptr as usize as *const u8,
                rgba_len.max(0) as usize,
            )
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_ikakim_live_IkakimBridge_nativePushAudioF32(
        _env: JniEnvPtr,
        _cls: JObject,
        handle: JLong,
        channel_index: JInt,
        pcm_ptr: JLong,
        sample_count: JInt,
    ) -> JInt {
        unsafe {
            ikakim_engine_push_audio_f32(
                jlong_to_handle(handle),
                channel_index.max(0) as u32,
                pcm_ptr as usize as *const f32,
                sample_count.max(0) as usize,
            )
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_ikakim_live_IkakimBridge_nativeStepTick(
        _env: JniEnvPtr,
        _cls: JObject,
        handle: JLong,
        queued_rtmp_bytes: JLong,
        elapsed_ms: JLong,
    ) -> JInt {
        unsafe {
            ikakim_engine_step_tick(
                jlong_to_handle(handle),
                queued_rtmp_bytes.max(0) as u64,
                elapsed_ms.max(0) as u64,
            )
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_ikakim_live_IkakimBridge_nativeRequestKeyframe(
        _env: JniEnvPtr,
        _cls: JObject,
        handle: JLong,
    ) -> JInt {
        unsafe { ikakim_engine_request_keyframe(jlong_to_handle(handle)) }
    }

    /// NDK `jni.h` `struct JNINativeInterface`:
    /// slot 230 = `void* (*GetDirectBufferAddress)(JNIEnv*, jobject)`
    /// slot 231 = `jlong (*GetDirectBufferCapacity)(JNIEnv*, jobject)`
    unsafe fn direct_buffer_slice<'a>(
        env: JniEnvPtr,
        buf: JObject,
    ) -> Option<(*const u8, usize)> {
        if env.is_null() || buf.is_null() {
            return None;
        }
        let vtable = *(env as *const *const *const c_void);
        if vtable.is_null() {
            return None;
        }
        type GetAddrFn = unsafe extern "C" fn(JniEnvPtr, JObject) -> *mut c_void;
        type GetCapFn = unsafe extern "C" fn(JniEnvPtr, JObject) -> i64;
        let get_addr: GetAddrFn = std::mem::transmute(*vtable.add(230));
        let get_cap: GetCapFn = std::mem::transmute(*vtable.add(231));
        let ptr = get_addr(env, buf) as *const u8;
        let cap = get_cap(env, buf);
        if ptr.is_null() || cap <= 0 {
            return None;
        }
        Some((ptr, cap as usize))
    }

    #[no_mangle]
    pub extern "system" fn Java_com_ikakim_live_IkakimBridge_nativeSubmitRgbaBuffer(
        env: JniEnvPtr,
        _cls: JObject,
        handle: JLong,
        layer_id: JInt,
        is_base_layer: JBoolean,
        top_bottom_alpha: JBoolean,
        width: JInt,
        height: JInt,
        direct_buf: JObject,
    ) -> JInt {
        unsafe {
            let Some((ptr, len)) = direct_buffer_slice(env, direct_buf) else {
                return -1;
            };
            ikakim_engine_submit_rgba_layer(
                jlong_to_handle(handle),
                layer_id.max(0) as u32,
                is_base_layer != 0,
                top_bottom_alpha != 0,
                width.max(0) as u32,
                height.max(0) as u32,
                ptr,
                len,
            )
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_ikakim_live_IkakimBridge_nativePushAudioF32Buffer(
        env: JniEnvPtr,
        _cls: JObject,
        handle: JLong,
        channel_index: JInt,
        direct_buf: JObject,
        sample_count: JInt,
    ) -> JInt {
        unsafe {
            let Some((ptr, byte_len)) = direct_buffer_slice(env, direct_buf) else {
                return -1;
            };
            let max_samples = byte_len / std::mem::size_of::<f32>();
            let count = (sample_count.max(0) as usize).min(max_samples);
            ikakim_engine_push_audio_f32(
                jlong_to_handle(handle),
                channel_index.max(0) as u32,
                ptr as *const f32,
                count,
            )
        }
    }
}
