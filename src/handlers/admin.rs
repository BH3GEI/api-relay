//! The admin API, for the admin page on the LAN side. Every handler takes an `Admin`.

use axum::{Json, extract::{Path, Query, State}, http::StatusCode};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{
    AppState, auth, catalog,
    db::{self, limits, providers::{Provider, ProviderInput, ProviderKey}, users::User},
    error::{OrNotFound, Result},
    extract::Admin,
    handlers::user::{LimitUse, UsageQuery, with_usage},
    pool, quota, tunnel,
};

type St = State<Arc<AppState>>;

// ── Users ──

pub async fn users(State(state): St, _: Admin) -> Result<Json<Vec<User>>> {
    Ok(Json(db::users::list(&state.db).await?))
}

#[derive(Deserialize)]
pub struct NewUser {
    username: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    is_admin: bool,
}

pub async fn create_user(State(state): St, _: Admin, Json(req): Json<NewUser>) -> Result<Json<User>> {
    let (email, password) = (req.email.unwrap_or_default(), req.password.unwrap_or_default());
    Ok(Json(auth::create_user(&state.db, &req.username, &email, &password, req.is_admin).await?))
}

#[derive(Deserialize)]
pub struct NewPassword {
    password: String,
}

/// For signing in on the public address.
pub async fn set_password(State(state): St, _: Admin, Path(id): Path<i64>, Json(req): Json<NewPassword>) -> Result<StatusCode> {
    auth::set_password(&state.db, id, req.password).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_user(State(state): St, _: Admin, Path(id): Path<i64>) -> Result<StatusCode> {
    db::users::delete(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Providers and their keys ──
// Changes refresh the catalog in the background, so the admin's request doesn't wait on upstreams.

pub async fn providers(State(state): St, _: Admin) -> Result<Json<Vec<Provider>>> {
    Ok(Json(db::providers::list(&state.db).await?))
}

pub async fn create_provider(State(state): St, _: Admin, Json(req): Json<ProviderInput>) -> Result<Json<Provider>> {
    let provider = db::providers::create(&state.db, &req).await?;
    catalog::refresh_in_background(&state);
    Ok(Json(provider))
}

pub async fn update_provider(State(state): St, _: Admin, Path(id): Path<i64>, Json(req): Json<ProviderInput>) -> Result<Json<Provider>> {
    let provider = db::providers::update(&state.db, id, &req).await?.or_not_found()?;
    catalog::refresh_in_background(&state);
    Ok(Json(provider))
}

pub async fn delete_provider(State(state): St, _: Admin, Path(id): Path<i64>) -> Result<StatusCode> {
    db::providers::delete(&state.db, id).await?;
    catalog::refresh_in_background(&state);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct KeyView {
    #[serde(flatten)]
    key: ProviderKey,
    status: pool::KeyStatus,
    /// Plan usage windows, when the upstream reports them
    quota: Option<Vec<quota::Window>>,
}

pub async fn keys(State(state): St, _: Admin, Path(provider_id): Path<i64>) -> Result<Json<Vec<KeyView>>> {
    let provider = db::providers::get(&state.db, provider_id).await?.or_not_found()?;
    let keys = db::providers::keys(&state.db, provider_id).await?;
    let quotas = futures::future::join_all(keys.iter().map(|k| state.quota.get(&state, &provider, &k.api_key))).await;
    Ok(Json(keys.into_iter().zip(quotas).map(|(key, quota)| {
        let status = state.keys.status(&key.api_key);
        KeyView { key, status, quota }
    }).collect()))
}

#[derive(Deserialize)]
pub struct NewKey {
    api_key: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    priority: i64,
}

pub async fn add_key(State(state): St, _: Admin, Path(provider_id): Path<i64>, Json(req): Json<NewKey>) -> Result<Json<ProviderKey>> {
    let key = db::providers::add_key(&state.db, provider_id, &req.api_key, &req.label, req.priority).await?;
    catalog::refresh_in_background(&state);
    Ok(Json(key))
}

pub async fn delete_key(State(state): St, _: Admin, Path(id): Path<i64>) -> Result<StatusCode> {
    db::providers::delete_key(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The model ids users can ask for.
pub async fn catalog(State(state): St, _: Admin) -> Json<Vec<String>> {
    Json(state.catalog.ids())
}

pub async fn refresh_catalog(State(state): St, _: Admin) -> Json<Vec<String>> {
    state.catalog.refresh(&state).await;
    Json(state.catalog.ids())
}

// ── Usage and limits ──

#[derive(Serialize)]
pub struct Usage {
    series: Vec<db::usage::Bucket>,
    rows: Vec<db::usage::UserModelTotals>,
}

/// Everyone's usage over a period.
pub async fn usage(State(state): St, _: Admin, Query(q): Query<UsageQuery>) -> Result<Json<Usage>> {
    Ok(Json(Usage {
        series: db::usage::series(&state.db, None, q.range()).await?,
        rows: db::usage::by_user_and_model(&state.db, q.range()).await?,
    }))
}

pub async fn limits(State(state): St, _: Admin) -> Result<Json<Vec<LimitUse>>> {
    Ok(Json(with_usage(&state, limits::list(&state.db).await?).await?))
}

#[derive(Deserialize)]
pub struct NewLimit {
    user_id: i64,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    window_secs: Option<i64>,
    #[serde(default)]
    max_requests: Option<i64>,
    #[serde(default)]
    max_tokens: Option<i64>,
}

pub async fn set_limit(State(state): St, _: Admin, Json(req): Json<NewLimit>) -> Result<Json<limits::Limit>> {
    let model = req.model.as_deref().unwrap_or(limits::ALL_MODELS);
    // A limit on a name that isn't a catalog id would silently never match
    if model != limits::ALL_MODELS && !state.catalog.has(model) {
        return Err(StatusCode::UNPROCESSABLE_ENTITY.into());
    }
    let window = req.window_secs.unwrap_or(86400);
    if window <= 0 {
        return Err(StatusCode::UNPROCESSABLE_ENTITY.into());
    }
    let (max_requests, max_tokens) = (req.max_requests.unwrap_or(limits::UNLIMITED), req.max_tokens.unwrap_or(limits::UNLIMITED));
    Ok(Json(limits::set(&state.db, req.user_id, model, window, max_requests, max_tokens).await?))
}

pub async fn delete_limit(State(state): St, _: Admin, Path(id): Path<i64>) -> Result<StatusCode> {
    limits::delete(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Public access ──

pub async fn tunnel(State(state): St, _: Admin) -> Json<tunnel::TunnelStatus> {
    Json(state.tunnel.status())
}

#[derive(Deserialize)]
pub struct TunnelToken {
    token: String,
}

/// Set the Cloudflare Tunnel token; an empty token turns the tunnel off.
pub async fn set_tunnel(State(state): St, _: Admin, Json(req): Json<TunnelToken>) -> Result<Json<tunnel::TunnelStatus>> {
    if !state.tunnel.status().available {
        return Err(StatusCode::NOT_IMPLEMENTED.into());
    }
    state.tunnel.set_token(&state, &req.token).await?;
    Ok(Json(state.tunnel.status()))
}

#[derive(Deserialize)]
pub struct PublicUrl {
    url: String,
}

/// The address users are told to configure; empty clears it.
pub async fn set_public_url(State(state): St, _: Admin, Json(req): Json<PublicUrl>) -> Result<StatusCode> {
    let url = req.url.trim().trim_end_matches('/');
    let valid = url.is_empty() || reqwest::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "https" | "http") && u.host_str().is_some() && u.path() == "/");
    if !valid {
        return Err(StatusCode::UNPROCESSABLE_ENTITY.into());
    }
    db::settings::set(&state.db, db::settings::PUBLIC_URL, url).await?;
    Ok(StatusCode::NO_CONTENT)
}
