let token = localStorage.getItem('admin_token');
let providers = [];

// On LazyCat, sign in as the current LazyCat user; elsewhere fall back to the password form
fetch(BASE + '/auth/sso').then(r => r.ok ? r.json() : null).then(res => {
  if (res) { token = res.token; localStorage.setItem('admin_token', token); }
  if (token) loadAdmin();
}).catch(() => { if (token) loadAdmin(); });

async function login() {
  try {
    const res = await api('/auth/login', {
      method: 'POST',
      body: JSON.stringify({ username: document.getElementById('username').value, password: document.getElementById('password').value })
    });
    token = res.token;
    localStorage.setItem('admin_token', token);
    loadAdmin();
  } catch (e) { showMsg('login-msg', e.message, false); }
}

async function loadAdmin() {
  document.getElementById('login-section').classList.add('hidden');
  document.getElementById('admin').classList.remove('hidden');
  try {
    await loadUsers();
    await loadProviders();
    await loadModels();
  } catch (e) {
    token = null; localStorage.removeItem('admin_token');
    location.reload();
  }
}

function showTab(name) {
  if (name === 'usage') loadUsage();
  if (name === 'limits') loadLimits();
  if (name === 'tunnel') loadTunnel();
  ['users','providers','models','usage','limits','tunnel'].forEach(t => {
    document.getElementById('tab-'+t).classList.toggle('hidden', t !== name);
  });
  document.querySelectorAll('.tab').forEach(el => el.classList.remove('active'));
  event.target.classList.add('active');
}

// Users
async function loadUsers() {
  const users = await api('/admin/users');
  const tb = document.getElementById('users-body');
  tb.innerHTML = users.map(u => `<tr>
    <td>${u.id}</td><td>${esc(u.username)}</td><td>${esc(u.email || '-')}</td><td><code style="font-size:11px">${esc(u.api_key)}</code></td>
    <td>${u.is_admin ? 'Yes' : 'No'}</td>
    <td><button class="secondary btn-sm" onclick="setPassword(${u.id})">Set Password</button> <button class="btn-danger btn-sm" onclick="deleteUser(${u.id})">Delete</button></td>
  </tr>`).join('');
}
async function createUser() {
  await api('/admin/users', { method:'POST', body: JSON.stringify({
    username: document.getElementById('new-user').value,
    email: document.getElementById('new-email').value || null,
    password: document.getElementById('new-pass').value,
    is_admin: document.getElementById('new-admin').checked
  })});
  loadUsers();
}
// For signing in on the public address
async function setPassword(id) {
  const pw = prompt('New password (at least 8 characters):');
  if (pw === null) return;
  try {
    await api('/admin/users/'+id+'/password', { method:'PUT', body: JSON.stringify({ password: pw }) });
    alert('Password set');
  } catch (e) { alert('Failed: ' + (e.message || 'password too short?')); }
}
async function deleteUser(id) {
  if (!confirm('Delete user?')) return;
  await api('/admin/users/'+id, { method:'DELETE' });
  loadUsers();
}

// Providers
// "8 keys · 7 OK · 1 cooling" for the providers table
function keySummary(keys) {
  const cooling = keys.filter(k => k.status.cooling_secs > 0).length;
  const parts = [`${keys.length} key${keys.length === 1 ? '' : 's'}`];
  if (keys.length) parts.push(`<span style="color:#4ade80">${keys.length - cooling} OK</span>`);
  if (cooling) parts.push(`<span style="color:#f87171">${cooling} cooling</span>`);
  return parts.join(' · ');
}

async function loadProviders() {
  providers = await api('/admin/providers');
  const keyLists = await Promise.all(providers.map(p => api(`/admin/keys/${p.id}`).catch(() => [])));
  const tb = document.getElementById('providers-body');
  tb.innerHTML = providers.map((p, i) => `<tr>
    <td><strong>${esc(p.name)}</strong>${p.enabled ? '' : ' <span style="color:#888">(disabled)</span>'}</td>
    <td style="white-space:nowrap">${keySummary(keyLists[i])}</td>
    <td>
      <div><span class="url-label">OpenAI</span><span class="url">${esc(p.base_url)}</span></div>
      <div><span class="url-label">Anthropic</span><span class="url">${esc(p.anthropic_base_url || '(derived)')}</span></div>
    </td>
    <td>${esc(p.models || '-')}</td>
    <td class="actions">
      <button class="btn-sm secondary" onclick="showKeys(${p.id})">Manage Keys</button>
      <button class="btn-danger btn-sm" onclick="deleteProvider(${p.id})">Delete</button>
    </td>
  </tr>`).join('');
}
async function createProvider() {
  await api('/admin/providers', { method:'POST', body: JSON.stringify({
    name: document.getElementById('p-name').value,
    base_url: document.getElementById('p-url').value,
    anthropic_base_url: document.getElementById('p-aurl').value,
    user_agent: document.getElementById('p-ua').value,
    models: document.getElementById('p-models').value,
    enabled: true
  })});
  loadProviders();
}
async function deleteProvider(id) {
  if (!confirm('Delete provider?')) return;
  await api('/admin/providers/'+id, { method:'DELETE' });
  loadProviders();
}

