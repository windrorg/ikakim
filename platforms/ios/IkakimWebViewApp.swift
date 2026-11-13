import AVFoundation
import SwiftUI
import UIKit
import WebKit

/// `WKWebView` wrapper loading the embedded `core-net-ui` control & MSE preview console.
public struct IkakimWebView: UIViewRepresentable {
    public let url: URL

    public init(url: URL) {
        self.url = url
    }

    public func makeUIView(context: Context) -> WKWebView {
        let cfg = WKWebViewConfiguration()
        cfg.allowsInlineMediaPlayback = true
        cfg.mediaTypesRequiringUserActionForPlayback = []
        let webView = WKWebView(frame: .zero, configuration: cfg)
        webView.scrollView.contentInsetAdjustmentBehavior = .automatic
        webView.load(URLRequest(url: url))
        return webView
    }

    public func updateUIView(_ uiView: WKWebView, context: Context) {
        if uiView.url?.port != url.port {
            uiView.load(URLRequest(url: url))
        }
    }
}

@main
public struct IkakimIosHostApp: App {
    @StateObject private var engine = IkakimEngine(
        mainWidth: 1920,
        mainHeight: 1080,
        previewWidth: 1280,
        previewHeight: 720,
        targetFps: 30,
        initialBitrateBps: 4_000_000,
        httpPort: 0
    )
    private let cameraBridge = AVCaptureCameraBridge()
    @State private var cameraStarted = false

    public init() {}

    public var body: some Scene {
        WindowGroup {
            VStack(spacing: 0) {
                HStack {
                    Text("ikakim iOS Host")
                        .font(.headline)
                    Spacer()
                    Text("ABR: \(engine.currentBitrateBps / 1000) kbps")
                        .font(.caption.monospacedDigit())
                    Button(cameraStarted ? "Stop Camera" : "Start Camera") {
                        toggleCamera()
                    }
                    .buttonStyle(.borderedProminent)
                }
                .padding(8)

                if let url = engine.uiURL {
                    IkakimWebView(url: url)
                } else {
                    Text("Starting local UI server...")
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                }
            }
            .onAppear {
                wireCaptureCallbacks()
                engine.startTickLoop()
            }
            .onDisappear {
                cameraBridge.stopCapture()
                engine.stopTickLoop()
            }
        }
    }

    private func wireCaptureCallbacks() {
        cameraBridge.onVideoFrame = { [weak engine] pixelBuffer, _ in
            Task { @MainActor in
                engine?.submitCVPixelBuffer(pixelBuffer, layerId: 0, isBaseLayer: true)
            }
        }
        cameraBridge.onAudioSamples = { [weak engine] samples in
            Task { @MainActor in
                engine?.pushAudioInterleavedF32(samples, channelIndex: 0)
            }
        }
    }

    private func toggleCamera() {
        if cameraStarted {
            cameraBridge.stopCapture()
            cameraStarted = false
        } else {
            do {
                try cameraBridge.startCapture(position: .back)
                cameraStarted = true
            } catch {
                cameraStarted = false
            }
        }
    }
}
