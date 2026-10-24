// Token usage over a period as a column chart (one column per hour / day / month), with a
// hover tooltip. Shared by the home page and the admin page; both are dark, so are the colors.
(function () {
  const BAR = '#3987e5';      // passes the dark-surface checks against #1a1a1a
  const BAR_HOVER = '#5a9cec';
  const GRID = '#2a2a2a';
  const AXIS_TEXT = '#888';

  // 220000000 -> "2.2亿", 35000 -> "3.5万"
  function compact(n) {
    if (n >= 1e8) return +(n / 1e8).toFixed(n >= 1e9 ? 0 : 1) + '亿';
    if (n >= 1e4) return +(n / 1e4).toFixed(n >= 1e5 ? 0 : 1) + '万';
    return String(n);
  }

  // Every bucket of the period, so quiet hours/days still get a slot
  function buckets(period) {
    const now = new Date();
    if (period === 'day') return Array.from({ length: 24 }, (_, i) => ({ key: String(i).padStart(2, '0'), label: i + '时', tick: i % 6 === 0 }));
    if (period === 'month') {
      const days = new Date(now.getFullYear(), now.getMonth() + 1, 0).getDate();
      return Array.from({ length: days }, (_, i) => ({ key: String(i + 1).padStart(2, '0'), label: (i + 1) + '日', tick: i === 0 || (i + 1) % 5 === 0 }));
    }
    return Array.from({ length: 12 }, (_, i) => ({ key: String(i + 1).padStart(2, '0'), label: (i + 1) + '月', tick: true }));
  }

  // A round top for the axis: 1, 2 or 5 times a power of ten
  function niceMax(v) {
    if (v <= 0) return 1;
    const p = Math.pow(10, Math.floor(Math.log10(v)));
    for (const m of [1, 2, 5, 10]) if (v <= m * p) return m * p;
    return 10 * p;
  }

  // Drawn charts, redrawn at the new width when the window resizes (a phone turning, say)
  const drawn = new Map();
  let resizeTimer;
  window.addEventListener('resize', () => {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(() => drawn.forEach(([series, period], el) => el.isConnected && render(el, series, period)), 150);
  });

  function render(el, series, period) {
    drawn.set(el, [series, period]);
    const byKey = Object.fromEntries((series || []).map(s => [s.bucket, s]));
    const data = buckets(period).map(b => ({ ...b, tokens: byKey[b.key]?.total_tokens || 0, requests: byKey[b.key]?.requests || 0 }));
    // Measure without the old chart, which would hold the box at its previous width
    el.innerHTML = '';
    const W = Math.max(el.clientWidth || 600, 200), H = 180;
    const left = 44, right = 8, top = 10, bottom = 22;
    const plotW = W - left - right, plotH = H - top - bottom;
    const max = niceMax(Math.max(...data.map(d => d.tokens)));
    const band = plotW / data.length;
    const barW = Math.max(2, Math.min(24, band - 2));
    const y = v => top + plotH - (v / max) * plotH;

    let svg = `<svg width="${W}" height="${H}" viewBox="0 0 ${W} ${H}" role="img" aria-label="token 用量图" style="display:block">`;
    for (const t of [0, max / 2, max]) {
      svg += `<line x1="${left}" x2="${W - right}" y1="${y(t)}" y2="${y(t)}" stroke="${GRID}" stroke-width="1"/>`;
      svg += `<text x="${left - 6}" y="${y(t) + 4}" text-anchor="end" font-size="11" fill="${AXIS_TEXT}" style="font-variant-numeric:tabular-nums">${compact(t)}</text>`;
    }
    data.forEach((d, i) => {
      const x = left + i * band + (band - barW) / 2;
      if (d.tokens > 0) {
        // Rounded at the data end, square at the baseline
        const h = Math.max(1, y(0) - y(d.tokens)), r = Math.min(4, barW / 2, h);
        const yt = y(0) - h;
        svg += `<path class="uc-bar" data-i="${i}" fill="${BAR}" d="M${x},${y(0)} V${yt + r} Q${x},${yt} ${x + r},${yt} H${x + barW - r} Q${x + barW},${yt} ${x + barW},${yt + r} V${y(0)} Z"/>`;
      }
      if (d.tick) svg += `<text x="${left + i * band + band / 2}" y="${H - 6}" text-anchor="middle" font-size="11" fill="${AXIS_TEXT}">${d.label}</text>`;
      // The hit target is the whole band, not the bar
      svg += `<rect class="uc-hit" data-i="${i}" x="${left + i * band}" y="${top}" width="${band}" height="${plotH}" fill="transparent"/>`;
    });
    svg += '</svg>';

    el.style.position = 'relative';
    el.innerHTML = svg + '<div class="uc-tip" style="position:absolute;pointer-events:none;display:none;background:#111;border:1px solid #333;border-radius:6px;padding:6px 8px;font-size:12px;color:#e0e0e0;white-space:nowrap;z-index:1"></div>';
    const tip = el.querySelector('.uc-tip');
    el.querySelectorAll('.uc-hit').forEach(hit => {
      const i = +hit.dataset.i, d = data[i];
      const bar = el.querySelector(`.uc-bar[data-i="${i}"]`);
      hit.addEventListener('mouseenter', () => {
        if (bar) bar.setAttribute('fill', BAR_HOVER);
        tip.innerHTML = `<div style="color:#888">${d.label}</div><div>${d.tokens.toLocaleString()} token</div><div style="color:#888">${d.requests.toLocaleString()} 次请求</div>`;
        tip.style.display = 'block';
        const cx = left + i * band + band / 2;
        tip.style.left = Math.min(Math.max(0, cx - tip.offsetWidth / 2), W - tip.offsetWidth) + 'px';
        tip.style.top = Math.max(0, y(d.tokens) - tip.offsetHeight - 8) + 'px';
      });
      hit.addEventListener('mouseleave', () => {
        if (bar) bar.setAttribute('fill', BAR);
        tip.style.display = 'none';
      });
    });
    return { tokens: data.reduce((a, d) => a + d.tokens, 0), requests: data.reduce((a, d) => a + d.requests, 0) };
  }

  window.UsageChart = { render, compact };
})();