// Models
function renderModels(ids) {
  document.getElementById('models-body').innerHTML = ids.map(id => `<tr><td><code>${esc(id)}</code></td></tr>`).join('')
    || '<tr><td style="color:#666">No models yet. Add a provider and a key.</td></tr>';
}
async function loadModels() { renderModels(await api('/admin/catalog')); }
async function refreshModels() { renderModels(await api('/admin/catalog/refresh', { method:'POST' })); }

// Usage
// All users, for today / this month / this year in this browser's time zone
let usagePeriod = 'day';
const PERIOD_NAMES = { day: 'Today', month: 'This month', year: 'This year' };
async function loadUsage(period = usagePeriod) {
  usagePeriod = period;
  document.querySelectorAll('[data-period]').forEach(b => b.classList.toggle('active', b.dataset.period === period));
  const usage = await api(`/admin/usage?period=${period}&tz=${-new Date().getTimezoneOffset()}`);
  const total = UsageChart.render(document.getElementById('usage-chart'), usage.series, period);
  document.getElementById('usage-total').textContent = `${PERIOD_NAMES[period]}: ${total.tokens.toLocaleString()} tokens · ${total.requests.toLocaleString()} requests`;
  document.getElementById('usage-body').innerHTML = usage.rows.map(u =>
    `<tr><td>${esc(u.username)}</td><td>${esc(u.model)}</td><td>${u.requests.toLocaleString()}</td><td>${u.total_tokens.toLocaleString()}</td></tr>`
  ).join('') || `<tr><td colspan="4" style="color:#666">No usage ${PERIOD_NAMES[period].toLowerCase()}</td></tr>`;
}

// Limits
let allUsers = [];
async function loadLimits() {
  const limits = await api('/admin/limits');
  if (!allUsers.length) {
    allUsers = await api('/admin/users');
    document.getElementById('lim-user').innerHTML = allUsers.map(u => `<option value="${u.id}">${esc(u.username)}</option>`).join('');
  }
  const windows = { 18000: '5 hours', 86400: 'day', 604800: 'week' };
  // "30,000 / 110,000,000" — usage in the current window against the cap
  const cell = (used, max) => {
    if (max < 0) return `${used.toLocaleString()} <span style="color:#666">/ ∞</span>`;
    const color = used >= max ? '#f87171' : used >= max * 0.8 ? '#fbbf24' : '#e0e0e0';
    return `<span style="color:${color}">${used.toLocaleString()}</span> <span style="color:#666">/ ${max.toLocaleString()}</span>`;
  };
  document.getElementById('limits-body').innerHTML = limits.map(l => {
    const uname = allUsers.find(u => u.id === l.user_id)?.username || l.user_id;
    return `<tr><td>${esc(uname)}</td><td>${esc(l.model)}</td><td>${windows[l.window_secs] || l.window_secs + 's'}</td>
      <td style="white-space:nowrap">${cell(l.used.requests, l.max_requests)}</td>
      <td style="white-space:nowrap">${cell(l.used.total_tokens, l.max_tokens)}</td>
      <td class="actions"><button class="btn-danger btn-sm" onclick="deleteLimit(${l.id})">Delete</button></td></tr>`;
  }).join('') || '<tr><td colspan="6" style="color:#666">No limits set</td></tr>';
}
// Empty means unlimited (-1); 0 is a real limit that blocks the model
function limitValue(id) {
  const v = parseInt(document.getElementById(id).value, 10);
  return Number.isNaN(v) ? -1 : v;
}
async function createLimit() {
  try {
    await api('/admin/limits', { method:'POST', body: JSON.stringify({
      user_id: parseInt(document.getElementById('lim-user').value),
      model: document.getElementById('lim-model').value || '*',
      window_secs: parseInt(document.getElementById('lim-window').value),
      max_requests: limitValue('lim-req'),
      max_tokens: limitValue('lim-tok')
    })});
  } catch (e) {
    alert('Could not set the limit. The model must be a full id from the Models tab (e.g. kimi/k3), a provider followed by /* (e.g. kimi/*), or * for all.');
  }
  loadLimits();
}
async function deleteLimit(id) {
  if (!confirm('Delete limit?')) return;
  await api('/admin/limits/'+id, { method:'DELETE' });
  loadLimits();
}

