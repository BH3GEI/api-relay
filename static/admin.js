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
  if (name === 'requests') loadRequests();
  ['users','providers','models','usage','requests','limits','tunnel'].forEach(t => {
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
    <td>${u.is_admin ? '是' : '否'}</td>
    <td>${u.record_content ? '是' : '<span style="color:#fbbf24">已关闭</span>'}</td>
    <td><button class="secondary btn-sm" onclick="setPassword(${u.id})">设置密码</button> <button class="btn-danger btn-sm" onclick="deleteUser(${u.id})">删除</button></td>
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
  const pw = prompt('新密码（至少 8 位）：');
  if (pw === null) return;
  try {
    await api('/admin/users/'+id+'/password', { method:'PUT', body: JSON.stringify({ password: pw }) });
    alert('密码已设置');
  } catch (e) { alert('失败：' + (e.message || '密码太短？')); }
}
async function deleteUser(id) {
  if (!confirm('删除该用户？')) return;
  await api('/admin/users/'+id, { method:'DELETE' });
  loadUsers();
}

// Providers
// "8 keys · 7 OK · 1 cooling" for the providers table
function keySummary(keys) {
  const cooling = keys.filter(k => k.status.cooling_secs > 0).length;
  const parts = [`${keys.length} 把 Key`];
  if (keys.length) parts.push(`<span style="color:#4ade80">${keys.length - cooling} 把正常</span>`);
  if (cooling) parts.push(`<span style="color:#f87171">${cooling} 把冷却中</span>`);
  return parts.join(' · ');
}

async function loadProviders() {
  providers = await api('/admin/providers');
  document.getElementById('onboard').classList.toggle('hidden', providers.length > 0);
  const keyLists = await Promise.all(providers.map(p => api(`/admin/keys/${p.id}`).catch(() => [])));
  const tb = document.getElementById('providers-body');
  tb.innerHTML = providers.map((p, i) => `<tr>
    <td><strong>${esc(p.name)}</strong>${p.enabled ? '' : ' <span style="color:#888">（已停用）</span>'}</td>
    <td style="white-space:nowrap">${keySummary(keyLists[i])}</td>
    <td>
      <div><span class="url-label">OpenAI</span><span class="url">${esc(p.base_url)}</span></div>
      <div><span class="url-label">Anthropic</span><span class="url">${esc(p.anthropic_base_url || '(derived)')}</span></div>
    </td>
    <td>${esc(p.models || '-')}</td>
    <td class="actions">
      <button class="btn-sm secondary" onclick="showKeys(${p.id})">管理 Key</button>
      <button class="btn-danger btn-sm" onclick="deleteProvider(${p.id})">删除</button>
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
  if (!confirm('删除该服务商？')) return;
  await api('/admin/providers/'+id, { method:'DELETE' });
  loadProviders();
}

// Models
function renderModels(ids) {
  document.getElementById('models-body').innerHTML = ids.map(id => `<tr><td><code>${esc(id)}</code></td></tr>`).join('')
    || '<tr><td style="color:#666">还没有模型。请先添加服务商和 Key。</td></tr>';
}
async function loadModels() { renderModels(await api('/admin/catalog')); loadGroups(); }
async function refreshModels() { renderModels(await api('/admin/catalog/refresh', { method:'POST' })); loadGroups(); }

// Model groups
let groups = [], editingGroup = null;
async function loadGroups() {
  groups = await api('/admin/groups');
  document.getElementById('groups-body').innerHTML = groups.map(g => `<tr>
    <td><code>${esc(g.name)}</code></td>
    <td>${g.models.map(m => `<code${g.unknown.includes(m) ? ' style="color:#f87171" title="当前不在模型列表中，会被跳过"' : ''}>${esc(m)}</code>`).join(' → ')}</td>
    <td class="actions">
      <button class="btn-sm secondary" onclick="editGroup(${g.id})">编辑</button>
      <button class="btn-danger btn-sm" onclick="deleteGroup(${g.id})">删除</button>
    </td></tr>`).join('') || '<tr><td colspan="3" style="color:#666">还没有模型组。</td></tr>';
}
function editGroup(id) {
  const g = groups.find(g => g.id === id);
  editingGroup = id;
  document.getElementById('g-name').value = g.name;
  document.getElementById('g-models').value = g.models.join(', ');
  document.getElementById('g-save').textContent = '保存模型组';
  document.getElementById('g-cancel').classList.remove('hidden');
}
function resetGroupForm() {
  editingGroup = null;
  document.getElementById('g-name').value = '';
  document.getElementById('g-models').value = '';
  document.getElementById('g-save').textContent = '添加模型组';
  document.getElementById('g-cancel').classList.add('hidden');
  document.getElementById('g-msg').textContent = '';
}
async function saveGroup() {
  const body = JSON.stringify({ name: document.getElementById('g-name').value, models: document.getElementById('g-models').value });
  try {
    if (editingGroup) await api('/admin/groups/' + editingGroup, { method:'PUT', body });
    else await api('/admin/groups', { method:'POST', body });
    resetGroupForm();
  } catch (e) {
    document.getElementById('g-msg').textContent = '未保存：组名必须是合法的模型名，且不能与任何服务商模型或其他组重名；成员必须是模型 id（不能是组）。';
  }
  loadGroups();
}
async function deleteGroup(id) {
  if (!confirm('删除该模型组？')) return;
  await api('/admin/groups/' + id, { method:'DELETE' });
  loadGroups();
}

// Usage
// All users, or one; any range, split and filtered
let usageView = null;
function loadUsage() {
  usageView ||= UsageView.mount(document.getElementById('usage-view'), { lang: 'zh', admin: true, fetch: q => api('/admin/usage?' + q) });
  usageView.load();
}

// Limits
let allUsers = [];
async function loadLimits() {
  const limits = await api('/admin/limits');
  if (!allUsers.length) {
    allUsers = await api('/admin/users');
    document.getElementById('lim-user').innerHTML = allUsers.map(u => `<option value="${u.id}">${esc(u.username)}</option>`).join('');
  }
  const windows = { 18000: '每 5 小时', 86400: '每天', 604800: '每周' };
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
      <td class="actions"><button class="btn-danger btn-sm" onclick="deleteLimit(${l.id})">删除</button></td></tr>`;
  }).join('') || '<tr><td colspan="6" style="color:#666">还没有设置限额</td></tr>';
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
    alert('无法设置限额。模型必须是「模型」页中的完整 id（如 kimi/k3），或 服务商/*（如 kimi/*），或 * 表示全部。');
  }
  loadLimits();
}
async function deleteLimit(id) {
  if (!confirm('删除该限额？')) return;
  await api('/admin/limits/'+id, { method:'DELETE' });
  loadLimits();
}

