use axum::{
    body::Body,
    extract::{Json, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use futures::StreamExt;
use serde_json::Value;
use std::sync::Arc;

use crate::{AppState, db};

pub async fn list_models(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    let api_key = crate::auth::extract_bearer(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
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
    url: &str,
    user_agent: &str,
    auth: &str,
    body: &str,
) -> Result<reqwest::Response, StatusCode> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .unwrap()
        .post(url)
        .header("Content-Type", "application/json")
        .header("Authorization", auth)
        .header("User-Agent", user_agent)
        .body(body.to_string())
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)
}

pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, StatusCode> {
    // Auth
    let api_key = crate::auth::extract_bearer(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    db::find_user_by_api_key(&state.db, &api_key).await.map_err(|_| StatusCode::UNAUTHORIZED)?;

    // Parse body
    let mut body_json: Value = serde_json::from_str(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let model_name = body_json["model"].as_str().ok_or(StatusCode::BAD_REQUEST)?.to_string();

    // Resolve model
    let resolved = db::resolve_model(&state.db, &model_name).await.map_err(|_| StatusCode::NOT_FOUND)?;
    body_json["model"] = Value::String(resolved.upstream_model.clone());
    let url = format!("{}{}", resolved.base_url.trim_end_matches('/'), resolved.upstream_path);
    let body_str = body_json.to_string();

    // Gather keys: server-side provider_keys first, then client's x-upstream-key as fallback
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
        let auth = format!("Bearer {key}");
        let resp = try_upstream(&url, &resolved.user_agent, &auth, &body_str).await?;
        let status_code = resp.status().as_u16();

        if !RETRY_STATUSES.contains(&status_code) {
            // Success or non-retryable error — return this response
            return stream_response(resp);
        }
        // Retryable error — try next key
        last_resp = Some(resp);
    }

    // All keys exhausted, return last response
    stream_response(last_resp.unwrap())
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