// Keys
let currentProviderId = null;
let keysTimer = null;
async function showKeys(pid) {
  currentProviderId = pid;
  const pname = providers.find(p => p.id === pid)?.name ?? pid;
  document.getElementById('keys-title').textContent = `Keys for "${pname}"`;
  document.getElementById('keys-section').classList.remove('hidden');
  await renderKeys();
  document.getElementById('keys-section').scrollIntoView({ behavior: 'smooth' });
  // Status changes as requests come in; keep it fresh while the list is open
  clearInterval(keysTimer);
  keysTimer = setInterval(() => { if (!document.hidden) renderKeys().catch(() => {}); }, 10000);
}

const FAIL_REASONS = { '401': 'invalid key', '403': 'forbidden', '402': 'no balance', '429': 'rate limited',
  '502': 'upstream error', '503': 'upstream error', '504': 'upstream timeout', 'network error': 'network error' };

function ago(unix) {
  if (!unix) return '-';
  const s = Math.max(0, Math.floor(Date.now() / 1000 - unix));
  if (s < 60) return s + 's ago';
  if (s < 3600) return Math.floor(s / 60) + 'm ago';
  if (s < 86400) return Math.floor(s / 3600) + 'h ago';
  return Math.floor(s / 86400) + 'd ago';
}

function keyStatus(st) {
  const reason = FAIL_REASONS[st.last_result] || st.last_result;
  if (st.cooling_secs > 0) {
    const left = st.cooling_secs >= 60 ? Math.ceil(st.cooling_secs / 60) + 'm' : st.cooling_secs + 's';
    return `<span style="color:#f87171">Cooling ${left}</span> <span style="color:#888">(${esc(reason)})</span>`;
  }
  if (!st.last_result) return '<span style="color:#888">Unused</span>';
  if (FAIL_REASONS[st.last_result]) return `<span style="color:#fbbf24">Ready</span> <span style="color:#888">(last: ${esc(reason)})</span>`;
  return '<span style="color:#4ade80">OK</span>';
}

// "5h 41% · week 29%", colored by how close each window is to its limit
function quotaCell(windows) {
  if (!windows) return '<span style="color:#666">n/a</span>';
  return windows.map(w => {
    const pct = Math.round(w.used_percent);
    const color = pct >= 80 ? '#f87171' : pct >= 50 ? '#fbbf24' : '#4ade80';
    const reset = w.resets_at ? 'resets ' + new Date(w.resets_at * 1000).toLocaleString() : '';
    return `<span title="${reset}"><span style="color:#888">${esc(w.label)}</span> <span style="color:${color}">${pct}%</span></span>`;
  }).join(' · ');
}

async function renderKeys() {
  const keys = await api(`/admin/keys/${currentProviderId}`);
  // Requests go to the first key (by priority) that isn't cooling down
  const inUse = keys.find(k => k.enabled && k.status.cooling_secs === 0);
  document.getElementById('keys-body').innerHTML = keys.map(k => `<tr>
    <td>${k.id}</td><td>${esc(k.label || '-')}${k === inUse ? ' <span style="color:#2563eb">● in use</span>' : ''}</td>
    <td><code style="font-size:11px">${esc(k.api_key)}</code></td>
    <td>${k.priority}</td>
    <td>${keyStatus(k.status)}</td>
    <td style="white-space:nowrap">${quotaCell(k.quota)}</td>
    <td>${ago(k.status.last_used)}</td>
    <td>${k.status.ok} / ${k.status.failed}</td>
    <td class="actions"><button class="btn-danger btn-sm" onclick="deleteKey(${k.id})">Delete</button></td>
  </tr>`).join('');
}
async function createKey() {
  await api(`/admin/keys/${currentProviderId}`, { method:'POST', body: JSON.stringify({
    api_key: document.getElementById('k-key').value,
    label: document.getElementById('k-label').value || null,
    priority: parseInt(document.getElementById('k-priority').value) || 0
  })});
  document.getElementById('k-key').value = '';
  await renderKeys();
  loadProviders();
}
async function deleteKey(id) {
  if (!confirm('Delete key?')) return;
  await api('/admin/keys/delete/'+id, { method:'DELETE' });
  await renderKeys();
  loadProviders();
}

