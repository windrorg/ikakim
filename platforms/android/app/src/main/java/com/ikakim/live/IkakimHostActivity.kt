package com.ikakim.live

import android.annotation.SuppressLint
import android.app.Activity
import android.graphics.Color
import android.os.Bundle
import android.util.Base64
import android.util.Log
import android.webkit.ConsoleMessage
import android.webkit.WebChromeClient
import android.webkit.WebSettings
import android.webkit.WebView
import android.webkit.WebViewClient
import java.io.File
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.math.PI
import kotlin.math.sin

class IkakimHostActivity : Activity() {
    private var engineHandle: Long = 0L
    private var webView: WebView? = null
    private val running = AtomicBoolean(false)
    private var tickerThread: Thread? = null
    private var didRunE2E = false

    @SuppressLint("SetJavaScriptEnabled")
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val virtualIo = intent.getBooleanExtra("virtual_io", false)
        val requestedPort = intent.getIntExtra("http_port", 0)
        val e2eJsName = intent.getStringExtra("e2e_js_name")
        val e2eResultName = intent.getStringExtra("e2e_result_name")

        engineHandle = IkakimBridge.nativeCreate(
            1280,
            720,
            640,
            360,
            30,
            4_000_000,
            requestedPort,
        )
        val uiPort = if (engineHandle != 0L) IkakimBridge.nativeUiPort(engineHandle) else 0
        Log.i("ikakim", "Engine initialized handle=$engineHandle uiPort=$uiPort virtualIo=$virtualIo")

        WebView.setWebContentsDebuggingEnabled(true)

        val wv = WebView(this).apply {
            setBackgroundColor(Color.parseColor("#050507"))
            settings.javaScriptEnabled = true
            settings.domStorageEnabled = true
            settings.mediaPlaybackRequiresUserGesture = false
            settings.cacheMode = WebSettings.LOAD_NO_CACHE
            webChromeClient = object : WebChromeClient() {
                override fun onConsoleMessage(cm: ConsoleMessage): Boolean {
                    if (cm.messageLevel() == ConsoleMessage.MessageLevel.ERROR) {
                        Log.e("ikakim", "WebView console.error: ${cm.message()} (${cm.sourceId()}:${cm.lineNumber()})")
                    }
                    return super.onConsoleMessage(cm)
                }
            }
            webViewClient = object : WebViewClient() {
                override fun onPageFinished(view: WebView, url: String) {
                    super.onPageFinished(view, url)
                    if (!didRunE2E && !e2eJsName.isNullOrEmpty()) {
                        didRunE2E = true
                        view.postDelayed({ runE2ESuite(view, e2eJsName, e2eResultName) }, 1200L)
                    }
                }
            }
        }
        webView = wv
        setContentView(wv)

        if (uiPort > 0) {
            wv.loadUrl("http://127.0.0.1:$uiPort/")
        }