// Keys
let currentProviderId = null;
let keysTimer = null;
async function showKeys(pid) {
  currentProviderId = pid;
  const pname = providers.find(p => p.id === pid)?.name ?? pid;
  document.getElementById('keys-title').textContent = `「${pname}」的 Key`;
  document.getElementById('keys-section').classList.remove('hidden');
  await renderKeys();
  document.getElementById('keys-section').scrollIntoView({ behavior: 'smooth' });
  // Status changes as requests come in; keep it fresh while the list is open
  clearInterval(keysTimer);
  keysTimer = setInterval(() => { if (!document.hidden) renderKeys().catch(() => {}); }, 10000);
}

const FAIL_REASONS = { '401': 'Key 无效', '403': '无权限', '402': '余额不足', '429': '被限流',
  '502': '上游出错', '503': '上游出错', '504': '上游超时', 'network error': '网络错误' };

function ago(unix) {
  if (!unix) return '-';
  const s = Math.max(0, Math.floor(Date.now() / 1000 - unix));
  if (s < 60) return s + ' 秒前';
  if (s < 3600) return Math.floor(s / 60) + ' 分钟前';
  if (s < 86400) return Math.floor(s / 3600) + ' 小时前';
  return Math.floor(s / 86400) + ' 天前';
}

function keyStatus(st) {
  const reason = FAIL_REASONS[st.last_result] || st.last_result;
  if (st.cooling_secs > 0) {
    const left = st.cooling_secs >= 60 ? Math.ceil(st.cooling_secs / 60) + 'm' : st.cooling_secs + 's';
    return `<span style="color:#f87171">冷却中 ${left}</span> <span style="color:#888">(${esc(reason)})</span>`;
  }
  if (!st.last_result) return '<span style="color:#888">未使用</span>';
  if (FAIL_REASONS[st.last_result]) return `<span style="color:#fbbf24">可用</span> <span style="color:#888">（上次：${esc(reason)}）</span>`;
  return '<span style="color:#4ade80">正常</span>';
}

