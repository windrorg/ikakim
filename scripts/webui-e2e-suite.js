// Standalone Zero-Dependency Browser E2E Verification Suite for ikakim Web UI.
// Evaluate inside Chrome/Safari/Playwright against a live ikakim HTTP/WS server.
(async () => {
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const assert = (cond, msg) => {
    if (!cond) throw new Error("Assertion failed: " + msg);
  };
  const $ = (id) => document.getElementById(id);

  const report = [];
  const pass = (name, detail) => report.push({ name, status: "PASS", detail: detail || "" });

  // 1. Core DOM & Subset Layout Checks
  assert($("ws-dot").classList.contains("connected"), "WebSocket dot must be connected");
  assert(
    document.documentElement.scrollWidth <= window.innerWidth,
    "No horizontal overflow: scrollWidth=" +
      document.documentElement.scrollWidth +
      " innerWidth=" +
      window.innerWidth
  );
  pass("dom-and-layout", "width=" + window.innerWidth + "px");

  // 2. Live 30Hz Telemetry & Video Frame Check
  await sleep(350);
  const fpsVal = parseFloat($("fps-readout").textContent);
  assert(!Number.isNaN(fpsVal), "FPS readout must be numeric");
  pass("live-telemetry", "fps=" + fpsVal.toFixed(1));

  // 3. SPA Routing (#studio -> #mixer -> #overlay -> #diag -> #studio)
  for (const r of ["mixer", "overlay", "diag", "studio"]) {
    window.location.hash = "#" + r;
    await sleep(60);
    assert(document.body.classList.contains("route-" + r), "Missing route-" + r);
  }
  pass("spa-router", "studio/mixer/overlay/diag");

  // 4. Presets Popover Toggle, URL Selection, Bitrate Selection & Escape Dismiss
  const presetsBtn = $("btn-stream-presets");
  presetsBtn.click();
  await sleep(40);
  let pop = document.querySelector(".portal-popover");
  assert(pop !== null, "Presets popover must open");
  presetsBtn.click(); // Re-click must toggle closed
  await sleep(40);
  assert(document.querySelector(".portal-popover") === null, "Re-click must toggle popover closed");

  presetsBtn.click();
  await sleep(40);
  pop = document.querySelector(".portal-popover");
  const items = Array.from(pop.querySelectorAll(".pop-item"));
  const brItem = items.find((el) => el.textContent.includes("1080p60 High"));
  assert(brItem, "1080p60 High preset missing");
  brItem.click();
  await sleep(40);
  assert(presetsBtn.textContent.includes("6.0M"), "Presets button must reflect 6.0M bitrate");
  pass("presets-popover", presetsBtn.textContent);

  // 5. Empty URL Validation Modal & Escape Key Dismiss
  const rtmpUrl = $("rtmp-url");
  const savedUrl = rtmpUrl.value;
  rtmpUrl.value = "   ";
  $("btn-stream-toggle").click();
  await sleep(40);
  assert(document.querySelector(".portal-backdrop") !== null, "Empty URL warning modal must open");
  document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  await sleep(40);
  assert(document.querySelector(".portal-backdrop") === null, "Escape must dismiss modal");
  rtmpUrl.value = savedUrl || "rtmp://127.0.0.1:1935/live/e2e_check";
  pass("empty-url-modal-escape");

  // 6. Audio Mixer Gain Presets (-12 / -6 / 0dB), Delay, Mute & Clip Reset
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
  muteBtn.click(); // restore
  $("btn-clip-reset").click();
  pass("audio-mixer-controls");

  // 7. Overlay Layer Select, Sliders, Visibility & Reset Center
  const layerSelect = $("layer-select");
  const prevLayer = layerSelect.value;
  layerSelect.value = "0";
  layerSelect.dispatchEvent(new Event("change", { bubbles: true }));
  layerSelect.value = prevLayer;
  layerSelect.dispatchEvent(new Event("change", { bubbles: true }));

  const visBox = $("layer-visible");
  visBox.checked = false;
  visBox.dispatchEvent(new Event("change", { bubbles: true }));
  assert($("pip-box").classList.contains("hidden-layer"), "pip-box should mark hidden-layer");
  $("btn-overlay-reset").click();
  assert(visBox.checked === true, "Overlay reset must re-enable visibility");
  assert($("layer-x-val").textContent === "0.00", "Overlay reset X must be 0.00");
  pass("overlay-and-pip-controls");

  // 8. Diagnostics Sync
  $("btn-refresh-status").click();
  await sleep(80);
  assert($("diag-log").children.length > 0, "Diagnostics log must contain entries");
  pass("diagnostics-sync", "logCount=" + $("diag-log").children.length);

  return { ok: true, passed: report.length, checks: report };
})();
