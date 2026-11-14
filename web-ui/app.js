(() => {
  "use strict";

  // =========================================================================
  // 1. Zero-Dependency Micro-Signal Core & DOM Level 1/2 Builder (~0.5 KB gzip)
  // =========================================================================
  let currentObserver = null;

  function signal(initial) {
    let value = initial;
    const subs = new Set();
    return {
      get val() {
        if (currentObserver) subs.add(currentObserver);
        return value;
      },
      set val(next) {
        if (Object.is(value, next)) return;
        value = next;
        const snapshot = Array.from(subs);
        for (let i = 0; i < snapshot.length; i++) {
          snapshot[i]();
        }
      },
    };
  }

  function derive(fn) {
    const run = () => {
      const prev = currentObserver;
      currentObserver = run;
      try {
        fn();
      } finally {
        currentObserver = prev;
      }
    };
    run();
  }

  function appendChildNode(parent, child) {
    if (child == null || child === false) return;
    if (Array.isArray(child)) {
      for (let i = 0; i < child.length; i++) appendChildNode(parent, child[i]);
      return;
    }
    if (typeof child === "function") {
      const tn = document.createTextNode("");
      parent.appendChild(tn);
      derive(() => {
        const res = child();
        tn.nodeValue = res == null ? "" : String(res);
      });
      return;
    }
    if (child instanceof Node) {
      parent.appendChild(child);
    } else {
      parent.appendChild(document.createTextNode(String(child)));
    }
  }

  function h(tag, props) {
    const el = document.createElement(tag);
    if (props && typeof props === "object") {
      const keys = Object.keys(props);
      for (let i = 0; i < keys.length; i++) {
        const k = keys[i];
        const v = props[k];
        if (k.slice(0, 2) === "on" && typeof v === "function") {
          el.addEventListener(k.slice(2).toLowerCase(), v);
        } else if (k === "className") {
          if (typeof v === "function") {
            derive(() => {
              el.className = v();
            });
          } else {
            el.className = v;
          }
        } else if (k === "value") {
          el.value = v;
        } else if (typeof v === "function") {
          derive(() => {
            el.setAttribute(k, String(v()));
          });
        } else if (v != null && v !== false) {
          el.setAttribute(k, v === true ? "" : String(v));
        }
      }
    }
    for (let i = 2; i < arguments.length; i++) {
      appendChildNode(el, arguments[i]);
    }
    return el;
  }

  // =========================================================================
  // 2. DOM References & Reactive Application Store
  // =========================================================================
  const $ = (id) => document.getElementById(id);
  const wsDot = $("ws-dot");
  const stateBadge = $("state-badge");
  const previewLivePill = $("preview-live-pill");
  const fpsReadout = $("fps-readout");
  const bitrateReadout = $("bitrate-readout");
  const latencyReadout = $("latency-readout");
  const codecLabel = $("codec-label");
  const videoEl = $("preview-video");
  const rtmpUrlEl = $("rtmp-url");
  const streamBtn = $("btn-stream-toggle");
  const presetsBtn = $("btn-stream-presets");
  const clipResetBtn = $("btn-clip-reset");
  const refreshBtn = $("btn-refresh-status");

  const masterLBar = $("master-l-bar");
  const masterLHold = $("master-l-hold");
  const masterLDb = $("master-l-db");
  const masterRBar = $("master-r-bar");
  const masterRHold = $("master-r-hold");
  const masterRDb = $("master-r-db");
  const masterClipBadge = $("master-clip-badge");
  const channelsContainer = $("channels-container");

  const pipStage = $("pip-stage");
  const pipBox = $("pip-box");
  const pipTag = $("pip-tag");
  const pipResizeHandle = $("pip-resize-handle");

  const layerSelect = $("layer-select");
  const layerVisible = $("layer-visible");
  const layerX = $("layer-x");
  const layerY = $("layer-y");
  const layerScale = $("layer-scale");
  const layerOpacity = $("layer-opacity");
  const layerXVal = $("layer-x-val");
  const layerYVal = $("layer-y-val");
  const layerScaleVal = $("layer-scale-val");
  const layerOpacityVal = $("layer-opacity-val");
  const overlayResetBtn = $("btn-overlay-reset");

  const diagStatsEl = $("diag-stats");
  const diagLogEl = $("diag-log");

  const connState = signal("Disconnected");
  const streamState = signal("Idle");
  const wsReconnects = signal(0);
  const rpcSentCount = signal(0);
  const lastRpcRttMs = signal("--");

  const renderedChannels = new Map();
  const overlaysStore = new Map();
  const pendingRpcMap = new Map();
  let activeDragKey = null;
  let rafRpcScheduled = false;
  let selectedBitrateBps = null;
  let activePopoverClose = null;
  let activeBlobUrl = null;
  let reconnectTimer = null;

  let ws = null;
  let rpcNextId = 1;
  const rpcSentAt = new Map();

  // Master & channel peak hold latches (GPU compositor transforms only, zero reflow)
  const masterPeak = { lHold: 0, lTime: 0, rHold: 0, rTime: 0, clipped: false };

  function logDiag(msg) {
    const now = new Date();
    const ts = now.toTimeString().slice(0, 8);
    const row = h("div", { className: "diag-log-item mono" }, "[" + ts + "] " + msg);
    diagLogEl.insertBefore(row, diagLogEl.firstChild);
    while (diagLogEl.children.length > 40) {
      diagLogEl.removeChild(diagLogEl.lastChild);
    }
  }

  // =========================================================================
  // 3. Hash SPA Router (#studio | #mixer | #overlay | #diag)
  // =========================================================================
  const VALID_ROUTES = { studio: true, mixer: true, overlay: true, diag: true };

  function syncRouteFromHash() {
    const raw = (window.location.hash || "").replace(/^#\/?/, "");
    const next = VALID_ROUTES[raw] ? raw : "studio";
    document.body.className = "app-shell route-" + next;
    const btns = document.querySelectorAll("[data-route]");
    for (let i = 0; i < btns.length; i++) {
      const b = btns[i];
      b.classList.toggle("active", b.getAttribute("data-route") === next);
    }
  }

  window.addEventListener("hashchange", syncRouteFromHash);
  const navBtns = document.querySelectorAll("[data-route]");
  for (let i = 0; i < navBtns.length; i++) {
    navBtns[i].addEventListener("click", function () {
      const target = this.getAttribute("data-route");
      if (target) window.location.hash = "#" + target;
    });
  }
  syncRouteFromHash();

  // =========================================================================
  // 4. Subset-Safe Portal Primitives (Modal, Popover, Hybrid Drag Gestures)
  // =========================================================================
  function openSafeModal(opts) {
    let closed = false;
    const prevFocus = document.activeElement;
    const close = () => {
      if (closed) return;
      closed = true;
      document.removeEventListener("keydown", onKey, true);
      if (backdrop.parentNode) backdrop.parentNode.removeChild(backdrop);
      if (prevFocus && typeof prevFocus.focus === "function") prevFocus.focus();
    };
    const onKey = (e) => {
      if (e.key === "Escape") close();
    };

    const confirmBtn = h(
      "button",
      {
        type: "button",
        className: opts.danger ? "btn-primary live" : "btn-primary",
        "data-action": "modal-confirm",
        onclick: () => {
          close();
          if (opts.onConfirm) opts.onConfirm();
        },
      },
      opts.confirmLabel || "Confirm"
    );

    const backdrop = h(
      "div",
      {
        className: "portal-backdrop",
        onclick: (e) => {
          if (e.target === backdrop) close();
        },
      },
      h(
        "div",
        { className: "portal-card", role: "dialog", "aria-modal": "true" },
        h("div", { className: "portal-title" }, opts.title || "Confirm"),
        h("div", { className: "portal-desc" }, opts.description || ""),
        h(
          "div",
          { className: "portal-actions" },
          h(
            "button",
            { type: "button", className: "btn-secondary", onclick: close },
            opts.cancelLabel || "Cancel"
          ),
          confirmBtn
        )
      )
    );
    document.addEventListener("keydown", onKey, true);
    document.body.appendChild(backdrop);
    setTimeout(() => confirmBtn.focus(), 0);
  }

  function placeSafePopover(anchorEl, contentEl) {
    if (activePopoverClose) activePopoverClose();
    const aRect = anchorEl.getBoundingClientRect();
    const vW = window.innerWidth;
    const vH = window.innerHeight;
    contentEl.style.visibility = "hidden";
    document.body.appendChild(contentEl);
    const pRect = contentEl.getBoundingClientRect();

    let top = aRect.bottom + 8;
    if (top + pRect.height > vH - 8 && aRect.top - pRect.height - 8 > 8) {
      top = aRect.top - pRect.height - 8;
    }
    let left = aRect.right - pRect.width;
    if (left < 8) left = aRect.left;
    left = Math.max(8, Math.min(left, vW - pRect.width - 8));

    contentEl.style.top = Math.round(top) + "px";
    contentEl.style.left = Math.round(left) + "px";
    contentEl.style.visibility = "visible";

    const close = () => {
      if (activePopoverClose === close) activePopoverClose = null;
      document.removeEventListener("click", onDocClick, true);
      document.removeEventListener("keydown", onKey, true);
      window.removeEventListener("resize", close);
      window.removeEventListener("hashchange", close);
      if (contentEl.parentNode) contentEl.parentNode.removeChild(contentEl);
    };
    const onDocClick = (e) => {
      if (!contentEl.contains(e.target) && !anchorEl.contains(e.target)) {
        close();
      }
    };
    const onKey = (e) => {
      if (e.key === "Escape") close();
    };
    activePopoverClose = close;
    setTimeout(() => {
      document.addEventListener("click", onDocClick, true);
      document.addEventListener("keydown", onKey, true);
      window.addEventListener("resize", close);
      window.addEventListener("hashchange", close);
    }, 0);
    return close;
  }

  function bindDragLock(el, getKey) {
    const unlock = () => {
      const key = typeof getKey === "function" ? getKey() : getKey;
      if (activeDragKey === key) activeDragKey = null;
      window.removeEventListener("mouseup", unlock);
      window.removeEventListener("touchend", unlock);
      window.removeEventListener("touchcancel", unlock);
      window.removeEventListener("blur", unlock);
    };
    const lock = () => {
      activeDragKey = typeof getKey === "function" ? getKey() : getKey;
      window.addEventListener("mouseup", unlock);
      window.addEventListener("touchend", unlock);
      window.addEventListener("touchcancel", unlock);
      window.addEventListener("blur", unlock);
    };
    el.addEventListener("mousedown", lock);
    el.addEventListener("touchstart", lock, { passive: true });
    el.addEventListener("change", unlock);
  }

  // Hybrid Mouse + Touch Drag Helper (Cobalt / Webf / Safari / Chrome safe)
  function bindDragGesture(targetEl, handlers) {
    let active = false;
    let startX = 0;
    let startY = 0;

    function ptFromEvent(e) {
      if (e.touches && e.touches.length > 0) {
        return { x: e.touches[0].clientX, y: e.touches[0].clientY };
      }
      if (e.changedTouches && e.changedTouches.length > 0) {
        return { x: e.changedTouches[0].clientX, y: e.changedTouches[0].clientY };
      }
      return { x: e.clientX, y: e.clientY };
    }

    function onStart(e) {
      if (e.type === "mousedown" && e.button !== 0) return;
      if (handlers.shouldIgnore && handlers.shouldIgnore(e)) return;
      e.preventDefault();
      e.stopPropagation();
      active = true;
      const pt = ptFromEvent(e);
      startX = pt.x;
      startY = pt.y;
      if (handlers.onStart) handlers.onStart(pt.x, pt.y, e);
      window.addEventListener("mousemove", onMove);
      window.addEventListener("mouseup", onEnd);
      window.addEventListener("touchmove", onMove, { passive: false });
      window.addEventListener("touchend", onEnd);
      window.addEventListener("touchcancel", onEnd);
    }

    function onMove(e) {
      if (!active) return;
      e.preventDefault();
      const pt = ptFromEvent(e);
      if (handlers.onMove) handlers.onMove(pt.x - startX, pt.y - startY, pt.x, pt.y);
    }

    function onEnd() {
      if (!active) return;
      active = false;
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onEnd);
      window.removeEventListener("touchmove", onMove);
      window.removeEventListener("touchend", onEnd);
      window.removeEventListener("touchcancel", onEnd);
      if (handlers.onEnd) handlers.onEnd();
    }

    targetEl.addEventListener("mousedown", onStart);
    targetEl.addEventListener("touchstart", onStart, { passive: false });
  }

  // =========================================================================
  // 5. MSE Low-Latency Preview Engine (<= 300ms Catch-up)
  // =========================================================================
  const MAX_LATENCY_SEC = 0.30; // 300ms 超過で自動キャッチアップシーク
  const TARGET_BUFFER_SEC = 0.05;
  let mediaSource = null;
  let sourceBuffer = null;
  let activeMime = 'video/mp4; codecs="avc1.42E01F"';
  const segmentQueue = [];
  const mseDroppedSegments = signal(0);
  const mseBufferedMs = signal(0);

  function initMse(mime) {
    if (mime) activeMime = mime;
    if (!("MediaSource" in window) || !MediaSource.isTypeSupported(activeMime)) {
      codecLabel.textContent = "MSE unsupported (" + activeMime + ")";
      return;
    }
    codecLabel.textContent = activeMime;
    segmentQueue.length = 0;
    try {
      if (mediaSource && mediaSource.readyState === "open") mediaSource.endOfStream();
    } catch (_) {}
    mediaSource = new MediaSource();
    sourceBuffer = null;
    if (activeBlobUrl) URL.revokeObjectURL(activeBlobUrl);
    activeBlobUrl = URL.createObjectURL(mediaSource);
    videoEl.src = activeBlobUrl;
    mediaSource.addEventListener("sourceopen", () => {
      try {
        sourceBuffer = mediaSource.addSourceBuffer(activeMime);
        sourceBuffer.mode = "segments";
        sourceBuffer.addEventListener("updateend", () => {
          trimOldBuffers();
          catchUpLatency();
          pumpSegmentQueue();
        });
        pumpSegmentQueue();
      } catch (err) {
        logDiag("MSE SourceBuffer init failed: " + err);
      }
    });
  }

  videoEl.addEventListener("click", () => {
    if (videoEl.paused) videoEl.play().catch(() => {});
  });

  function pumpSegmentQueue() {
    if (!sourceBuffer || sourceBuffer.updating || segmentQueue.length === 0) return;
    const nextBuf = segmentQueue.shift();
    try {
      sourceBuffer.appendBuffer(nextBuf);
    } catch (err) {
      logDiag("MSE appendBuffer err: " + err);
    }
  }

  function trimOldBuffers() {
    if (!sourceBuffer || sourceBuffer.updating || videoEl.buffered.length === 0) return;
    const start = videoEl.buffered.start(0);
    const keepFrom = videoEl.currentTime - 2.0;
    if (keepFrom > start + 0.5) {
      try {
        sourceBuffer.remove(start, keepFrom);
      } catch (_) {}
    }
  }

  function catchUpLatency() {
    if (videoEl.buffered.length === 0) return;
    const end = videoEl.buffered.end(videoEl.buffered.length - 1);
    const latency = Math.max(0, end - videoEl.currentTime);
    const latMs = Math.round(latency * 1000);
    mseBufferedMs.val = latMs;
    latencyReadout.textContent = latMs + " ms";
    if (latency > MAX_LATENCY_SEC) {
      videoEl.currentTime = Math.max(0, end - TARGET_BUFFER_SEC);
    }
    if (videoEl.paused) {
      videoEl.play().catch(() => {});
    }
  }

  setInterval(catchUpLatency, 100);

  // =========================================================================
  // 6. GPU-Composited 30Hz Peak Meters & Audio Mixer (Reflow = 0)
  // =========================================================================
  function dbfsToRatio(db) {
    const v = Number.isFinite(db) ? db : -96;
    const clamped = Math.max(-60, Math.min(0, v));
    return (clamped + 60) / 60;
  }

  function updateMeterChannel(fillEl, holdEl, dbEl, state, side, db, now) {
    const ratio = dbfsToRatio(db);
    fillEl.style.transform = "scaleX(" + ratio.toFixed(4) + ")";
    dbEl.textContent = (Number.isFinite(db) ? db : -96.0).toFixed(1);

    const holdKey = side === "l" ? "lHold" : "rHold";
    const timeKey = side === "l" ? "lTime" : "rTime";
    if (ratio >= state[holdKey] || now - state[timeKey] > 1400) {
      state[holdKey] = ratio;
      state[timeKey] = now;
    } else {
      state[holdKey] = Math.max(ratio, state[holdKey] - 0.015);
    }
    const tx = ((state[holdKey] - 1) * 100).toFixed(2);
    holdEl.style.transform = "translateX(" + tx + "%)";

    if (db >= -1.0) {
      state.clipped = true;
    }
  }

  clipResetBtn.addEventListener("click", () => {
    masterPeak.clipped = false;
    masterClipBadge.textContent = "PEAK OK";
    masterClipBadge.classList.remove("clipped");
    renderedChannels.forEach((refs) => {
      refs.peakState.clipped = false;
      refs.clipTag.textContent = "OK";
      refs.clipTag.classList.remove("clipped");
    });
  });

  masterClipBadge.addEventListener("click", () => {
    if (masterPeak.clipped) clipResetBtn.click();
  });

  function sendRpc(method, params) {
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      logDiag("WS offline — skipped " + method);
      return false;
    }
    const id = rpcNextId++;
    if (rpcSentAt.size > 64) rpcSentAt.clear();
    rpcSentAt.set(id, performance.now ? performance.now() : Date.now());
    rpcSentCount.val += 1;
    ws.send(
      JSON.stringify({
        jsonrpc: "2.0",
        id,
        method,
        params: params || {},
      })
    );
    return true;
  }

  // Coalesce high-frequency slider / PiP drag updates to max 1 WS frame per animation frame
  function scheduleRpc(coalesceKey, method, params) {
    pendingRpcMap.set(coalesceKey, { method, params });
    if (rafRpcScheduled) return;
    rafRpcScheduled = true;
    const flush = () => {
      rafRpcScheduled = false;
      pendingRpcMap.forEach((item) => {
        sendRpc(item.method, item.params);
      });
      pendingRpcMap.clear();
    };
    if (typeof window.requestAnimationFrame === "function") {
      window.requestAnimationFrame(flush);
    } else {
      setTimeout(flush, 16);
    }
  }

  function ensureChannelCards(channels, now) {
    const activeIds = new Set();
    for (let i = 0; i < channels.length; i++) {
      activeIds.add(channels[i].id);
    }
    renderedChannels.forEach((refs, cid) => {
      if (!activeIds.has(cid)) {
        if (refs.card && refs.card.parentNode) refs.card.parentNode.removeChild(refs.card);
        renderedChannels.delete(cid);
      }
    });

    for (let i = 0; i < channels.length; i++) {
      const ch = channels[i];
      if (renderedChannels.has(ch.id)) {
        const refs = renderedChannels.get(ch.id);
        updateMeterChannel(refs.lBar, refs.lHold, refs.lDb, refs.peakState, "l", ch.left_dbfs, now);
        updateMeterChannel(refs.rBar, refs.rHold, refs.rDb, refs.peakState, "r", ch.right_dbfs, now);
        if (refs.peakState.clipped) {
          refs.clipTag.textContent = "CLIP";
          refs.clipTag.classList.add("clipped");
        }
        refs.isMuted = !!ch.muted;
        refs.muteBtn.classList.toggle("muted", refs.isMuted);
        refs.muteBtn.textContent = refs.isMuted ? "Muted" : "Mute";

        if (activeDragKey !== "gain-" + ch.id && typeof ch.gain_db === "number") {
          refs.gainSlider.value = String(ch.gain_db);
          refs.gainVal.textContent = ch.gain_db.toFixed(1) + " dB";
        }
        if (activeDragKey !== "delay-" + ch.id && typeof ch.delay_ms === "number") {
          refs.delaySlider.value = String(Math.round(ch.delay_ms));
          refs.delayVal.textContent = Math.round(ch.delay_ms) + " ms";
        }
        continue;
      }

      const lBar = h("div", { className: "fill ch-l" });
      const lHold = h("div", { className: "peak-hold" });
      const lDb = h("span", { className: "db mono ch-l-db" }, "-96.0");
      const rBar = h("div", { className: "fill ch-r" });
      const rHold = h("div", { className: "peak-hold" });
      const rDb = h("span", { className: "db mono ch-r-db" }, "-96.0");

      const clipTag = h("span", { className: "clip-badge" }, "OK");
      const gainVal = h("span", { className: "mono" }, ch.gain_db.toFixed(1) + " dB");
      const delayVal = h("span", { className: "mono" }, Math.round(ch.delay_ms) + " ms");

      const gainSlider = h("input", {
        type: "range",
        className: "gain-slider",
        min: "-60",
        max: "12",
        step: "0.5",
        value: String(ch.gain_db),
      });
      const delaySlider = h("input", {
        type: "range",
        className: "delay-slider",
        min: "0",
        max: "500",
        step: "1",
        value: String(Math.round(ch.delay_ms)),
      });
      const muteBtn = h(
        "button",
        { type: "button", className: "mute-btn" + (ch.muted ? " muted" : "") },
        ch.muted ? "Muted" : "Mute"
      );

      const setGain = (val) => {
        const clamped = Math.max(-60, Math.min(12, val));
        gainSlider.value = String(clamped);
        gainVal.textContent = clamped.toFixed(1) + " dB";
        scheduleRpc("gain-" + ch.id, "audio.set_gain", { channel: ch.id, gain_db: clamped });
      };

      const setDelay = (val) => {
        const clamped = Math.max(0, Math.min(500, Math.round(val)));
        delaySlider.value = String(clamped);
        delayVal.textContent = clamped + " ms";
        scheduleRpc("delay-" + ch.id, "audio.set_delay", { channel: ch.id, delay_ms: clamped });
      };

      bindDragLock(gainSlider, "gain-" + ch.id);
      gainSlider.addEventListener("input", () => setGain(parseFloat(gainSlider.value)));
      gainSlider.addEventListener("dblclick", () => setGain(0));

      bindDragLock(delaySlider, "delay-" + ch.id);
      delaySlider.addEventListener("input", () => setDelay(parseFloat(delaySlider.value)));
      delaySlider.addEventListener("dblclick", () => setDelay(0));

      const presetBtn = (label, dbVal) =>
        h(
          "button",
          {
            type: "button",
            className: "preset-pill mono",
            onclick: () => setGain(dbVal),
          },
          label
        );

      const card = h(
        "div",
        { className: "channel-card" },
        h(
          "div",
          { className: "ch-head" },
          h("strong", {}, ch.name + " (#" + ch.id + ")"),
          h(
            "div",
            { className: "ch-head-actions" },
            presetBtn("-12", -12),
            presetBtn("-6", -6),
            presetBtn("0dB", 0),
            clipTag,
            muteBtn
          )
        ),
        h(
          "div",
          { className: "bar-row" },
          h("span", { className: "ch-tag" }, "L"),
          h("div", { className: "track" }, lBar, lHold),
          lDb
        ),
        h(
          "div",
          { className: "bar-row" },
          h("span", { className: "ch-tag" }, "R"),
          h("div", { className: "track" }, rBar, rHold),
          rDb
        ),
        h(
          "div",
          { className: "ch-controls" },
          h(
            "div",
            { className: "ch-ctrl-box" },
            h("div", { className: "ch-ctrl-label" }, h("span", {}, "Gain"), gainVal),
            gainSlider
          ),
          h(
            "div",
            { className: "ch-ctrl-box" },
            h("div", { className: "ch-ctrl-label" }, h("span", {}, "Delay"), delayVal),
            delaySlider
          )
        )
      );

      const stateRef = {
        card,
        lBar,
        lHold,
        lDb,
        rBar,
        rHold,
        rDb,
        clipTag,
        muteBtn,
        gainSlider,
        gainVal,
        delaySlider,
        delayVal,
        isMuted: !!ch.muted,
        peakState: { lHold: 0, lTime: now, rHold: 0, rTime: now, clipped: false },
      };

      muteBtn.addEventListener("click", () => {
        stateRef.isMuted = !stateRef.isMuted;
        muteBtn.classList.toggle("muted", stateRef.isMuted);
        muteBtn.textContent = stateRef.isMuted ? "Muted" : "Mute";
        sendRpc("audio.set_mute", { channel: ch.id, muted: stateRef.isMuted });
      });

      clipTag.addEventListener("click", () => {
        if (!stateRef.peakState.clipped) return;
        stateRef.peakState.clipped = false;
        clipTag.textContent = "OK";
        clipTag.classList.remove("clipped");
      });

      channelsContainer.appendChild(card);
      renderedChannels.set(ch.id, stateRef);
      updateMeterChannel(lBar, lHold, lDb, stateRef.peakState, "l", ch.left_dbfs, now);
      updateMeterChannel(rBar, rHold, rDb, stateRef.peakState, "r", ch.right_dbfs, now);
    }
  }

  // =========================================================================
  // 7. Interactive Direct PiP Gizmo & Overlay Sync
  // =========================================================================
  function renderPipBoxFromInputs() {
    const x = parseFloat(layerX.value) || 0;
    const y = parseFloat(layerY.value) || 0;
    const sc = parseFloat(layerScale.value) || 0.5;
    const op = parseFloat(layerOpacity.value);
    const vis = !!layerVisible.checked;

    layerXVal.textContent = x.toFixed(2);
    layerYVal.textContent = y.toFixed(2);
    layerScaleVal.textContent = sc.toFixed(2);
    layerOpacityVal.textContent = (Number.isFinite(op) ? op : 1).toFixed(2);

    // WGSL NDC coordinate space: x in [-1..1] left->right, y in [-1..1] bottom->top
    const wPct = Math.max(5, sc * 100);
    const hPct = Math.max(5, sc * 100);
    const cxPct = ((x + 1) * 0.5) * 100;
    const cyPct = ((1 - y) * 0.5) * 100;

    pipBox.style.width = wPct.toFixed(2) + "%";
    pipBox.style.height = hPct.toFixed(2) + "%";
    pipBox.style.left = (cxPct - wPct * 0.5).toFixed(2) + "%";
    pipBox.style.top = (cyPct - hPct * 0.5).toFixed(2) + "%";
    pipBox.style.opacity = vis ? String(Math.max(0.15, Number.isFinite(op) ? op : 1)) : "0.25";
    pipBox.classList.toggle("hidden-layer", !vis);
    pipTag.textContent = "Layer " + layerSelect.value + (vis ? "" : " (Hidden)");
  }

  function loadSelectedOverlayIntoInputs() {
    const id = parseInt(layerSelect.value, 10);
    const ov = overlaysStore.get(id);
    if (!ov) {
      renderPipBoxFromInputs();
      return;
    }
    if (activeDragKey !== "overlay-" + id) {
      layerX.value = String(Number(ov.x || 0).toFixed(2));
      layerY.value = String(Number(ov.y || 0).toFixed(2));
      layerScale.value = String(Number(ov.scale_x || 0.5).toFixed(2));
      layerOpacity.value = String(Number(ov.opacity != null ? ov.opacity : 1).toFixed(2));
      layerVisible.checked = ov.visible !== false;
    }
    renderPipBoxFromInputs();
  }

  function syncOverlaysFromSnapshot(overlays) {
    if (!Array.isArray(overlays)) return;
    for (let i = 0; i < overlays.length; i++) {
      const ov = overlays[i];
      overlaysStore.set(ov.layer_id, ov);
      const valStr = String(ov.layer_id);
      let opt = null;
      for (let j = 0; j < layerSelect.options.length; j++) {
        if (layerSelect.options[j].value === valStr) {
          opt = layerSelect.options[j];
          break;
        }
      }
      if (!opt) {
        opt = h("option", { value: valStr }, "Layer " + valStr);
        layerSelect.appendChild(opt);
      }
      if (ov.name) {
        const expected = "Layer " + ov.layer_id + ": " + ov.name;
        if (opt.textContent !== expected) opt.textContent = expected;
      }
    }
    loadSelectedOverlayIntoInputs();
  }

  function sendOverlayTransform() {
    const id = parseInt(layerSelect.value, 10);
    const x = parseFloat(layerX.value);
    const y = parseFloat(layerY.value);
    const sc = parseFloat(layerScale.value);
    const op = parseFloat(layerOpacity.value);
    const vis = !!layerVisible.checked;

    const prev = overlaysStore.get(id) || {};
    overlaysStore.set(id, Object.assign({}, prev, {
      layer_id: id,
      x,
      y,
      scale_x: sc,
      scale_y: sc,
      opacity: op,
      visible: vis,
    }));
    renderPipBoxFromInputs();

    scheduleRpc("overlay-" + id, "overlay.set_transform", {
      layer_id: id,
      x,
      y,
      scale_x: sc,
      scale_y: sc,
      opacity: op,
      visible: vis,
    });
  }

  const getOverlayKey = () => "overlay-" + layerSelect.value;
  [layerX, layerY, layerScale, layerOpacity, layerVisible].forEach((el) => {
    bindDragLock(el, getOverlayKey);
    el.addEventListener("input", sendOverlayTransform);
    el.addEventListener("change", sendOverlayTransform);
  });

  [
    [layerX, () => "0"],
    [layerY, () => "0"],
    [layerScale, () => (parseInt(layerSelect.value, 10) === 0 ? "1.0" : "0.5")],
    [layerOpacity, () => "1"],
  ].forEach(([el, defFn]) => {
    el.addEventListener("dblclick", () => {
      el.value = defFn();
      sendOverlayTransform();
    });
  });

  layerSelect.addEventListener("change", () => {
    activeDragKey = null;
    loadSelectedOverlayIntoInputs();
  });

  overlayResetBtn.addEventListener("click", () => {
    const id = parseInt(layerSelect.value, 10);
    layerX.value = "0";
    layerY.value = "0";
    layerScale.value = id === 0 ? "1.0" : "0.5";
    layerOpacity.value = "1";
    layerVisible.checked = true;
    sendOverlayTransform();
  });

  pipBox.addEventListener("dblclick", (e) => {
    e.preventDefault();
    if (window.getSelection) {
      window.getSelection().removeAllRanges();
    }
    overlayResetBtn.click();
  });

  // Direct Drag Move inside #pip-box
  let dragInitX = 0;
  let dragInitY = 0;
  bindDragGesture(pipBox, {
    shouldIgnore: (e) => e.target === pipResizeHandle,
    onStart: () => {
      activeDragKey = "overlay-" + layerSelect.value;
      dragInitX = parseFloat(layerX.value) || 0;
      dragInitY = parseFloat(layerY.value) || 0;
    },
    onMove: (dx, dy) => {
      const rect = pipStage.getBoundingClientRect();
      if (rect.width <= 0 || rect.height <= 0) return;
      const nx = Math.max(-1, Math.min(1, dragInitX + (dx / rect.width) * 2));
      const ny = Math.max(-1, Math.min(1, dragInitY - (dy / rect.height) * 2));
      layerX.value = nx.toFixed(2);
      layerY.value = ny.toFixed(2);
      sendOverlayTransform();
    },
    onEnd: () => {
      activeDragKey = null;
    },
  });

  // Corner Handle Resize on #pip-resize-handle
  let resizeInitScale = 0.5;
  bindDragGesture(pipResizeHandle, {
    onStart: () => {
      activeDragKey = "overlay-" + layerSelect.value;
      resizeInitScale = parseFloat(layerScale.value) || 0.5;
    },
    onMove: (dx, dy) => {
      const rect = pipStage.getBoundingClientRect();
      if (rect.width <= 0 || rect.height <= 0) return;
      const delta = dx / rect.width + dy / rect.height;
      const ns = Math.max(0.1, Math.min(1.5, resizeInitScale + delta));
      layerScale.value = ns.toFixed(2);
      sendOverlayTransform();
    },
    onEnd: () => {
      activeDragKey = null;
    },
  });

  renderPipBoxFromInputs();

  // =========================================================================
  // 8. Stream Controls, Presets Popover & Confirm Modal
  // =========================================================================
  function startStreamWithUrl(url, bitrateOpt) {
    const params = { rtmp_url: url };
    if (bitrateOpt) params.bitrate_bps = bitrateOpt;
    sendRpc("stream.start", params);
    logDiag("Requested stream.start -> " + url);
  }

  rtmpUrlEl.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && streamState.val !== "Streaming" && streamState.val !== "Reconnecting") {
      e.preventDefault();
      streamBtn.click();
    }
  });

  streamBtn.addEventListener("click", (e) => {
    const state = streamState.val;
    if (state === "Streaming" || state === "Reconnecting") {
      if (e.shiftKey) {
        sendRpc("stream.stop", {});
        logDiag("Immediate stream.stop (Shift override)");
        return;
      }
      openSafeModal({
        title: "Stop Live Stream?",
        description: "現在の RTMP 配信接続を終了します。プレビューは引き続き動作します。",
        confirmLabel: "Stop Stream",
        cancelLabel: "Continue Streaming",
        danger: true,
        onConfirm: () => {
          sendRpc("stream.stop", {});
          logDiag("Confirmed stream.stop");
        },
      });
    } else {
      const targetUrl = rtmpUrlEl.value.trim();
      if (!targetUrl) {
        openSafeModal({
          title: "RTMP URL Required",
          description: "配信先の rtmp:// または rtmps:// URL を入力するか、Presets から選択してください。",
          confirmLabel: "OK",
          cancelLabel: "Close",
        });
        return;
      }
      startStreamWithUrl(targetUrl, selectedBitrateBps);
    }
  });

  presetsBtn.addEventListener("click", () => {
    if (activePopoverClose) {
      activePopoverClose();
      return;
    }
    let closePop = null;
    const curUrl = rtmpUrlEl.value.trim();
    const makeUrlPreset = (label, sub, url) =>
      h(
        "button",
        {
          type: "button",
          className: "pop-item" + (curUrl === url ? " active" : ""),
          onclick: () => {
            rtmpUrlEl.value = url;
            logDiag("Selected URL preset: " + label);
            if (closePop) closePop();
          },
        },
        h("span", {}, label),
        h("span", { className: "pop-sub mono" }, sub)
      );

    const makeBrPreset = (label, bps) =>
      h(
        "button",
        {
          type: "button",
          className: "pop-item" + (selectedBitrateBps === bps ? " active" : ""),
          onclick: () => {
            selectedBitrateBps = bps;
            presetsBtn.textContent = "Presets · " + (bps / 1000000).toFixed(1) + "M";
            logDiag("Selected target bitrate: " + Math.round(bps / 1000) + " kbps");
            if (closePop) closePop();
          },
        },
        h("span", {}, label),
        h("span", { className: "pop-sub mono" }, Math.round(bps / 1000) + " kbps")
      );

    const pop = h(
      "div",
      { className: "portal-popover" },
      h("div", { className: "pop-section-title" }, "RTMP ENDPOINT PRESETS"),
      makeUrlPreset("Local SRS", "127.0.0.1:1935", "rtmp://127.0.0.1:1935/live/livestream"),
      makeUrlPreset("YouTube RTMPS", "a.rtmps.youtube.com", "rtmps://a.rtmps.youtube.com/live2/"),
      makeUrlPreset("Twitch Ingest", "live.twitch.tv", "rtmp://live.twitch.tv/app/"),
      h("div", { className: "pop-section-title" }, "TARGET VIDEO BITRATE"),
      makeBrPreset("1080p60 High", 6000000),
      makeBrPreset("1080p30 / 720p60 Balanced", 4000000),
      makeBrPreset("Mobile / Constrained", 2500000)
    );
    closePop = placeSafePopover(presetsBtn, pop);
  });

  refreshBtn.addEventListener("click", () => {
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      connectWs();
      logDiag("Manual WebSocket reconnect triggered");
    } else {
      sendRpc("engine.get_status", {});
      logDiag("Manual status sync requested");
    }
  });

  // =========================================================================
  // 9. Diagnostics Reactive View
  // =========================================================================
  function renderDiagnosticsKv(k, valFn) {
    return h(
      "div",
      { className: "diag-kv" },
      h("span", { className: "diag-k" }, k),
      h("span", { className: "diag-v mono" }, valFn)
    );
  }

  diagStatsEl.appendChild(renderDiagnosticsKv("WebSocket", () => connState.val));
  diagStatsEl.appendChild(renderDiagnosticsKv("Pipeline", () => streamState.val));
  diagStatsEl.appendChild(renderDiagnosticsKv("WS Reconnects", () => String(wsReconnects.val)));
  diagStatsEl.appendChild(renderDiagnosticsKv("RPC Latency (RTT)", () => String(lastRpcRttMs.val) + " ms"));
  diagStatsEl.appendChild(renderDiagnosticsKv("MSE Catch-up Buffer", () => String(mseBufferedMs.val) + " ms"));
  diagStatsEl.appendChild(renderDiagnosticsKv("MSE Queue Drops", () => String(mseDroppedSegments.val)));

  // =========================================================================
  // 10. Snapshot Sync & Resilient WebSocket Transport
  // =========================================================================
  function applySnapshot(snap) {
    if (!snap) return;
    const now = performance.now ? performance.now() : Date.now();
    if (snap.stream) {
      const nextState = snap.stream.state || "Idle";
      streamState.val = nextState;
      stateBadge.textContent = nextState;
      const streaming = nextState === "Streaming";
      const reconnecting = nextState === "Reconnecting";
      const activeStream = streaming || reconnecting;
      stateBadge.classList.toggle("streaming", streaming);
      stateBadge.classList.toggle("reconnecting", reconnecting);
      previewLivePill.classList.toggle("live", streaming);
      previewLivePill.textContent = streaming ? "LIVE" : reconnecting ? "RETRY" : "PREVIEW";
      streamBtn.textContent = activeStream ? "Stop Stream" : "Start Stream";
      streamBtn.classList.toggle("live", activeStream);

      if (!rtmpUrlEl.value && snap.stream.rtmp_url) {
        rtmpUrlEl.value = snap.stream.rtmp_url;
      }
      fpsReadout.textContent = (snap.stream.fps || 0).toFixed(1);
      bitrateReadout.textContent = Math.round((snap.stream.bitrate_bps || 0) / 1000) + " kbps";
    }
    if (snap.preview_mime && snap.preview_mime !== activeMime) {
      initMse(snap.preview_mime);
    }
    if (snap.master) {
      updateMeterChannel(
        masterLBar,
        masterLHold,
        masterLDb,
        masterPeak,
        "l",
        snap.master.left_dbfs,
        now
      );
      updateMeterChannel(
        masterRBar,
        masterRHold,
        masterRDb,
        masterPeak,
        "r",
        snap.master.right_dbfs,
        now
      );
      if (masterPeak.clipped) {
        masterClipBadge.textContent = "CLIP!";
        masterClipBadge.classList.add("clipped");
      }
    }
    if (Array.isArray(snap.channels)) {
      ensureChannelCards(snap.channels, now);
    }
    if (Array.isArray(snap.overlays)) {
      syncOverlaysFromSnapshot(snap.overlays);
    }
  }

  function connectWs() {
    if (reconnectTimer) {
      clearTimeout(reconnectTimer);
      reconnectTimer = null;
    }
    if (ws && (ws.readyState === WebSocket.CONNECTING || ws.readyState === WebSocket.OPEN)) {
      return;
    }
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    ws = new WebSocket(proto + "//" + location.host + "/ws");
    ws.binaryType = "arraybuffer";

    ws.onopen = () => {
      connState.val = "Connected";
      wsDot.classList.add("connected");
      wsDot.title = "WebSocket: Connected";
      logDiag("WebSocket connected");
      initMse(activeMime);
      sendRpc("engine.get_status", {});
    };

    ws.onmessage = (ev) => {
      if (typeof ev.data === "string") {
        try {
          const msg = JSON.parse(ev.data);
          if (msg.id != null && rpcSentAt.has(msg.id)) {
            const t0 = rpcSentAt.get(msg.id);
            rpcSentAt.delete(msg.id);
            const now = performance.now ? performance.now() : Date.now();
            lastRpcRttMs.val = (now - t0).toFixed(1);
          }
          if (msg.error) {
            logDiag("RPC error (" + msg.error.code + "): " + msg.error.message);
          } else if (msg.method === "meter.update") {
            applySnapshot(msg.params);
          } else if (msg.method === "preview.codec" && msg.params && msg.params.mime) {
            initMse(msg.params.mime);
            logDiag("MSE codec negotiated: " + msg.params.mime);
          } else if (msg.result && msg.result.stream) {
            applySnapshot(msg.result);
          }
        } catch (_) {}
      } else if (ev.data instanceof ArrayBuffer) {
        segmentQueue.push(new Uint8Array(ev.data));
        if (segmentQueue.length > 30) {
          segmentQueue.shift();
          mseDroppedSegments.val += 1;
        }
        pumpSegmentQueue();
      }
    };

    ws.onclose = () => {
      connState.val = "Reconnecting...";
      wsDot.classList.remove("connected");
      wsDot.title = "WebSocket: Reconnecting...";
      wsReconnects.val += 1;
      logDiag("WebSocket closed, retrying in 1s");
      reconnectTimer = setTimeout(connectWs, 1000);
    };
  }

  logDiag("ikakim zero-dep Studio UI initialized");
  connectWs();
})();
