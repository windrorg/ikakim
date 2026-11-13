package com.ikakim.live

/**
 * JNI bridge matching `crates/ffi-capi/src/android_jni.rs` (`Java_com_ikakim_live_IkakimBridge_*`).
 * Expects `libffi_capi.so` built with `cargo ndk -t arm64-v8a build --release -p ffi-capi`.
 */
object IkakimBridge {
    init {
        System.loadLibrary("ffi_capi")
    }

    external fun nativeCreate(
        mainWidth: Int,
        mainHeight: Int,
        previewWidth: Int,
        previewHeight: Int,
        targetFps: Int,
        initialBitrateBps: Int,
        httpPort: Int
    ): Long

    external fun nativeDestroy(handle: Long)

    external fun nativeUiPort(handle: Long): Int

    external fun nativeSubmitAhbMeta(
        handle: Long,
        layerId: Int,
        isBaseLayer: Boolean,
        bufferId: Long,
        width: Int,
        height: Int,
        androidFormat: Int,
        usageFlags: Long
    ): Int

    external fun nativeSubmitRgba(
        handle: Long,
        layerId: Int,
        isBaseLayer: Boolean,
        topBottomAlpha: Boolean,
        width: Int,
        height: Int,
        rgbaPtr: Long,
        rgbaLen: Int
    ): Int

    external fun nativePushAudioF32(
        handle: Long,
        channelIndex: Int,
        pcmPtr: Long,
        sampleCount: Int
    ): Int

    external fun nativeStepTick(
        handle: Long,
        queuedRtmpBytes: Long,
        elapsedMs: Long
    ): Int

    external fun nativeRequestKeyframe(handle: Long): Int
}
