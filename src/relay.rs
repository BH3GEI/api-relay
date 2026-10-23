use axum::{
    body::{Body, Bytes},
    extract::{Json, Path, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::Response,
};
use futures::StreamExt;
use serde_json::Value;
use std::sync::Arc;

use crate::{AppState, db};

// Extract API key from either "Authorization: Bearer xxx" or "x-api-key: xxx"
fn extract_api_key(headers: &HeaderMap) -> Option<String> {
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
    let api_key = extract_api_key(req.headers()).ok_or(StatusCode::UNAUTHORIZED)?;
    db::find_user_by_api_key(&state.db, &api_key).await.map_err(|_| StatusCode::UNAUTHORIZED)?;
    Ok(next.run(req).await)
}

pub async fn list_models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    let api_key = extract_api_key(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    db::find_user_by_api_key(&state.db, &api_key).await.map_err(|_| StatusCode::UNAUTHORIZED)?;

    let providers: std::collections::HashMap<i64, String> = db::list_providers(&state.db).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .into_iter().map(|p| (p.id, p.name)).collect();
    let data: Vec<Value> = state.catalog.entries().into_iter().map(|e| {
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
    }).collect();

    Ok(Json(serde_json::json!({
        "object": "list",
        "data": data,
        "has_more": false,
    })))
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

/// Authenticate the caller and check their limit; returns the user id.
async fn authorize(state: &AppState, headers: &HeaderMap, model_name: &str) -> Result<i64, StatusCode> {
    let api_key = extract_api_key(headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let user = db::find_user_by_api_key(&state.db, &api_key).await.map_err(|_| StatusCode::UNAUTHORIZED)?;
    let allowed = db::check_limit(&state.db, user.id, model_name).await.unwrap_or(true);
    if !allowed {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    Ok(user.id)
}

async fn provider_keys(state: &AppState, provider_id: i64) -> Vec<String> {
    db::get_provider_keys(&state.db, provider_id).await
        .map(|keys| keys.into_iter().map(|k| k.api_key).collect())
        .unwrap_or_default()
}

async fn relay(
    state: &AppState,
    headers: &HeaderMap,
    body: &str,
    endpoint: Endpoint,
) -> Result<Response, StatusCode> {
    // Parse body
    let mut body_json: Value = serde_json::from_str(body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let requested = body_json["model"].as_str().ok_or(StatusCode::BAD_REQUEST)?.to_string();

    // Resolve model
    let (provider, entry) = state.catalog.resolve(state, &requested).await.ok_or(StatusCode::NOT_FOUND)?;
    // Limits and usage use the catalog id, so `ark-code-latest` and `ark/ark-code-latest` count as one model
    let model_name = entry.id;
    let upstream_model = entry.upstream_model;
    let user_id = authorize(state, headers, &model_name).await?;
    body_json["model"] = Value::String(upstream_model.clone());

    let is_stream = body_json.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let url = endpoint.url(&provider);
    let body_str = body_json.to_string();

    let keys = provider_keys(state, provider.id).await;

    // Anthropic clients pick the API version and opt into beta features per request
    let anthropic_version = headers.get("anthropic-version").and_then(|v| v.to_str().ok()).unwrap_or("2023-06-01");
    let anthropic_beta = headers.get("anthropic-beta").and_then(|v| v.to_str().ok());

    let resp = state.keys.send(keys, |key| {
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
    }).await?;

    let status_code = resp.status().as_u16();
    // Token counting is not usage
    if !(200..300).contains(&status_code) || matches!(endpoint, Endpoint::CountTokens) {
        return stream_response(resp);
    }

    if is_stream {
        // Streaming: log request count only, can't read tokens
        let _ = db::log_usage(&state.db, user_id, &model_name, 0, 0).await;
        return stream_response(resp);
    }

    // Non-streaming: read body, extract tokens, then return
    let resp_bytes = resp.bytes().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
    let (input_t, output_t) = extract_tokens(&resp_bytes);
    let _ = db::log_usage(&state.db, user_id, &model_name, input_t, output_t).await;
    Response::builder()
        .status(status_code)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(resp_bytes))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, StatusCode> {
    relay(&state, &headers, &body, Endpoint::ChatCompletions).await
}

pub async fn responses(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, StatusCode> {
    relay(&state, &headers, &body, Endpoint::Responses).await
}

pub async fn anthropic_messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, StatusCode> {
    relay(&state, &headers, &body, Endpoint::Messages).await
}

pub async fn count_tokens(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, StatusCode> {
    relay(&state, &headers, &body, Endpoint::CountTokens).await
}

pub async fn image_generations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, StatusCode> {
    relay_generic(&state, &headers, &body, false).await
}

pub async fn video_generations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, StatusCode> {
    relay_generic(&state, &headers, &body, true).await
}

/// Generic relay for non-chat endpoints (images, videos).
/// For videos, uses Google's native predict endpoint.
async fn relay_generic(
    state: &AppState,
    headers: &HeaderMap,
    body: &str,
    is_video: bool,
) -> Result<Response, StatusCode> {
    let mut body_json: Value = serde_json::from_str(body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let requested = body_json["model"].as_str().ok_or(StatusCode::BAD_REQUEST)?.to_string();

    let (provider, entry) = state.catalog.resolve(state, &requested).await.ok_or(StatusCode::NOT_FOUND)?;
    // Limits and usage use the catalog id, so `ark-code-latest` and `ark/ark-code-latest` count as one model
    let model_name = entry.id;
    let upstream_model = entry.upstream_model;
    let user_id = authorize(state, headers, &model_name).await?;
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

    let resp = state.keys.send(keys, |key| {
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
    }).await?;

    if resp.status().is_success() {
        let _ = db::log_usage(&state.db, user_id, &model_name, 0, 0).await;
    }
    stream_response(resp)
}

fn extract_tokens(body: &[u8]) -> (i64, i64) {
    if let Ok(json) = serde_json::from_slice::<Value>(body) {
        let usage = &json["usage"];
        let input = usage["prompt_tokens"].as_i64()
            .or_else(|| usage["input_tokens"].as_i64())
            .unwrap_or(0);
        let output = usage["completion_tokens"].as_i64()
            .or_else(|| usage["output_tokens"].as_i64())
            .unwrap_or(0);
        (input, output)
    } else {
        (0, 0)
    }
}

fn stream_response(resp: reqwest::Response) -> Result<Response, StatusCode> {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = resp.headers().get("content-type").cloned();

    let stream = resp.bytes_stream().map(|chunk| {
        chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    });

    let mut builder = Response::builder().status(status);
    if let Some(ct) = content_type {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }

    builder.body(Body::from_stream(stream)).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Proxy /v1/files/* requests to Kimi's file API.
/// Forwards the raw body (multipart or JSON) as-is.
async fn file_proxy(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
    method: &str,
    body: Bytes,
) -> Result<Response, StatusCode> {
    let api_key = extract_api_key(headers).ok_or(StatusCode::UNAUTHORIZED)?;
    db::find_user_by_api_key(&state.db, &api_key).await.map_err(|_| StatusCode::UNAUTHORIZED)?;

    let keys = provider_keys(state, 1).await;
    let url = format!("https://api.moonshot.cn/{}", path.trim_start_matches('/'));
    let content_type = headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream");

    let resp = state.keys.send(keys, |key| {
        let req = match method {
            "GET" => state.http_client.get(&url),
            "DELETE" => state.http_client.delete(&url),
            _ => state.http_client.post(&url),
        };
        req.header("Content-Type", content_type)
            .header("Authorization", format!("Bearer {key}"))
            .body(body.clone())
    }).await?;

    stream_response(resp)
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
) -> Result<Response, StatusCode> {
    file_proxy(&state, &headers, "/v1/files", "POST", body).await
}

pub async fn files_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    file_proxy(&state, &headers, "/v1/files", "GET", Bytes::new()).await
}

pub async fn files_action(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(file_id): Path<String>,
    body: Bytes,
) -> Result<Response, StatusCode> {
    check_file_id(&file_id)?;
    file_proxy(&state, &headers, &format!("/v1/files/{file_id}"), "GET", body).await
}

pub async fn files_content(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(file_id): Path<String>,
) -> Result<Response, StatusCode> {
    check_file_id(&file_id)?;
    file_proxy(&state, &headers, &format!("/v1/files/{file_id}/content"), "GET", Bytes::new()).await
}

pub async fn files_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(file_id): Path<String>,
) -> Result<Response, StatusCode> {
    check_file_id(&file_id)?;
    file_proxy(&state, &headers, &format!("/v1/files/{file_id}"), "DELETE", Bytes::new()).await
}
