import AVFoundation
import CoreMedia
import CoreVideo
import Foundation

/// iOS `AVFoundation` camera + microphone + external video/audio input bridge.
/// Configures camera capture as NV12 (`420v`) matching `apple-common`'s `CVMetalTextureCache` importer.
public final class AVCaptureCameraBridge: NSObject,
    AVCaptureVideoDataOutputSampleBufferDelegate,
    AVCaptureAudioDataOutputSampleBufferDelegate
{
    public typealias PixelBufferHandler = (CVPixelBuffer, CMTime) -> Void
    public typealias AudioInterleavedHandler = ([Float]) -> Void

    private let session = AVCaptureSession()
    private let videoQueue = DispatchQueue(label: "com.ikakim.ios.camera.video")
    private let audioQueue = DispatchQueue(label: "com.ikakim.ios.camera.audio")
    private var isConfigured = false

    public var onVideoFrame: PixelBufferHandler?
    public var onAudioSamples: AudioInterleavedHandler?

    public func startCapture(position: AVCaptureDevice.Position = .back) throws {
        if !isConfigured {
            try configureSession(position: position)
            isConfigured = true
        }
        videoQueue.async { [session] in
            if !session.isRunning {
                session.startRunning()
            }
        }
    }

    public func stopCapture() {
        videoQueue.async { [session] in
            if session.isRunning {
                session.stopRunning()
            }
        }
    }

    /// Injects an external `CVPixelBuffer` (e.g. USB/NDI/ReplayKit Extension or Screen Capture)
    /// into the exact same zero-copy handler used for the built-in camera.
    public func ingestExternalPixelBuffer(_ pixelBuffer: CVPixelBuffer, pts: CMTime) {
        onVideoFrame?(pixelBuffer, pts)
    }

    private func configureSession(position: AVCaptureDevice.Position) throws {
        session.beginConfiguration()
        defer { session.commitConfiguration() }

        session.sessionPreset = .hd1920x1080

        if let camDevice = AVCaptureDevice.default(.builtInWideAngleCamera, for: .video, position: position) {
            let videoInput = try AVCaptureDeviceInput(device: camDevice)
            if session.canAddInput(videoInput) {
                session.addInput(videoInput)
            }
        }

        let videoOut = AVCaptureVideoDataOutput()
        videoOut.alwaysDiscardsLateVideoFrames = true
        videoOut.videoSettings = [
            kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        ]
        videoOut.setSampleBufferDelegate(self, queue: videoQueue)
        if session.canAddOutput(videoOut) {
            session.addOutput(videoOut)
        }

        if let micDevice = AVCaptureDevice.default(for: .audio) {
            let audioInput = try AVCaptureDeviceInput(device: micDevice)
            if session.canAddInput(audioInput) {
                session.addInput(audioInput)
            }
        }

        let audioOut = AVCaptureAudioDataOutput()
        audioOut.setSampleBufferDelegate(self, queue: audioQueue)
        if session.canAddOutput(audioOut) {
            session.addOutput(audioOut)
        }
    }

    public func captureOutput(
        _ output: AVCaptureOutput,
        didOutput sampleBuffer: CMSampleBuffer,
        from connection: AVCaptureConnection
    ) {
        if output is AVCaptureVideoDataOutput {
            guard let pb = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
            let pts = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
            onVideoFrame?(pb, pts)
        } else if output is AVCaptureAudioDataOutput {
            if let pcm = extractStereoInterleavedF32(from: sampleBuffer) {
                onAudioSamples?(pcm)
            }
        }
    }

    private func extractStereoInterleavedF32(from sampleBuffer: CMSampleBuffer) -> [Float]? {
        guard let fmtDesc = CMSampleBufferGetFormatDescription(sampleBuffer),
              let asbdPtr = CMAudioFormatDescriptionGetStreamBasicDescription(fmtDesc)
        else {
            return nil
        }
        let asbd = asbdPtr.pointee
        let channels = max(1, Int(asbd.mChannelsPerFrame))

        guard let blockBuffer = CMSampleBufferGetDataBuffer(sampleBuffer) else { return nil }
        var totalLen = 0
        var dataPtr: UnsafeMutablePointer<CChar>?
        let status = CMBlockBufferGetDataPointer(
            blockBuffer,
            atOffset: 0,
            lengthAtOffsetOut: nil,
            totalLengthOut: &totalLen,
            dataPointerOut: &dataPtr
        )
        guard status == kCMBlockBufferNoErr, let base = dataPtr, totalLen > 0 else { return nil }

        let isFloat = (asbd.mFormatFlags & kAudioFormatFlagIsFloat) != 0
        if isFloat && asbd.mBitsPerChannel == 32 {
            let count = totalLen / MemoryLayout<Float>.size
            let floats = UnsafeBufferPointer(
                start: UnsafeRawPointer(base).assumingMemoryBound(to: Float.self),
                count: count
            )
            if channels == 2 {
                return Array(floats)
            }
            // Upmix mono to interleaved stereo
            var stereo = [Float]()
            stereo.reserveCapacity(count * 2)
            for s in floats {
                stereo.append(s)
                stereo.append(s)
            }
            return stereo
        }

        // Convert signed 16-bit integer PCM to normalized stereo f32
        let sampleCount = totalLen / MemoryLayout<Int16>.size
        let ints = UnsafeBufferPointer(
            start: UnsafeRawPointer(base).assumingMemoryBound(to: Int16.self),
            count: sampleCount
        )
        let inv: Float = 1.0 / 32768.0
        var stereo = [Float]()
        if channels >= 2 {
            let frames = sampleCount / channels
            stereo.reserveCapacity(frames * 2)
            for f in 0..<frames {
                stereo.append(Float(ints[f * channels]) * inv)
                stereo.append(Float(ints[f * channels + 1]) * inv)
            }
        } else {
            stereo.reserveCapacity(sampleCount * 2)
            for s in ints {
                let v = Float(s) * inv
                stereo.append(v)
                stereo.append(v)
            }
        }
        return stereo
    }
}