// "5h 41% · week 29%", colored by how close each window is to its limit
function quotaCell(windows) {
  if (!windows) return '<span style="color:#666">n/a</span>';
  return windows.map(w => {
    const pct = Math.round(w.used_percent);
    const color = pct >= 80 ? '#f87171' : pct >= 50 ? '#fbbf24' : '#4ade80';
    const reset = w.resets_at ? '重置于 ' + new Date(w.resets_at * 1000).toLocaleString() : '';
    return `<span title="${reset}"><span style="color:#888">${esc(w.label)}</span> <span style="color:${color}">${pct}%</span></span>`;
  }).join(' · ');
}

async function renderKeys() {
  const keys = await api(`/admin/keys/${currentProviderId}`);
  // Requests go to the first key (by priority) that isn't cooling down
  const inUse = keys.find(k => k.enabled && k.status.cooling_secs === 0);
  document.getElementById('keys-body').innerHTML = keys.map(k => `<tr>
    <td>${k.id}</td><td>${esc(k.label || '-')}${k === inUse ? ' <span style="color:#2563eb">● 使用中</span>' : ''}</td>
    <td><code style="font-size:11px">${esc(k.api_key)}</code></td>
    <td><input type="number" value="${k.priority}" style="width:60px;margin:0" title="数字小的优先" onchange="setPriority(${k.id}, this.value)"></td>
    <td>${keyStatus(k.status)}</td>
    <td style="white-space:nowrap">${quotaCell(k.quota)}</td>
    <td>${ago(k.status.last_used)}</td>
    <td>${k.status.ok} / ${k.status.failed}</td>
    <td class="actions"><button class="btn-danger btn-sm" onclick="deleteKey(${k.id})">删除</button></td>
  </tr>`).join('');
}
async function setPriority(id, value) {
  await api(`/admin/keys/priority/${id}`, { method: 'PUT', body: JSON.stringify({ priority: parseInt(value) || 0 }) });
  await renderKeys();
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
  if (!confirm('删除该 Key？')) return;
  await api('/admin/keys/delete/'+id, { method:'DELETE' });
  await renderKeys();
  loadProviders();
}

// Public access
const TUNNEL_STATES = { off: ['关闭', '#888'], starting: ['连接中…', '#fbbf24'], connected: ['已连接', '#4ade80'], reconnecting: ['重连中…', '#fbbf24'], stopped: ['已停止（重试中）', '#f87171'] };
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
  el.textContent = st.configured || st.state !== 'off' ? label : '未配置';
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
    msg.textContent = '已保存，首页会显示这个地址。'; msg.style.color = '#4ade80';
  } catch (e) {
    msg.textContent = '必须是不带路径的 https:// 地址。'; msg.style.color = '#f87171';
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
  if (!confirm('关闭公网访问？')) return;
  await api('/admin/tunnel', { method: 'PUT', body: JSON.stringify({ token: '' }) });
  loadTunnel();
}