// Public access
const TUNNEL_STATES = { off: ['Off', '#888'], starting: ['Connecting…', '#fbbf24'], connected: ['Connected', '#4ade80'], reconnecting: ['Reconnecting…', '#fbbf24'], stopped: ['Stopped (retrying)', '#f87171'] };
let tunnelTimer = null;
async function loadTunnel() {
  const st = await api('/admin/tunnel');
  const urlInput = document.getElementById('public-url');
  if (!urlInput.dataset.loaded) {
    urlInput.dataset.loaded = '1';
    urlInput.value = (await api('/user/config')).public_url || '';
  }
  document.getElementById('tunnel-unavailable').classList.toggle('hidden', st.available);
  const [label, color] = TUNNEL_STATES[st.state] || [st.state, '#888'];
  const el = document.getElementById('tunnel-state');
  el.textContent = st.configured || st.state !== 'off' ? label : 'Not configured';
  el.style.color = color;
  document.getElementById('tunnel-log').textContent = st.log.join('\n');
  await loadFrp();
  clearTimeout(tunnelTimer);
  if (!document.getElementById('tab-tunnel').classList.contains('hidden')) tunnelTimer = setTimeout(loadTunnel, 3000);
}
async function savePublicUrl() {
  const msg = document.getElementById('public-url-msg');
  try {
    await api('/admin/public-url', { method: 'PUT', body: JSON.stringify({ url: document.getElementById('public-url').value }) });
    msg.textContent = 'Saved. The home page now shows this address.'; msg.style.color = '#4ade80';
  } catch (e) {
    msg.textContent = 'Must be a bare https:// address with no path.'; msg.style.color = '#f87171';
  }
}
async function saveTunnel() {
  const token = document.getElementById('tunnel-token').value.trim();
  if (!token) return;
  await api('/admin/tunnel', { method: 'PUT', body: JSON.stringify({ token }) });
  document.getElementById('tunnel-token').value = '';
  loadTunnel();
}
async function disableTunnel() {
  if (!confirm('Turn off public access?')) return;
  await api('/admin/tunnel', { method: 'PUT', body: JSON.stringify({ token: '' }) });
  loadTunnel();
}

const CERT_STATES = { none: ['None', '#888'], issuing: ['Requesting…', '#fbbf24'], valid: ['Valid', '#4ade80'], failed: ['Failed (retrying hourly)', '#f87171'] };
async function loadFrp() {
  const st = await api('/admin/frp');
  document.getElementById('frp-unavailable').classList.toggle('hidden', st.available);
  const fields = { 'frp-server': st.server, 'frp-port': st.remote_port || '', 'frp-domain': st.domain };
  for (const [id, v] of Object.entries(fields)) {
    const el = document.getElementById(id);
    if (!el.dataset.loaded) { el.dataset.loaded = '1'; el.value = v; }
  }
  document.getElementById('frp-token').placeholder = st.has_token ? 'frp token (saved)' : 'frp token';
  document.getElementById('frp-dns-token').placeholder = st.has_dns_token ? 'Cloudflare DNS API token (saved)' : 'Cloudflare DNS API token';
  const [label, color] = TUNNEL_STATES[st.state] || [st.state, '#888'];
  const el = document.getElementById('frp-state');
  el.textContent = st.configured || st.state !== 'off' ? label : 'Not configured';
  el.style.color = color;
  const [cl, cc] = CERT_STATES[st.cert] || [st.cert, '#888'];
  const cert = document.getElementById('frp-cert');
  cert.textContent = cl + (st.cert === 'valid' && st.cert_expires ? `, until ${new Date(st.cert_expires * 1000).toLocaleDateString()}` : '');
  cert.style.color = cc;
  document.getElementById('frp-url').textContent = st.url || '';
  document.getElementById('frp-log').textContent = st.log.join('\n');
}
async function saveFrp() {
  const msg = document.getElementById('frp-msg');
  const body = {
    server: document.getElementById('frp-server').value.trim(),
    token: document.getElementById('frp-token').value.trim(),
    remote_port: Number(document.getElementById('frp-port').value) || 0,
    domain: document.getElementById('frp-domain').value.trim(),
    dns_token: document.getElementById('frp-dns-token').value.trim(),
  };
  try {
    await api('/admin/frp', { method: 'PUT', body: JSON.stringify(body) });
    document.getElementById('frp-token').value = '';
    document.getElementById('frp-dns-token').value = '';
    msg.textContent = 'Saved.'; msg.style.color = '#4ade80';
  } catch (e) {
    msg.textContent = 'The server must be host:port.'; msg.style.color = '#f87171';
  }
  loadFrp();
}
async function disableFrp() {
  if (!confirm('Turn off frp?')) return;
  await api('/admin/frp', { method: 'PUT', body: JSON.stringify({ server: '' }) });
  document.getElementById('frp-server').value = '';
  loadFrp();
}

function logout() {
  token = null;
  localStorage.removeItem('admin_token');
  location.reload();
}
