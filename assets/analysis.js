// Polls the opponent-analysis status endpoint and swaps in the server-rendered
// status fragment until the job stops running.
(function () {
  var box = document.getElementById('analysis-status');
  function poll() {
    fetch('/history/analyze-status')
      .then(function (r) { return r.json(); })
      .then(function (status) {
        box.innerHTML = status.html;
        if (status.state === 'running') { setTimeout(poll, 1500); }
      })
      .catch(function () { box.innerHTML = '<div class="pt-empty">Status unavailable.</div>'; });
  }
  poll();
})();
