//! What a signed-in user does on the home page — and, by API key, under /v1/user.

use axum::{Json, extract::{Query, State}, http::StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::{
    AppState, auth,
    db::{self, usage::{Filter, Group, Period, Step}},
    error::{Error, Result},
    extract::{Caller, Origin},
    quota,
};

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
    /// The current day, week, month or year, when no `from`/`to` is given
    #[serde(default)]
    period: Period,
    /// The viewer's UTC offset in minutes, so "today" and each bucket are theirs
    #[serde(default)]
    tz: i64,
    /// `[from, to)`, seconds since the epoch
    from: Option<i64>,
    to: Option<i64>,
    /// Picked from the range's length when not given
    step: Option<Step>,
    #[serde(default)]
    group: Group,
    /// One model (`kimi/m1`) or provider (`kimi/*`)
    model: Option<String>,
    /// One user's (admin only)
    user: Option<i64>,
    /// A second range to total, for "versus last week"
    compare_from: Option<i64>,
    compare_to: Option<i64>,
}

#[derive(Serialize)]
pub struct UserRef {
    id: i64,
    username: String,
}

#[derive(Serialize)]
pub struct Report {
    from: i64,
    to: i64,
    step: Step,
    series: Vec<db::usage::Bucket>,
    totals: db::usage::Sums,
    /// Totals over the compared range, when one was asked for
    previous: Option<db::usage::Sums>,
    /// The models and users with usage in the range, for filter choices
    present_models: Vec<String>,
    present_users: Vec<UserRef>,
    #[serde(skip)]
    pub rows: Vec<db::usage::Row>,
}

/// Usage over the asked range, for one user or (`None`) everyone.
pub async fn report(state: &AppState, q: &UsageQuery, user_id: Option<i64>) -> Result<Report> {
    let bad = || Error::from(StatusCode::BAD_REQUEST);
    let (from, to) = match (q.from, q.to) {
        (Some(from), Some(to)) => (from, to),
        (None, None) => db::usage::period_bounds(&state.db, q.period, q.tz).await?,
        _ => return Err(bad()),
    };
    let step = q.step.unwrap_or(Step::for_span(to - from));
    if from >= to || to - from > step.max_span() {
        return Err(bad());
    }
    let filter = Filter { from, to, user_id, model: q.model.as_deref().filter(|m| !m.is_empty()) };
    let previous = match (q.compare_from, q.compare_to) {
        (Some(from), Some(to)) if from < to => Some(db::usage::sums(&state.db, Filter { from, to, ..filter }).await?),
        (None, None) => None,
        _ => return Err(bad()),
    };
    let (present_models, users) = db::usage::present(&state.db, filter).await?;
    Ok(Report {
        from,
        to,
        step,
        series: db::usage::series(&state.db, filter, step, q.tz, q.group).await?,
        totals: db::usage::sums(&state.db, filter).await?,
        previous,
        present_models,
        present_users: users.into_iter().map(|(id, username)| UserRef { id, username }).collect(),
        rows: db::usage::rows(&state.db, filter).await?,
    })
}

#[derive(Serialize)]
pub struct ModelUsage {
    model: String,
    #[serde(flatten)]
    sums: db::usage::Sums,
}

#[derive(Serialize)]
pub struct Usage {
    #[serde(flatten)]
    report: Report,
    /// Per model, biggest first
    models: Vec<ModelUsage>,
}

/// The caller's own usage; split by user and filtered to a user are an admin's views.
pub async fn usage(State(state): State<Arc<AppState>>, Caller(user): Caller, Query(q): Query<UsageQuery>) -> Result<Json<Usage>> {
    if q.group == Group::User || q.user.is_some() {
        return Err(StatusCode::BAD_REQUEST.into());
    }
    let mut report = report(&state, &q, Some(user.id)).await?;
    report.present_users.clear();
    let models = std::mem::take(&mut report.rows).into_iter().map(|r| ModelUsage { model: r.model, sums: r.sums }).collect();
    Ok(Json(Usage { report, models }))
}

impl UsageQuery {
    pub fn user(&self) -> Option<i64> {
        self.user
    }
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

#[derive(Serialize, Deserialize)]
pub struct Recording {
    /// Whether requests and responses are recorded in full; when off, only what a call was
    /// (time, model, tokens, timing, status) is
    content: bool,
}

pub async fn recording(Caller(user): Caller) -> Json<Recording> {
    Json(Recording { content: user.record_content })
}

pub async fn set_recording(State(state): State<Arc<AppState>>, Caller(user): Caller, Json(req): Json<Recording>) -> Result<Json<Recording>> {
    db::users::set_record_content(&state.db, user.id, req.content).await?;
    Ok(Json(req))
}

/// What the home page should tell users to configure: the public address when there is one, and a backup.
pub async fn config(State(state): State<Arc<AppState>>, _: Caller) -> Result<Json<Value>> {
    let public_url = db::settings::get_nonempty(&state.db, db::settings::PUBLIC_URL).await?;
    // The frp address, once it answers with a trusted certificate, is the way in when the public one is down
    let frp = state.frp.status();
    let backup_url = (frp.state == "connected" && frp.cert == "valid").then_some(frp.url).flatten();
    Ok(Json(json!({ "public_url": public_url, "backup_url": backup_url })))
}
