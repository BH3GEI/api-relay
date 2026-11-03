//! A record of every relayed model call, kept to study how the models are used: who called
//! what, through which key, how it went and how long it took — and, unless the caller opted
//! out, the request and the response in full.
//!
//! Records live in a database of their own, so the relay's stays small. Agents send the whole
//! conversation again on every turn, so a request's messages, system prompt and tools are each
//! stored once, by hash, and the request keeps references to them; reading it back puts them in
//! place again. Everything stored is zstd-compressed.

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{
    QueryBuilder, Sqlite, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous},
};
use std::{str::FromStr, time::{Instant, SystemTime, UNIX_EPOCH}};

use crate::{db::users::User, error::Error, extract::Origin, pool::Attempt};

pub type Store = SqlitePool;

/// Stands in for a part of a request that is stored apart: `{"$relay_ref": "<hash>"}`
const REF: &str = "$relay_ref";
/// Parts smaller than this stay in the request; a reference would save little
const MIN_SHARED: usize = 128;
const LEVEL: i32 = 3;

const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS requests (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        started_at INTEGER NOT NULL,            -- unix milliseconds
        user_id INTEGER NOT NULL,
        username TEXT NOT NULL,
        endpoint TEXT NOT NULL,
        model TEXT NOT NULL DEFAULT '',         -- as the client asked for it
        provider TEXT NOT NULL DEFAULT '',
        upstream_model TEXT NOT NULL DEFAULT '',
        stream INTEGER NOT NULL DEFAULT 0,
        entry TEXT NOT NULL,                    -- lan, tunnel or frp
        client_ip TEXT,
        user_agent TEXT,
        headers TEXT NOT NULL DEFAULT '{}',     -- the client's, without credentials
        status INTEGER,                         -- what the client got; NULL when it left before an answer
        error TEXT,
        key TEXT,                               -- tail of the upstream key that answered
        attempts TEXT NOT NULL DEFAULT '[]',
        first_byte_ms INTEGER,
        total_ms INTEGER NOT NULL DEFAULT 0,
        complete INTEGER NOT NULL DEFAULT 0,    -- the response was passed on to its end
        input_tokens INTEGER NOT NULL DEFAULT 0,
        output_tokens INTEGER NOT NULL DEFAULT 0,
        request_bytes INTEGER NOT NULL DEFAULT 0,
        response_bytes INTEGER NOT NULL DEFAULT 0,
        content INTEGER NOT NULL,               -- whether the request and response were kept
        request BLOB,
        response BLOB
    )",
    "CREATE INDEX IF NOT EXISTS idx_requests_started ON requests(started_at)",
    "CREATE INDEX IF NOT EXISTS idx_requests_user ON requests(user_id, id)",
    "CREATE TABLE IF NOT EXISTS parts (
        hash TEXT PRIMARY KEY,
        data BLOB NOT NULL
    )",
];

pub async fn open(url: &str) -> Store {
    let opts = SqliteConnectOptions::from_str(url).expect("Bad capture database URL")
        .create_if_missing(true)
        // Readers (the admin page, exports) don't hold up the writes
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal);
    let store = SqlitePool::connect_with(opts).await.expect("Failed to open capture database");
    for stmt in SCHEMA {
        sqlx::query(stmt).execute(&store).await.unwrap();
    }
    store
}

/// One call being recorded. It is written when dropped, whenever that is: after the response
/// has streamed out, on an error, or when the client hangs up midway.
#[derive(Default)]
pub struct Capture(Option<Pending>);

struct Pending {
    store: Store,
    started: Instant,
    row: Row,
    /// The request as sent upstream, or as received until then
    body: String,
    /// Compressed as it arrives, so even a huge response is kept whole in little memory
    response: zstd::stream::write::Encoder<'static, Vec<u8>>,
}

#[derive(Default)]
struct Row {
    started_at: i64,
    user_id: i64,
    username: String,
    endpoint: &'static str,
    model: String,
    provider: String,
    upstream_model: String,
    stream: bool,
    entry: &'static str,
    client_ip: Option<String>,
    user_agent: Option<String>,
    headers: String,
    status: Option<u16>,
    error: Option<String>,
    key: Option<String>,
    attempts: Vec<Attempt>,
    first_byte_ms: Option<i64>,
    complete: bool,
    input_tokens: i64,
    output_tokens: i64,
    response_bytes: i64,
    content: bool,
}

