// The usage view shared by the home page and the admin page: a day / week / month / year (or any
// range) you can page back through, split by model, provider or user, filtered to one of them,
// with input and output kept apart and a comparison with the period before. Both pages are dark,
// so are the colors.
(function () {
  // The validated dark categorical order; anything past it folds into "other"
  const SLOTS = ['#3987e5', '#d95926', '#199e70', '#c98500', '#d55181', '#008300', '#9085e9'];
  const OTHER = '#6b6b68';
  const SURFACE = '#1a1a1a';
  const GRID = '#2a2a2a';
  const MUTED = '#888';

  const TEXT = {
    zh: {
      locale: 'zh-CN',
      periods: { day: '日', week: '周', month: '月', year: '年', custom: '自定义' },
      current: { day: ['今天', '昨天'], week: ['本周', '上周'], month: ['本月', '上月'], year: ['今年', '去年'] },
      previous: { day: '前一天', week: '前一周', month: '前一个月', year: '前一年', custom: '前一段' },
      versus: (name, partial) => `较${name}${partial ? '同期' : ''}`,
      noPrevious: '前一段没有用量',
      to: '至',
      allModels: '全部模型', allOf: p => `${p}（全部）`, allUsers: '全部用户',
      split: { none: '不拆分', model: '按模型', provider: '按厂商', user: '按用户' },
      metric: { total: '全部 token', input: '只看输入', output: '只看输出' },
      input: '输入', output: '输出', total: '合计', other: '其他',
      requests: n => `${n} 次请求`, tokens: n => `${n} token`,
      cols: { user: '用户', model: '模型', requests: '请求', input: '输入', output: '输出', total: '合计' },
      empty: '这段时间没有用量',
      clickHint: '点一行或一个图例只看它',
      clear: '清除筛选',
      failed: '加载失败',
    },
    en: {
      locale: 'en-US',
      periods: { day: 'Day', week: 'Week', month: 'Month', year: 'Year', custom: 'Custom' },
      current: { day: ['Today', 'Yesterday'], week: ['This week', 'Last week'], month: ['This month', 'Last month'], year: ['This year', 'Last year'] },
      previous: { day: 'previous day', week: 'previous week', month: 'previous month', year: 'previous year', custom: 'previous range' },
      versus: (name, partial) => `vs. ${name}${partial ? ' so far' : ''}`,
      noPrevious: 'nothing in the previous range',
      to: 'to',
      allModels: 'All models', allOf: p => `${p} (all)`, allUsers: 'All users',
      split: { none: 'No split', model: 'By model', provider: 'By provider', user: 'By user' },
      metric: { total: 'All tokens', input: 'Input only', output: 'Output only' },
      input: 'Input', output: 'Output', total: 'Total', other: 'Other',
      requests: n => `${n} requests`, tokens: n => `${n} tokens`,
      cols: { user: 'User', model: 'Model', requests: 'Requests', input: 'Input', output: 'Output', total: 'Total' },
      empty: 'No usage in this range',
      clickHint: 'Click a row or a legend entry to look at just that',
      clear: 'Clear filters',
      failed: 'Failed to load',
    },
  };

  const STYLE = `
.uv-row { display: flex; flex-wrap: wrap; gap: 6px; align-items: center; margin-bottom: 10px; }
.uv .tab { padding: 6px 12px; font-size: 13px; margin: 0; }
.uv select, .uv input[type=date] { width: auto; margin: 0; padding: 6px 8px; font-size: 13px; border: 1px solid #333; border-radius: 6px; background: #111; color: #e0e0e0; }
.uv-nav { display: flex; align-items: center; gap: 6px; }
.uv-nav button { width: 28px; height: 28px; padding: 0; margin: 0; background: #222; color: #ccc; font-size: 15px; border-radius: 6px; }
.uv-nav button:disabled { opacity: .35; cursor: default; }
.uv-nav button:hover:not(:disabled) { background: #333; }
.uv-label { color: #e0e0e0; font-size: 14px; min-width: 8em; text-align: center; }
.uv-label small { color: ${MUTED}; font-size: 12px; margin-left: 4px; }
.uv-sum { display: flex; flex-wrap: wrap; align-items: baseline; gap: 4px 14px; margin: 4px 0 8px; }
.uv-big { font-size: 26px; color: #f0f0f0; font-variant-numeric: tabular-nums; }
.uv-sub { color: ${MUTED}; font-size: 13px; font-variant-numeric: tabular-nums; }
.uv-legend { display: flex; flex-wrap: wrap; gap: 4px 12px; font-size: 12px; color: #bbb; margin-bottom: 4px; }
.uv-legend span { cursor: pointer; display: inline-flex; align-items: center; gap: 5px; }
.uv-legend span:hover { color: #fff; }
.uv-legend i { width: 10px; height: 10px; border-radius: 2px; display: inline-block; }
.uv-chart { position: relative; margin: 4px 0 12px; }
.uv-tip { position: absolute; pointer-events: none; display: none; background: #111; border: 1px solid #333; border-radius: 6px; padding: 6px 8px; font-size: 12px; color: #e0e0e0; white-space: nowrap; z-index: 1; font-variant-numeric: tabular-nums; }
.uv-tip i { width: 8px; height: 8px; border-radius: 2px; display: inline-block; margin-right: 5px; }
.uv-tip td { padding: 1px 0 1px 10px; border: none; text-align: right; }
.uv-tip td:first-child { padding-left: 0; text-align: left; }
.uv-table { width: 100%; border-collapse: collapse; font-size: 13px; }
.uv-table th, .uv-table td { text-align: left; padding: 7px 8px; border-bottom: 1px solid #222; }
.uv-table th { color: ${MUTED}; font-weight: 600; white-space: nowrap; }
.uv-table td.n, .uv-table th.n { text-align: right; font-variant-numeric: tabular-nums; white-space: nowrap; }
.uv-table tbody tr { cursor: pointer; }
.uv-table tbody tr:hover td { background: #202020; }
.uv-hint { color: #666; font-size: 12px; margin-top: 6px; }
.uv-hint a { color: #6ea8ff; cursor: pointer; margin-left: 8px; }
`;

  const pad = n => String(n).padStart(2, '0');
  const dayKey = d => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
  const parseDay = s => { const [y, m, d] = s.split('-').map(Number); return new Date(y, m - 1, d); };
  const secs = d => Math.floor(d.getTime() / 1000);

  // The bucket key the relay writes for a moment: "2026-11-01 13", "2026-11-01", "2026-11"
  function keyOf(d, step) {
    if (step === 'hour') return dayKey(d) + ' ' + pad(d.getHours());
    if (step === 'day') return dayKey(d);
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}`;
  }

  // Every bucket from `from` to `to`, so quiet hours and days still get a slot
  function buckets(from, to, step) {
    const d = new Date(from);
    if (step === 'hour') d.setMinutes(0, 0, 0);
    else d.setHours(0, 0, 0, 0);
    if (step === 'month') d.setDate(1);
    const out = [];
    while (d < to && out.length < 1000) {
      out.push(new Date(d));
      if (step === 'hour') d.setHours(d.getHours() + 1);
      else if (step === 'day') d.setDate(d.getDate() + 1);
      else d.setMonth(d.getMonth() + 1);
    }
    return out;
  }

  // [start, end) of the period `offset` periods back from the current one, in local time
  function periodRange(period, offset, now = new Date()) {
    const y = now.getFullYear(), m = now.getMonth(), d = now.getDate();
    if (period === 'day') return [new Date(y, m, d + offset), new Date(y, m, d + offset + 1)];
    if (period === 'week') {
      const monday = d - (now.getDay() + 6) % 7 + 7 * offset;
      return [new Date(y, m, monday), new Date(y, m, monday + 7)];
    }
    if (period === 'month') return [new Date(y, m + offset, 1), new Date(y, m + offset + 1, 1)];
    return [new Date(y + offset, 0, 1), new Date(y + offset + 1, 0, 1)];
  }

  // A round top for the axis: 1, 2 or 5 times a power of ten
  function niceMax(v) {
    if (v <= 0) return 1;
    const p = Math.pow(10, Math.floor(Math.log10(v)));
    for (const k of [1, 2, 5, 10]) if (v <= k * p) return k * p;
    return 10 * p;
  }

  function mount(el, opts) {
    const t = TEXT[opts.lang] || TEXT.zh;
    const admin = !!opts.admin;
    const fmt = (o) => new Intl.DateTimeFormat(t.locale, o);
    const md = fmt({ month: 'short', day: 'numeric' }), wd = fmt({ weekday: 'short' });
    const ym = fmt({ year: 'numeric', month: 'long' }), yr = fmt({ year: 'numeric' });
    const num = n => n.toLocaleString(t.locale);
    // 220000000 -> "2.2亿" / "220M"
    const compact = n => {
      if (opts.lang === 'en') {
        for (const [v, s] of [[1e9, 'B'], [1e6, 'M'], [1e3, 'K']]) if (n >= v) return +(n / v).toFixed(n >= 10 * v ? 0 : 1) + s;
        return String(n);
      }
      if (n >= 1e8) return +(n / 1e8).toFixed(n >= 1e9 ? 0 : 1) + '亿';
      if (n >= 1e4) return +(n / 1e4).toFixed(n >= 1e5 ? 0 : 1) + '万';
      return String(n);
    };

    const today = new Date();
    const monthAgo = new Date(today.getFullYear(), today.getMonth(), today.getDate() - 29);
    const s = { period: 'day', offset: 0, from: dayKey(monthAgo), to: dayKey(today), group: 'none', metric: 'total', model: '', user: '' };
    let data = null, seq = 0;

    if (!document.getElementById('uv-style')) {
      const st = document.createElement('style');
      st.id = 'uv-style';
      st.textContent = STYLE;
      document.head.appendChild(st);
    }
    el.classList.add('uv');
    el.innerHTML = `
      <div class="uv-row">
        ${['day', 'week', 'month', 'year', 'custom'].map(p => `<button class="tab" data-p="${p}">${t.periods[p]}</button>`).join('')}
      </div>
      <div class="uv-row">
        <div class="uv-nav"><button data-nav="-1" aria-label="previous">‹</button><span class="uv-label"></span><button data-nav="1" aria-label="next">›</button></div>
        <span class="uv-custom"><input type="date" data-k="from"> ${t.to} <input type="date" data-k="to"></span>
      </div>
      <div class="uv-row">
        <select data-k="model"></select>
        ${admin ? '<select data-k="user"></select>' : ''}
        <select data-k="group">${['none', 'model', 'provider', ...(admin ? ['user'] : [])].map(g => `<option value="${g}">${t.split[g]}</option>`).join('')}</select>
        <select data-k="metric">${['total', 'input', 'output'].map(m => `<option value="${m}">${t.metric[m]}</option>`).join('')}</select>
      </div>
      <div class="uv-sum"></div>
      <div class="uv-legend"></div>
      <div class="uv-chart"></div>
      <div class="uv-tablewrap"></div>`;
    const $ = sel => el.querySelector(sel);

    el.querySelectorAll('[data-p]').forEach(b => b.onclick = () => { s.period = b.dataset.p; s.offset = 0; load(); });
    el.querySelectorAll('[data-nav]').forEach(b => b.onclick = () => { s.offset = Math.min(0, s.offset + +b.dataset.nav); load(); });
    el.querySelectorAll('select, input').forEach(c => c.onchange = () => {
      s[c.dataset.k] = c.value;
      if (s.from > s.to) [s.from, s.to] = [s.to, s.from];
      load();
    });

    // The range on screen, the one it's compared with, and how it's cut
    function ranges() {
      let start, end, prevStart, prevEnd, step;
      if (s.period === 'custom') {
        start = parseDay(s.from);
        end = parseDay(s.to);
        end.setDate(end.getDate() + 1);
        prevStart = new Date(start - (end - start));
        prevEnd = start;
      } else {
        [start, end] = periodRange(s.period, s.offset);
        [prevStart, prevEnd] = periodRange(s.period, s.offset - 1);
        step = { day: 'hour', week: 'day', month: 'day', year: 'month' }[s.period];
      }
      // A period still under way is compared with the same stretch of the one before
      const now = new Date();
      const partial = start <= now && now < end;
      if (partial) prevEnd = new Date(Math.min(prevEnd, +prevStart + (now - start)));
      return { start, end, prevStart, prevEnd, step, partial };
    }

    function label(r) {
      const p = s.period;
      if (p === 'custom') return '';
      const named = t.current[p][-s.offset];
      let dates;
      if (p === 'day') dates = `${md.format(r.start)} ${wd.format(r.start)}`;
      else if (p === 'week') dates = `${md.format(r.start)} – ${md.format(new Date(r.end - 86400000))}`;
      else if (p === 'month') dates = ym.format(r.start);
      else dates = yr.format(r.start);
      return named ? `${named}<small>${dates}</small>` : dates;
    }

    async function load() {
      const r = ranges();
      el.querySelectorAll('[data-p]').forEach(b => b.classList.toggle('active', b.dataset.p === s.period));
      $('.uv-nav').style.display = s.period === 'custom' ? 'none' : '';
      $('.uv-custom').style.display = s.period === 'custom' ? '' : 'none';
      $('[data-k=from]').value = s.from;
      $('[data-k=to]').value = s.to;
      $('[data-k=group]').value = s.group;
      $('[data-k=metric]').value = s.metric;
      $('.uv-label').innerHTML = label(r);
      $('[data-nav="1"]').disabled = s.offset >= 0;

      const q = new URLSearchParams({
        from: secs(r.start), to: secs(r.end), tz: -r.start.getTimezoneOffset(), group: s.group,
        compare_from: secs(r.prevStart), compare_to: secs(r.prevEnd),
      });
      if (r.step) q.set('step', r.step);
      if (s.model) q.set('model', s.model);
      if (s.user) q.set('user', s.user);
      const mine = ++seq;
      try {
        const res = await opts.fetch(q.toString());
        if (mine !== seq) return;
        data = { res, r };
        render();
      } catch (e) {
        if (mine === seq) $('.uv-sum').innerHTML = `<span class="uv-sub">${t.failed}</span>`;
      }
    }

    // Filter choices: each provider as a whole, then each model; a chosen one stays even when absent
    function choices(res) {
      const models = res.present_models.slice();
      if (s.model && !s.model.endsWith('/*') && !models.includes(s.model)) models.push(s.model);
      const providers = [...new Set(models.filter(m => m.includes('/')).map(m => m.split('/')[0]))];
      if (s.model.endsWith('/*') && !providers.includes(s.model.slice(0, -2))) providers.push(s.model.slice(0, -2));
      $('[data-k=model]').innerHTML = `<option value="">${t.allModels}</option>` +
        providers.sort().map(p => `<option value="${esc(p)}/*">${esc(t.allOf(p))}</option>`).join('') +
        models.sort().map(m => `<option value="${esc(m)}">${esc(m)}</option>`).join('');
      $('[data-k=model]').value = s.model;
      if (admin) {
        const users = res.present_users.slice();
        if (s.user && !users.some(u => String(u.id) === s.user)) users.push({ id: s.user, username: (data.names || {})[s.user] || '#' + s.user });
        data.names = Object.fromEntries(users.map(u => [String(u.id), u.username]));
        $('[data-k=user]').innerHTML = `<option value="">${t.allUsers}</option>` +
          users.map(u => `<option value="${esc(u.id)}">${esc(u.username)}</option>`).join('');
        $('[data-k=user]').value = s.user;
      }
    }

    const pick = (x) => s.metric === 'input' ? x.input_tokens : s.metric === 'output' ? x.output_tokens : x.total_tokens;

    function render() {
      const { res, r } = data;
      choices(res);

      // Summary: the total, its parts, and the change from the period before
      const tot = res.totals, prev = res.previous;
      let change = '';
      if (prev) {
        const name = s.period === 'custom' ? t.previous.custom
          : s.offset === 0 && t.current[s.period][1] ? t.current[s.period][1] : t.previous[s.period];
        const before = pick(prev), now = pick(tot);
        if (before > 0) {
          const pct = Math.round((now - before) / before * 100);
          change = `<span class="uv-sub" title="${esc(num(before))}">${esc(t.versus(name, r.partial))} ${pct >= 0 ? '▲' : '▼'} ${Math.abs(pct)}%</span>`;
        } else if (now > 0) change = `<span class="uv-sub">${t.noPrevious}</span>`;
      }
      $('.uv-sum').innerHTML = `<span class="uv-big">${compact(pick(tot))}</span>
        <span class="uv-sub">${t.input} ${num(tot.input_tokens)} · ${t.output} ${num(tot.output_tokens)} · ${t.requests(num(tot.requests))}</span>${change}`;

      // Segments: input and output when not split, else the groups biggest first, the tail as "other"
      let segs, valueOf;
      const step = res.step;
      const byBucket = {};
      if (s.group === 'none') {
        segs = s.metric === 'total' ? [{ key: 'in', name: t.input, color: SLOTS[0] }, { key: 'out', name: t.output, color: SLOTS[1] }]
          : [{ key: s.metric === 'input' ? 'in' : 'out', name: t.metric[s.metric], color: SLOTS[0] }];
        for (const b of res.series) byBucket[b.bucket] = { in: b.input_tokens, out: b.output_tokens, requests: b.requests };
      } else {
        const sums = {};
        for (const b of res.series) sums[b.group] = (sums[b.group] || 0) + pick(b);
        const order = Object.keys(sums).filter(g => sums[g] > 0).sort((a, b) => sums[b] - sums[a]);
        const shown = order.length > SLOTS.length ? order.slice(0, SLOTS.length - 1) : order;
        segs = shown.map((g, i) => ({ key: g, name: g, color: SLOTS[i] }));
        if (shown.length < order.length) segs.push({ key: '\u0000other', name: t.other, color: OTHER, rest: true });
        for (const b of res.series) {
          const k = shown.includes(b.group) ? b.group : '\u0000other';
          const o = byBucket[b.bucket] ||= { requests: 0 };
          o[k] = (o[k] || 0) + pick(b);
          o.requests += b.requests;
        }
      }
      const slots = buckets(new Date(res.from * 1000), new Date(res.to * 1000), step).map(d => {
        const o = byBucket[keyOf(d, step)] || { requests: 0 };
        const vals = segs.map(g => o[g.key] || 0);
        return { d, vals, sum: vals.reduce((a, v) => a + v, 0), requests: o.requests };
      });

      // Legend: always for two or more segments; clicking one filters to it
      const segTotals = segs.map((_, i) => slots.reduce((a, b) => a + b.vals[i], 0));
      $('.uv-legend').innerHTML = segs.length > 1 ? segs.map((g, i) =>
        `<span data-seg="${i}"><i style="background:${g.color}"></i>${esc(g.name)} <b style="color:${MUTED};font-weight:normal">${compact(segTotals[i])}</b></span>`).join('') : '';
      $('.uv-legend').querySelectorAll('[data-seg]').forEach(sp => sp.onclick = () => focus(segs[+sp.dataset.seg]));

      drawChart(slots, segs, step, r);
      table(res);
    }

    // Look at just one group: a model, a provider's models, or a user
    function focus(seg) {
      if (seg.rest || s.group === 'none') return;
      if (s.group === 'model') s.model = seg.key;
      else if (s.group === 'provider') s.model = seg.key + '/*';
      else if (s.group === 'user') s.user = String(data.res.present_users.find(u => u.username === seg.key)?.id || '');
      s.group = 'none';
      load();
    }

    function tick(d, step, i, n) {
      if (step === 'hour') return d.getHours() % 6 === 0 ? (opts.lang === 'en' ? `${d.getHours()}:00` : `${d.getHours()}时`) : null;
      if (step === 'day') {
        if (n <= 7) return wd.format(d);
        const every = n <= 31 ? 5 : Math.ceil(n / 8);
        return n <= 31 ? (d.getDate() === 1 || d.getDate() % every === 0 ? String(d.getDate()) : null) : (i % every === 0 ? md.format(d) : null);
      }
      const every = Math.ceil(n / 12);
      if (i % every !== 0) return null;
      return n <= 12 ? fmt({ month: 'short' }).format(d) : ym.format(d);
    }

    function bucketName(d, step) {
      if (step === 'hour') return `${md.format(d)} ${pad(d.getHours())}:00–${pad(d.getHours() + 1)}:00`;
      if (step === 'day') return `${md.format(d)} ${wd.format(d)}`;
      return ym.format(d);
    }

    function drawChart(slots, segs, step) {
      const box = $('.uv-chart');
      box.innerHTML = '';
      const W = Math.max(box.clientWidth || 600, 200), H = 200;
      const left = 48, right = 8, top = 10, bottom = 22;
      const plotW = W - left - right, plotH = H - top - bottom;
      const max = niceMax(Math.max(0, ...slots.map(b => b.sum)));
      const band = plotW / Math.max(1, slots.length);
      const barW = Math.max(2, Math.min(28, band - 2));
      const y = v => top + plotH - (v / max) * plotH;

      let svg = `<svg width="${W}" height="${H}" viewBox="0 0 ${W} ${H}" role="img" aria-label="token" style="display:block">`;
      for (const v of [0, max / 2, max]) {
        svg += `<line x1="${left}" x2="${W - right}" y1="${y(v)}" y2="${y(v)}" stroke="${GRID}" stroke-width="1"/>`;
        svg += `<text x="${left - 6}" y="${y(v) + 4}" text-anchor="end" font-size="11" fill="${MUTED}" style="font-variant-numeric:tabular-nums">${compact(v)}</text>`;
      }
      slots.forEach((b, i) => {
        const x = left + i * band + (band - barW) / 2;
        // Segments bottom up, a 2px surface gap between them; only the top one gets round corners
        let base = y(0);
        const last = b.vals.reduce((k, v, j) => v > 0 ? j : k, -1);
        b.vals.forEach((v, j) => {
          if (v <= 0) return;
          const h = Math.max(1, (v / max) * plotH), yt = base - h;
          const gap = j === last ? 0 : Math.min(2, h / 2);
          if (j === last) {
            const r = Math.min(4, barW / 2, h);
            svg += `<path class="uv-bar" data-i="${i}" fill="${segs[j].color}" d="M${x},${base} V${yt + r} Q${x},${yt} ${x + r},${yt} H${x + barW - r} Q${x + barW},${yt} ${x + barW},${yt + r} V${base} Z"/>`;
          } else {
            svg += `<rect class="uv-bar" data-i="${i}" fill="${segs[j].color}" x="${x}" y="${yt + gap}" width="${barW}" height="${h - gap}"/>`;
          }
          base = yt;
        });
        const tk = tick(b.d, step, i, slots.length);
        if (tk) svg += `<text x="${left + i * band + band / 2}" y="${H - 6}" text-anchor="middle" font-size="11" fill="${MUTED}">${esc(tk)}</text>`;
        svg += `<rect class="uv-hit" data-i="${i}" x="${left + i * band}" y="${top}" width="${band}" height="${plotH}" fill="transparent"/>`;
      });
      svg += '</svg>';
      box.innerHTML = svg + '<div class="uv-tip"></div>';

      const tip = box.querySelector('.uv-tip');
      box.querySelectorAll('.uv-hit').forEach(hit => {
        const i = +hit.dataset.i, b = slots[i];
        const bars = box.querySelectorAll(`.uv-bar[data-i="${i}"]`);
        hit.addEventListener('mouseenter', () => {
          bars.forEach(p => p.setAttribute('opacity', '0.8'));
          const rows = segs.map((g, j) => [g, b.vals[j]]).filter(([, v]) => v > 0).reverse()
            .map(([g, v]) => `<tr><td><i style="background:${g.color}"></i>${esc(g.name)}</td><td>${num(v)}</td></tr>`).join('');
          tip.innerHTML = `<div style="color:${MUTED}">${esc(bucketName(b.d, step))}</div>
            <table>${segs.length > 1 ? rows : ''}<tr><td>${t.total}</td><td>${num(b.sum)}</td></tr></table>
            <div style="color:${MUTED}">${t.requests(num(b.requests))}</div>`;
          tip.style.display = 'block';
          const cx = left + i * band + band / 2;
          tip.style.left = Math.min(Math.max(0, cx - tip.offsetWidth / 2), W - tip.offsetWidth) + 'px';
          tip.style.top = Math.max(0, y(b.sum) - tip.offsetHeight - 8) + 'px';
        });
        hit.addEventListener('mouseleave', () => {
          bars.forEach(p => p.removeAttribute('opacity'));
          tip.style.display = 'none';
        });
      });
    }

    function table(res) {
      const rows = admin ? res.rows : res.models;
      const filtered = s.model || s.user;
      const hint = `<div class="uv-hint">${t.clickHint}${filtered ? `<a data-clear>${t.clear}</a>` : ''}</div>`;
      if (!rows.length) {
        $('.uv-tablewrap').innerHTML = `<p class="uv-sub">${t.empty}</p>` + (filtered ? hint : '');
      } else {
        const c = t.cols;
        $('.uv-tablewrap').innerHTML = `<table class="uv-table"><thead><tr>${admin ? `<th>${c.user}</th>` : ''}<th>${c.model}</th>
          <th class="n">${c.requests}</th><th class="n">${c.input}</th><th class="n">${c.output}</th><th class="n">${c.total}</th></tr></thead><tbody>` +
          rows.map((row, i) => `<tr data-row="${i}">${admin ? `<td>${esc(row.username)}</td>` : ''}<td>${esc(row.model)}</td>
            <td class="n">${num(row.requests)}</td><td class="n">${num(row.input_tokens)}</td><td class="n">${num(row.output_tokens)}</td><td class="n">${num(row.total_tokens)}</td></tr>`).join('') +
          '</tbody></table>' + hint;
        $('.uv-tablewrap').querySelectorAll('[data-row]').forEach(tr => tr.onclick = () => {
          const row = rows[+tr.dataset.row];
          s.model = row.model;
          if (admin) s.user = String(row.user_id);
          if (s.group === 'model' || (admin && s.group === 'user')) s.group = 'none';
          load();
        });
      }
      const clear = $('[data-clear]');
      if (clear) clear.onclick = () => { s.model = ''; s.user = ''; load(); };
    }

    // Redrawn at the new width when the window resizes (a phone turning, say)
    let resizeTimer;
    window.addEventListener('resize', () => {
      clearTimeout(resizeTimer);
      resizeTimer = setTimeout(() => data && el.isConnected && el.offsetParent && render(), 150);
    });

    return { load, reload: load };
  }

  window.UsageView = { mount };
})();
