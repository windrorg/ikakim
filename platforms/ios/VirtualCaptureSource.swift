import CoreVideo
import Foundation

/// Virtual camera + overlay + 48kHz stereo PCM generator for iOS/iPadOS Simulator and automated E2E.
/// Generates Metal-compatible `CVPixelBuffer` (BGRA) frames so the same zero-copy `apple-common`
/// / `VideoToolbox` pipeline exercised on real hardware runs end-to-end on Simulator.
@MainActor
public final class VirtualCaptureSource {
    public private(set) var isRunning = false
    private var timer: Timer?
    private var frameCounter: UInt64 = 0
    private var audioPhase: Double = 0.0
    private var pixelBufferPool: CVPixelBufferPool?
    private let width: Int
    private let height: Int
    private let fps: Double

    public var onVideoFrame: ((CVPixelBuffer) -> Void)?
    public var onOverlayRgba: ((Data, UInt32, UInt32) -> Void)?
    public var onAudioSamples: (([Float]) -> Void)?

    public init(width: Int = 1280, height: Int = 720, fps: Double = 30.0) {
        self.width = width
        self.height = height
        self.fps = max(1.0, fps)
    }

    public func start() {
        guard !isRunning else { return }
        isRunning = true
        frameCounter = 0
        setupPixelBufferPool()

        let interval = 1.0 / fps
        timer = Timer.scheduledTimer(withTimeInterval: interval, repeats: true) { [weak self] _ in
            Task { @MainActor in
                self?.emitTick()
            }
        }
    }

    public func stop() {
        timer?.invalidate()
        timer = nil
        isRunning = false
    }

    private func setupPixelBufferPool() {
        guard pixelBufferPool == nil else { return }
        let poolAttrs: [String: Any] = [
            kCVPixelBufferPoolMinimumBufferCountKey as String: 4
        ]
        let pbAttrs: [String: Any] = [
            kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA,
            kCVPixelBufferWidthKey as String: width,
            kCVPixelBufferHeightKey as String: height,
            kCVPixelBufferMetalCompatibilityKey as String: true,
            kCVPixelBufferIOSurfacePropertiesKey as String: [:] as [String: Any],
        ]
        var pool: CVPixelBufferPool?
        CVPixelBufferPoolCreate(kCFAllocatorDefault, poolAttrs as CFDictionary, pbAttrs as CFDictionary, &pool)
        pixelBufferPool = pool
    }

    private func emitTick() {
        guard isRunning else { return }
        frameCounter &+= 1

        if let pb = makeAnimatedVideoFrame(frame: frameCounter) {
            onVideoFrame?(pb)
        }

        // Submit an animated RGBA overlay (Layer 1) every 2nd tick so PiP / overlay compositing is visibly active.
        if frameCounter % 2 == 1 {
            let ow: UInt32 = 160
            let oh: UInt32 = 90
            onOverlayRgba?(makeAnimatedOverlayRgba(width: Int(ow), height: Int(oh), frame: frameCounter), ow, oh)
        }

        // 48kHz stereo PCM chunk matching the tick interval.
        let framesPerTick = Int(48_000.0 / fps)
        onAudioSamples?(makeStereoSinePcm(frames: framesPerTick))
    }

    private func makeAnimatedVideoFrame(frame: UInt64) -> CVPixelBuffer? {
        guard let pool = pixelBufferPool else { return nil }
        var pbOut: CVPixelBuffer?
        guard CVPixelBufferPoolCreatePixelBuffer(kCFAllocatorDefault, pool, &pbOut) == kCVReturnSuccess,
              let pb = pbOut else {
            return nil
        }

        CVPixelBufferLockBaseAddress(pb, [])
        defer { CVPixelBufferUnlockBaseAddress(pb, []) }

        guard let base = CVPixelBufferGetBaseAddress(pb) else { return nil }
        let bytesPerRow = CVPixelBufferGetBytesPerRow(pb)
        let ptr = base.assumingMemoryBound(to: UInt8.self)

        // Draw animated vertical SMPTE-inspired color bars + moving scan bar (BGRA).
        let barCount = 6
        let barWidth = max(1, width / barCount)
        let shift = Int((frame * 8) % UInt64(width))
        let colors: [(UInt8, UInt8, UInt8)] = [
            (235, 235, 235), // White
            (32, 220, 220),  // Yellow (BGRA)
            (220, 220, 32),  // Cyan
            (64, 210, 64),   // Green
            (210, 64, 210),  // Magenta
            (64, 64, 220),   // Red
        ]

        // Fill top row then memcpy to remaining rows for high speed on Simulator CPU.
        for x in 0..<width {
            let c = colors[(x / barWidth) % barCount]
            let off = x * 4
            ptr[off] = c.0
            ptr[off + 1] = c.1
            ptr[off + 2] = c.2
            ptr[off + 3] = 255
        }
        for y in 1..<height {
            memcpy(ptr.advanced(by: y * bytesPerRow), ptr, width * 4)
        }

        // Moving white/blue indicator bar so motion is clearly visible in MSE preview & encoded stream.
        let boxYStart = (height / 2) - 24
        let boxYEnd = min(height, boxYStart + 48)
        for y in max(0, boxYStart)..<boxYEnd {
            let row = ptr.advanced(by: y * bytesPerRow)
            for dx in 0..<96 {
                let x = (shift + dx) % width
                let off = x * 4
                row[off] = 255
                row[off + 1] = 130
                row[off + 2] = 24
                row[off + 3] = 255
            }
        }

        return pb
    }

    private func makeAnimatedOverlayRgba(width: Int, height: Int, frame: UInt64) -> Data {
        var data = Data(count: width * height * 4)
        let pulse = UInt8(140 + ((frame * 6) % 100))
        data.withUnsafeMutableBytes { raw in
            guard let p = raw.bindMemory(to: UInt8.self).baseAddress else { return }
            for y in 0..<height {
                for x in 0..<width {
                    let idx = (y * width + x) * 4
                    let border = (x < 3 || x >= width - 3 || y < 3 || y >= height - 3)
                    if border {
                        p[idx] = 0
                        p[idx + 1] = 112
                        p[idx + 2] = 243
                        p[idx + 3] = 255
                    } else {
                        p[idx] = 16
                        p[idx + 1] = 24
                        p[idx + 2] = pulse
                        p[idx + 3] = 210
                    }
                }
            }
        }
        return data
    }

    private func makeStereoSinePcm(frames: Int) -> [Float] {
        var out = [Float](repeating: 0.0, count: frames * 2)
        let sampleRate = 48_000.0
        let freqL = 440.0
        let freqR = 554.37
        let twoPi = 2.0 * Double.pi
        for i in 0..<frames {
            let t = audioPhase + Double(i) / sampleRate
            out[i * 2] = Float(0.18 * sin(twoPi * freqL * t))
            out[i * 2 + 1] = Float(0.18 * sin(twoPi * freqR * t))
        }
        audioPhase += Double(frames) / sampleRate
        if audioPhase > 10.0 {
            audioPhase -= 10.0
        }
        return out
    }
}