/// Headers that carry credentials; the rest are kept.
const SECRET_HEADERS: &[&str] = &["authorization", "x-api-key", "cookie", "proxy-authorization", "x-relay-force-fallback"];

impl Capture {
    pub fn start(store: &Store, user: &User, origin: &Origin, headers: &axum::http::HeaderMap, endpoint: &'static str, body: &str) -> Capture {
        let kept: serde_json::Map<String, Value> = headers.iter()
            .filter(|(name, _)| !SECRET_HEADERS.contains(&name.as_str()))
            .map(|(name, v)| (name.to_string(), Value::String(String::from_utf8_lossy(v.as_bytes()).into_owned())))
            .collect();
        let row = Row {
            started_at: now_ms(),
            user_id: user.id,
            username: user.username.clone(),
            endpoint,
            entry: if origin.frp { "frp" } else if origin.public { "tunnel" } else { "lan" },
            client_ip: origin.ip.clone(),
            user_agent: headers.get("user-agent").map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()),
            headers: Value::Object(kept).to_string(),
            content: user.record_content,
            ..Row::default()
        };
        Capture(Some(Pending { store: store.clone(), started: Instant::now(), row, body: body.to_string(), response: zstd::stream::write::Encoder::new(Vec::new(), LEVEL).expect("zstd encoder") }))
    }

    fn row(&mut self) -> Option<&mut Row> {
        self.0.as_mut().map(|p| &mut p.row)
    }

    /// Moves the record out, to go along with a streamed response.
    pub fn take(&mut self) -> Capture {
        Capture(self.0.take())
    }

    pub fn model(&mut self, model: &str) {
        if let Some(r) = self.row() {
            r.model = model.to_string();
        }
    }

    pub fn route(&mut self, provider: &str, upstream_model: &str, stream: bool) {
        if let Some(r) = self.row() {
            (r.provider, r.upstream_model, r.stream) = (provider.to_string(), upstream_model.to_string(), stream);
        }
    }

    /// The request as it goes upstream, with the upstream's model name.
    pub fn sent(&mut self, body: &str) {
        if let Some(p) = self.0.as_mut() {
            p.body = body.to_string();
        }
    }

    pub fn attempts(&mut self, attempts: Vec<Attempt>) {
        if let Some(r) = self.row() {
            r.key = attempts.last().filter(|a| a.served).map(|a| a.key.clone());
            r.attempts = attempts;
        }
    }

    pub fn answer(&mut self, status: u16) {
        if let Some(r) = self.row() {
            r.status = Some(status);
        }
    }

    /// The relay's own refusal, or a failure on the way.
    pub fn failed(&mut self, e: &Error) {
        if let Some(r) = self.row() {
            let (status, message) = e.describe();
            (r.status, r.error) = (Some(status), Some(message));
        }
    }

    pub fn chunk(&mut self, bytes: &[u8]) {
        let Some(p) = self.0.as_mut() else { return };
        p.row.first_byte_ms.get_or_insert_with(|| p.started.elapsed().as_millis() as i64);
        p.row.response_bytes += bytes.len() as i64;
        if p.row.content {
            let _ = std::io::Write::write_all(&mut p.response, bytes);
        }
    }

    pub fn tokens(&mut self, input: i64, output: i64) {
        if let Some(r) = self.row() {
            (r.input_tokens, r.output_tokens) = (input, output);
        }
    }

    pub fn complete(&mut self) {
        if let Some(r) = self.row() {
            r.complete = true;
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let Some(p) = self.0.take() else { return };
        let total_ms = p.started.elapsed().as_millis() as i64;
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                if let Err(e) = save(p, total_ms).await {
                    eprintln!("capture: {e}");
                }
            });
        }
    }
}

