import AVFoundation
import OSLog
import SwiftUI
import UIKit
import WebKit

private let ikakimLog = Logger(subsystem: "com.ikakim.live.ios", category: "host")

/// `WKWebView` wrapper loading the embedded `core-net-ui` control & MSE preview console.
/// Enables Safari Web Inspector (`isInspectable = true`) and optional automated E2E execution.
public struct IkakimWebView: UIViewRepresentable {
    public let url: URL
    public let e2eJsPath: String?
    public let e2eResultPath: String?

    public init(url: URL, e2eJsPath: String? = nil, e2eResultPath: String? = nil) {
        self.url = url
        self.e2eJsPath = e2eJsPath
        self.e2eResultPath = e2eResultPath
    }

    public func makeCoordinator() -> Coordinator {
        Coordinator(e2eJsPath: e2eJsPath, e2eResultPath: e2eResultPath)
    }

    public func makeUIView(context: Context) -> WKWebView {
        let cfg = WKWebViewConfiguration()
        cfg.allowsInlineMediaPlayback = true
        cfg.mediaTypesRequiringUserActionForPlayback = []
        let errorProbeScript = WKUserScript(
            source: """
            window.__ikakimConsoleErrors = [];
            const origErr = console.error.bind(console);
            console.error = (...args) => {
              window.__ikakimConsoleErrors.push(args.map(String).join(' '));
              origErr(...args);
            };
            window.addEventListener('error', (e) => {
              window.__ikakimConsoleErrors.push('pageerror: ' + (e.message || e.error || 'unknown'));
            });
            window.addEventListener('unhandledrejection', (e) => {
              window.__ikakimConsoleErrors.push('unhandledrejection: ' + String(e.reason));
            });
            """,
            injectionTime: .atDocumentStart,
            forMainFrameOnly: true
        )
        cfg.userContentController.addUserScript(errorProbeScript)
        let webView = WKWebView(frame: .zero, configuration: cfg)
        if #available(iOS 16.4, *) {
            webView.isInspectable = true
        }
        webView.navigationDelegate = context.coordinator
        webView.scrollView.contentInsetAdjustmentBehavior = .automatic
        webView.load(URLRequest(url: url))
        return webView
    }

    public func updateUIView(_ uiView: WKWebView, context: Context) {
        if uiView.url?.port != url.port {
            uiView.load(URLRequest(url: url))
        }
    }

    public final class Coordinator: NSObject, WKNavigationDelegate {
        private let e2eJsPath: String?
        private let e2eResultPath: String?
        private var didRunE2E = false

        init(e2eJsPath: String?, e2eResultPath: String?) {
            self.e2eJsPath = e2eJsPath
            self.e2eResultPath = e2eResultPath
        }

        public func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
            guard !didRunE2E,
                  let jsPath = e2eJsPath, !jsPath.isEmpty,
                  let jsSource = try? String(contentsOfFile: jsPath, encoding: .utf8) else {
                return
            }
            didRunE2E = true

            // Allow WebSocket connection + initial 30Hz telemetry before running the in-WebView E2E suite.
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) { [weak webView] in
                guard let webView else { return }
                let b64 = Data(jsSource.utf8).base64EncodedString()
                let runner = """
                window.__ikakimE2E = { done: false, result: null, error: null };
                (() => {
                  try {
                    const bin = atob('\(b64)');
                    const bytes = Uint8Array.from(bin, (c) => c.charCodeAt(0));
                    const code = new TextDecoder('utf-8').decode(bytes);
                    Promise.resolve((0, eval)(code))
                      .then((res) => {
                        const errs = window.__ikakimConsoleErrors || [];
                        const merged = Object.assign({}, res || {}, { consoleErrors: errs });
                        if (errs.length > 0) {
                          window.__ikakimE2E = { done: true, result: merged, error: 'Console/Page errors: ' + errs.join('; ') };
                        } else {
                          window.__ikakimE2E = { done: true, result: merged, error: null };
                        }
                      })
                      .catch((err) => {
                        const msg = err && err.message ? `${err.message} | ${err.stack || ''}` : String(err);
                        window.__ikakimE2E = { done: true, result: null, error: msg };
                      });
                  } catch (err) {
                    const msg = err && err.message ? `${err.message} | ${err.stack || ''}` : String(err);
                    window.__ikakimE2E = { done: true, result: null, error: 'Eval error: ' + msg };
                  }
                })();
                """
                webView.evaluateJavaScript(runner) { _, err in
                    if let err {
                        self.writeResult(["ok": false, "error": "evaluateJavaScript failed: \(err.localizedDescription)"])
                        return
                    }
                    self.pollE2EResult(webView: webView, attemptsRemaining: 40)
                }
            }
        }

        private func pollE2EResult(webView: WKWebView, attemptsRemaining: Int) {
            if attemptsRemaining <= 0 {
                writeResult(["ok": false, "error": "E2E suite timed out inside WKWebView"])
                return
            }
            webView.evaluateJavaScript("JSON.stringify(window.__ikakimE2E)") { value, err in
                if let err {
                    self.writeResult(["ok": false, "error": err.localizedDescription])
                    return
                }
                if let jsonStr = value as? String,
                   let data = jsonStr.data(using: .utf8),
                   let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                   (obj["done"] as? Bool) == true {
                    if let errMsg = obj["error"] as? String {
                        self.writeResult(["ok": false, "error": errMsg])
                    } else if let res = obj["result"] as? [String: Any] {
                        self.writeResult(res)
                    } else {
                        self.writeResult(["ok": false, "error": "Unexpected E2E payload: \(jsonStr)"])
                    }
                    return
                }
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.25) { [weak self, weak webView] in
                    guard let self, let webView else { return }
                    self.pollE2EResult(webView: webView, attemptsRemaining: attemptsRemaining - 1)
                }
            }
        }

        private func writeResult(_ payload: [String: Any]) {
            guard let data = try? JSONSerialization.data(withJSONObject: payload, options: [.prettyPrinted]),
                  let text = String(data: data, encoding: .utf8) else {
                return
            }
            ikakimLog.info("WKWebView E2E result: \(text, privacy: .public)")
            if let outPath = e2eResultPath, !outPath.isEmpty {
                try? text.write(toFile: outPath, atomically: true, encoding: .utf8)
            }
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
        httpPort: Self.configuredHttpPort()
    )
    private let cameraBridge = AVCaptureCameraBridge()
    private let virtualSource = VirtualCaptureSource(width: 1280, height: 720, fps: 30.0)
    @State private var cameraStarted = false
    @State private var virtualStarted = false

    private let e2eJsPath = ProcessInfo.processInfo.environment["IKAKIM_E2E_JS_PATH"]
    private let e2eResultPath = ProcessInfo.processInfo.environment["IKAKIM_E2E_RESULT_PATH"]

    public init() {}

    private static func configuredHttpPort() -> UInt16 {
        if let raw = ProcessInfo.processInfo.environment["IKAKIM_HTTP_PORT"],
           let parsed = UInt16(raw) {
            return parsed
        }
        return 0
    }

    public var body: some Scene {
        WindowGroup {
            VStack(spacing: 0) {
                HStack(spacing: 8) {
                    Text("ikakim")
                        .font(.subheadline.weight(.semibold))
                        .lineLimit(1)
                    Spacer()
                    Text("\(engine.currentBitrateBps / 1000) kbps")
                        .font(.caption.monospacedDigit())
                        .lineLimit(1)
                    Button(virtualStarted ? "Virtual: ON" : "Virtual I/O") {
                        toggleVirtualIO()
                    }
                    .buttonStyle(.bordered)
                    .controlSize(.small)
                    .lineLimit(1)
                    Button(cameraStarted ? "Stop Cam" : "Camera") {
                        toggleCamera()
                    }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.small)
                    .lineLimit(1)
                }
                .padding(.horizontal, 10)
                .padding(.vertical, 6)

                if let url = engine.uiURL {
                    IkakimWebView(url: url, e2eJsPath: e2eJsPath, e2eResultPath: e2eResultPath)
                } else {
                    Text("Starting local UI server...")
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                }
            }
            .onAppear {
                ikakimLog.info("IkakimIosHostApp started uiPort=\(engine.uiPort)")
                wireCaptureCallbacks()
                engine.startTickLoop()
                if ProcessInfo.processInfo.environment["IKAKIM_VIRTUAL_IO"] == "1" {
                    startVirtualIO()
                }
            }
            .onDisappear {
                cameraBridge.stopCapture()
                virtualSource.stop()
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
        virtualSource.onVideoFrame = { [weak engine] pixelBuffer in
            engine?.submitCVPixelBuffer(pixelBuffer, layerId: 0, isBaseLayer: true)
        }
        virtualSource.onOverlayRgba = { [weak engine] data, w, h in
            engine?.submitRgbaLayer(data, width: w, height: h, layerId: 1, isBaseLayer: false, topBottomAlpha: false)
        }
        virtualSource.onAudioSamples = { [weak engine] samples in
            engine?.pushAudioInterleavedF32(samples, channelIndex: 0)
        }
    }

    private func startVirtualIO() {
        guard !virtualStarted else { return }
        virtualSource.start()
        virtualStarted = true
        ikakimLog.info("VirtualCaptureSource started (1280x720@30fps CVPixelBuffer + Layer1 RGBA + 48kHz PCM)")
    }

    private func toggleVirtualIO() {
        if virtualStarted {
            virtualSource.stop()
            virtualStarted = false
        } else {
            startVirtualIO()
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