const CERT_STATES = { none: ['无', '#888'], issuing: ['申请中…', '#fbbf24'], valid: ['有效', '#4ade80'], failed: ['失败（每小时重试）', '#f87171'] };
async function loadFrp() {
  const st = await api('/admin/frp');
  document.getElementById('frp-unavailable').classList.toggle('hidden', st.available);
  const fields = { 'frp-server': st.server, 'frp-port': st.remote_port || '', 'frp-domain': st.domain };
  for (const [id, v] of Object.entries(fields)) {
    const el = document.getElementById(id);
    if (!el.dataset.loaded) { el.dataset.loaded = '1'; el.value = v; }
  }
  document.getElementById('frp-token').placeholder = st.has_token ? 'frp token（已保存）' : 'frp token';
  document.getElementById('frp-dns-token').placeholder = st.has_dns_token ? 'Cloudflare DNS API token（已保存）' : 'Cloudflare DNS API token';
  const [label, color] = TUNNEL_STATES[st.state] || [st.state, '#888'];
  const el = document.getElementById('frp-state');
  el.textContent = st.configured || st.state !== 'off' ? label : '未配置';
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
    msg.textContent = '已保存。'; msg.style.color = '#4ade80';
  } catch (e) {
    msg.textContent = '服务器格式必须是 主机:端口。'; msg.style.color = '#f87171';
  }
  loadFrp();
}
async function disableFrp() {
  if (!confirm('关闭 frp？')) return;
  await api('/admin/frp', { method: 'PUT', body: JSON.stringify({ server: '' }) });
  document.getElementById('frp-server').value = '';
  loadFrp();
}

