// Draws every server-rendered EV canvas on the page. Each canvas carries its
// decimated dataset in `data-points` as JSON `[[action_index, ev_loss], ...]`;
// only the second element of each pair is plotted. Shared by the tournaments
// listing and the single-tournament detail page.
(() => {
  "use strict";
  document.querySelectorAll("canvas[data-points]").forEach((canvas) => {
    const ctx = canvas.getContext("2d");
    const values = JSON.parse(canvas.dataset.points || "[]").map((point) => point[1]);
    if (values.length < 2) return;
    const max = Math.max(1, ...values);
    const step = canvas.width / (values.length - 1);
    ctx.beginPath();
    values.forEach((value, i) => {
      const x = i * step;
      const y = canvas.height - (value / max) * (canvas.height - 6) - 3;
      if (i === 0) ctx.moveTo(x, y);
      else ctx.lineTo(x, y);
    });
    ctx.strokeStyle = "#f59e0b";
    ctx.lineWidth = 2;
    ctx.stroke();
  });
})();
