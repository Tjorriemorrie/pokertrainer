(() => {
  "use strict";

  const table = document.getElementById("table");
  const startingHands = document.getElementById("starting-hands");
  const feedback = document.getElementById("feedback");
  const statusEl = document.getElementById("ws-status");
  const soundBtn = document.getElementById("sound-toggle");
  const chartData = [];
  let ws = null;
  let retryTimer = null;
  let finished = false;
  let currentDecision = null;

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

  // Keeps the hero's starting-hand grid pointing at whatever is currently
  // dealt, without re-rendering the (static, once-per-connection) grid
  // itself — see data-hero-hand on #table-state and data-hand on each
  // .pt-range-cell in starting_hands_panel.html.
  function highlightHeroHand(label) {
    const grid = document.getElementById("hero-range-table");
    if (!grid) return;
    grid.querySelectorAll(".pt-range-cell.pt-hero-current").forEach((cell) => {
      cell.classList.remove("pt-hero-current");
    });
    if (!label) return;
    const cell = grid.querySelector(`.pt-range-cell[data-hand="${label}"]`);
    if (cell) cell.classList.add("pt-hero-current");
  }

  /* ------------------------------------------------------ card dealing */

  function cardSnapshot() {
    const seen = new Map();
    document.querySelectorAll("#table .pt-seat-cards, #table .pt-board").forEach((container) => {
      const seat = container.closest("[data-seat]");
      const scope = seat ? `seat:${seat.dataset.seat}` : "board";
      container.querySelectorAll(":scope > .pt-card").forEach((card, i) => {
        const code = card.dataset.code || (card.classList.contains("back") ? "back" : "");
        seen.set(`${scope}:${i}:${code}`, true);
      });
    });
    return seen;
  }

  function freshCardGroups(before) {
    const board = [];
    const seats = [];
    document.querySelectorAll("#table .pt-seat-cards, #table .pt-board").forEach((container) => {
      const seat = container.closest("[data-seat]");
      const scope = seat ? `seat:${seat.dataset.seat}` : "board";
      const bucket = seat ? seats : board;
      container.querySelectorAll(":scope > .pt-card").forEach((card, i) => {
        const code = card.dataset.code || (card.classList.contains("back") ? "back" : "");
        if (!before.has(`${scope}:${i}:${code}`)) bucket.push(card);
      });
    });
    return { board, seats };
  }

  function revealCards(cards, onDone) {
    if (cards.length === 0) {
      if (onDone) onDone();
      return;
    }
    cards.forEach((card) => card.classList.add("pt-card-hidden"));
    cards.forEach((card, i) => {
      setTimeout(() => {
        card.classList.remove("pt-card-hidden");
        card.classList.add("pt-card-dealt");
      }, i * 300);
    });
    if (onDone) setTimeout(onDone, (cards.length - 1) * 300 + 260);
  }

  // Showdown gets its own pacing: hole cards are already rendered face-up by
  // the fragment (no re-dealing them), so the only thing worth animating is
  // any board street the hand skipped past (e.g. a preflop all-in running
  // the board out at once) — then the winner ribbon lights up.
  function animateShowdown(before) {
    const { board } = freshCardGroups(before);
    const winners = document.querySelectorAll("#table .pt-seat.pt-winner");
    winners.forEach((seat) => seat.classList.add("pt-win-pending"));
    revealCards(board, () => {
      setTimeout(() => {
        winners.forEach((seat) => seat.classList.remove("pt-win-pending"));
      }, 200);
    });
  }

function setStatus(text, cls) {
    statusEl.textContent = text;
    statusEl.className = cls;
  }

  function formatK(n) {
    if (n >= 1_000_000) return (n / 1_000_000).toFixed(1).replace(/\.0$/, "") + "M";
    if (n >= 1_000) return (n / 1_000).toFixed(1).replace(/\.0$/, "") + "k";
    return String(n);
  }

  function setSolverStatus(msg) {
    const mctsEl = document.getElementById("mcts-status");
    if (!mctsEl) return;
    const depth = `d${msg.tree_depth}/${msg.max_depth}`;
    const text = `${depth} · ${formatK(msg.iterations_done)}`;
    let cls;
    if (msg.phase === "READY") {
      cls = "status-ok";
    } else if (msg.phase === "DEPTH_REACHED") {
      cls = "status-wait";
    } else {
      cls = "status-bad";
    }
    mctsEl.textContent = text;
    mctsEl.className = `mcts-status ${cls}`;
  }

  function resetSolverStatus() {
    currentDecision = null;
    setDockLocked(true);
    const mctsEl = document.getElementById("mcts-status");
    if (!mctsEl) return;
    mctsEl.textContent = "solver idle";
    mctsEl.className = "mcts-status status-bad";
  }

  function setDockLocked(locked) {
    const dock = document.getElementById("action-panel");
    if (dock) dock.classList.toggle("locked", locked);
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
      const label = raiseBtn.dataset.kind === "bet" ? "Bet" : "Raise to";
      raiseBtn.innerHTML = value >= max
        ? `All-in<span class="amt">${value}</span>`
        : `${label}<span class="amt">${value}</span>`;
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

  function showTournamentModal(won, url) {
    const modal = document.getElementById("tournament-modal");
    const title = document.getElementById("tournament-modal-title");
    const body = document.getElementById("tournament-modal-body");
    const continueBtn = document.getElementById("tournament-modal-continue");
    if (!modal || !title || !body || !continueBtn) return;
    modal.classList.toggle("win", won);
    modal.classList.toggle("loss", !won);
    title.textContent = won ? "You won the tournament!" : "You lost";
    body.textContent = won
      ? "You took down the Spin & Gold — nice run."
      : "Better luck next time — review your hands to see where it slipped.";
    continueBtn.onclick = () => { window.location.href = url; };
    modal.hidden = false;
  }
  function handleMessage(raw) {
    let msg;
    try {
      msg = JSON.parse(raw.data);
    } catch {
      return;
    }
    switch (msg.type) {
      case "TABLE_STATE_UPDATE": {
        const before = cardSnapshot();
        swap(table, msg.fragment);
        bindDock();
        const { board, seats } = freshCardGroups(before);
        const isShowdown =
          seats.length > 0 && document.querySelectorAll("#table .pt-seat.pt-winner").length > 0;
        if (isShowdown) {
          animateShowdown(before);
        } else {
          revealCards(board.concat(seats));
        }
        const block = document.querySelector("#table-state .pt-action-block");
        currentDecision = block ? (block.dataset.decision || null) : null;
        setDockLocked(true);
        const shell = document.getElementById("table-state");
        if (shell && shell.dataset.sounds) {
          try {
            playSounds(JSON.parse(shell.dataset.sounds));
          } catch {
            /* ignore malformed sound cues */
          }
        }
        if (shell) highlightHeroHand(shell.dataset.heroHand);
        const logLines = document.getElementById("pt-hlog-lines");
        if (logLines) logLines.scrollTop = logLines.scrollHeight;
        break;
      }
      case "RANGE_TABLES_UPDATE":
        if (startingHands) swap(startingHands, msg.fragment);
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
        if (!currentDecision || msg.decision !== currentDecision) {
          break;
        }
        setSolverStatus(msg);
        if (msg.phase === "READY") {
          setDockLocked(false);
        }
        break;
      case "TOURNAMENT_FINISHED":
        finished = true;
        showTournamentModal(msg.won, msg.url);
        break;
      case "SESSION_FINISHED":
        finished = true;
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
      resetSolverStatus();
      if (finished) {
        setStatus("table finished", "status-ok");
        return;
      }
      setStatus("disconnected — retrying…", "status-bad");
      clearTimeout(retryTimer);
      retryTimer = setTimeout(connect, 1500);
    };
    ws.onerror = () => ws.close();
  }

  connect();
})();