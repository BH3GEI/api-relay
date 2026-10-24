// What users configure is SHOW, the public address when set
let SHOW = BASE;
document.querySelectorAll('.base-url').forEach(el => el.textContent = BASE);
let token = localStorage.getItem('token');
let apiKey = '';

// The public address has no admin and no LazyCat sign-in; registration only bootstraps the first user
let PUBLIC = false;
fetch(BASE + '/auth/options').then(r => r.ok ? r.json() : {}).catch(() => ({})).then(opts => {
  PUBLIC = opts.public === true;
  if (!opts.register) document.getElementById('tab-register-btn').classList.add('hidden');
  // On LazyCat, sign in as the current LazyCat user; elsewhere fall back to the password form
  const sso = PUBLIC ? Promise.resolve(null) : fetch(BASE + '/auth/sso').then(r => r.ok ? r.json() : null).catch(() => null);
  return sso.then(res => {
    if (res) { token = res.token; localStorage.setItem('token', token); }
    if (token) loadDashboard();
  });
});

function showAuthTab(tab) {
  document.getElementById('login-form').classList.toggle('hidden', tab !== 'login');
  document.getElementById('register-form').classList.toggle('hidden', tab !== 'register');
  document.getElementById('tab-login-btn').classList.toggle('active', tab === 'login');
  document.getElementById('tab-register-btn').classList.toggle('active', tab === 'register');
  document.getElementById('auth-msg').classList.add('hidden');
}

async function login() {
  try {
    const res = await api('/auth/login', {
      method: 'POST',
      body: JSON.stringify({ username: document.getElementById('login-username').value, password: document.getElementById('login-password').value })
    });
    token = res.token;
    apiKey = res.api_key;
    localStorage.setItem('token', token);
    loadDashboard();
  } catch (e) { showMsg('auth-msg', 'Login failed: ' + e.message, false); }
}

async function register() {
  const pw = document.getElementById('reg-password').value;
  const pw2 = document.getElementById('reg-password2').value;
  if (pw !== pw2) { showMsg('auth-msg', 'Passwords do not match', false); return; }
  try {
    const res = await api('/auth/register', {
      method: 'POST',
      body: JSON.stringify({ username: document.getElementById('reg-username').value, email: document.getElementById('reg-email').value, password: pw })
    });
    token = res.token;
    apiKey = res.api_key;
    localStorage.setItem('token', token);
    loadDashboard();
  } catch (e) { showMsg('auth-msg', 'Register failed: ' + e.message, false); }
}

// The JWT payload says whether this user is an admin; only admins get the admin entry
function isAdmin() {
  try {
    const b64 = token.split('.')[1].replace(/-/g, '+').replace(/_/g, '/');
    return JSON.parse(atob(b64)).is_admin === true;
  } catch { return false; }
}

