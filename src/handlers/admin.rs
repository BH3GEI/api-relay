//! The admin API, for the admin page on the LAN side. Every handler takes an `Admin`.

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::Response,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{
    AppState, auth, capture, catalog,
    db::{self, limits, providers::{Provider, ProviderInput, ProviderKey}, users::User},
    error::{OrNotFound, Result},
    extract::Admin,
    handlers::user::{self, LimitUse, UsageQuery, with_usage},
    frp, pool, quota, tunnel,
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

#[derive(Deserialize)]
pub struct Priority {
    priority: i64,
}

pub async fn set_key_priority(State(state): St, _: Admin, Path(id): Path<i64>, Json(req): Json<Priority>) -> Result<Json<ProviderKey>> {
    Ok(Json(db::providers::set_priority(&state.db, id, req.priority).await?.or_not_found()?))
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
    #[serde(flatten)]
    report: user::Report,
    /// Per user and model, biggest first
    rows: Vec<db::usage::Row>,
}

/// Everyone's usage, or one user's.
pub async fn usage(State(state): St, _: Admin, Query(q): Query<UsageQuery>) -> Result<Json<Usage>> {
    let mut report = user::report(&state, &q, q.user()).await?;
    let rows = std::mem::take(&mut report.rows);
    Ok(Json(Usage { report, rows }))
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
    // A limit on a name that isn't a catalog id or a provider would silently never match
    let known = match model.strip_suffix("/*") {
        Some(provider) => db::providers::list(&state.db).await?.iter().any(|p| p.name == provider),
        None => model == limits::ALL_MODELS || state.catalog.has(model),
    };
    if !known {
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

pub async fn frp(State(state): St, _: Admin) -> Json<frp::FrpStatus> {
    Json(state.frp.status())
}

/// Set up frp; secrets left empty keep the saved ones, and an empty server turns it off.
pub async fn set_frp(State(state): St, _: Admin, Json(req): Json<frp::FrpSettings>) -> Result<Json<frp::FrpStatus>> {
    if !state.frp.status().available {
        return Err(StatusCode::NOT_IMPLEMENTED.into());
    }
    let valid = req.server.is_empty() || req.server.trim().rsplit_once(':').is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok());
    if !valid {
        return Err(StatusCode::UNPROCESSABLE_ENTITY.into());
    }
    state.frp.set(&state, req).await?;
    Ok(Json(state.frp.status()))
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

// ── Recorded calls ──

#[derive(Deserialize)]
pub struct RequestQuery {
    user: Option<i64>,
    model: Option<String>,
    /// "ok" or "error"
    status: Option<String>,
    /// `[from, to)`, unix milliseconds
    from: Option<i64>,
    to: Option<i64>,
    /// The page before this record (newest first)
    before: Option<i64>,
    limit: Option<i64>,
}

impl RequestQuery {
    fn filter(&self) -> capture::Filter {
        capture::Filter {
            user_id: self.user,
            model: self.model.clone().filter(|m| !m.is_empty()),
            status: self.status.clone().filter(|s| !s.is_empty()),
            from: self.from,
            to: self.to,
        }
    }
}

pub async fn requests(State(state): St, _: Admin, Query(q): Query<RequestQuery>) -> Result<Json<Vec<capture::Summary>>> {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    Ok(Json(capture::list(&state.capture, &q.filter(), q.before, limit).await?))
}

pub async fn request(State(state): St, _: Admin, Path(id): Path<i64>) -> Result<Json<capture::Detail>> {
    Ok(Json(capture::detail(&state.capture, id).await?.or_not_found()?))
}

pub async fn request_stats(State(state): St, _: Admin) -> Result<Json<capture::Stats>> {
    Ok(Json(capture::stats(&state.capture).await?))
}

/// Every matching call in full, oldest first, one JSON object per line — streamed, so an
/// export can be as big as the record.
pub async fn export_requests(State(state): St, _: Admin, Query(q): Query<RequestQuery>) -> Response {
    let filter = q.filter();
    let lines = futures::stream::unfold(Some(0i64), move |after| {
        let state = state.clone();
        let filter = filter.clone();
        async move {
            let after = after?;
            match capture::details_after(&state.capture, &filter, after, 50).await {
                Ok(page) if page.is_empty() => None,
                Ok(page) => {
                    let next = page.last().map(|d| d.summary.id);
                    let mut out = Vec::new();
                    for d in &page {
                        serde_json::to_writer(&mut out, d).ok();
                        out.push(b'\n');
                    }
                    Some((Ok::<_, std::io::Error>(out), next))
                }
                Err(e) => Some((Err(std::io::Error::other(e)), None)),
            }
        }
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"requests-{}.jsonl\"", tunnel::now_utc().replace(['-', ':'], "")))
        .body(Body::from_stream(lines))
        .unwrap()
}