async fn save(p: Pending, total_ms: i64) -> Result<(), String> {
    let Pending { store, row, body, response, .. } = p;
    let request_bytes = body.len() as i64;
    let content = row.content;
    let (request, parts, response) = if content {
        tokio::task::spawn_blocking(move || {
            let (skeleton, parts) = split(&body);
            (zip(&skeleton), parts, response.finish().unwrap_or_default())
        }).await.map_err(|e| e.to_string())?
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

    // Only parts not stored yet are compressed
    let mut new_parts = Vec::new();
    for (hash, bytes) in parts {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM parts WHERE hash = ?").bind(&hash).fetch_one(&store).await.map_err(|e| e.to_string())?;
        if n == 0 && !new_parts.iter().any(|(h, _): &(String, Vec<u8>)| *h == hash) {
            new_parts.push((hash, bytes));
        }
    }
    let new_parts = tokio::task::spawn_blocking(move || {
        new_parts.into_iter().map(|(h, b)| (h, zip(&b))).collect::<Vec<_>>()
    }).await.map_err(|e| e.to_string())?;

    let mut tx = store.begin().await.map_err(|e| e.to_string())?;
    for (hash, data) in new_parts {
        sqlx::query("INSERT OR IGNORE INTO parts (hash, data) VALUES (?, ?)").bind(hash).bind(data).execute(&mut *tx).await.map_err(|e| e.to_string())?;
    }
    sqlx::query("INSERT INTO requests (started_at, user_id, username, endpoint, model, provider, upstream_model, stream, entry,
            client_ip, user_agent, headers, status, error, key, attempts, first_byte_ms, total_ms, complete,
            input_tokens, output_tokens, request_bytes, response_bytes, content, request, response)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(row.started_at).bind(row.user_id).bind(row.username).bind(row.endpoint).bind(row.model)
        .bind(row.provider).bind(row.upstream_model).bind(row.stream).bind(row.entry)
        .bind(row.client_ip).bind(row.user_agent).bind(row.headers).bind(row.status).bind(row.error).bind(row.key)
        .bind(serde_json::to_string(&row.attempts).unwrap_or_default()).bind(row.first_byte_ms).bind(total_ms).bind(row.complete)
        .bind(row.input_tokens).bind(row.output_tokens).bind(request_bytes).bind(row.response_bytes).bind(content)
        .bind(content.then_some(request)).bind(content.then_some(response))
        .execute(&mut *tx).await.map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}

/// The request with its conversation parts replaced by references, and those parts by hash.
/// A body that isn't JSON is kept as it is.
fn split(body: &str) -> (Vec<u8>, Vec<(String, Vec<u8>)>) {
    let Ok(mut v) = serde_json::from_str::<Value>(body) else {
        return (body.as_bytes().to_vec(), Vec::new());
    };
    let mut parts = Vec::new();
    if let Some(obj) = v.as_object_mut() {
        // Chat Completions and Messages: messages; Responses: input; Gemini: contents
        for key in ["messages", "input", "contents"] {
            if let Some(Value::Array(items)) = obj.get_mut(key) {
                for item in items {
                    shelve(item, &mut parts);
                }
            }
        }
        for key in ["system", "instructions", "tools"] {
            if let Some(item) = obj.get_mut(key) {
                shelve(item, &mut parts);
            }
        }
    }
    (serde_json::to_vec(&v).unwrap_or_default(), parts)
}

fn shelve(item: &mut Value, parts: &mut Vec<(String, Vec<u8>)>) {
    let bytes = serde_json::to_vec(item).unwrap_or_default();
    if bytes.len() < MIN_SHARED {
        return;
    }
    let hash: String = Sha256::digest(&bytes)[..16].iter().map(|b| format!("{b:02x}")).collect();
    *item = json!({ REF: hash });
    parts.push((hash, bytes));
}

fn zip(bytes: &[u8]) -> Vec<u8> {
    zstd::encode_all(bytes, LEVEL).unwrap_or_default()
}

fn unzip(bytes: &[u8]) -> Vec<u8> {
    zstd::decode_all(bytes).unwrap_or_default()
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

// ── Reading records back ──

/// What a list shows of a call.
#[derive(Serialize, sqlx::FromRow)]
pub struct Summary {
    pub id: i64,
    pub started_at: i64,
    pub user_id: i64,
    pub username: String,
    pub endpoint: String,
    pub model: String,
    pub provider: String,
    pub upstream_model: String,
    pub stream: bool,
    pub entry: String,
    pub client_ip: Option<String>,
    pub status: Option<i64>,
    pub error: Option<String>,
    pub key: Option<String>,
    pub first_byte_ms: Option<i64>,
    pub total_ms: i64,
    pub complete: bool,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub request_bytes: i64,
    pub response_bytes: i64,
    pub content: bool,
}

const SUMMARY: &str = "id, started_at, user_id, username, endpoint, model, provider, upstream_model, stream, entry, client_ip,
    status, error, key, first_byte_ms, total_ms, complete, input_tokens, output_tokens, request_bytes, response_bytes, content";

/// A call in full.
#[derive(Serialize)]
pub struct Detail {
    #[serde(flatten)]
    pub summary: Summary,
    pub user_agent: Option<String>,
    pub headers: Value,
    pub attempts: Value,
    /// The request as sent upstream: JSON, or a string when it wasn't JSON
    pub request: Option<Value>,
    /// The response as passed on: raw text, including a stream's events
    pub response: Option<String>,
}

#[derive(Default, Clone)]
pub struct Filter {
    pub user_id: Option<i64>,
    /// One model, or a provider's with `prov/*`
    pub model: Option<String>,
    /// "ok" (2xx) or "error" (anything else)
    pub status: Option<String>,
    /// `[from, to)`, unix milliseconds
    pub from: Option<i64>,
    pub to: Option<i64>,
}

impl Filter {
    fn push<'a>(&'a self, q: &mut QueryBuilder<'a, Sqlite>) {
        q.push(" WHERE 1=1");
        if let Some(u) = self.user_id {
            q.push(" AND user_id = ").push_bind(u);
        }
        match self.model.as_deref() {
            Some(m) if m.ends_with("/*") => {
                q.push(" AND provider = ").push_bind(m.trim_end_matches("/*"));
            }
            Some(m) => {
                q.push(" AND model = ").push_bind(m);
            }
            None => {}
        }
        match self.status.as_deref() {
            Some("ok") => {
                q.push(" AND status BETWEEN 200 AND 299");
            }
            Some("error") => {
                q.push(" AND (status IS NULL OR status NOT BETWEEN 200 AND 299)");
            }
            _ => {}
        }
        if let Some(f) = self.from {
            q.push(" AND started_at >= ").push_bind(f);
        }
        if let Some(t) = self.to {
            q.push(" AND started_at < ").push_bind(t);
        }
    }
}

/// Newest first, before the record `before` when given.
pub async fn list(store: &Store, filter: &Filter, before: Option<i64>, limit: i64) -> sqlx::Result<Vec<Summary>> {
    let mut q = QueryBuilder::new(format!("SELECT {SUMMARY} FROM requests"));
    filter.push(&mut q);
    if let Some(b) = before {
        q.push(" AND id < ").push_bind(b);
    }
    q.push(" ORDER BY id DESC LIMIT ").push_bind(limit);
    q.build_query_as().fetch_all(store).await
}

/// Oldest first, after the record `after`; for exports.
pub async fn details_after(store: &Store, filter: &Filter, after: i64, limit: i64) -> sqlx::Result<Vec<Detail>> {
    let mut q = QueryBuilder::new("SELECT id FROM requests");
    filter.push(&mut q);
    q.push(" AND id > ").push_bind(after).push(" ORDER BY id LIMIT ").push_bind(limit);
    let ids: Vec<(i64,)> = q.build_query_as().fetch_all(store).await?;
    let mut out = Vec::with_capacity(ids.len());
    for (id,) in ids {
        if let Some(d) = detail(store, id).await? {
            out.push(d);
        }
    }
    Ok(out)
}

pub async fn detail(store: &Store, id: i64) -> sqlx::Result<Option<Detail>> {
    let Some(summary) = sqlx::query_as::<_, Summary>(&format!("SELECT {SUMMARY} FROM requests WHERE id = ?"))
        .bind(id).fetch_optional(store).await? else { return Ok(None) };
    let (user_agent, headers, attempts, request, response): (Option<String>, String, String, Option<Vec<u8>>, Option<Vec<u8>>) =
        sqlx::query_as("SELECT user_agent, headers, attempts, request, response FROM requests WHERE id = ?")
            .bind(id).fetch_one(store).await?;
    let request = match request {
        Some(z) => Some(expand(store, &unzip(&z)).await?),
        None => None,
    };
    Ok(Some(Detail {
        summary,
        user_agent,
        headers: serde_json::from_str(&headers).unwrap_or_default(),
        attempts: serde_json::from_str(&attempts).unwrap_or_default(),
        request,
        response: response.map(|z| String::from_utf8_lossy(&unzip(&z)).into_owned()),
    }))
}

/// A stored request with its parts back in place.
async fn expand(store: &Store, skeleton: &[u8]) -> sqlx::Result<Value> {
    let Ok(mut v) = serde_json::from_slice::<Value>(skeleton) else {
        return Ok(Value::String(String::from_utf8_lossy(skeleton).into_owned()));
    };
    let mut refs = Vec::new();
    collect_refs(&mut v, &mut refs);
    for slot in refs {
        let hash = slot[REF].as_str().unwrap_or_default().to_string();
        let data: Option<(Vec<u8>,)> = sqlx::query_as("SELECT data FROM parts WHERE hash = ?").bind(&hash).fetch_optional(store).await?;
        if let Some(part) = data.and_then(|(z,)| serde_json::from_slice(&unzip(&z)).ok()) {
            *slot = part;
        }
    }
    Ok(v)
}

fn collect_refs<'a>(v: &'a mut Value, out: &mut Vec<&'a mut Value>) {
    let is_ref = v.as_object().is_some_and(|o| o.len() == 1 && o.get(REF).is_some_and(Value::is_string));
    if is_ref {
        out.push(v);
        return;
    }
    match v {
        Value::Array(items) => items.iter_mut().for_each(|i| collect_refs(i, out)),
        Value::Object(o) => o.values_mut().for_each(|i| collect_refs(i, out)),
        _ => {}
    }
}

