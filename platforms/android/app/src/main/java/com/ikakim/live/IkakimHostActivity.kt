package com.ikakim.live

import android.Manifest
import android.annotation.SuppressLint
import android.app.Activity
import android.content.pm.PackageManager
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import java.util.concurrent.atomic.AtomicLong

/**
 * Android Local WebView Host Activity connecting `core-net-ui` (`http://127.0.0.1:<port>/`)
 * with the Rust `IkakimEngineHandle` via `IkakimBridge` and `Camera2AhbCapture`.
 */
class IkakimHostActivity : Activity() {
    private var engineHandle: Long = 0L
    private var cameraCapture: Camera2AhbCapture? = null
    private lateinit var webView: WebView
    private val mainHandler = Handler(Looper.getMainLooper())
    private val frameCounter = AtomicLong(1L)

    private val tickRunnable = object : Runnable {
        override fun run() {
            val h = engineHandle
            if (h != 0L) {
                IkakimBridge.nativeStepTick(h, 0L, 33L)
            }
            mainHandler.postDelayed(this, 33L)
        }
    }

    @SuppressLint("SetJavaScriptEnabled")
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        webView = WebView(this).apply {
            settings.javaScriptEnabled = true
            settings.domStorageEnabled = true
            settings.mediaPlaybackRequiresUserGesture = false
            settings.mixedContentMode = WebSettings.MIXED_CONTENT_NEVER_ALLOW
            webViewClient = WebViewClient()
        }
        setContentView(webView)

        engineHandle = IkakimBridge.nativeCreate(1920, 1080, 1280, 720, 30, 4_000_000, 0)
        if (engineHandle != 0L) {
            val port = IkakimBridge.nativeUiPort(engineHandle)
            if (port > 0) {
                webView.loadUrl("http://127.0.0.1:$port/")
            }
            mainHandler.post(tickRunnable)
        }

        ensurePermissionsAndStartCamera()
    }

    private fun ensurePermissionsAndStartCamera() {
        val needed = arrayOf(Manifest.permission.CAMERA, Manifest.permission.RECORD_AUDIO)
        val missing = needed.filter {
            checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED
        }
        if (missing.isEmpty()) {
            startCameraBridge()
        } else {
            requestPermissions(missing.toTypedArray(), 1001)
        }
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        if (requestCode == 1001 && grantResults.all { it == PackageManager.PERMISSION_GRANTED }) {
            startCameraBridge()
        }
    }

    private fun startCameraBridge() {
        if (cameraCapture != null) return
        val capture = Camera2AhbCapture(this) { _, w, h, fmt, usage ->
            val handle = engineHandle
            if (handle != 0L) {
                val seqId = frameCounter.getAndIncrement()
                IkakimBridge.nativeSubmitAhbMeta(handle, 0, true, seqId, w, h, fmt, usage)
            }
        }
        capture.start()
        cameraCapture = capture
    }

    override fun onDestroy() {
        mainHandler.removeCallbacks(tickRunnable)
        cameraCapture?.stop()
        cameraCapture = null
        if (engineHandle != 0L) {
            IkakimBridge.nativeDestroy(engineHandle)
            engineHandle = 0L
        }
        super.onDestroy()
    }
}
