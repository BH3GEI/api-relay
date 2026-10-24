use axum::{
    Router,
    extract::{DefaultBodyLimit, Json, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{Html, Response},
    routing::{delete, get, post, put},
};
use serde::Deserialize;
use sqlx::SqlitePool;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

mod auth;
mod catalog;
mod db;
mod guard;
mod pool;
mod quota;
mod relay;
mod tunnel;
mod usage;

pub struct AppState {
    pub db: SqlitePool,
    pub jwt_secret: String,
    pub http_client: reqwest::Client,
    pub keys: pool::KeyPool,
    pub catalog: catalog::Catalog,
    pub quota: quota::QuotaCache,
    pub guard: guard::AuthGuard,
    /// Serializes limit checks with recording the request, so parallel requests can't overshoot a limit
    pub admission: tokio::sync::Mutex<()>,
    /// Running as a LazyCat app: trust the identity headers its gateway sets
    pub lazycat: bool,
    pub tunnel: tunnel::Tunnel,
}

#[tokio::main]
async fn main() {
    let db_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:relay.db?mode=rwc".into());
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".into());
    // Optional second listener for internet traffic (a Cloudflare Tunnel): API routes only
    let public_bind_addr = std::env::var("PUBLIC_BIND_ADDR").ok().filter(|s| !s.is_empty());

    let pool = SqlitePool::connect(&db_url).await.expect("Failed to connect to database");
    db::migrate(&pool).await;
    let jwt_secret = match std::env::var("JWT_SECRET") {
        Ok(s) => s,
        Err(_) => db::jwt_secret(&pool).await.expect("Failed to load JWT secret"),
    };

    let http_client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(86400))
        .pool_max_idle_per_host(20)
        // A redirect could carry x-api-key to another host
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let lazycat = std::env::var("LAZYCAT").is_ok_and(|v| v == "1");

    let state = Arc::new(AppState {
        db: pool,
        jwt_secret,
        http_client,
        keys: pool::KeyPool::default(),
        catalog: catalog::Catalog::default(),
        quota: quota::QuotaCache::default(),
        guard: guard::AuthGuard::default(),
        admission: tokio::sync::Mutex::new(()),
        lazycat,
        tunnel: tunnel::Tunnel::new(std::env::var("CLOUDFLARED_PATH").ok(), public_bind_addr.as_deref()),
    });
    catalog::spawn_refresher(state.clone());
    tunnel::spawn(state.clone());

    let app = Router::new()
        .route("/admin", get(page_admin))
        .route("/healthz", get(|| async { "ok" }))
        .route("/auth/register", post(auth::register))
        .route("/auth/sso", get(auth::sso))
        .route("/admin/usage", get(admin_usage))
        .route("/admin/limits", get(admin_list_limits).post(admin_create_limit))
        .route("/admin/limits/:id", delete(admin_delete_limit))
        .route("/admin/users", get(admin_list_users).post(admin_create_user))
        .route("/admin/users/:id", delete(admin_delete_user))
        .route("/admin/users/:id/password", put(admin_set_password))
        .route("/admin/providers", get(admin_list_providers).post(admin_create_provider))
        .route("/admin/providers/:id", put(admin_update_provider).delete(admin_delete_provider))
        .route("/admin/keys/:provider_id", get(admin_list_provider_keys).post(admin_create_provider_key))
        .route("/admin/keys/delete/:id", delete(admin_delete_provider_key))
        .route("/admin/catalog", get(admin_catalog))
        .route("/admin/catalog/refresh", post(admin_refresh_catalog))
        .route("/admin/tunnel", get(admin_tunnel_status).put(admin_set_tunnel))
        .route("/admin/public-url", put(admin_set_public_url))
        .merge(user_routes())
        // Nothing outside the API needs a big body; unauthenticated callers can reach /auth
        .layer(DefaultBodyLimit::max(64 * 1024))
        .merge(api_routes(&state))
        .layer(middleware::map_response(security_headers))
        .with_state(state.clone());

    if let Some(addr) = public_bind_addr {
        // Users sign in here with a password; LazyCat sign-in and the admin stay on the LAN side
        let public = user_routes()
            .layer(DefaultBodyLimit::max(64 * 1024))
            .merge(api_routes(&state))
            .layer(axum::Extension(guard::PublicRequest))
            .layer(middleware::map_response(security_headers))
            .with_state(state.clone());
        println!("Public API listening on {addr}");
        let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
        tokio::spawn(async move { axum::serve(listener, public).await.unwrap() });
    }

    println!("API Relay listening on {bind_addr}");
    let listener = tokio::net::TcpListener::bind(&bind_addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

/// The home page and what it needs once signed in. Served on both listeners.
fn user_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(page_index))
        .route("/auth/login", post(auth::login))
        .route("/auth/options", get(auth::options))
        .route("/user/api-key", get(auth::get_api_key))
        .route("/user/api-key/refresh", post(auth::refresh_api_key))
        .route("/user/password", put(auth::change_password))
        .route("/user/usage", get(user_usage))
        .route("/user/quota", get(user_quota))
        .route("/user/config", get(user_config))
}

