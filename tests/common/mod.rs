//! A relay wired to a fake upstream, driven through its routers without opening a port for it.

use api_relay::{App, config::Config};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, atomic::{AtomicU64, Ordering}};
use tower::ServiceExt;

/// A request the fake upstream received.
#[derive(Clone, Debug)]
pub struct Seen {
    pub path: String,
    pub headers: HeaderMap,
    pub body: Value,
}

/// An upstream that answers like Kimi would: an OpenAI side under /v1 and an Anthropic side.
/// The key `bad` is always refused with 401, `chatbad` only for chat; any other key works.
pub struct Upstream {
    pub base: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

impl Upstream {
    pub async fn start() -> Upstream {
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let app = Router::new()
            .route("/v1/models", get(models))
            .route("/v1/chat/completions", post(chat))
            .route("/v1/messages", post(messages))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Upstream { base, seen }
    }

    /// Requests to `path`, oldest first.
    pub fn seen(&self, path: &str) -> Vec<Seen> {
        self.seen.lock().unwrap().iter().filter(|s| s.path == path).cloned().collect()
    }
}

fn record(seen: &Mutex<Vec<Seen>>, path: &str, headers: &HeaderMap, body: &Bytes) {
    let body = serde_json::from_slice(body).unwrap_or(Value::Null);
    seen.lock().unwrap().push(Seen { path: path.into(), headers: headers.clone(), body });
}

fn refused(headers: &HeaderMap) -> bool {
    key(headers) == "bad"
}

fn key(headers: &HeaderMap) -> &str {
    let auth = headers.get("authorization").or(headers.get("x-api-key")).and_then(|v| v.to_str().ok()).unwrap_or("");
    auth.strip_prefix("Bearer ").unwrap_or(auth)
}

async fn models(State(seen): State<Arc<Mutex<Vec<Seen>>>>, headers: HeaderMap) -> Response {
    record(&seen, "/v1/models", &headers, &Bytes::new());
    if refused(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    axum::Json(json!({ "data": [{ "id": "m1", "context_length": 1000 }, { "id": "m2" }] })).into_response()
}

async fn chat(State(seen): State<Arc<Mutex<Vec<Seen>>>>, headers: HeaderMap, body: Bytes) -> Response {
    record(&seen, "/v1/chat/completions", &headers, &body);
    if refused(&headers) || key(&headers) == "chatbad" {
        return (StatusCode::UNAUTHORIZED, r#"{"error":"account a@b.c is suspended"}"#).into_response();
    }
    let req: Value = serde_json::from_slice(&body).unwrap_or_default();
    if req["stream"] == true {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n\
                   data: {\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":4}}\n\n\
                   data: [DONE]\n\n";
        return ([("content-type", "text/event-stream")], sse).into_response();
    }
    axum::Json(json!({
        "choices": [{ "message": { "role": "assistant", "content": "ok" } }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 5 },
    })).into_response()
}

async fn messages(State(seen): State<Arc<Mutex<Vec<Seen>>>>, headers: HeaderMap, body: Bytes) -> Response {
    record(&seen, "/v1/messages", &headers, &body);
    if refused(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    axum::Json(json!({ "type": "message", "content": [{ "type": "text", "text": "ok" }], "usage": { "input_tokens": 3, "output_tokens": 2 } })).into_response()
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub text: String,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or_else(|_| panic!("not JSON ({}): {}", self.status, self.text))
    }
}

/// Which listener a request arrives on.
#[derive(Clone, Copy)]
pub enum Side {
    Lan,
    Public,
}

pub struct Relay {
    pub app: App,
    pub upstream: Upstream,
    pub admin: String,
}

static DB_SEQ: AtomicU64 = AtomicU64::new(0);

impl Relay {
    /// A relay on LazyCat with a public listener; the LazyCat admin `yao` is signed in.
    pub async fn start() -> Relay {
        let path = std::env::temp_dir().join(format!("api-relay-test-{}-{}.db", std::process::id(), DB_SEQ.fetch_add(1, Ordering::SeqCst)));
        let _ = std::fs::remove_file(&path);
        let config = Config {
            database_url: format!("sqlite:{}?mode=rwc", path.display()),
            bind_addr: "unused".into(),
            public_bind_addr: Some("unused".into()),
            jwt_secret: Some("test-secret".into()),
            lazycat: true,
            cloudflared_path: None,
            tunnel_log: None,
        };
        let app = App::new(&config).await;
        let upstream = Upstream::start().await;
        let mut relay = Relay { app, upstream, admin: String::new() };
        let sso = relay.lan("GET", "/auth/sso", &[("x-hc-user-id", "yao"), ("x-hc-user-role", "ADMIN")], None).await;
        assert_eq!(sso.status, StatusCode::OK, "{}", sso.text);
        relay.admin = sso.json()["token"].as_str().unwrap().to_string();
        relay
    }

    pub async fn send(&self, side: Side, method: &str, path: &str, headers: &[(&str, &str)], body: Option<Value>) -> Resp {
        let router = match side {
            Side::Lan => self.app.lan.clone(),
            Side::Public => self.app.public.clone().unwrap(),
        };
        let mut req = Request::builder().method(method).uri(path);
        if let Side::Public = side {
            // What Cloudflare adds; tests that care about the client address set their own
            if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("cf-connecting-ip")) {
                req = req.header("cf-connecting-ip", "198.51.100.1");
            }
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let body = match body {
            Some(v) => {
                req = req.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let resp = router.oneshot(req.body(body).unwrap()).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        Resp { status, headers, text: String::from_utf8_lossy(&bytes).into_owned() }
    }

    pub async fn lan(&self, method: &str, path: &str, headers: &[(&str, &str)], body: Option<Value>) -> Resp {
        self.send(Side::Lan, method, path, headers, body).await
    }

    pub async fn public(&self, method: &str, path: &str, headers: &[(&str, &str)], body: Option<Value>) -> Resp {
        self.send(Side::Public, method, path, headers, body).await
    }

    /// An admin API call on the LAN side.
    pub async fn admin(&self, method: &str, path: &str, body: Option<Value>) -> Resp {
        let auth = format!("Bearer {}", self.admin);
        self.lan(method, path, &[("authorization", &auth)], body).await
    }

    /// A user with a password; returns (id, api key).
    pub async fn user(&self, name: &str, password: &str) -> (i64, String) {
        let u = self.admin("POST", "/admin/users", Some(json!({ "username": name }))).await;
        assert_eq!(u.status, StatusCode::OK, "{}", u.text);
        let u = u.json();
        let id = u["id"].as_i64().unwrap();
        let set = self.admin("PUT", &format!("/admin/users/{id}/password"), Some(json!({ "password": password }))).await;
        assert_eq!(set.status, StatusCode::NO_CONTENT, "{}", set.text);
        (id, u["api_key"].as_str().unwrap().to_string())
    }

    /// A provider on the fake upstream with these keys, first key first; its models in the catalog.
    pub async fn provider(&self, name: &str, keys: &[&str]) -> i64 {
        let p = self.admin("POST", "/admin/providers", Some(json!({
            "name": name,
            "base_url": format!("{}/v1", self.upstream.base),
            "user_agent": "test-agent",
        }))).await;
        assert_eq!(p.status, StatusCode::OK, "{}", p.text);
        let id = p.json()["id"].as_i64().unwrap();
        for (i, key) in keys.iter().enumerate() {
            let k = self.admin("POST", &format!("/admin/keys/{id}"), Some(json!({ "api_key": key, "label": format!("{name}-{i}"), "priority": i }))).await;
            assert_eq!(k.status, StatusCode::OK, "{}", k.text);
        }
        let r = self.admin("POST", "/admin/catalog/refresh", None).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        id
    }

    /// Sign in on the public side; returns the session token.
    pub async fn login(&self, name: &str, password: &str) -> String {
        let r = self.public("POST", "/auth/login", &[], Some(json!({ "username": name, "password": password }))).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.text);
        r.json()["token"].as_str().unwrap().to_string()
    }

    /// Run SQL against the relay's database (to plant usage at chosen times).
    pub async fn sql(&self, sql: &str) {
        sqlx::query(sql).execute(&self.app.state.db).await.unwrap();
    }
}

pub fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}
