use axum::{
    body::{Body, Bytes},
    extract::{Json, Path, State},
    http::{HeaderMap, StatusCode, header},
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

pub async fn list_models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    let api_key = extract_api_key(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    db::find_user_by_api_key(&state.db, &api_key).await.map_err(|_| StatusCode::UNAUTHORIZED)?;

    let models = db::list_models(&state.db).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let data: Vec<Value> = models.iter().map(|m| {
        serde_json::json!({
            "id": m.public_name,
            "object": "model",
            "owned_by": "relay",
        })
    }).collect();

    Ok(Json(serde_json::json!({
        "object": "list",
        "data": data,
    })))
}

const RETRY_STATUSES: &[u16] = &[401, 403, 429, 503];

async fn try_upstream(
    client: &reqwest::Client,
    url: &str,
    user_agent: &str,
    key: &str,
    body: &str,
    use_anthropic_auth: bool,
) -> Result<reqwest::Response, StatusCode> {
    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("User-Agent", user_agent)
        .body(body.to_string());

    if use_anthropic_auth {
        req = req
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01");
    } else {
        req = req.header("Authorization", format!("Bearer {key}"));
    }

    req.send().await.map_err(|_| StatusCode::BAD_GATEWAY)
}

async fn relay_with_fallback(
    state: &AppState,
    headers: &HeaderMap,
    body: &str,
    use_anthropic_auth: bool,
) -> Result<Response, StatusCode> {
    // Auth
    let api_key = extract_api_key(headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let user = db::find_user_by_api_key(&state.db, &api_key).await.map_err(|_| StatusCode::UNAUTHORIZED)?;

    // Parse body
    let mut body_json: Value = serde_json::from_str(body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let model_name = body_json["model"].as_str().ok_or(StatusCode::BAD_REQUEST)?.to_string();

    // Check rate limit
    let allowed = db::check_limit(&state.db, user.id, &model_name).await.unwrap_or(true);
    if !allowed {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }

    // Resolve model
    let resolved = db::resolve_model(&state.db, &model_name).await.map_err(|_| StatusCode::NOT_FOUND)?;
    body_json["model"] = Value::String(resolved.upstream_model.clone());

    let is_stream = body_json.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    let anthropic_path;
    let upstream_path = if use_anthropic_auth {
        anthropic_path = resolved.upstream_path.replace("/chat/completions", "/messages");
        &anthropic_path
    } else {
        &resolved.upstream_path
    };
    let url = format!("{}{}", resolved.base_url.trim_end_matches('/'), upstream_path);
    let body_str = body_json.to_string();

    // Gather keys
    let mut keys: Vec<String> = Vec::new();
    if let Ok(provider_keys) = db::get_provider_keys(&state.db, resolved.provider_id).await {
        for pk in &provider_keys {
            keys.push(pk.api_key.clone());
        }
    }
    if let Some(client_key) = headers.get("x-upstream-key").and_then(|v| v.to_str().ok()) {
        keys.push(client_key.to_string());
    }

    if keys.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Try each key with fallback
    let mut last_resp = None;
    for key in &keys {
        let resp = try_upstream(&state.http_client, &url, &resolved.user_agent, key, &body_str, use_anthropic_auth).await?;
        let status_code = resp.status().as_u16();

        if !RETRY_STATUSES.contains(&status_code) {
            // Log usage
            if status_code >= 200 && status_code < 300 {
                if is_stream {
                    // Streaming: log request count only, can't read tokens
                    let _ = db::log_usage(&state.db, user.id, &model_name, 0, 0).await;
                    return stream_response(resp);
                } else {
                    // Non-streaming: read body, extract tokens, then return
                    let resp_bytes = resp.bytes().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
                    let (input_t, output_t) = extract_tokens(&resp_bytes);
                    let _ = db::log_usage(&state.db, user.id, &model_name, input_t, output_t).await;
                    let resp_status = StatusCode::from_u16(status_code).unwrap_or(StatusCode::OK);
                    return Ok(Response::builder()
                        .status(resp_status)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(resp_bytes))
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?);
                }
            }
            return stream_response(resp);
        }
        last_resp = Some(resp);
    }

    stream_response(last_resp.unwrap())
}

pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, StatusCode> {
    relay_with_fallback(&state, &headers, &body, false).await
}

pub async fn anthropic_messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, StatusCode> {
    relay_with_fallback(&state, &headers, &body, true).await
}

pub async fn image_generations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, StatusCode> {
    relay_generic(&state, &headers, &body, "/v1beta/openai/images/generations").await
}

pub async fn video_generations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, StatusCode> {
    relay_generic(&state, &headers, &body, "").await
}

/// Generic relay for non-chat endpoints (images, videos).
/// For videos, uses Google's native predict endpoint.
async fn relay_generic(
    state: &AppState,
    headers: &HeaderMap,
    body: &str,
    override_path: &str,
) -> Result<Response, StatusCode> {
    let api_key = extract_api_key(headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let user = db::find_user_by_api_key(&state.db, &api_key).await.map_err(|_| StatusCode::UNAUTHORIZED)?;

    let mut body_json: Value = serde_json::from_str(body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let model_name = body_json["model"].as_str().ok_or(StatusCode::BAD_REQUEST)?.to_string();

    // Check rate limit
    let allowed = db::check_limit(&state.db, user.id, &model_name).await.unwrap_or(true);
    if !allowed {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }

    let resolved = db::resolve_model(&state.db, &model_name).await.map_err(|_| StatusCode::NOT_FOUND)?;
    body_json["model"] = Value::String(resolved.upstream_model.clone());

    let url = if override_path.is_empty() {
        // Video: use native Google predict endpoint
        format!("{}/v1beta/models/{}:predictLongRunning",
            resolved.base_url.trim_end_matches('/'), resolved.upstream_model)
    } else {
        format!("{}{}", resolved.base_url.trim_end_matches('/'), override_path)
    };

    let body_str = body_json.to_string();

    let mut keys: Vec<String> = Vec::new();
    if let Ok(provider_keys) = db::get_provider_keys(&state.db, resolved.provider_id).await {
        for pk in &provider_keys {
            keys.push(pk.api_key.clone());
        }
    }
    if keys.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let mut last_resp = None;
    for key in &keys {
        let mut req = state.http_client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("User-Agent", &resolved.user_agent)
            .body(body_str.clone());

        if override_path.is_empty() {
            // Native Google API uses x-goog-api-key
            req = req.header("x-goog-api-key", key.as_str());
        } else {
            req = req.header("Authorization", format!("Bearer {key}"));
        }

        let resp = req.send().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
        let status_code = resp.status().as_u16();
        if !RETRY_STATUSES.contains(&status_code) {
            if status_code >= 200 && status_code < 300 {
                let _ = db::log_usage(&state.db, user.id, &model_name, 0, 0).await;
            }
            return stream_response(resp);
        }
        last_resp = Some(resp);
    }

    stream_response(last_resp.unwrap())
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

    let provider_keys = db::get_provider_keys(&state.db, 1).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if provider_keys.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let url = format!("https://api.moonshot.cn/{}", path.trim_start_matches('/'));

    let content_type = headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream");

    let mut last_resp = None;
    for pk in &provider_keys {
        let req = match method {
            "DELETE" => state.http_client.delete(&url),
            _ => state.http_client.post(&url),
        };
        let resp = req
            .header("Content-Type", content_type)
            .header("Authorization", format!("Bearer {}", pk.api_key))
            .body(body.clone())
            .send()
            .await
            .map_err(|_| StatusCode::BAD_GATEWAY)?;

        let status_code = resp.status().as_u16();
        if !RETRY_STATUSES.contains(&status_code) {
            return stream_response(resp);
        }
        last_resp = Some(resp);
    }

    stream_response(last_resp.unwrap())
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
    file_proxy(&state, &headers, &format!("/v1/files/{file_id}"), "GET", body).await
}

pub async fn files_content(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(file_id): Path<String>,
) -> Result<Response, StatusCode> {
    file_proxy(&state, &headers, &format!("/v1/files/{file_id}/content"), "GET", Bytes::new()).await
}

pub async fn files_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(file_id): Path<String>,
) -> Result<Response, StatusCode> {
    file_proxy(&state, &headers, &format!("/v1/files/{file_id}"), "DELETE", Bytes::new()).await
}
