(() => {
  "use strict";

  const table = document.getElementById("table");
  const overlay = document.getElementById("overlay");
  const statusEl = document.getElementById("ws-status");
  const chartData = [];
  let ws = null;
  let retryTimer = null;

  const swap = (el, html) => {
    el.innerHTML = html;
  };

  function setStatus(text, cls) {
    statusEl.textContent = text;
    statusEl.className = cls;
  }

  function sendAction(kind, extra) {
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    const action = Object.assign({ kind: kind }, extra || {});
    ws.send(JSON.stringify({ type: "ACTION_SUBMIT", action: action }));
  }

  function bindPanel() {
    document.querySelectorAll("#table [data-kind]").forEach((el) => {
      el.addEventListener("click", () => {
        if (el.dataset.kind === "custom") {
          const input = document.getElementById("custom-amount");
          if (!input) return;
          const amount = parseInt(input.value, 10);
          if (!Number.isFinite(amount) || amount <= 0) return;
          sendAction(el.dataset.customKind, { amount: amount });
          return;
        }
        const extra = {};
        if (el.dataset.bucket) extra.bucket = el.dataset.bucket;
        sendAction(el.dataset.kind, extra);
      });
    });
    document.querySelectorAll("#overlay [data-overlay-close]").forEach((el) => {
      el.addEventListener("click", () => {
        if (overlay) overlay.innerHTML = "";
      });
    });
    document.querySelectorAll("#overlay [data-overlay-confirm]").forEach((el) => {
      el.addEventListener("click", () => {
        if (!ws || ws.readyState !== WebSocket.OPEN) return;
        ws.send(JSON.stringify({ type: "REVIEW_DONE" }));
        if (overlay) overlay.innerHTML = "";
      });
    });
  }

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
        ctx.fillStyle = "#f59e0b";
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
    ctx.strokeStyle = "#f59e0b";
    ctx.lineWidth = 2;
    ctx.stroke();
  }

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
        bindPanel();
        break;
      case "TRIGGER_TACTICAL_OVERLAY":
        swap(overlay, msg.fragment);
        bindPanel();
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
      clearTimeout(retryTimer);
      retryTimer = setTimeout(connect, 1500);
    };
    ws.onerror = () => ws.close();
  }

  connect();
})();