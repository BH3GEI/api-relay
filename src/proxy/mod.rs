//! The relayed API: requests are forwarded as they are to the provider behind the requested
//! model, with only the model name changed, on the first of its keys that works.

pub mod files;
pub mod models;

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use futures::StreamExt;
use serde_json::Value;
use std::sync::Arc;

use crate::{
    AppState,
    capture::Capture,
    db::{self, Db, providers::Provider, users::User},
    error::{Error, Result},
    extract::{Caller, Origin},
    pool, tokens,
};

/// The endpoints that relay a model call.
#[derive(Clone, Copy, PartialEq)]
enum Endpoint {
    ChatCompletions,
    Responses,
    Messages,
    CountTokens,
    Images,
    /// Google's native long-running predict API, reached from its OpenAI-compatible base
    Videos,
}

/// How the upstream wants the key.
enum KeyHeader {
    Bearer,
    Anthropic,
    Google,
}

impl Endpoint {
    fn upstream(self, provider: &Provider, model: &str) -> (String, KeyHeader) {
        let base = provider.base_url.trim_end_matches('/');
        match self {
            Endpoint::ChatCompletions => (format!("{base}/chat/completions"), KeyHeader::Bearer),
            Endpoint::Responses => (format!("{base}/responses"), KeyHeader::Bearer),
            Endpoint::Messages => (format!("{}/v1/messages", provider.anthropic_base()), KeyHeader::Anthropic),
            Endpoint::CountTokens => (format!("{}/v1/messages/count_tokens", provider.anthropic_base()), KeyHeader::Anthropic),
            Endpoint::Images => (format!("{base}/images/generations"), KeyHeader::Bearer),
            Endpoint::Videos => {
                // …/v1beta/openai → …/v1beta/models/{model}:predictLongRunning
                let native = base.strip_suffix("/openai").unwrap_or(base);
                (format!("{native}/models/{model}:predictLongRunning"), KeyHeader::Google)
            }
        }
    }

    /// How the record of a call names it.
    fn name(self) -> &'static str {
        match self {
            Endpoint::ChatCompletions => "chat_completions",
            Endpoint::Responses => "responses",
            Endpoint::Messages => "messages",
            Endpoint::CountTokens => "count_tokens",
            Endpoint::Images => "images",
            Endpoint::Videos => "videos",
        }
    }

    /// Text endpoints, whose responses report token usage.
    fn is_text(self) -> bool {
        !matches!(self, Endpoint::Images | Endpoint::Videos)
    }

    /// Whether a call counts as usage; counting tokens doesn't.
    fn counts(self) -> bool {
        self != Endpoint::CountTokens
    }
}

pub async fn chat_completions(state: State<Arc<AppState>>, caller: Caller, origin: Origin, headers: HeaderMap, body: String) -> Result<Response> {
    relay(state, caller, origin, headers, body, Endpoint::ChatCompletions).await
}

pub async fn responses(state: State<Arc<AppState>>, caller: Caller, origin: Origin, headers: HeaderMap, body: String) -> Result<Response> {
    relay(state, caller, origin, headers, body, Endpoint::Responses).await
}

pub async fn messages(state: State<Arc<AppState>>, caller: Caller, origin: Origin, headers: HeaderMap, body: String) -> Result<Response> {
    relay(state, caller, origin, headers, body, Endpoint::Messages).await
}

pub async fn count_tokens(state: State<Arc<AppState>>, caller: Caller, origin: Origin, headers: HeaderMap, body: String) -> Result<Response> {
    relay(state, caller, origin, headers, body, Endpoint::CountTokens).await
}

pub async fn image_generations(state: State<Arc<AppState>>, caller: Caller, origin: Origin, headers: HeaderMap, body: String) -> Result<Response> {
    relay(state, caller, origin, headers, body, Endpoint::Images).await
}

pub async fn video_generations(state: State<Arc<AppState>>, caller: Caller, origin: Origin, headers: HeaderMap, body: String) -> Result<Response> {
    relay(state, caller, origin, headers, body, Endpoint::Videos).await
}

/// Relay one call, and record it whatever becomes of it.
async fn relay(State(state): State<Arc<AppState>>, Caller(user): Caller, origin: Origin, headers: HeaderMap, body: String, endpoint: Endpoint) -> Result<Response> {
    let mut capture = Capture::start(&state.capture, &user, &origin, &headers, endpoint.name(), &body);
    let result = forward(&state, &user, &headers, body, endpoint, &mut capture).await;
    if let Err(e) = &result {
        capture.failed(e);
    }
    result
}