/// The API: the caller's key is checked before the request body is read.
/// Served on the main listener and, alone, on the public one.
fn api_routes(state: &Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/models", get(relay::list_models))
        .route("/v1/models", get(relay::list_models))
        .route("/v1/models/*id", get(relay::get_model))
        .route("/v1/quota", get(api_quota))
        .route("/v1/chat/completions", post(relay::chat_completions))
        .route("/v1/responses", post(relay::responses))
        .route("/v1/messages", post(relay::anthropic_messages))
        .route("/v1/messages/count_tokens", post(relay::count_tokens))
        .route("/v1/images/generations", post(relay::image_generations))
        .route("/v1/videos/generations", post(relay::video_generations))
        .route("/v1/files", post(relay::files_upload).get(relay::files_list))
        .route("/v1/files/:file_id", get(relay::files_action).delete(relay::files_delete))
        .route("/v1/files/:file_id/content", get(relay::files_content))
        .route_layer(middleware::from_fn_with_state(state.clone(), relay::require_api_key))
        .layer(DefaultBodyLimit::max(100 * 1024 * 1024))
        // Browser-based API clients need CORS; the pages and admin API don't
        .layer(CorsLayer::permissive())
}

async fn security_headers(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    resp
}

async fn page_index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn page_admin() -> Html<&'static str> {
    Html(include_str!("../static/admin.html"))
}

// ── Admin handlers ──

async fn admin_list_users(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Json<Vec<db::User>>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    db::list_users(&state.db).await.map(Json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Deserialize)]
struct CreateUserReq { username: String, email: Option<String>, password: Option<String>, is_admin: Option<bool> }

async fn admin_create_user(State(state): State<Arc<AppState>>, headers: HeaderMap, Json(req): Json<CreateUserReq>) -> Result<Json<db::User>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    auth::create_user_internal(&state.db, &req.username, req.email.as_deref().unwrap_or(""), req.password.as_deref().unwrap_or(""), req.is_admin.unwrap_or(false))
        .await.map(Json).map_err(|_| StatusCode::CONFLICT)
}

#[derive(Deserialize)]
struct SetPasswordReq { password: String }

async fn admin_set_password(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<i64>, Json(req): Json<SetPasswordReq>) -> Result<StatusCode, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    auth::set_password(&state.db, id, req.password).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn admin_delete_user(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<i64>) -> Result<StatusCode, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    db::delete_user(&state.db, id).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn admin_list_providers(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Json<Vec<db::Provider>>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    db::list_providers(&state.db).await.map(Json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Deserialize)]
struct ProviderReq { name: String, base_url: String, anthropic_base_url: Option<String>, user_agent: String, api_key: Option<String>, models: Option<String>, enabled: Option<bool> }

impl ProviderReq {
    fn input(&self) -> db::ProviderInput<'_> {
        db::ProviderInput {
            name: &self.name,
            base_url: &self.base_url,
            anthropic_base_url: self.anthropic_base_url.as_deref().unwrap_or(""),
            user_agent: &self.user_agent,
            api_key: self.api_key.as_deref(),
            models: self.models.as_deref().unwrap_or(""),
            enabled: self.enabled.unwrap_or(true),
        }
    }
}

async fn admin_create_provider(State(state): State<Arc<AppState>>, headers: HeaderMap, Json(req): Json<ProviderReq>) -> Result<Json<db::Provider>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    let provider = db::create_provider(&state.db, &req.input()).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    catalog::refresh_in_background(&state);
    Ok(Json(provider))
}

async fn admin_update_provider(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<i64>, Json(req): Json<ProviderReq>) -> Result<Json<db::Provider>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    let provider = db::update_provider(&state.db, id, &req.input()).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    catalog::refresh_in_background(&state);
    Ok(Json(provider))
}

async fn admin_delete_provider(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<i64>) -> Result<StatusCode, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    db::delete_provider(&state.db, id).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    catalog::refresh_in_background(&state);
    Ok(StatusCode::NO_CONTENT)
}

async fn admin_catalog(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Json<Vec<String>>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    Ok(Json(state.catalog.entries().into_iter().map(|e| e.id).collect()))
}

async fn admin_refresh_catalog(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Json<Vec<String>>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    state.catalog.refresh(&state).await;
    Ok(Json(state.catalog.entries().into_iter().map(|e| e.id).collect()))
}

// ── Provider Keys ──

#[derive(serde::Serialize)]
struct KeyWithStatus {
    #[serde(flatten)]
    key: db::ProviderKey,
    status: pool::KeyStatus,
    /// Plan usage windows, when the upstream reports them
    quota: Option<Vec<quota::Window>>,
}