async function loadDashboard() {
  const showAdmin = isAdmin() && !PUBLIC;
  document.getElementById('admin-link').classList.toggle('hidden', !showAdmin);
  document.getElementById('admin-link-bottom').classList.toggle('hidden', !showAdmin);
  document.getElementById('auth-section').classList.add('hidden');
  document.getElementById('dashboard').classList.remove('hidden');
  try {
    const keyRes = await api('/user/api-key');
    apiKey = keyRes.api_key;
    document.getElementById('api-key-display').textContent = apiKey;
    try {
      const cfg = await api('/user/config');
      if (cfg.public_url) SHOW = cfg.public_url;
    } catch (e) {}
    document.querySelectorAll('.base-url').forEach(el => el.textContent = SHOW);
    document.getElementById('cfg-base').textContent = SHOW;
    document.getElementById('cfg-lan-row').classList.toggle('hidden', SHOW === BASE);
    document.getElementById('cfg-lan').textContent = BASE;
    document.getElementById('cfg-key').textContent = apiKey;

    loadUsage(usagePeriod);

    // Pool quota: "5h 62% left · resets 18:00"
    try {
      const q = await api('/user/quota');
      const lt = document.getElementById('limits-table');
      if (!q.limits.length) {
        lt.innerHTML = '<tr><td>所有模型</td><td style="color:#4ade80">不限量</td></tr>';
      } else {
        lt.innerHTML = q.limits.map(l => {
          const model = l.model === '*' ? '所有模型' : esc(l.model);
          if (l.max_requests === 0 || l.max_tokens === 0) return `<tr><td>${model}</td><td style="color:#f87171">不可用</td></tr>`;
          const parts = [];
          if (l.max_tokens > 0) parts.push(meter(l.used.total_tokens, l.max_tokens, 'token'));
          if (l.max_requests > 0) parts.push(meter(l.used.requests, l.max_requests, '次'));
          return `<tr><td>${model}<br><span style="color:#666">${windowLabel(l.window_secs)}</span></td><td>${parts.join('<br>') || '<span style="color:#4ade80">不限量</span>'}</td></tr>`;
        }).join('');
      }

      const qt = document.getElementById('quota-table');
      if (!q.providers.length) {
        qt.innerHTML = '<tr><td style="color:#666">上游没有提供余量信息</td><td></td></tr>';
      } else {
        const windows = ws => ws.map(w => {
          const left = Math.round(w.remaining_percent);
          const color = left <= 20 ? '#f87171' : left <= 50 ? '#fbbf24' : '#4ade80';
          const reset = w.resets_at ? ` <span style="color:#666">· ${new Date(w.resets_at * 1000).toLocaleString([], { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' })} 重置</span>` : '';
          return `<span style="color:#888">${esc(w.label)}</span> <span style="color:${color}">剩 ${left}%</span>${reset}`;
        }).join('<br>');
        // Each provider's total, then its keys one by one
        qt.innerHTML = q.providers.map(p =>
          `<tr><td><strong style="color:#e0e0e0">${esc(p.provider)}</strong><br><span style="color:#666">合计</span></td><td>${windows(p.windows)}</td></tr>` +
          p.keys.map(k => `<tr><td style="padding-left:22px">${esc(k.label)}</td><td>${windows(k.windows)}</td></tr>`).join('')
        ).join('');
      }
    } catch(e) {}

    const modelsRes = await fetch(BASE + '/models', { headers: { 'Authorization': 'Bearer ' + apiKey } });
    const models = await modelsRes.json();
    const list = document.getElementById('model-list');
    list.innerHTML = '';
    (models.data || []).forEach(m => {
      const li = document.createElement('li');
      li.textContent = m.id;
      list.appendChild(li);
    });

    const exModel = models.data?.[0]?.id || 'MODEL_NAME';
    // A sensible default for coding agents: Kimi's k3 when it's there
    const claudeModel = (models.data || []).find(m => m.id.endsWith('/k3'))?.id || exModel;

    document.getElementById('claude-code-example').textContent =
`export ANTHROPIC_BASE_URL=${SHOW}
export ANTHROPIC_AUTH_TOKEN=${apiKey}
export ANTHROPIC_MODEL=${claudeModel}
export ANTHROPIC_DEFAULT_HAIKU_MODEL=${claudeModel}
claude`;

    document.getElementById('python-example').textContent =
`from openai import OpenAI

client = OpenAI(
    base_url="${SHOW}/v1",
    api_key="${apiKey}",
)

response = client.chat.completions.create(
    model="${exModel}",
    messages=[{"role": "user", "content": "hello"}],
)
print(response.choices[0].message.content)`;

    document.getElementById('python-anthropic-example').textContent =
`import anthropic

client = anthropic.Anthropic(
    base_url="${SHOW}",
    api_key="${apiKey}",
)

message = client.messages.create(
    model="${claudeModel}",
    max_tokens=1024,
    messages=[{"role": "user", "content": "hello"}],
)
print(message.content[0].text)`;

    document.getElementById('curl-example').textContent =
`curl ${SHOW}/v1/chat/completions \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer ${apiKey}" \\
  -d '{
    "model": "${exModel}",
    "messages": [{"role": "user", "content": "hello"}]
  }'`;

    document.getElementById('imagen-example').textContent =
`curl ${SHOW}/v1/images/generations \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer ${apiKey}" \\
  -d '{
    "model": "imagen-4.0-generate-001",
    "prompt": "a cute cat sitting on a windowsill",
    "n": 1
  }'

# Response contains base64-encoded image in data[0].b64_json`;

    document.getElementById('veo-example').textContent =
`curl ${SHOW}/v1/videos/generations \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer ${apiKey}" \\
  -d '{
    "model": "veo-3.0-generate-001",
    "instances": [{"prompt": "a cat walking on a beach"}],
    "parameters": {"sampleCount": 1}
  }'

# Response returns an operation ID (async). Video generation takes time.`;

    document.getElementById('file-upload-example').textContent =
`# Upload an image for vision understanding
curl ${SHOW}/v1/files \\
  -H "Authorization: Bearer ${apiKey}" \\
  -F "file=@image.png" \\
  -F "purpose=image"

# Use the returned file ID in chat
curl ${SHOW}/v1/chat/completions \\
  -H "Content-Type: application/json" \\
  -H "Authorization: Bearer ${apiKey}" \\
  -d '{
    "model": "kimi-coding",
    "messages": [{
      "role": "user",
      "content": [
        {"type": "text", "text": "describe this image"},
        {"type": "image_url", "image_url": {"url": "ms://FILE_ID"}}
      ]
    }]
  }'`;

  } catch (e) {
    token = null;
    localStorage.removeItem('token');
    document.getElementById('auth-section').classList.remove('hidden');
    document.getElementById('dashboard').classList.add('hidden');
  }
}

