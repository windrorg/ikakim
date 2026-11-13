package com.ikakim.live

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.ImageFormat
import android.hardware.HardwareBuffer
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.media.ImageReader
import android.os.Build
import android.os.Handler
import android.os.HandlerThread

/**
 * Android Camera2 + `ImageReader` (`HardwareBuffer`) capture bridge.
 * Forwards acquired `HardwareBuffer` descriptors to `IkakimBridge` without CPU pixel copies,
 * targeting `VK_ANDROID_external_memory_android_hardware_buffer`.
 */
class Camera2AhbCapture(
    private val context: Context,
    private val width: Int = 1920,
    private val height: Int = 1080,
    private val onHardwareBuffer: (HardwareBuffer, Int, Int, Int, Long) -> Unit
) {
    private var handlerThread: HandlerThread? = null
    private var handler: Handler? = null
    private var imageReader: ImageReader? = null
    private var cameraDevice: CameraDevice? = null
    private var captureSession: CameraCaptureSession? = null

    @SuppressLint("MissingPermission")
    fun start() {
        val thread = HandlerThread("ikakim-camera2").apply { start() }
        val bgHandler = Handler(thread.looper)
        handlerThread = thread
        handler = bgHandler

        val reader = ImageReader.newInstance(
            width,
            height,
            ImageFormat.YUV_420_888,
            3,
            HardwareBuffer.USAGE_GPU_SAMPLED_IMAGE or HardwareBuffer.USAGE_GPU_COLOR_OUTPUT
        )
        reader.setOnImageAvailableListener({ r ->
            val image = r.acquireLatestImage() ?: return@setOnImageAvailableListener
            try {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
                    val hwBuf = image.hardwareBuffer
                    if (hwBuf != null) {
                        try {
                            onHardwareBuffer(
                                hwBuf,
                                hwBuf.width,
                                hwBuf.height,
                                hwBuf.format,
                                hwBuf.usage
                            )
                        } finally {
                            hwBuf.close()
                        }
                    }
                }
            } finally {
                image.close()
            }
        }, bgHandler)
        imageReader = reader

        val camMgr = context.getSystemService(Context.CAMERA_SERVICE) as CameraManager
        val cameraId = camMgr.cameraIdList.firstOrNull { id ->
            val chars = camMgr.getCameraCharacteristics(id)
            chars.get(CameraCharacteristics.LENS_FACING) == CameraCharacteristics.LENS_FACING_BACK
        } ?: camMgr.cameraIdList.firstOrNull() ?: return

        camMgr.openCamera(cameraId, object : CameraDevice.StateCallback() {
            override fun onOpened(camera: CameraDevice) {
                cameraDevice = camera
                val targetSurface = reader.surface
                val reqBuilder = camera.createCaptureRequest(CameraDevice.TEMPLATE_RECORD).apply {
                    addTarget(targetSurface)
                    set(CaptureRequest.CONTROL_MODE, CaptureRequest.CONTROL_MODE_AUTO)
                }
                @Suppress("DEPRECATION")
                camera.createCaptureSession(
                    listOf(targetSurface),
                    object : CameraCaptureSession.StateCallback() {
                        override fun onConfigured(session: CameraCaptureSession) {
                            captureSession = session
                            session.setRepeatingRequest(reqBuilder.build(), null, bgHandler)
                        }

                        override fun onConfigureFailed(session: CameraCaptureSession) {}
                    },
                    bgHandler
                )
            }

            override fun onDisconnected(camera: CameraDevice) {
                camera.close()
                cameraDevice = null
            }

            override fun onError(camera: CameraDevice, error: Int) {
                camera.close()
                cameraDevice = null
            }
        }, bgHandler)
    }

    fun stop() {
        captureSession?.close()
        captureSession = null
        cameraDevice?.close()
        cameraDevice = null
        imageReader?.close()
        imageReader = null
        handlerThread?.quitSafely()
        handlerThread = null
        handler = null
    }
}
