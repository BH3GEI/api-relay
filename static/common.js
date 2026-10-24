// Shared by both pages: requests to this origin, escaping, messages.

// Requests go to this page's own origin
const BASE = location.origin;

function esc(v) {
  return String(v ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}

function showMsg(id, msg, ok) {
  const el = document.getElementById(id);
  el.textContent = msg;
  el.className = 'msg ' + (ok ? 'ok' : 'error');
  el.classList.remove('hidden');
}

async function api(path, opts = {}) {
  const headers = { 'Content-Type': 'application/json', ...(opts.headers || {}) };
  if (token) headers['Authorization'] = 'Bearer ' + token;
  const r = await fetch(BASE + path, { ...opts, headers });
  if (!r.ok) throw new Error(await r.text() || r.statusText);
  if (r.status === 204) return null;
  return r.json();
}
