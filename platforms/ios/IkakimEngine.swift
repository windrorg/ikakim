import AVFoundation
import CoreVideo
import Foundation
import IkakimFFI

/// Swift wrapper over the Rust `ffi-capi` (`IkakimEngineHandle`) backed by `apple-common`
/// (`CVMetalTextureCache` zero-copy, `VideoToolbox`, `AudioToolbox`) and `core-engine`.
@MainActor
public final class IkakimEngine: ObservableObject {
    @Published public private(set) var uiPort: UInt16 = 0
    @Published public private(set) var isStreaming: Bool = false
    @Published public private(set) var currentBitrateBps: UInt32 = 4_000_000
    @Published public private(set) var ticks: UInt64 = 0
    @Published public private(set) var lastErrorMessage: String = ""

    private var handle: OpaquePointer?
    private var tickTimer: Timer?
    private let targetFps: UInt32

    public init(
        mainWidth: UInt32 = 1920,
        mainHeight: UInt32 = 1080,
        previewWidth: UInt32 = 1280,
        previewHeight: UInt32 = 720,
        targetFps: UInt32 = 60,
        initialBitrateBps: UInt32 = 4_000_000,
        httpPort: UInt16 = 0
    ) {
        self.targetFps = max(1, targetFps)
        var cfg = ikakim_engine_default_config()
        cfg.main_width = mainWidth
        cfg.main_height = mainHeight
        cfg.preview_width = previewWidth
        cfg.preview_height = previewHeight
        cfg.target_fps = self.targetFps
        cfg.initial_bitrate_bps = initialBitrateBps
        cfg.http_port = httpPort

        if let raw = ikakim_engine_create(&cfg) {
            self.handle = raw
            self.uiPort = ikakim_engine_ui_port(raw)
        } else {
            self.lastErrorMessage = "Failed to create IkakimEngineHandle"
        }
    }

    deinit {
        tickTimer?.invalidate()
        if let raw = handle {
            ikakim_engine_destroy(raw)
        }
    }

    public var uiURL: URL? {
        guard uiPort > 0 else { return nil }
        return URL(string: "http://127.0.0.1:\(uiPort)/")
    }

    public func startTickLoop() {
        tickTimer?.invalidate()
        let interval = 1.0 / Double(targetFps)
        tickTimer = Timer.scheduledTimer(withTimeInterval: interval, repeats: true) { [weak self] _ in
            Task { @MainActor in
                self?.stepTick(queuedRtmpBytes: 0, elapsedMs: UInt64(interval * 1000.0))
            }
        }
    }

    public func stopTickLoop() {
        tickTimer?.invalidate()
        tickTimer = nil
    }

    /// Zero-copy submit of a CoreVideo `CVPixelBuffer` (NV12 or BGRA from AVFoundation / ReplayKit / external capture).
    @discardableResult
    public func submitCVPixelBuffer(
        _ pixelBuffer: CVPixelBuffer,
        layerId: UInt32 = 0,
        isBaseLayer: Bool = true
    ) -> Bool {
        guard let raw = handle else { return false }
        let ptr = Unmanaged.passUnretained(pixelBuffer).toOpaque()
        let rc = ikakim_engine_submit_cv_pixel_buffer(raw, layerId, isBaseLayer, ptr)
        if rc != 0 {
            refreshLastError()
            return false
        }
        return true
    }

    /// Submit an external RGBA buffer (e.g. PNG graphic or Top-Bottom Alpha overlay).
    @discardableResult
    public func submitRgbaLayer(
        _ data: Data,
        width: UInt32,
        height: UInt32,
        layerId: UInt32 = 1,
        isBaseLayer: Bool = false,
        topBottomAlpha: Bool = false
    ) -> Bool {
        guard let raw = handle else { return false }
        let rc = data.withUnsafeBytes { rawBuf -> Int32 in
            guard let base = rawBuf.bindMemory(to: UInt8.self).baseAddress else { return -1 }
            return ikakim_engine_submit_rgba_layer(
                raw,
                layerId,
                isBaseLayer,
                topBottomAlpha,
                width,
                height,
                base,
                data.count
            )
        }
        if rc != 0 {
            refreshLastError()
            return false
        }
        return true
    }

    /// Push stereo interleaved f32 PCM samples from AVAudioEngine / AVCaptureAudioDataOutput.
    public func pushAudioInterleavedF32(_ samples: [Float], channelIndex: UInt32 = 0) {
        guard let raw = handle, !samples.isEmpty else { return }
        samples.withUnsafeBufferPointer { buf in
            guard let ptr = buf.baseAddress else { return }
            let rc = ikakim_engine_push_audio_f32(raw, channelIndex, ptr, buf.count)
            if rc != 0 {
                refreshLastError()
            }
        }
    }

    public func stepTick(queuedRtmpBytes: UInt64, elapsedMs: UInt64) {
        guard let raw = handle else { return }
        let rc = ikakim_engine_step_tick(raw, queuedRtmpBytes, elapsedMs)
        if rc != 0 {
            refreshLastError()
        }
        var stats = IkakimEngineStats()
        if ikakim_engine_get_stats(raw, &stats) == 0 {
            ticks = stats.ticks
            uiPort = stats.ui_port
            currentBitrateBps = stats.current_bitrate_bps
            isStreaming = stats.streaming_active
        }
    }

    @discardableResult
    public func startRtmps(url: String) -> Bool {
        guard let raw = handle else { return false }
        let rc = url.withCString { cStr in
            ikakim_engine_start_rtmps(raw, cStr)
        }
        if rc != 0 {
            refreshLastError()
            return false
        }
        isStreaming = true
        return true
    }

    public func stopRtmps() {
        guard let raw = handle else { return }
        _ = ikakim_engine_stop_rtmps(raw)
        isStreaming = false
    }

    public func requestKeyframe() {
        guard let raw = handle else { return }
        _ = ikakim_engine_request_keyframe(raw)
    }

    private func refreshLastError() {
        guard let raw = handle else { return }
        var buf = [CChar](repeating: 0, count: 512)
        let len = ikakim_engine_last_error(raw, &buf, buf.count)
        if len > 0 {
            lastErrorMessage = String(cString: buf)
        }
    }
}