// Recorded calls
// Newest first; "Load more" pages back from the oldest shown
let reqOldest = null;
const REQ_PAGE = 50;
function reqFilter() {
  const q = new URLSearchParams();
  const user = document.getElementById('req-user').value;
  const model = document.getElementById('req-model').value.trim();
  const status = document.getElementById('req-status').value;
  if (user) q.set('user', user);
  if (model) q.set('model', model);
  if (status) q.set('status', status);
  return q;
}
function fmtBytes(n) {
  if (n < 1024) return n + ' B';
  if (n < 1 << 20) return (n / 1024).toFixed(1) + ' KB';
  if (n < 1 << 30) return (n / (1 << 20)).toFixed(1) + ' MB';
  return (n / (1 << 30)).toFixed(2) + ' GB';
}
function fmtMs(ms) { return ms == null ? '-' : ms < 1000 ? ms + ' ms' : (ms / 1000).toFixed(1) + ' s'; }
function fmtTime(ms) { return new Date(ms).toLocaleString([], { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit', second: '2-digit' }); }
function statusCell(c) {
  if (c.status == null) return '<span style="color:#888" title="客户端在收到回答前断开">已断开</span>';
  const ok = c.status >= 200 && c.status < 300;
  const cut = ok && !c.complete ? ' <span style="color:#fbbf24" title="回复未结束时客户端挂断">中断</span>' : '';
  return `<span style="color:${ok ? '#4ade80' : '#f87171'}" title="${esc(c.error || '')}">${c.status}</span>${cut}`;
}
async function loadRequests(more) {
  if (!more) {
    reqOldest = null;
    const exp = document.getElementById('req-export');
    exp.textContent = '导出 JSONL';
    api('/admin/requests/export/size?' + reqFilter()).then(s => { exp.textContent = `导出 JSONL（约 ${fmtBytes(s.bytes)}）`; }).catch(() => {});
    api('/admin/requests/stats').then(s => {
      const since = s.since ? '（自 ' + new Date(s.since).toLocaleDateString() + ' 起）' : '';
      document.getElementById('req-stats').textContent =
        `${s.requests.toLocaleString()} 次调用已记录${since}，其中 ${s.with_content.toLocaleString()} 次含内容 · ${s.parts.toLocaleString()} 个共享片段 · 占用 ${fmtBytes(s.bytes)}`;
    }).catch(() => {});
    const sel = document.getElementById('req-user');
    if (sel.options.length === 1) {
      (await api('/admin/users')).forEach(u => sel.add(new Option(u.username, u.id)));
    }
  }
  const q = reqFilter();
  q.set('limit', REQ_PAGE);
  if (more && reqOldest) q.set('before', reqOldest);
  const rows = await api('/admin/requests?' + q);
  if (rows.length) reqOldest = rows[rows.length - 1].id;
  const html = rows.map(c => `<tr onclick="showRequest(${c.id})">
    <td style="white-space:nowrap">${fmtTime(c.started_at)}</td>
    <td>${esc(c.username)}</td>
    <td><code>${esc(c.model || '-')}</code>${c.provider && c.upstream_model && c.model !== c.provider + '/' + c.upstream_model && c.model !== c.upstream_model ? ` <span style="color:#888">→ <code>${esc(c.provider + '/' + c.upstream_model)}</code></span>` : ''}${c.stream ? ' <span style="color:#666">stream</span>' : ''}${c.endpoint === 'chat_completions' ? '' : ` <span style="color:#666">${esc(c.endpoint)}</span>`}</td>
    <td>${statusCell(c)}</td>
    <td><code>${esc(c.key || '-')}</code></td>
    <td>${esc(c.entry)}</td>
    <td style="white-space:nowrap">${c.input_tokens.toLocaleString()} / ${c.output_tokens.toLocaleString()}</td>
    <td>${fmtMs(c.first_byte_ms)}</td>
    <td>${fmtMs(c.total_ms)}</td>
    <td style="white-space:nowrap">${fmtBytes(c.request_bytes)} / ${fmtBytes(c.response_bytes)}${c.content ? '' : ' <span style="color:#666" title="该用户关闭了内容记录">无内容</span>'}</td>
  </tr>`).join('');
  const tb = document.getElementById('req-body');
  if (more) tb.insertAdjacentHTML('beforeend', html);
  else tb.innerHTML = html || '<tr><td colspan="10" style="color:#666">还没有调用记录。</td></tr>';
  document.getElementById('req-more').classList.toggle('hidden', rows.length < REQ_PAGE);
}

// A message's text: plain content, or the text parts of a list, other parts as JSON
function partText(p) {
  if (typeof p === 'string') return p;
  if (p && typeof p.text === 'string') return p.text;
  return JSON.stringify(p, null, 2);
}
function messageText(m) {
  const c = m.content ?? m.parts ?? m;
  const text = Array.isArray(c) ? c.map(partText).join('\n\n') : partText(c);
  const calls = m.tool_calls ? '\n\n' + JSON.stringify(m.tool_calls, null, 2) : '';
  return text + calls;
}
// What a streamed answer said, put together from its events
function streamText(raw) {
  let text = '';
  for (const line of raw.split('\n')) {
    if (!line.startsWith('data:')) continue;
    let ev;
    try { ev = JSON.parse(line.slice(5)); } catch { continue; }
    const d = ev.choices?.[0]?.delta;
    if (d) text += (d.reasoning_content || '') + (d.content || '') + (d.tool_calls ? JSON.stringify(d.tool_calls) : '');
    if (ev.type === 'content_block_delta') text += ev.delta?.text || ev.delta?.thinking || ev.delta?.partial_json || '';
    if (ev.type === 'response.output_text.delta') text += ev.delta || '';
  }
  return text;
}
async function showRequest(id) {
  const d = await api('/admin/requests/' + id);
  const box = document.getElementById('req-detail');
  const meta = [
    ['时间', fmtTime(d.started_at)], ['用户', d.username], ['接口', d.endpoint + (d.stream ? '（流式）' : '')],
    ['模型', `${d.model} → ${d.provider ? d.provider + ' / ' + d.upstream_model : '-'}`], ['状态', `${d.status ?? '客户端已断开'}${d.error ? ' · ' + d.error : ''}${d.complete ? '' : ' · 未完整'}`],
    ['Key', d.key || '-'], ['入口', `${d.entry} · ${d.client_ip || '-'}`], ['User-Agent', d.user_agent || '-'],
    ['Token', `输入 ${d.input_tokens.toLocaleString()} / 输出 ${d.output_tokens.toLocaleString()}`],
    ['耗时', `首字节 ${fmtMs(d.first_byte_ms)} · 总计 ${fmtMs(d.total_ms)}`],
    ['大小', `请求 ${fmtBytes(d.request_bytes)} / 回复 ${fmtBytes(d.response_bytes)}`],
  ].map(([k, v]) => `<div>${k}</div><div>${esc(v)}</div>`).join('');
  const attempts = (d.attempts || []).map(a => `<tr><td><code>${esc(a.key)}</code></td><td>${a.status ?? '网络错误'}</td><td>${esc(({ Busy: '并发已满', OutOfQuota: '套餐额度用完', Refused: 'Key 被拒绝', Other: '限流或上游出错' })[a.failure] || a.failure || (a.served ? '已应答' : ''))}</td><td>${fmtMs(a.ms)}</td><td style="font-size:11px;color:#888">${esc(a.detail || '')}</td></tr>`).join('');

  let request = '<p style="color:#666">未记录：该用户关闭了内容记录。</p>';
  if (d.request != null) {
    const r = d.request;
    const convo = [];
    if (r.system) convo.push(['system', Array.isArray(r.system) ? r.system.map(partText).join('\n\n') : r.system]);
    if (r.instructions) convo.push(['instructions', r.instructions]);
    const msgs = Array.isArray(r.messages) ? r.messages : Array.isArray(r.input) ? r.input : Array.isArray(r.contents) ? r.contents : [];
    msgs.forEach(m => convo.push([m.role || m.type || '', messageText(m)]));
    request = `<p style="color:#888;font-size:13px">${msgs.length} messages${r.tools ? ' · ' + r.tools.length + ' tools' : ''}</p>` +
      convo.map(([role, text]) => `<div class="msg-role">${esc(role)}</div><pre>${esc(text)}</pre>`).join('') +
      `<details><summary>完整请求 JSON</summary><pre>${esc(JSON.stringify(r, null, 2))}</pre></details>`;
  }
  let response = '<p style="color:#666">未记录。</p>';
  if (d.response != null) {
    const assembled = d.stream ? streamText(d.response) : '';
    let pretty = d.response;
    try { pretty = JSON.stringify(JSON.parse(d.response), null, 2); } catch {}
    response = (assembled ? `<pre>${esc(assembled)}</pre><details><summary>原始流</summary><pre>${esc(d.response)}</pre></details>` : `<pre>${esc(pretty)}</pre>`);
  }
  box.innerHTML = `<div style="display:flex;justify-content:space-between;align-items:center"><h2 style="margin-top:0">调用 #${d.id}</h2><button class="secondary" onclick="closeRequest()">关闭</button></div>
    <div class="meta-grid">${meta}</div>
    <h2>尝试过程</h2>${attempts ? `<table><thead><tr><th>Key</th><th>状态</th><th>结果</th><th>耗时</th><th>上游返回</th></tr></thead><tbody>${attempts}</tbody></table>` : '<p style="color:#666">没有尝试任何上游。</p>'}
    <h2>请求</h2>${request}
    <h2>回复</h2>${response}
    <details><summary>客户端请求头</summary><pre>${esc(JSON.stringify(d.headers, null, 2))}</pre></details>`;
  box.classList.remove('hidden');
  box.scrollTop = 0;
}
function closeRequest() { document.getElementById('req-detail').classList.add('hidden'); }
document.addEventListener('keydown', e => { if (e.key === 'Escape') closeRequest(); });
// A plain link, so the browser saves it straight to disk; LazyCat's gateway vouches for the admin
function exportRequests() {
  location.href = BASE + '/admin/requests/export?' + reqFilter();
}

// Each cell knows its column's name, for the phone layout (admin.css)
function labelCells() {
  document.querySelectorAll('table').forEach(t => {
    const heads = [...t.querySelectorAll('thead th')].map(th => th.textContent.trim());
    t.querySelectorAll('tbody tr').forEach(tr => [...tr.children].forEach((td, i) => {
      if (heads[i] && td.colSpan === 1) td.dataset.label = heads[i];
    }));
  });
}
new MutationObserver(labelCells).observe(document.body, { childList: true, subtree: true });

function logout() {
  token = null;
  localStorage.removeItem('admin_token');
  location.reload();
}
