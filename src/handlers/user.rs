//! What a signed-in user does on the home page — and, by API key, under /v1/user.

use axum::{Json, extract::{Query, State}, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::{AppState, auth, db, error::Result, extract::{Caller, Origin}, quota};

pub async fn api_key(Caller(user): Caller) -> Json<Value> {
    Json(json!({ "api_key": user.api_key }))
}

/// A new key; the old one stops working at once.
pub async fn rotate_api_key(State(state): State<Arc<AppState>>, Caller(user): Caller) -> Result<Json<Value>> {
    let key = auth::new_api_key(&user.username);
    db::users::set_api_key(&state.db, user.id, &key).await?;
    Ok(Json(json!({ "api_key": key })))
}

#[derive(Deserialize)]
pub struct PasswordChange {
    current_password: String,
    new_password: String,
}

/// Needs the current password, so a stolen session or key alone can't take the account.
pub async fn change_password(State(state): State<Arc<AppState>>, origin: Origin, Caller(user): Caller, Json(req): Json<PasswordChange>) -> Result<StatusCode> {
    origin.check(&state)?;
    if !auth::verify_password(req.current_password, Some(user.password_hash)).await {
        origin.failed(&state);
        return Err(StatusCode::FORBIDDEN.into());
    }
    auth::set_password(&state.db, user.id, req.new_password).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct UsageQuery {
    #[serde(default)]
    period: db::usage::Period,
    /// The viewer's UTC offset in minutes, so "today" is their today
    #[serde(default)]
    tz: i64,
}

impl UsageQuery {
    pub fn range(&self) -> db::usage::Range {
        db::usage::Range { period: self.period, tz_minutes: self.tz }
    }
}

#[derive(Serialize)]
pub struct Usage {
    series: Vec<db::usage::Bucket>,
    models: Vec<db::usage::ModelTotals>,
}

pub async fn usage(State(state): State<Arc<AppState>>, Caller(user): Caller, Query(q): Query<UsageQuery>) -> Result<Json<Usage>> {
    Ok(Json(Usage {
        series: db::usage::series(&state.db, Some(user.id), q.range()).await?,
        models: db::usage::by_model(&state.db, user.id, q.range()).await?,
    }))
}

#[derive(Serialize)]
pub struct LimitUse {
    #[serde(flatten)]
    pub limit: db::limits::Limit,
    /// Usage so far in the limit's current window
    pub used: db::usage::Totals,
}

/// Each limit with what has been used against it.
pub async fn with_usage(state: &AppState, limits: Vec<db::limits::Limit>) -> Result<Vec<LimitUse>> {
    let mut out = Vec::with_capacity(limits.len());
    for limit in limits {
        let used = db::usage::in_window(&state.db, limit.user_id, &limit.model, limit.window_secs).await?;
        out.push(LimitUse { limit, used });
    }
    Ok(out)
}

#[derive(Serialize)]
pub struct Quota {
    /// The user's own limits; none means unlimited
    limits: Vec<LimitUse>,
    /// What's left of the shared upstream plans
    providers: Vec<quota::PoolQuota>,
}

pub async fn quota(State(state): State<Arc<AppState>>, Caller(user): Caller) -> Result<Json<Quota>> {
    let limits = with_usage(&state, db::limits::for_user(&state.db, user.id).await?).await?;
    Ok(Json(Quota { limits, providers: quota::pool(&state).await }))
}

/// What the home page should tell users to configure: the public address when there is one.
pub async fn config(State(state): State<Arc<AppState>>, _: Caller) -> Result<Json<Value>> {
    let public_url = db::settings::get_nonempty(&state.db, db::settings::PUBLIC_URL).await?;
    Ok(Json(json!({ "public_url": public_url })))
}
