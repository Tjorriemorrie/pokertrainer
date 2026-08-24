(() => {
  "use strict";

  const table = document.getElementById("table");
  const feedback = document.getElementById("feedback");
  const statusEl = document.getElementById("ws-status");
  const mctsEl = document.getElementById("mcts-status");
  const soundBtn = document.getElementById("sound-toggle");
  const chartData = [];
  let ws = null;
  let retryTimer = null;

  /* ------------------------------------------------------------- audio */

  let actx = null;
  let noiseCache = null;
  let muted = localStorage.getItem("pt-muted") === "1";

  function ensureAudio() {
    if (!actx) {
      const AC = window.AudioContext || window.webkitAudioContext;
      if (!AC) return null;
      actx = new AC();
      noiseCache = null;
    }
    if (actx.state === "suspended") actx.resume();
    return actx;
  }

  function noiseBuffer(ctx) {
    if (!noiseCache) {
      const length = Math.floor(ctx.sampleRate * 0.4);
      noiseCache = ctx.createBuffer(1, length, ctx.sampleRate);
      const data = noiseCache.getChannelData(0);
      for (let i = 0; i < length; i += 1) data[i] = Math.random() * 2 - 1;
    }
    return noiseCache;
  }

  function tone(ctx, at, freq, dur, type, gain, slideTo) {
    const osc = ctx.createOscillator();
    const env = ctx.createGain();
    osc.type = type;
    osc.frequency.setValueAtTime(freq, at);
    if (slideTo) osc.frequency.exponentialRampToValueAtTime(slideTo, at + dur);
    env.gain.setValueAtTime(0.0001, at);
    env.gain.exponentialRampToValueAtTime(gain, at + 0.008);
    env.gain.exponentialRampToValueAtTime(0.0001, at + dur);
    osc.connect(env).connect(ctx.destination);
    osc.start(at);
    osc.stop(at + dur + 0.02);
  }

  function noise(ctx, at, dur, gain, filterType, freqFrom, freqTo) {
    const src = ctx.createBufferSource();
    src.buffer = noiseBuffer(ctx);
    const env = ctx.createGain();
    env.gain.setValueAtTime(gain, at);
    env.gain.exponentialRampToValueAtTime(0.0001, at + dur);
    let node = src;
    if (filterType) {
      const filter = ctx.createBiquadFilter();
      filter.type = filterType;
      filter.frequency.setValueAtTime(freqFrom, at);
      filter.frequency.exponentialRampToValueAtTime(freqTo, at + dur);
      filter.Q.value = 1.1;
      node.connect(filter);
      node = filter;
    }
    node.connect(env).connect(ctx.destination);
    src.start(at);
    src.stop(at + dur + 0.02);
  }

  function playChip(at) {
    const ctx = ensureAudio();
    if (!ctx) return;
    tone(ctx, at, 1500, 0.045, "square", 0.16, 620);
    noise(ctx, at, 0.028, 0.2, "bandpass", 3400, 2400);
    noise(ctx, at + 0.055, 0.03, 0.14, "bandpass", 2200, 1500);
  }

  function playDeal(at) {
    const ctx = ensureAudio();
    if (!ctx) return;
    noise(ctx, at, 0.05, 0.24, "bandpass", 2800, 1900);
    noise(ctx, at + 0.09, 0.05, 0.24, "bandpass", 2600, 1700);
    noise(ctx, at + 0.2, 0.04, 0.12, "bandpass", 2200, 1400);
  }

  function playFold(at) {
    const ctx = ensureAudio();
    if (!ctx) return;
    noise(ctx, at, 0.16, 0.2, "lowpass", 1300, 320);
  }

  function playWin(at) {
    const ctx = ensureAudio();
    if (!ctx) return;
    [523.25, 659.25, 783.99].forEach((freq, i) => {
      tone(ctx, at + i * 0.11, freq, 0.16, "triangle", 0.18);
    });
  }

  function playTag(tag, at) {
    switch (tag) {
      case "deal":
        playDeal(at);
        break;
      case "chip":
        playChip(at);
        break;
      case "fold":
        playFold(at);
        break;
      case "win":
        playWin(at);
        break;
      default:
        break;
    }
  }

  function playSounds(tags) {
    if (muted || !tags || tags.length === 0) return;
    const ctx = ensureAudio();
    if (!ctx) return;
    const now = ctx.currentTime + 0.02;
    let offset = 0;
    tags.forEach((tag) => {
      playTag(tag, now + offset);
      offset += tag === "deal" ? 0.24 : 0.12;
    });
  }

  function refreshSoundButton() {
    if (!soundBtn) return;
    soundBtn.textContent = muted ? "🔇" : "🔊";
    soundBtn.classList.toggle("muted", muted);
  }

  if (soundBtn) {
    refreshSoundButton();
    soundBtn.addEventListener("click", () => {
      muted = !muted;
      localStorage.setItem("pt-muted", muted ? "1" : "0");
      refreshSoundButton();
      if (!muted) ensureAudio();
    });
  }

  document.addEventListener("pointerdown", () => {
    if (!muted) ensureAudio();
  });

  /* ------------------------------------------------------- alt bb reveal */

  window.addEventListener("keydown", (event) => {
    if (event.key === "Alt") document.body.classList.add("alt-held");
  });
  window.addEventListener("keyup", (event) => {
    if (event.key === "Alt") document.body.classList.remove("alt-held");
  });
  window.addEventListener("blur", () => {
    document.body.classList.remove("alt-held");
  });

  /* ------------------------------------------------------------ ws / dom */

  const swap = (el, html) => {
    el.innerHTML = html;
  };

function setStatus(text, cls) {
    statusEl.textContent = text;
    statusEl.className = cls;
  }

  function setSolverStatus(msg) {
    if (!mctsEl) return;
    if (msg.phase === "READY") {
      mctsEl.textContent = `search d${msg.tree_depth}/${msg.max_depth} · done`;
      mctsEl.className = "mcts-status status-ok";
      return;
    }
    const target = msg.target_iterations || 1;
    const pct = Math.round((100 * msg.iterations_done) / target);
    const cls = pct >= 66 ? "status-ok" : pct >= 33 ? "status-wait" : "status-bad";
    mctsEl.textContent = `search d${msg.tree_depth}/${msg.max_depth} · ${pct}%`;
    mctsEl.className = `mcts-status ${cls}`;
  }

  function resetSolverStatus() {
    if (!mctsEl) return;
    mctsEl.textContent = "solver idle";
    mctsEl.className = "mcts-status status-bad";
  }

  function sendAction(kind, extra) {
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    const action = Object.assign({ kind: kind }, extra || {});
    ws.send(JSON.stringify({ type: "ACTION_SUBMIT", action: action }));
  }

  /* ------------------------------------------------------- action dock */

  function bindDock() {
    const slider = document.getElementById("custom-amount");
    const number = document.getElementById("custom-amount-num");
    const raiseBtn = document.getElementById("raise-btn");

    const syncNumber = () => {
      if (slider && number) number.value = slider.value;
    };
    const syncLabel = () => {
      if (!slider || !raiseBtn) return;
      const value = Number(slider.value);
      const max = Number(slider.max);
      if (value >= max) {
        raiseBtn.textContent = "All-in";
      } else if (raiseBtn.dataset.kind === "bet") {
        raiseBtn.textContent = `Bet ${value}`;
      } else {
        raiseBtn.textContent = `Raise to ${value}`;
      }
    };
    const setValue = (value) => {
      if (!slider) return;
      const clamped = Math.min(Number(slider.max), Math.max(Number(slider.min), value));
      slider.value = clamped;
      number.value = clamped;
      syncLabel();
    };

    if (slider) {
      syncNumber();
      syncLabel();
      slider.addEventListener("input", () => {
        syncNumber();
        syncLabel();
      });
      slider.addEventListener("wheel", (event) => {
        event.preventDefault();
        const step = event.shiftKey ? 25 : Number(slider.step) || 5;
        setValue(Number(slider.value) + (event.deltaY < 0 ? step : -step));
      }, { passive: false });
    }

    if (number) {
      number.addEventListener("change", () => setValue(Number(number.value)));
      number.addEventListener("wheel", (event) => {
        event.preventDefault();
        const step = event.shiftKey ? 25 : Number(slider.step) || 5;
        setValue(Number(number.value) + (event.deltaY < 0 ? step : -step));
      }, { passive: false });
    }

    document.querySelectorAll("#table [data-bucket]").forEach((el) => {
      el.addEventListener("click", () => {
        setValue(Number(el.dataset.size));
      });
    });

    document.querySelectorAll("#table [data-step]").forEach((el) => {
      el.addEventListener("click", () => {
        if (!slider) return;
        const step = Number(el.dataset.step) * (Number(slider.step) || 5);
        setValue(Number(slider.value) + step);
      });
    });

    document.querySelectorAll("#table [data-kind]").forEach((el) => {
      el.addEventListener("click", () => {
        const kind = el.dataset.kind;
        if (kind === "bet" || kind === "raise") {
          if (!slider) return;
          const value = Number(slider.value);
          if (value >= Number(slider.max)) {
            sendAction("all_in");
          } else {
            sendAction(kind, { amount: value });
          }
          return;
        }
        sendAction(kind);
      });
    });
  }

  function bindFeedback() {
    document.querySelectorAll("#feedback [data-overlay-close]").forEach((el) => {
      el.addEventListener("click", () => {
        if (feedback) feedback.innerHTML = "";
      });
    });
    document.querySelectorAll("#feedback [data-overlay-confirm]").forEach((el) => {
      el.addEventListener("click", () => {
        if (!ws || ws.readyState !== WebSocket.OPEN) return;
        ws.send(JSON.stringify({ type: "REVIEW_DONE" }));
        if (feedback) feedback.innerHTML = "";
      });
    });
  }

  /* ------------------------------------------------------------ chart */

  const finishButton = document.getElementById("finish-table");
  if (finishButton) {
    finishButton.addEventListener("click", () => {
      if (ws && ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ type: "FINISH_TABLE" }));
      }
    });
  }

  function drawChart() {
    const canvas = document.getElementById("ev-chart");
    if (!canvas) return;
    const ctx = canvas.getContext("2d");
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    const values = chartData.slice(-1000);
    if (values.length < 2) {
      if (values.length === 1) {
        ctx.fillStyle = "#cf9f5d";
        ctx.fillRect(0, canvas.height - 2, 3, 3);
      }
      return;
    }
    const max = Math.max(...values, 1);
    const step = canvas.width / 999;
    ctx.beginPath();
    values.forEach((value, i) => {
      const x = i * step;
      const y = canvas.height - (value / max) * (canvas.height - 4) - 2;
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    });
    ctx.strokeStyle = "#cf9f5d";
    ctx.lineWidth = 2;
    ctx.stroke();
  }

  /* --------------------------------------------------------- messages */

  function handleMessage(raw) {
    let msg;
    try {
      msg = JSON.parse(raw.data);
    } catch {
      return;
    }
    switch (msg.type) {
      case "TABLE_STATE_UPDATE":
        swap(table, msg.fragment);
        bindDock();
        const shell = document.getElementById("table-state");
        if (shell && shell.dataset.sounds) {
          try {
            playSounds(JSON.parse(shell.dataset.sounds));
          } catch {
            /* ignore malformed sound cues */
          }
        }
        const logLines = document.getElementById("pt-hlog-lines");
        if (logLines) logLines.scrollTop = logLines.scrollHeight;
        break;
      case "TRIGGER_TACTICAL_OVERLAY":
        swap(feedback, msg.fragment);
        bindFeedback();
        break;
      case "CHART_TICK":
        chartData.push(msg.ev_loss);
        drawChart();
        break;
case "CHART_SNAPSHOT":
        chartData.length = 0;
        (msg.points || []).forEach((point) => chartData.push(point[1]));
        drawChart();
        break;
      case "SEARCH_STATUS":
        setSolverStatus(msg);
        break;
      case "SESSION_FINISHED":
        window.location.href = msg.url;
        break;
      case "ERROR":
        console.warn("server:", msg.message);
        break;
      default:
        console.warn("unknown message type:", msg.type);
    }
  }

  function connect() {
    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    ws = new WebSocket(`${proto}//${location.host}/ws`);
    setStatus("connecting…", "status-wait");
    ws.onopen = () => setStatus("connected", "status-ok");
    ws.onmessage = handleMessage;
ws.onclose = () => {
      setStatus("disconnected — retrying…", "status-bad");
      resetSolverStatus();
      clearTimeout(retryTimer);
      retryTimer = setTimeout(connect, 1500);
    };
    ws.onerror = () => ws.close();
  }

  connect();
})();