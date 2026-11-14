package com.ikakim.live

import java.nio.ByteBuffer

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
        httpPort: Int,
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
        usageFlags: Long,
    ): Int

    external fun nativeSubmitRgba(
        handle: Long,
        layerId: Int,
        isBaseLayer: Boolean,
        topBottomAlpha: Boolean,
        width: Int,
        height: Int,
        rgbaPtr: Long,
        rgbaLen: Int,
    ): Int

    external fun nativePushAudioF32(
        handle: Long,
        channelIndex: Int,
        pcmPtr: Long,
        sampleCount: Int,
    ): Int

    external fun nativeStepTick(
        handle: Long,
        queuedRtmpBytes: Long,
        elapsedMs: Long,
    ): Int

    external fun nativeRequestKeyframe(handle: Long): Int

    external fun nativeSubmitRgbaBuffer(
        handle: Long,
        layerId: Int,
        isBaseLayer: Boolean,
        topBottomAlpha: Boolean,
        width: Int,
        height: Int,
        directBuffer: ByteBuffer,
    ): Int

    external fun nativePushAudioF32Buffer(
        handle: Long,
        channelIndex: Int,
        directBuffer: ByteBuffer,
        sampleCount: Int,
    ): Int
}
