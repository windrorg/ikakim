(() => {
  "use strict";

  const wsDot = document.getElementById("ws-dot");
  const stateBadge = document.getElementById("state-badge");
  const fpsReadout = document.getElementById("fps-readout");
  const bitrateReadout = document.getElementById("bitrate-readout");
  const latencyReadout = document.getElementById("latency-readout");
  const codecLabel = document.getElementById("codec-label");
  const videoEl = document.getElementById("preview-video");
  const rtmpUrlEl = document.getElementById("rtmp-url");
  const streamBtn = document.getElementById("btn-stream-toggle");
  const masterLBar = document.getElementById("master-l-bar");
  const masterRBar = document.getElementById("master-r-bar");
  const masterLDb = document.getElementById("master-l-db");
  const masterRDb = document.getElementById("master-r-db");
  const channelsContainer = document.getElementById("channels-container");

  const layerSelect = document.getElementById("layer-select");
  const layerVisible = document.getElementById("layer-visible");
  const layerX = document.getElementById("layer-x");
  const layerY = document.getElementById("layer-y");
  const layerScale = document.getElementById("layer-scale");
  const layerOpacity = document.getElementById("layer-opacity");

  let ws = null;
  let rpcNextId = 1;
  let currentState = "Idle";
  let renderedChannels = new Map();

  // MSE 低遅延プレビューパイプライン
  const MAX_LATENCY_SEC = 0.30; // 300ms 超過で自動キャッチアップシーク
  const TARGET_BUFFER_SEC = 0.05;
  let mediaSource = null;
  let sourceBuffer = null;
  let activeMime = 'video/mp4; codecs="avc1.42E01F"';
  const segmentQueue = [];

  function initMse(mime) {
    if (mime) {
      activeMime = mime;
    }
    codecLabel.textContent = activeMime;
    if (!("MediaSource" in window) || !MediaSource.isTypeSupported(activeMime)) {
      return;
    }
    segmentQueue.length = 0;
    mediaSource = new MediaSource();
    sourceBuffer = null;
    videoEl.src = URL.createObjectURL(mediaSource);
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
        console.warn("MSE SourceBuffer init failed:", err);
      }
    });
  }

  function pumpSegmentQueue() {
    if (!sourceBuffer || sourceBuffer.updating || segmentQueue.length === 0) {
      return;
    }
    const nextBuf = segmentQueue.shift();
    try {
      sourceBuffer.appendBuffer(nextBuf);
    } catch (err) {
      console.warn("appendBuffer error:", err);
    }
  }

  function trimOldBuffers() {
    if (!sourceBuffer || sourceBuffer.updating || videoEl.buffered.length === 0) {
      return;
    }
    const start = videoEl.buffered.start(0);
    const keepFrom = videoEl.currentTime - 2.0;
    if (keepFrom > start + 0.5) {
      try {
        sourceBuffer.remove(start, keepFrom);
      } catch (_) {}
    }
  }

  function catchUpLatency() {
    if (videoEl.buffered.length === 0) {
      return;
    }
    const end = videoEl.buffered.end(videoEl.buffered.length - 1);
    const latency = Math.max(0, end - videoEl.currentTime);
    latencyReadout.textContent = `${Math.round(latency * 1000)} ms`;
    if (latency > MAX_LATENCY_SEC) {
      videoEl.currentTime = Math.max(0, end - TARGET_BUFFER_SEC);
    }
    if (videoEl.paused) {
      videoEl.play().catch(() => {});
    }
  }

  setInterval(catchUpLatency, 100);

  function dbfsToPercent(db) {
    const clamped = Math.max(-60, Math.min(0, db));
    return ((clamped + 60) / 60) * 100;
  }

  function sendRpc(method, params) {
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      return;
    }
    const payload = {
      jsonrpc: "2.0",
      id: rpcNextId++,
      method,
      params: params || {},
    };
    ws.send(JSON.stringify(payload));
  }

  function ensureChannelCards(channels) {
    for (const ch of channels) {
      if (renderedChannels.has(ch.id)) {
        const refs = renderedChannels.get(ch.id);
        refs.lBar.style.width = `${dbfsToPercent(ch.left_dbfs)}%`;
        refs.rBar.style.width = `${dbfsToPercent(ch.right_dbfs)}%`;
        refs.lDb.textContent = ch.left_dbfs.toFixed(1);
        refs.rDb.textContent = ch.right_dbfs.toFixed(1);
        refs.muteBtn.classList.toggle("muted", !!ch.muted);
        refs.muteBtn.textContent = ch.muted ? "Muted" : "Mute";
        continue;
      }

      const card = document.createElement("div");
      card.className = "channel-card";
      card.innerHTML = `
        <div class="ch-head">
          <strong>${ch.name} (#${ch.id})</strong>
          <button class="mute-btn">Mute</button>
        </div>
        <div class="bar-row"><span>L</span><div class="track"><div class="fill ch-l"></div></div><span class="db ch-l-db">-96.0</span></div>
        <div class="bar-row"><span>R</span><div class="track"><div class="fill ch-r"></div></div><span class="db ch-r-db">-96.0</span></div>
        <div class="ch-controls">
          <label>Gain (<span class="gain-val">${ch.gain_db.toFixed(1)}</span> dB)
            <input type="range" class="gain-slider" min="-60" max="12" step="0.5" value="${ch.gain_db}" />
          </label>
          <label>Delay (<span class="delay-val">${Math.round(ch.delay_ms)}</span> ms)
            <input type="range" class="delay-slider" min="0" max="500" step="1" value="${ch.delay_ms}" />
          </label>
        </div>
      `;

      const muteBtn = card.querySelector(".mute-btn");
      const gainSlider = card.querySelector(".gain-slider");
      const delaySlider = card.querySelector(".delay-slider");
      const gainVal = card.querySelector(".gain-val");
      const delayVal = card.querySelector(".delay-val");
      let isMuted = !!ch.muted;

      muteBtn.addEventListener("click", () => {
        isMuted = !isMuted;
        sendRpc("audio.set_mute", { channel: ch.id, muted: isMuted });
      });
      gainSlider.addEventListener("input", () => {
        const val = parseFloat(gainSlider.value);
        gainVal.textContent = val.toFixed(1);
        sendRpc("audio.set_gain", { channel: ch.id, gain_db: val });
      });
      delaySlider.addEventListener("input", () => {
        const val = parseFloat(delaySlider.value);
        delayVal.textContent = `${Math.round(val)}`;
        sendRpc("audio.set_delay", { channel: ch.id, delay_ms: val });
      });

      channelsContainer.appendChild(card);
      renderedChannels.set(ch.id, {
        lBar: card.querySelector(".ch-l"),
        rBar: card.querySelector(".ch-r"),
        lDb: card.querySelector(".ch-l-db"),
        rDb: card.querySelector(".ch-r-db"),
        muteBtn,
      });
    }
  }

  function applySnapshot(snap) {
    if (!snap) return;
    if (snap.stream) {
      currentState = snap.stream.state || "Idle";
      stateBadge.textContent = currentState;
      const streaming = currentState === "Streaming";
      streamBtn.textContent = streaming ? "Stop Stream" : "Start Stream";
      streamBtn.classList.toggle("live", streaming);
      if (!rtmpUrlEl.value && snap.stream.rtmp_url) {
        rtmpUrlEl.value = snap.stream.rtmp_url;
      }
      fpsReadout.textContent = (snap.stream.fps || 0).toFixed(1);
      bitrateReadout.textContent = `${Math.round((snap.stream.bitrate_bps || 0) / 1000)} kbps`;
    }
    if (snap.preview_mime && snap.preview_mime !== activeMime) {
      initMse(snap.preview_mime);
    }
    if (snap.master) {
      masterLBar.style.width = `${dbfsToPercent(snap.master.left_dbfs)}%`;
      masterRBar.style.width = `${dbfsToPercent(snap.master.right_dbfs)}%`;
      masterLDb.textContent = snap.master.left_dbfs.toFixed(1);
      masterRDb.textContent = snap.master.right_dbfs.toFixed(1);
    }
    if (Array.isArray(snap.channels)) {
      ensureChannelCards(snap.channels);
    }
  }

  streamBtn.addEventListener("click", () => {
    if (currentState === "Streaming") {
      sendRpc("stream.stop", {});
    } else {
      sendRpc("stream.start", { rtmp_url: rtmpUrlEl.value.trim() });
    }
  });

  function sendOverlayTransform() {
    const id = parseInt(layerSelect.value, 10);
    const x = parseFloat(layerX.value);
    const y = parseFloat(layerY.value);
    const sc = parseFloat(layerScale.value);
    const op = parseFloat(layerOpacity.value);
    document.getElementById("layer-x-val").textContent = x.toFixed(2);
    document.getElementById("layer-y-val").textContent = y.toFixed(2);
    document.getElementById("layer-scale-val").textContent = sc.toFixed(2);
    document.getElementById("layer-opacity-val").textContent = op.toFixed(2);

    sendRpc("overlay.set_transform", {
      layer_id: id,
      x,
      y,
      scale_x: sc,
      scale_y: sc,
      opacity: op,
      visible: layerVisible.checked,
    });
  }

  [layerX, layerY, layerScale, layerOpacity, layerVisible].forEach((el) => {
    el.addEventListener("input", sendOverlayTransform);
    el.addEventListener("change", sendOverlayTransform);
  });

  function connectWs() {
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    ws = new WebSocket(`${proto}//${location.host}/ws`);
    ws.binaryType = "arraybuffer";

    ws.onopen = () => {
      wsDot.classList.add("connected");
      initMse(activeMime);
      sendRpc("engine.get_status", {});
    };

    ws.onmessage = (ev) => {
      if (typeof ev.data === "string") {
        try {
          const msg = JSON.parse(ev.data);
          if (msg.method === "meter.update") {
            applySnapshot(msg.params);
          } else if (msg.method === "preview.codec" && msg.params && msg.params.mime) {
            initMse(msg.params.mime);
          } else if (msg.result && msg.result.stream) {
            applySnapshot(msg.result);
          }
        } catch (_) {}
      } else if (ev.data instanceof ArrayBuffer) {
        segmentQueue.push(new Uint8Array(ev.data));
        if (segmentQueue.length > 30) {
          segmentQueue.shift();
        }
        pumpSegmentQueue();
      }
    };

    ws.onclose = () => {
      wsDot.classList.remove("connected");
      setTimeout(connectWs, 1000);
    };
  }

  connectWs();
})();