        running.set(true)
        tickerThread = Thread({
            runTickerLoop(virtualIo)
        }, "ikakim-tick").apply { start() }
    }

    private fun runTickerLoop(virtualIo: Boolean) {
        val startNs = System.nanoTime()
        val vw = 1280
        val vh = 720
        val ow = 160
        val oh = 90
        val samplesPerTick = 1600 * 2 // 48kHz stereo @ 30fps

        var videoBuf: ByteBuffer? = null
        var overlayBuf: ByteBuffer? = null
        var audioBuf: ByteBuffer? = null
        if (virtualIo) {
            videoBuf = ByteBuffer.allocateDirect(vw * vh * 4).order(ByteOrder.nativeOrder())
            overlayBuf = ByteBuffer.allocateDirect(ow * oh * 4).order(ByteOrder.nativeOrder())
            audioBuf = ByteBuffer.allocateDirect(samplesPerTick * 4).order(ByteOrder.nativeOrder())
        }

        var frame = 0L
        var audioPhase = 0.0
        while (running.get() && engineHandle != 0L) {
            frame += 1L
            if (virtualIo && videoBuf != null && overlayBuf != null && audioBuf != null) {
                fillVirtualRgbaBars(videoBuf, vw, vh, frame)
                IkakimBridge.nativeSubmitRgbaBuffer(engineHandle, 0, true, false, vw, vh, videoBuf)

                if (frame % 2L == 1L) {
                    fillVirtualOverlay(overlayBuf, ow, oh, frame)
                    IkakimBridge.nativeSubmitRgbaBuffer(engineHandle, 1, false, false, ow, oh, overlayBuf)
                }

                audioPhase = fillStereoSine(audioBuf, samplesPerTick / 2, audioPhase)
                IkakimBridge.nativePushAudioF32Buffer(engineHandle, 0, audioBuf, samplesPerTick)
            }

            val elapsedMs = (System.nanoTime() - startNs) / 1_000_000L
            IkakimBridge.nativeStepTick(engineHandle, 0L, elapsedMs)
            try {
                Thread.sleep(33)
            } catch (_: InterruptedException) {
                break
            }
        }
    }

    private fun fillVirtualRgbaBars(buf: ByteBuffer, w: Int, h: Int, frame: Long) {
        buf.clear()
        val barColors = intArrayOf(
            0xFFEBEBEB.toInt(),
            0xFFDCDC20.toInt(),
            0xFF20DCDC.toInt(),
            0xFF40D240.toInt(),
            0xFFD240D2.toInt(),
            0xFF4040DC.toInt(),
        )
        val rowBytes = ByteArray(w * 4)
        val barW = (w / barColors.size).coerceAtLeast(1)
        for (x in 0 until w) {
            val c = barColors[(x / barW) % barColors.size]
            val off = x * 4
            rowBytes[off] = ((c ushr 16) and 0xFF).toByte()
            rowBytes[off + 1] = ((c ushr 8) and 0xFF).toByte()
            rowBytes[off + 2] = (c and 0xFF).toByte()
            rowBytes[off + 3] = 0xFF.toByte()
        }
        for (y in 0 until h) {
            buf.put(rowBytes)
        }
        // Moving blue scan marker so motion is visible in MSE preview
        val shift = ((frame * 12L) % w.toLong()).toInt()
        val boxYStart = (h / 2) - 20
        for (y in boxYStart until (boxYStart + 40).coerceAtMost(h)) {
            for (dx in 0 until 80) {
                val x = (shift + dx) % w
                val pos = (y * w + x) * 4
                buf.put(pos, 0x18.toByte())
                buf.put(pos + 1, 0x82.toByte())
                buf.put(pos + 2, 0xFF.toByte())
                buf.put(pos + 3, 0xFF.toByte())
            }
        }
        buf.rewind()
    }

    private fun fillVirtualOverlay(buf: ByteBuffer, w: Int, h: Int, frame: Long) {
        buf.clear()
        val pulse = (140 + ((frame * 6L) % 100L)).toInt()
        for (y in 0 until h) {
            for (x in 0 until w) {
                val border = (x < 3 || x >= w - 3 || y < 3 || y >= h - 3)
                if (border) {
                    buf.put(0x00.toByte())
                    buf.put(0x70.toByte())
                    buf.put(0xF3.toByte())
                    buf.put(0xFF.toByte())
                } else {
                    buf.put(0x10.toByte())
                    buf.put(0x18.toByte())
                    buf.put(pulse.toByte())
                    buf.put(0xD2.toByte())
                }
            }
        }
        buf.rewind()
    }

    private fun fillStereoSine(buf: ByteBuffer, frames: Int, startPhase: Double): Double {
        buf.clear()
        val fb = buf.asFloatBuffer()
        val sr = 48_000.0
        val twoPi = 2.0 * PI
        for (i in 0 until frames) {
            val t = startPhase + i.toDouble() / sr
            fb.put((0.18 * sin(twoPi * 440.0 * t)).toFloat())
            fb.put((0.18 * sin(twoPi * 554.37 * t)).toFloat())
        }
        var nextPhase = startPhase + frames.toDouble() / sr
        if (nextPhase > 10.0) nextPhase -= 10.0
        buf.rewind()
        return nextPhase
    }

    private fun runE2ESuite(view: WebView, jsName: String, resultName: String?) {
        val extDir = getExternalFilesDir(null) ?: filesDir
        val jsFile = File(extDir, jsName)
        if (!jsFile.exists()) {
            Log.e("ikakim", "E2E JS file not found: ${jsFile.absolutePath}")
            return
        }
        val b64 = Base64.encodeToString(jsFile.readBytes(), Base64.NO_WRAP)
        val runner = """
        window.__ikakimE2E = { done: false, result: null, error: null };
        (() => {
          try {
            const bin = atob('$b64');
            const bytes = Uint8Array.from(bin, (c) => c.charCodeAt(0));
            const code = new TextDecoder('utf-8').decode(bytes);
            Promise.resolve((0, eval)(code))
              .then((res) => {
                window.__ikakimE2E = { done: true, result: res, error: null };
              })
              .catch((err) => {
                const msg = err && err.message ? (err.message + ' | ' + (err.stack || '')) : String(err);
                window.__ikakimE2E = { done: true, result: null, error: msg };
              });
          } catch (err) {
            const msg = err && err.message ? (err.message + ' | ' + (err.stack || '')) : String(err);
            window.__ikakimE2E = { done: true, result: null, error: 'Eval error: ' + msg };
          }
        })();
        """.trimIndent()
        view.evaluateJavascript(runner) {
            pollE2EResult(view, extDir, resultName ?: "e2e_result.json", 40)
        }
    }

    private fun pollE2EResult(view: WebView, outDir: File, resultName: String, attemptsLeft: Int) {
        if (attemptsLeft <= 0) {
            val errJson = """{"ok":false,"error":"Android WebView E2E timed out"}"""
            File(outDir, resultName).writeText(errJson)
            Log.e("ikakim", "WebView E2E result: $errJson")
            return
        }
        view.evaluateJavascript("JSON.stringify(window.__ikakimE2E)") { raw ->
            if (raw != null && raw != "null") {
                val unescaped = org.json.JSONTokener(raw).nextValue() as? String
                if (unescaped != null) {
                    val obj = org.json.JSONObject(unescaped)
                    if (obj.optBoolean("done", false)) {
                        val outText = if (!obj.isNull("error")) {
                            org.json.JSONObject().put("ok", false).put("error", obj.optString("error")).toString(2)
                        } else {
                            obj.optJSONObject("result")?.toString(2) ?: unescaped
                        }
                        File(outDir, resultName).writeText(outText)
                        Log.i("ikakim", "WebView E2E result: $outText")
                        return@evaluateJavascript
                    }
                }
            }
            view.postDelayed({ pollE2EResult(view, outDir, resultName, attemptsLeft - 1) }, 250L)
        }
    }

    override fun onDestroy() {
        running.set(false)
        tickerThread?.interrupt()
        tickerThread = null
        webView?.destroy()
        webView = null
        if (engineHandle != 0L) {
            IkakimBridge.nativeDestroy(engineHandle)
            engineHandle = 0L
        }
        super.onDestroy()
    }
}
