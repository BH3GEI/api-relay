use axum::{
    body::{Body, Bytes},
    extract::{Json, Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use futures::StreamExt;
use serde_json::Value;
use std::sync::Arc;

use crate::{AppState, db};

// Extract API key from either "Authorization: Bearer xxx" or "x-api-key: xxx"
pub fn extract_api_key(headers: &HeaderMap) -> Option<String> {
    // Try Bearer token first
    if let Some(key) = crate::auth::extract_bearer(headers) {
        return Some(key);
    }
    // Then try x-api-key
    headers.get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Reject unauthenticated API calls before their (possibly large) body is read.
pub async fn require_api_key(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let public = req.extensions().get::<crate::guard::PublicRequest>().is_some();
    let ip = crate::guard::client_ip(req.headers(), public);
    if ip.as_deref().is_some_and(|ip| state.guard.blocked(ip)) {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    let user = match extract_api_key(req.headers()) {
        Some(key) => db::find_user_by_api_key(&state.db, &key).await.ok(),
        None => None,
    };
    if user.is_none() {
        let from = ip.as_deref().unwrap_or("unknown");
        eprintln!("auth failed: {} {} from {from}", req.method(), req.uri().path());
        if let Some(ip) = &ip {
            state.guard.fail(ip);
        }
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(req).await)
}

/// Catalog entries in a shape both OpenAI and Anthropic model-list clients accept.
async fn model_objects(state: &AppState) -> Result<Vec<Value>, StatusCode> {
    let providers: std::collections::HashMap<i64, String> = db::list_providers(&state.db).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .into_iter().map(|p| (p.id, p.name)).collect();
    Ok(state.catalog.entries().into_iter().map(|e| {
        // Keep the upstream's metadata (context length, modalities, …) under our id
        let mut m = match e.meta {
            Value::Object(o) => o,
            _ => Default::default(),
        };
        if !m.contains_key("display_name") {
            m.insert("display_name".into(), Value::String(e.id.clone()));
        }
        m.insert("id".into(), Value::String(e.id));
        m.insert("object".into(), "model".into());
        // Anthropic-style field, for clients that list models in that format
        m.insert("type".into(), "model".into());
        m.insert("owned_by".into(), providers.get(&e.provider_id).cloned().unwrap_or_default().into());
        Value::Object(m)
    }).collect())
}

/// `GET /v1/models` (OpenAI and Anthropic list formats)
pub async fn list_models(State(state): State<Arc<AppState>>) -> Result<Json<Value>, StatusCode> {
    let data = model_objects(&state).await?;
    let id = |m: Option<&Value>| m.map(|m| m["id"].clone()).unwrap_or(Value::Null);
    Ok(Json(serde_json::json!({
        "object": "list",
        "first_id": id(data.first()),
        "last_id": id(data.last()),
        "has_more": false,
        "data": data,
    })))
}

/// `GET /v1/models/{id}`; ids contain a slash (`kimi/k3`), so this takes the rest of the path.
pub async fn get_model(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Result<Json<Value>, StatusCode> {
    model_objects(&state).await?.into_iter()
        .find(|m| m["id"] == id.as_str())
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// Agent-facing endpoints. Each is forwarded as-is to the same endpoint upstream.
#[derive(Clone, Copy)]
enum Endpoint {
    ChatCompletions,
    Responses,
    Messages,
    CountTokens,
}

impl Endpoint {
    fn is_anthropic(self) -> bool {
        matches!(self, Endpoint::Messages | Endpoint::CountTokens)
    }

    fn url(self, provider: &db::Provider) -> String {
        match self {
            Endpoint::ChatCompletions => format!("{}/chat/completions", provider.base_url.trim_end_matches('/')),
            Endpoint::Responses => format!("{}/responses", provider.base_url.trim_end_matches('/')),
            Endpoint::Messages => format!("{}/v1/messages", anthropic_base(provider)),
            Endpoint::CountTokens => format!("{}/v1/messages/count_tokens", anthropic_base(provider)),
        }
    }
}

/// Anthropic base URL; falls back to the OpenAI base minus its trailing /v1.
fn anthropic_base(provider: &db::Provider) -> &str {
    let explicit = provider.anthropic_base_url.trim_end_matches('/');
    if !explicit.is_empty() {
        return explicit;
    }
    let base = provider.base_url.trim_end_matches('/');
    base.strip_suffix("/v1").unwrap_or(base)
}

async fn provider_keys(state: &AppState, provider_id: i64) -> Vec<String> {
    db::get_provider_keys(&state.db, provider_id).await
        .map(|keys| keys.into_iter().map(|k| k.api_key).collect())
        .unwrap_or_default()
}

/// Check the caller's limits for `model` and, when `counts`, record the request before it is
/// forwarded. Both happen under one lock, so parallel requests can't all slip past a limit.
/// Returns the user id and the usage row to fill in.
async fn admit(state: &AppState, headers: &HeaderMap, model: &str, counts: bool) -> Result<(i64, Option<i64>), Response> {
    let api_key = extract_api_key(headers).ok_or_else(|| StatusCode::UNAUTHORIZED.into_response())?;
    let user = db::find_user_by_api_key(&state.db, &api_key).await.map_err(|_| StatusCode::UNAUTHORIZED.into_response())?;
    let _admission = state.admission.lock().await;
    // A database error must not mean "unlimited"
    if !db::check_limit(&state.db, user.id, model).await.unwrap_or(false) {
        return Err(error_response(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", &format!("Usage limit reached for {model}")));
    }
    let usage = match counts {
        true => Some(db::start_usage(&state.db, user.id, model).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?),
        false => None,
    };
    Ok((user.id, usage))
}

/// An error body both OpenAI and Anthropic clients understand.
fn error_response(status: StatusCode, kind: &str, message: &str) -> Response {
    let body = serde_json::json!({ "type": "error", "error": { "type": kind, "message": message } });
    (status, Json(body)).into_response()
}

/// Every upstream key failed; report it without passing on the upstream's own error body.
fn exhausted_response(e: crate::pool::Exhausted) -> Response {
    match e.last_status {
        None => error_response(StatusCode::BAD_GATEWAY, "api_error", "No upstream key could serve this request"),
        Some(code) => {
            let status = StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY);
            error_response(status, "api_error", &format!("All upstream keys are unavailable (last upstream status {code})"))
        }
    }
}

/// Fills in a usage row's token counts once the response is done, even if the client hangs up.
struct UsageRecorder {
    db: sqlx::SqlitePool,
    id: i64,
    scanner: crate::usage::StreamScanner,
}

impl Drop for UsageRecorder {
    fn drop(&mut self) {
        let (db, id, input, output) = (self.db.clone(), self.id, self.scanner.input, self.scanner.output);
        tokio::spawn(async move { let _ = db::finish_usage(&db, id, input, output).await; });
    }
}

async fn relay(
    state: &AppState,
    headers: &HeaderMap,
    body: &str,
    endpoint: Endpoint,
) -> Result<Response, Response> {
    // Parse body
    let mut body_json: Value = serde_json::from_str(body).map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
    let requested = body_json["model"].as_str().ok_or_else(|| StatusCode::BAD_REQUEST.into_response())?.to_string();

    // Resolve model
    let (provider, entry) = state.catalog.resolve(state, &requested).await
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "not_found_error", &format!("Unknown model {requested}")))?;
    // Limits and usage use the catalog id, so `ark-code-latest` and `ark/ark-code-latest` count as one model
    let model_name = entry.id;
    // Token counting isn't usage
    let (_, usage_id) = admit(state, headers, &model_name, !matches!(endpoint, Endpoint::CountTokens)).await?;
    body_json["model"] = Value::String(entry.upstream_model);

    let is_stream = body_json.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    if is_stream && matches!(endpoint, Endpoint::ChatCompletions) && body_json.get("stream_options").is_none() {
        // Without this, streamed Chat Completions never report token usage
        body_json["stream_options"] = serde_json::json!({ "include_usage": true });
    }
    let url = endpoint.url(&provider);
    let body_str = body_json.to_string();

    let keys = provider_keys(state, provider.id).await;

    // Anthropic clients pick the API version and opt into beta features per request
    let anthropic_version = headers.get("anthropic-version").and_then(|v| v.to_str().ok()).unwrap_or("2023-06-01");
    let anthropic_beta = headers.get("anthropic-beta").and_then(|v| v.to_str().ok());

    let sent = state.keys.send(keys, |key| {
        let mut req = state.http_client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("User-Agent", &provider.user_agent)
            .header("x-app", "cli")
            .body(body_str.clone());
        if endpoint.is_anthropic() {
            req = req.header("x-api-key", key).header("anthropic-version", anthropic_version);
            if let Some(beta) = anthropic_beta {
                req = req.header("anthropic-beta", beta);
            }
        } else {
            req = req.header("Authorization", format!("Bearer {key}"));
        }
        req
    }).await;

    let resp = match sent {
        Ok(resp) if resp.status().is_success() => resp,
        other => {
            // Refused or failed requests don't count against the user
            if let Some(id) = usage_id {
                let _ = db::drop_usage(&state.db, id).await;
            }
            return match other {
                Ok(resp) => stream_response(resp, None).map_err(IntoResponse::into_response),
                Err(e) => Err(exhausted_response(e)),
            };
        }
    };

    let Some(usage_id) = usage_id else {
        return stream_response(resp, None).map_err(IntoResponse::into_response);
    };
    if is_stream {
        let recorder = UsageRecorder { db: state.db.clone(), id: usage_id, scanner: Default::default() };
        return stream_response(resp, Some(recorder)).map_err(IntoResponse::into_response);
    }

    // Non-streaming: read body, extract tokens, then return
    let status_code = resp.status();
    let resp_bytes = resp.bytes().await.map_err(|_| StatusCode::BAD_GATEWAY.into_response())?;
    let (input_t, output_t) = crate::usage::from_body(&resp_bytes);
    let _ = db::finish_usage(&state.db, usage_id, input_t, output_t).await;
    Response::builder()
        .status(status_code)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(resp_bytes))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, Response> {
    relay(&state, &headers, &body, Endpoint::ChatCompletions).await
}

pub async fn responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, Response> {
    relay(&state, &headers, &body, Endpoint::Responses).await
}

pub async fn anthropic_messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, Response> {
    relay(&state, &headers, &body, Endpoint::Messages).await
}

pub async fn count_tokens(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, Response> {
    relay(&state, &headers, &body, Endpoint::CountTokens).await
}

pub async fn image_generations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, Response> {
    relay_generic(&state, &headers, &body, false).await
}

pub async fn video_generations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, Response> {
    relay_generic(&state, &headers, &body, true).await
}

/// Generic relay for non-chat endpoints (images, videos).
/// For videos, uses Google's native predict endpoint.
async fn relay_generic(
    state: &AppState,
    headers: &HeaderMap,
    body: &str,
    is_video: bool,
) -> Result<Response, Response> {
    let mut body_json: Value = serde_json::from_str(body).map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
    let requested = body_json["model"].as_str().ok_or_else(|| StatusCode::BAD_REQUEST.into_response())?.to_string();

    let (provider, entry) = state.catalog.resolve(state, &requested).await
        .ok_or_else(|| error_response(StatusCode::NOT_FOUND, "not_found_error", &format!("Unknown model {requested}")))?;
    let (_, usage_id) = admit(state, headers, &entry.id, true).await?;
    let upstream_model = entry.upstream_model;
    body_json["model"] = Value::String(upstream_model.clone());

    let base = provider.base_url.trim_end_matches('/');
    let url = if is_video {
        // Native Google API: …/v1beta/openai → …/v1beta/models/{model}:predictLongRunning
        let native = base.strip_suffix("/openai").unwrap_or(base);
        format!("{native}/models/{upstream_model}:predictLongRunning")
    } else {
        format!("{base}/images/generations")
    };

    let body_str = body_json.to_string();
    let keys = provider_keys(state, provider.id).await;

    let sent = state.keys.send(keys, |key| {
        let req = state.http_client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("User-Agent", &provider.user_agent)
            .body(body_str.clone());
        if is_video {
            // Native Google API uses x-goog-api-key
            req.header("x-goog-api-key", key)
        } else {
            req.header("Authorization", format!("Bearer {key}"))
        }
    }).await;

    let ok = matches!(&sent, Ok(r) if r.status().is_success());
    if let (false, Some(id)) = (ok, usage_id) {
        let _ = db::drop_usage(&state.db, id).await;
    }
    match sent {
        Ok(resp) => stream_response(resp, None).map_err(IntoResponse::into_response),
        Err(e) => Err(exhausted_response(e)),
    }
}

fn stream_response(resp: reqwest::Response, mut recorder: Option<UsageRecorder>) -> Result<Response, StatusCode> {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = resp.headers().get("content-type").cloned();

    let stream = resp.bytes_stream().map(move |chunk| {
        if let (Ok(bytes), Some(r)) = (&chunk, recorder.as_mut()) {
            r.scanner.feed(bytes);
        }
        chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    });

    let mut builder = Response::builder().status(status);
    if let Some(ct) = content_type {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }

    builder.body(Body::from_stream(stream)).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Proxy /v1/files/* requests to Kimi's file API. Admins only: every user would share one
/// upstream account, so anyone could list and download everyone else's files.
/// Forwards the raw body (multipart or JSON) as-is.
async fn file_proxy(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
    method: &str,
    body: Bytes,
) -> Result<Response, Response> {
    let api_key = extract_api_key(headers).ok_or_else(|| StatusCode::UNAUTHORIZED.into_response())?;
    let user = db::find_user_by_api_key(&state.db, &api_key).await.map_err(|_| StatusCode::UNAUTHORIZED.into_response())?;
    if !user.is_admin {
        return Err(error_response(StatusCode::FORBIDDEN, "permission_error", "File endpoints are limited to admins"));
    }

    // Only a Kimi provider's keys may go to Moonshot
    let providers = db::list_enabled_providers(&state.db).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
    let kimi = providers.iter().find(|p| {
        reqwest::Url::parse(&p.base_url).ok().and_then(|u| u.host_str().map(|h| h == "api.kimi.com")).unwrap_or(false)
    }).ok_or_else(|| error_response(StatusCode::NOT_FOUND, "not_found_error", "No Kimi provider configured"))?;
    let keys = provider_keys(state, kimi.id).await;
    let url = format!("https://api.moonshot.cn/{}", path.trim_start_matches('/'));
    let content_type = headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream");

    let sent = state.keys.send(keys, |key| {
        let req = match method {
            "GET" => state.http_client.get(&url),
            "DELETE" => state.http_client.delete(&url),
            _ => state.http_client.post(&url),
        };
        req.header("Content-Type", content_type)
            .header("Authorization", format!("Bearer {key}"))
            .body(body.clone())
    }).await;

    match sent {
        Ok(resp) => stream_response(resp, None).map_err(IntoResponse::into_response),
        Err(e) => Err(exhausted_response(e)),
    }
}

/// File ids go into the upstream URL; anything but a plain id could reach other upstream endpoints.
fn check_file_id(file_id: &str) -> Result<(), StatusCode> {
    let ok = !file_id.is_empty() && file_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok { Ok(()) } else { Err(StatusCode::BAD_REQUEST) }
}

pub async fn files_upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Response> {
    file_proxy(&state, &headers, "/v1/files", "POST", body).await
}

pub async fn files_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    file_proxy(&state, &headers, "/v1/files", "GET", Bytes::new()).await
}

pub async fn files_action(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(file_id): Path<String>,
    body: Bytes,
) -> Result<Response, Response> {
    check_file_id(&file_id).map_err(IntoResponse::into_response)?;
    file_proxy(&state, &headers, &format!("/v1/files/{file_id}"), "GET", body).await
}

pub async fn files_content(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(file_id): Path<String>,
) -> Result<Response, Response> {
    check_file_id(&file_id).map_err(IntoResponse::into_response)?;
    file_proxy(&state, &headers, &format!("/v1/files/{file_id}/content"), "GET", Bytes::new()).await
}

pub async fn files_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(file_id): Path<String>,
) -> Result<Response, Response> {
    check_file_id(&file_id).map_err(IntoResponse::into_response)?;
    file_proxy(&state, &headers, &format!("/v1/files/{file_id}"), "DELETE", Bytes::new()).await
}