function copyKey() {
  navigator.clipboard.writeText(apiKey);
}

function copyCode(id) {
  navigator.clipboard.writeText(document.getElementById(id).textContent);
}

async function refreshKey() {
  if (!confirm('Regenerate API key? Old key will stop working.')) return;
  const res = await api('/user/api-key/refresh', { method: 'POST' });
  apiKey = res.api_key;
  document.getElementById('api-key-display').textContent = apiKey;
  loadDashboard();
}

// Usage for today / this month / this year, in this browser's time zone
let usagePeriod = 'day';
const PERIOD_NAMES = { day: '今天', month: '本月', year: '今年' };
async function loadUsage(period) {
  usagePeriod = period;
  document.querySelectorAll('[data-period]').forEach(b => b.classList.toggle('active', b.dataset.period === period));
  try {
    const u = await api(`/user/usage?period=${period}&tz=${-new Date().getTimezoneOffset()}`);
    const total = UsageChart.render(document.getElementById('usage-chart'), u.series, period);
    document.getElementById('usage-total').textContent = `${PERIOD_NAMES[period]}共 ${total.tokens.toLocaleString()} token · ${total.requests.toLocaleString()} 次请求`;
    const ut = document.getElementById('usage-table');
    ut.innerHTML = u.models.length
      ? '<tr><td style="color:#888;font-weight:600">模型</td><td style="color:#888;font-weight:600">请求 / Token</td></tr>' +
        u.models.map(m => `<tr><td>${esc(m.model)}</td><td>${m.requests.toLocaleString()} 次 / ${m.total_tokens.toLocaleString()} token</td></tr>`).join('')
      : `<tr><td style="color:#666">${PERIOD_NAMES[period]}还没有用量</td><td></td></tr>`;
  } catch (e) {}
}

// 220000000 -> "2.2 亿"
function bigNum(n) {
  if (n >= 1e8) return +(n / 1e8).toFixed(2) + ' 亿';
  if (n >= 1e4) return +(n / 1e4).toFixed(1) + ' 万';
  return String(n);
}

function windowLabel(secs) {
  if (secs % 86400 === 0) return `近 ${secs / 86400} 天`;
  if (secs % 3600 === 0) return `近 ${secs / 3600} 小时`;
  return `近 ${Math.round(secs / 60)} 分钟`;
}

// "已用 3000 万 / 2.2 亿 token（剩 86%）"
function meter(used, max, unit) {
  const left = Math.max(0, Math.round((1 - used / max) * 100));
  const color = left <= 20 ? '#f87171' : left <= 50 ? '#fbbf24' : '#4ade80';
  return `已用 ${bigNum(used)} / ${bigNum(max)} ${unit} <span style="color:${color}">（剩 ${left}%）</span>`;
}

async function changePassword() {
  const pw = document.getElementById('pw-new').value;
  if (pw !== document.getElementById('pw-new2').value) { showMsg('pw-msg', '两次输入的新密码不一致', false); return; }
  if (pw.length < 8) { showMsg('pw-msg', '新密码至少 8 位', false); return; }
  try {
    await api('/user/password', { method: 'PUT', body: JSON.stringify({ current_password: document.getElementById('pw-current').value, new_password: pw }) });
    ['pw-current', 'pw-new', 'pw-new2'].forEach(id => document.getElementById(id).value = '');
    showMsg('pw-msg', '密码已修改', true);
  } catch (e) { showMsg('pw-msg', '修改失败：当前密码不对，或尝试次数太多', false); }
}

function logout() {
  token = null;
  localStorage.removeItem('token');
  location.reload();
}