async fn admin_list_provider_keys(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(provider_id): Path<i64>) -> Result<Json<Vec<KeyWithStatus>>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    let provider = db::get_provider(&state.db, provider_id).await.map_err(|_| StatusCode::NOT_FOUND)?;
    let keys = db::list_provider_keys(&state.db, provider_id).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let quotas = futures::future::join_all(keys.iter().map(|k| state.quota.get(&state, &provider, &k.api_key))).await;
    Ok(Json(keys.into_iter().zip(quotas).map(|(key, quota)| {
        let status = state.keys.status(&key.api_key);
        KeyWithStatus { key, status, quota }
    }).collect()))
}

#[derive(Deserialize)]
struct ProviderKeyReq { api_key: String, label: Option<String>, priority: Option<i64> }

async fn admin_create_provider_key(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(provider_id): Path<i64>, Json(req): Json<ProviderKeyReq>) -> Result<Json<db::ProviderKey>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    let key = db::create_provider_key(&state.db, provider_id, &req.api_key, req.label.as_deref().unwrap_or(""), req.priority.unwrap_or(0))
        .await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    catalog::refresh_in_background(&state);
    Ok(Json(key))
}

async fn admin_delete_provider_key(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<i64>) -> Result<StatusCode, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    db::delete_provider_key(&state.db, id).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Usage & Limits ──

async fn user_usage(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Json<Vec<db::UsageStats>>, StatusCode> {
    let user = auth::current_user(&state, &headers).await?;
    db::get_user_usage_today(&state.db, user.id).await.map(Json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Plan quota left in the shared pool, for the signed-in home page
async fn user_quota(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Json<serde_json::Value>, StatusCode> {
    auth::current_user(&state, &headers).await?;
    Ok(Json(serde_json::json!({ "providers": quota::pool(&state).await })))
}

/// The same for API clients (status lines, agents); the key was checked by the API middleware
async fn api_quota(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "providers": quota::pool(&state).await }))
}

async fn admin_usage(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Json<Vec<db::AdminUsageStats>>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    db::get_all_usage_today(&state.db).await.map(Json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(serde::Serialize)]
struct LimitWithUsage {
    #[serde(flatten)]
    limit: db::UsageLimit,
    /// Usage so far in the limit's current window
    used: db::WindowUsage,
}

async fn admin_list_limits(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Json<Vec<LimitWithUsage>>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    let limits = db::list_limits(&state.db).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut out = Vec::with_capacity(limits.len());
    for limit in limits {
        let used = db::window_usage(&state.db, limit.user_id, &limit.model, limit.window_secs).await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        out.push(LimitWithUsage { limit, used });
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
struct LimitReq { user_id: i64, model: Option<String>, window_secs: Option<i64>, max_requests: Option<i64>, max_tokens: Option<i64> }

async fn admin_create_limit(State(state): State<Arc<AppState>>, headers: HeaderMap, Json(req): Json<LimitReq>) -> Result<Json<db::UsageLimit>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    let model = req.model.as_deref().unwrap_or("*");
    // A limit on a name that isn't a catalog id would silently never match
    if model != "*" && !state.catalog.entries().iter().any(|e| e.id == model) {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let window = req.window_secs.unwrap_or(86400);
    if window <= 0 {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    db::create_limit(&state.db, req.user_id, model, window, req.max_requests.unwrap_or(-1), req.max_tokens.unwrap_or(-1))
        .await.map(Json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn admin_delete_limit(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<i64>) -> Result<StatusCode, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    db::delete_limit(&state.db, id).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

// ── Tunnel ──

async fn admin_tunnel_status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Json<tunnel::TunnelStatus>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    Ok(Json(state.tunnel.status()))
}

#[derive(Deserialize)]
struct TunnelReq { token: String }

/// Set the Cloudflare Tunnel token; an empty token turns the tunnel off.
async fn admin_set_tunnel(State(state): State<Arc<AppState>>, headers: HeaderMap, Json(req): Json<TunnelReq>) -> Result<Json<tunnel::TunnelStatus>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    if !state.tunnel.status().available {
        return Err(StatusCode::NOT_IMPLEMENTED);
    }
    state.tunnel.set_token(&state, &req.token).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(state.tunnel.status()))
}

// ── Public URL ──

const PUBLIC_URL_SETTING: &str = "public_url";

/// What the home page should tell users to configure: the public address when there is one.
async fn user_config(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Json<serde_json::Value>, StatusCode> {
    auth::current_user(&state, &headers).await?;
    let public_url = db::get_setting(&state.db, PUBLIC_URL_SETTING).await.ok().flatten().filter(|u| !u.is_empty());
    Ok(Json(serde_json::json!({ "public_url": public_url })))
}

#[derive(Deserialize)]
struct PublicUrlReq { url: String }

async fn admin_set_public_url(State(state): State<Arc<AppState>>, headers: HeaderMap, Json(req): Json<PublicUrlReq>) -> Result<StatusCode, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    let url = req.url.trim().trim_end_matches('/');
    let valid = url.is_empty() || reqwest::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "https" | "http") && u.host_str().is_some() && u.path() == "/");
    if !valid {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    db::set_setting(&state.db, PUBLIC_URL_SETTING, url).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}