#[derive(Serialize)]
pub struct Stats {
    pub requests: i64,
    pub with_content: i64,
    pub parts: i64,
    /// Since when, unix milliseconds
    pub since: Option<i64>,
    /// The database file, without its write-ahead log
    pub bytes: i64,
}

pub async fn stats(store: &Store) -> sqlx::Result<Stats> {
    let (requests, with_content, since): (i64, Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT COUNT(*), SUM(content), MIN(started_at) FROM requests").fetch_one(store).await?;
    let (parts,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM parts").fetch_one(store).await?;
    let (pages,): (i64,) = sqlx::query_as("PRAGMA page_count").fetch_one(store).await?;
    let (page_size,): (i64,) = sqlx::query_as("PRAGMA page_size").fetch_one(store).await?;
    Ok(Stats { requests, with_content: with_content.unwrap_or(0), parts, since, bytes: pages * page_size })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_turns_share_their_parts() {
        let long = "x".repeat(200);
        let turn1 = json!({ "model": "m", "system": long, "messages": [{ "role": "user", "content": long }] }).to_string();
        let turn2 = json!({ "model": "m", "system": long, "messages": [{ "role": "user", "content": long }, { "role": "assistant", "content": "ok" }] }).to_string();
        let (_, p1) = split(&turn1);
        let (skeleton, p2) = split(&turn2);
        assert_eq!(p1.len(), 2);
        assert_eq!(p2[..2].iter().map(|p| &p.0).collect::<Vec<_>>(), p1.iter().map(|p| &p.0).collect::<Vec<_>>());
        // The short reply stays inline
        let s: Value = serde_json::from_slice(&skeleton).unwrap();
        assert_eq!(s["messages"][1]["content"], "ok");
        assert!(s["messages"][0][REF].is_string());
    }

    #[test]
    fn a_body_that_is_not_json_is_kept_as_is() {
        let (skeleton, parts) = split("not json");
        assert_eq!((skeleton, parts.len()), (b"not json".to_vec(), 0));
    }
}
