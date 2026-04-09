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
    // Authenticate by api_key
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

pub async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, StatusCode> {
    // Auth by api_key
    let api_key = crate::auth::extract_bearer(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    db::find_user_by_api_key(&state.db, &api_key).await.map_err(|_| StatusCode::UNAUTHORIZED)?;

    // Parse body to get model
    let mut body_json: Value = serde_json::from_str(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let model_name = body_json["model"].as_str().ok_or(StatusCode::BAD_REQUEST)?.to_string();

    // Resolve model -> provider
    let resolved = db::resolve_model(&state.db, &model_name).await.map_err(|_| {
        StatusCode::NOT_FOUND
    })?;

    // Replace model name with upstream model
    body_json["model"] = Value::String(resolved.upstream_model.clone());

    // Build upstream URL
    let url = format!("{}{}", resolved.base_url.trim_end_matches('/'), resolved.upstream_path);

    // Determine auth: use provider's key if set, otherwise pass through client's original auth
    let upstream_auth = if let Some(ref pk) = resolved.provider_api_key {
        format!("Bearer {pk}")
    } else {
        // Pass through the original Authorization header (which is the relay api_key)
        // This won't work — client should send upstream key in a custom header
        // For now, require provider api_key to be set, or client sends upstream key
        headers.get("x-upstream-key")
            .and_then(|v| v.to_str().ok())
            .map(|k| format!("Bearer {k}"))
            .ok_or(StatusCode::BAD_REQUEST)?
    };

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", &upstream_auth)
        .header("User-Agent", &resolved.user_agent)
        .body(body_json.to_string())
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = resp.headers().get("content-type").cloned();

    // Stream the response back
    let stream = resp.bytes_stream().map(|chunk| {
        chunk.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    });

    let mut builder = Response::builder().status(status);
    if let Some(ct) = content_type {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }

    builder
        .body(Body::from_stream(stream))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
