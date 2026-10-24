//! `/v1/files`, proxied to Kimi's (Moonshot's) file API for image and video understanding.
//!
//! Admins only: every user would share one upstream account, so anyone could list and
//! download everyone else's files.

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, Method, StatusCode},
    response::Response,
};
use std::sync::Arc;

use crate::{AppState, db, error::{Error, Result}, extract::Caller};

const KIMI_HOST: &str = "api.kimi.com";
const MOONSHOT: &str = "https://api.moonshot.cn";

pub async fn upload(State(state): State<Arc<AppState>>, caller: Caller, headers: HeaderMap, body: Bytes) -> Result<Response> {
    forward(&state, caller, &headers, Method::POST, "/v1/files", body).await
}

pub async fn list(State(state): State<Arc<AppState>>, caller: Caller, headers: HeaderMap) -> Result<Response> {
    forward(&state, caller, &headers, Method::GET, "/v1/files", Bytes::new()).await
}

pub async fn get(State(state): State<Arc<AppState>>, caller: Caller, headers: HeaderMap, Path(id): Path<String>) -> Result<Response> {
    forward(&state, caller, &headers, Method::GET, &format!("/v1/files/{}", file_id(&id)?), Bytes::new()).await
}

pub async fn content(State(state): State<Arc<AppState>>, caller: Caller, headers: HeaderMap, Path(id): Path<String>) -> Result<Response> {
    forward(&state, caller, &headers, Method::GET, &format!("/v1/files/{}/content", file_id(&id)?), Bytes::new()).await
}

pub async fn delete(State(state): State<Arc<AppState>>, caller: Caller, headers: HeaderMap, Path(id): Path<String>) -> Result<Response> {
    forward(&state, caller, &headers, Method::DELETE, &format!("/v1/files/{}", file_id(&id)?), Bytes::new()).await
}

/// File ids go into the upstream URL; anything but a plain id could reach other upstream endpoints.
fn file_id(id: &str) -> Result<&str> {
    let plain = !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if plain { Ok(id) } else { Err(StatusCode::BAD_REQUEST.into()) }
}

/// Forwards the raw body (multipart or JSON) as it is.
async fn forward(state: &AppState, Caller(user): Caller, headers: &HeaderMap, method: Method, path: &str, body: Bytes) -> Result<Response> {
    if !user.is_admin {
        return Err(Error::api(StatusCode::FORBIDDEN, "permission_error", "File endpoints are limited to admins"));
    }
    // Only a Kimi provider's keys may go to Moonshot
    let kimi = db::providers::list_enabled(&state.db).await?.into_iter()
        .find(|p| p.host().as_deref() == Some(KIMI_HOST))
        .ok_or_else(|| Error::api(StatusCode::NOT_FOUND, "not_found_error", "No Kimi provider configured"))?;
    let keys = db::providers::usable_keys(&state.db, kimi.id).await?;
    let url = format!("{MOONSHOT}{path}");
    let content_type = headers.get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream");

    let sent = state.keys.send(keys, |key| {
        state.http_client.request(method.clone(), &url)
            .header("Content-Type", content_type)
            .header("Authorization", format!("Bearer {key}"))
            .body(body.clone())
    }).await;
    match sent {
        Ok(resp) => super::pass_through(resp, None),
        Err(e) => Err(super::exhausted(e)),
    }
}