async fn forward(state: &AppState, user: &User, headers: &HeaderMap, body: String, endpoint: Endpoint, capture: &mut Capture) -> Result<Response> {
    let mut body: Value = serde_json::from_str(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let requested = body["model"].as_str().ok_or(StatusCode::BAD_REQUEST)?.to_string();
    capture.model(&requested);
    let (provider, entry) = state.catalog.resolve(state, &requested).await
        .ok_or_else(|| Error::api(StatusCode::NOT_FOUND, "not_found_error", format!("Unknown model {requested}")))?;
    let stream = body["stream"].as_bool().unwrap_or(false);
    capture.route(&provider.name, &entry.upstream_model, stream);
    // Limits and usage go by the catalog id, so `m1` and `kimi/m1` count as one model
    let usage = admit(state, user, &entry.id, endpoint.counts()).await?;

    body["model"] = Value::String(entry.upstream_model.clone());
    if stream && endpoint == Endpoint::ChatCompletions && body.get("stream_options").is_none() {
        // Without this, streamed Chat Completions never report token usage
        body["stream_options"] = serde_json::json!({ "include_usage": true });
    }
    let body = body.to_string();
    capture.sent(&body);

    let (url, key_header) = endpoint.upstream(&provider, &entry.upstream_model);
    // Anthropic clients pick the API version and opt into beta features per request
    let anthropic_version = headers.get("anthropic-version").and_then(|v| v.to_str().ok()).unwrap_or("2023-06-01");
    let anthropic_beta = headers.get("anthropic-beta").and_then(|v| v.to_str().ok());

    let keys = db::providers::usable_keys(&state.db, provider.id).await?;
    let mut attempts = Vec::new();
    let sent = state.keys.send(keys, &mut attempts, |key| {
        let mut req = state.http_client.post(&url)
            .header("Content-Type", "application/json")
            .header("User-Agent", &provider.user_agent)
            .body(body.clone());
        if endpoint.is_text() {
            req = req.header("x-app", "cli");
        }
        match key_header {
            KeyHeader::Bearer => req.header("Authorization", format!("Bearer {key}")),
            KeyHeader::Google => req.header("x-goog-api-key", key),
            KeyHeader::Anthropic => {
                req = req.header("x-api-key", key).header("anthropic-version", anthropic_version);
                match anthropic_beta {
                    Some(beta) => req.header("anthropic-beta", beta),
                    None => req,
                }
            }
        }
    }).await;
    capture.attempts(attempts);

    let resp = match sent {
        Ok(resp) if resp.status().is_success() => resp,
        refused => {
            // Refused or failed requests don't count against the user
            if let Some(usage) = usage {
                usage.discard().await;
            }
            return match refused {
                Ok(resp) => pass_through(resp, None, capture.take()),
                Err(e) => Err(exhausted(e)),
            };
        }
    };
    match usage {
        Some(usage) if endpoint.is_text() && stream => pass_through(resp, Some(usage), capture.take()),
        Some(usage) if endpoint.is_text() => {
            let status = resp.status();
            capture.answer(status.as_u16());
            let bytes = resp.bytes().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
            capture.chunk(&bytes);
            let (input, output) = tokens::from_body(&bytes);
            capture.tokens(input, output);
            capture.complete();
            usage.finish(input, output).await;
            Ok(Response::builder().status(status).header(header::CONTENT_TYPE, "application/json").body(Body::from(bytes)).unwrap())
        }
        // Generated media report no tokens; the request is what counts
        _ => pass_through(resp, None, capture.take()),
    }
}

/// A request being counted: recorded as it starts, so concurrent requests count against limits.
struct Usage {
    db: Db,
    id: i64,
}

impl Usage {
    async fn finish(self, input: i64, output: i64) {
        let _ = db::usage::finish(&self.db, self.id, input, output).await;
    }

    async fn discard(self) {
        let _ = db::usage::discard(&self.db, self.id).await;
    }
}

/// Watches a response as it streams out: picks token counts out of it, and records them and
/// the call when the stream is done — even if the client hangs up first.
struct Tap {
    usage: Option<Usage>,
    scanner: tokens::StreamScanner,
    capture: Capture,
}

impl Tap {
    fn feed(&mut self, bytes: &[u8]) {
        self.scanner.feed(bytes);
        self.capture.chunk(bytes);
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        let (input, output) = (self.scanner.input, self.scanner.output);
        self.capture.tokens(input, output);
        if let Some(usage) = self.usage.take() {
            tokio::spawn(usage.finish(input, output));
        }
        // The capture records itself as it is dropped, right after this
    }
}

/// Check the caller's limits for `model` and, when the call `counts`, record it — both under
/// one lock, so parallel requests can't all slip past a limit.
async fn admit(state: &AppState, user: &User, model: &str, counts: bool) -> Result<Option<Usage>> {
    let _admission = state.admission.lock().await;
    if !db::limits::allows(&state.db, user.id, model).await? {
        return Err(Error::api(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", format!("Usage limit reached for {model}")));
    }
    if !counts {
        return Ok(None);
    }
    let id = db::usage::start(&state.db, user.id, model).await?;
    Ok(Some(Usage { db: state.db.clone(), id }))
}

/// Every key failed; report it without passing on the upstream's own error body, which can
/// name the account behind the key.
fn exhausted(e: pool::Exhausted) -> Error {
    match e.last_status {
        None => Error::api(StatusCode::BAD_GATEWAY, "api_error", "No upstream key could serve this request"),
        Some(code) => Error::api(
            StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
            "api_error",
            format!("All upstream keys are unavailable (last upstream status {code})"),
        ),
    }
}

/// The upstream's response as it streams in, with its status and content type.
fn pass_through(resp: reqwest::Response, usage: Option<Usage>, mut capture: Capture) -> Result<Response> {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    capture.answer(status.as_u16());
    let content_type = resp.headers().get(header::CONTENT_TYPE).cloned();
    let tap = Tap { usage, scanner: Default::default(), capture };
    let stream = futures::stream::unfold((resp.bytes_stream(), tap), |(mut chunks, mut tap)| async move {
        match chunks.next().await {
            Some(chunk) => {
                if let Ok(bytes) = &chunk {
                    tap.feed(bytes);
                }
                Some((chunk.map_err(std::io::Error::other), (chunks, tap)))
            }
            None => {
                tap.capture.complete();
                None
            }
        }
    });
    let mut builder = Response::builder().status(status);
    if let Some(ct) = content_type {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }
    builder.body(Body::from_stream(stream)).map_err(|e| Error::Internal(e.to_string()))
}
