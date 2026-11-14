// Extended iOS/iPadOS WKWebView + Virtual I/O + Native Engine Integration E2E Suite.
// Evaluates all interactive Web UI flows, MSE video playback, audio meter activity,
// PiP pointer drag, and optional live RTMP start/stop against the native IkakimEngine.
(async () => {
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const assert = (cond, msg) => {
    if (!cond) throw new Error("Assertion failed: " + msg);
  };
  const $ = (id) => document.getElementById(id);

  const report = [];
  const pass = (name, detail) => report.push({ name, status: "PASS", detail: detail || "" });

  // 1. Core DOM & Responsive Layout (wait up to 5s for WebSocket handshake on cold WebView launch)
  let wsConnected = false;
  for (let i = 0; i < 50; i++) {
    if ($("ws-dot") && $("ws-dot").classList.contains("connected")) {
      wsConnected = true;
      break;
    }
    await sleep(100);
  }
  assert(wsConnected, "WebSocket dot must be connected");
  assert(
    document.documentElement.scrollWidth <= window.innerWidth,
    "No horizontal overflow: scrollWidth=" +
      document.documentElement.scrollWidth +
      " innerWidth=" +
      window.innerWidth
  );
  pass("dom-and-layout", "width=" + window.innerWidth + "x" + window.innerHeight);

  // 2. Virtual I/O -> VideoToolbox/SwH264 -> fMP4 -> WebView MSE Playback Check
  let videoOk = false;
  let videoInfo = "";
  const videoEl = $("preview-video");
  for (let i = 0; i < 30; i++) {
    await sleep(200);
    if (videoEl && videoEl.videoWidth > 0 && videoEl.readyState >= 2) {
      videoOk = true;
      videoInfo = `${videoEl.videoWidth}x${videoEl.videoHeight} t=${videoEl.currentTime.toFixed(2)}s`;
      break;
    }
  }
  const fpsVal = parseFloat($("fps-readout").textContent);
  assert(!Number.isNaN(fpsVal), "FPS readout must be numeric");
  const vidErr = videoEl && videoEl.error ? `err=${videoEl.error.code}:${videoEl.error.message}` : "err=none";
  const diagTxt = $("diag-log") ? Array.from($("diag-log").children).slice(0, 4).map((e) => e.textContent).join(" | ") : "";
  assert(
    videoOk,
    `MSE/ManagedMediaSource video preview must decode frames (codec=${$("codec-label").textContent} rs=${videoEl ? videoEl.readyState : -1} ${vidErr} diag=${diagTxt})`
  );
  pass("virtual-io-mse-and-telemetry", `fps=${fpsVal.toFixed(1)} video=${videoInfo}`);

  // 3. Audio Meter Activity Check (48kHz Virtual Stereo Sine -> CoreAudio Mixer -> 30Hz WS -> #master-l-db)
  let meterActive = false;
  let meterDbText = "";
  for (let i = 0; i < 15; i++) {
    await sleep(100);
    const dbEl = $("master-l-db");
    if (dbEl) {
      meterDbText = dbEl.textContent || "";
      const val = parseFloat(meterDbText);
      if (!Number.isNaN(val) && val > -50.0) {
        meterActive = true;
        break;
      }
    }
  }
  assert(meterActive, "Master L meter (#master-l-db) should reflect active 48kHz virtual PCM input, got: " + meterDbText);
  pass("virtual-audio-30hz-meter", meterDbText);

  // 4. SPA Routing (#mixer -> #overlay -> #diag -> #studio)
  for (const r of ["mixer", "overlay", "diag", "studio"]) {
    window.location.hash = "#" + r;
    await sleep(60);
    assert(document.body.classList.contains("route-" + r), "Missing route-" + r);
  }
  pass("spa-router", "studio/mixer/overlay/diag");

  // 5. Presets Popover Toggle, Bitrate Selection & Escape Dismiss
  const presetsBtn = $("btn-stream-presets");
  presetsBtn.click();
  await sleep(40);
  let pop = document.querySelector(".portal-popover");
  assert(pop !== null, "Presets popover must open");
  presetsBtn.click();
  await sleep(40);
  assert(document.querySelector(".portal-popover") === null, "Re-click must close popover");

  presetsBtn.click();
  await sleep(40);
  pop = document.querySelector(".portal-popover");
  const brItem = Array.from(pop.querySelectorAll(".pop-item")).find((el) =>
    el.textContent.includes("1080p60 High")
  );
  assert(brItem, "1080p60 High preset missing");
  brItem.click();
  await sleep(40);
  assert(presetsBtn.textContent.includes("6.0M"), "Presets button must reflect 6.0M bitrate");
  pass("presets-popover", presetsBtn.textContent);

  // 6. Empty URL Validation Modal & Escape Key Dismiss
  const rtmpUrl = $("rtmp-url");
  const savedUrl = rtmpUrl.value;
  rtmpUrl.value = "   ";
  $("btn-stream-toggle").click();
  await sleep(40);
  assert(document.querySelector(".portal-backdrop") !== null, "Empty URL warning modal must open");
  document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  await sleep(40);
  assert(document.querySelector(".portal-backdrop") === null, "Escape must dismiss modal");
  rtmpUrl.value = savedUrl || "rtmp://127.0.0.1:1935/live/ios_sim_e2e";
  pass("empty-url-modal-escape");

  // 7. Audio Mixer Gain Presets (-12 / -6 / 0dB), Delay, Mute & Clip Reset
  const chCard = $("channels-container").querySelector(".channel-card");
  assert(chCard !== null, "At least one channel card required");
  const preset6 = Array.from(chCard.querySelectorAll(".preset-pill")).find((b) => b.textContent === "-6");
  preset6.click();
  const gainSlider = chCard.querySelector(".gain-slider");
  assert(parseFloat(gainSlider.value) === -6, "Gain preset -6 failed");
  gainSlider.dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
  assert(parseFloat(gainSlider.value) === 0, "Gain dblclick reset to 0 failed");

  const delaySlider = chCard.querySelector(".delay-slider");
  delaySlider.value = "35";
  delaySlider.dispatchEvent(new Event("input", { bubbles: true }));
  delaySlider.dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
  assert(parseFloat(delaySlider.value) === 0, "Delay dblclick reset to 0 failed");

  const muteBtn = chCard.querySelector(".mute-btn");
  const prevMute = muteBtn.textContent;
  muteBtn.click();
  await sleep(40);
  assert(muteBtn.textContent !== prevMute, "Mute toggle state did not change");
  muteBtn.click();
  $("btn-clip-reset").click();
  pass("audio-mixer-controls");

  // 8. Overlay Layer Select, PiP Pointer Drag, Visibility & Reset Center
  const layerSelect = $("layer-select");
  layerSelect.value = "1";
  layerSelect.dispatchEvent(new Event("change", { bubbles: true }));
  await sleep(40);

  const pip = $("pip-box");
  const rect = pip.getBoundingClientRect();
  pip.dispatchEvent(new PointerEvent("pointerdown", {
    clientX: rect.left + rect.width / 2,
    clientY: rect.top + rect.height / 2,
    pointerId: 1,
    bubbles: true,
  }));
  window.dispatchEvent(new PointerEvent("pointermove", {
    clientX: rect.left + rect.width / 2 + 24,
    clientY: rect.top + rect.height / 2 + 16,
    pointerId: 1,
    bubbles: true,
  }));
  window.dispatchEvent(new PointerEvent("pointerup", { pointerId: 1, bubbles: true }));
  await sleep(50);

  const visBox = $("layer-visible");
  visBox.checked = false;
  visBox.dispatchEvent(new Event("change", { bubbles: true }));
  assert(pip.classList.contains("hidden-layer"), "pip-box should mark hidden-layer");
  $("btn-overlay-reset").click();
  await sleep(40);
  assert(visBox.checked === true, "Overlay reset must re-enable visibility");
  assert($("layer-x-val").textContent === "0.00", "Overlay reset X must be 0.00");
  pass("overlay-pip-drag-and-reset");

  // 9. Diagnostics Sync
  $("btn-refresh-status").click();
  await sleep(80);
  assert($("diag-log").children.length > 0, "Diagnostics log must contain entries");
  pass("diagnostics-sync", "logCount=" + $("diag-log").children.length);

  // 10. Optional Live RTMP Stream Start -> Verify -> Stop (when SRS is active on 127.0.0.1:1935)
  if (window.__IKAKIM_ENABLE_LIVE_STREAM_E2E) {
    rtmpUrl.value = window.__IKAKIM_LIVE_STREAM_URL || "rtmp://127.0.0.1:1935/live/ios_sim_e2e";
    $("btn-stream-toggle").click();
    let started = false;
    for (let i = 0; i < 30; i++) {
      await sleep(100);
      if ($("btn-stream-toggle").textContent.includes("Stop")) {
        started = true;
        break;
      }
    }
    assert(started, "Stream button should switch to Stop Stream");
    await sleep(800);
    $("btn-stream-toggle").click();
    await sleep(60);
    const stopModal = document.querySelector(".portal-backdrop");
    assert(stopModal !== null, "Stop Live Stream confirmation modal must open");
    const confirmBtn = Array.from(stopModal.querySelectorAll("button")).find((b) =>
      b.textContent.includes("Stop Stream")
    );
    assert(confirmBtn, "Stop Stream confirm button missing in modal");
    confirmBtn.click();
    let stopped = false;
    for (let i = 0; i < 30; i++) {
      await sleep(100);
      if ($("btn-stream-toggle").textContent.includes("Start")) {
        stopped = true;
        break;
      }
    }
    assert(stopped, "Stream button should return to Start Stream");
    pass("live-rtmp-stream-toggle", rtmpUrl.value);
  }

  return { ok: true, passed: report.length, checks: report };
})();
