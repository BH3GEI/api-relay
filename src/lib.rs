//! An API relay: one endpoint and one key per user in front of a pool of upstream LLM keys.
//!
//! Two listeners. The main one sits behind LazyCat's gateway, which signs users in; it serves
//! the pages, the admin API and the relayed API. The public one is reached through a Cloudflare
//! Tunnel around that gateway, so it serves only what is safe without it: the relayed API, and
//! the home page with password sign-in. frp, when set up, is a second way in to the same routes
//! (see `frp`).

mod acme;
mod auth;
mod catalog;
pub mod config;
mod db;
mod error;
mod extract;
mod frp;
mod guard;
mod handlers;
mod pages;
mod pool;
mod proxy;
mod quota;
mod tokens;
mod tunnel;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    http::{HeaderValue, header},
    middleware,
    response::Response,
    routing::{delete, get, post, put},
};
use std::sync::Arc;
use tower_http::cors::CorsLayer;

use handlers::{admin, sign_in, user};

pub struct AppState {
    pub db: sqlx::SqlitePool,
    jwt_secret: String,
    http_client: reqwest::Client,
    keys: pool::KeyPool,
    catalog: catalog::Catalog,
    quota: quota::QuotaCache,
    guard: guard::AuthGuard,
    /// Serializes limit checks with recording the request, so parallel requests can't overshoot a limit
    admission: tokio::sync::Mutex<()>,
    /// Running as a LazyCat app: trust the identity headers its gateway sets
    lazycat: bool,
    tunnel: tunnel::Tunnel,
    frp: frp::Frp,
}

/// The relay, ready to serve: the main router and, when configured, the public one.
pub struct App {
    pub state: Arc<AppState>,
    pub lan: Router,
    pub public: Option<Router>,
}

/// Nothing outside the relayed API needs a big body; unauthenticated callers reach /auth.
const PAGE_BODY_LIMIT: usize = 64 * 1024;
/// Requests may carry images and files.
const API_BODY_LIMIT: usize = 100 * 1024 * 1024;

impl App {
    /// Open the database, start the background tasks and build the routers.
    pub async fn new(config: &config::Config) -> App {
        let db = sqlx::SqlitePool::connect(&config.database_url).await.expect("Failed to connect to database");
        db::migrate(&db).await;
        let jwt_secret = match &config.jwt_secret {
            Some(s) => s.clone(),
            None => db::settings::jwt_secret(&db).await.expect("Failed to load JWT secret"),
        };
        let http_client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            // Agent turns can stream for a long time
            .timeout(std::time::Duration::from_secs(86400))
            .pool_max_idle_per_host(20)
            // A redirect could carry a key to another host
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        let state = Arc::new(AppState {
            db,
            jwt_secret,
            http_client,
            keys: pool::KeyPool::default(),
            catalog: catalog::Catalog::default(),
            quota: quota::QuotaCache::default(),
            guard: guard::AuthGuard::default(),
            admission: tokio::sync::Mutex::new(()),
            lazycat: config.lazycat,
            tunnel: tunnel::Tunnel::new(config.cloudflared_path.clone(), config.public_bind_addr.as_deref(), config.tunnel_log.clone()),
            frp: frp::Frp::new(config.frpc_path.clone(), config.frp_bind_addr.clone()),
        });
        catalog::spawn_refresher(state.clone());
        tunnel::spawn(state.clone());

        let lan = lan_routes()
            .merge(home_routes())
            .layer(DefaultBodyLimit::max(PAGE_BODY_LIMIT))
            .merge(api_routes(&state))
            .layer(middleware::map_response(security_headers))
            .with_state(state.clone());
        let public = config.public_bind_addr.as_ref().map(|_| {
            home_routes()
                .layer(DefaultBodyLimit::max(PAGE_BODY_LIMIT))
                .merge(api_routes(&state))
                .layer(axum::Extension(extract::PublicListener))
                .layer(middleware::map_response(security_headers))
                .with_state(state.clone())
        });
        frp::spawn(state.clone(), public.clone());
        App { state, lan, public }
    }
}

type Routes = Router<Arc<AppState>>;

/// Only behind LazyCat's gateway: its sign-in, the admin page and the admin API.
fn lan_routes() -> Routes {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/admin", get(pages::admin))
        .route("/auth/sso", get(sign_in::lazycat))
        .route("/auth/register", post(sign_in::register))
        .route("/admin/users", get(admin::users).post(admin::create_user))
        .route("/admin/users/:id", delete(admin::delete_user))
        .route("/admin/users/:id/password", put(admin::set_password))
        .route("/admin/providers", get(admin::providers).post(admin::create_provider))
        .route("/admin/providers/:id", put(admin::update_provider).delete(admin::delete_provider))
        .route("/admin/keys/:provider_id", get(admin::keys).post(admin::add_key))
        .route("/admin/keys/delete/:id", delete(admin::delete_key))
        .route("/admin/catalog", get(admin::catalog))
        .route("/admin/catalog/refresh", post(admin::refresh_catalog))
        .route("/admin/usage", get(admin::usage))
        .route("/admin/limits", get(admin::limits).post(admin::set_limit))
        .route("/admin/limits/:id", delete(admin::delete_limit))
        .route("/admin/tunnel", get(admin::tunnel).put(admin::set_tunnel))
        .route("/admin/frp", get(admin::frp).put(admin::set_frp))
        .route("/admin/public-url", put(admin::set_public_url))
}

/// The home page and what it needs, on both listeners.
fn home_routes() -> Routes {
    Router::new()
        .route("/", get(pages::index))
        .route("/static/:name", get(pages::file))
        .route("/auth/login", post(sign_in::login))
        .route("/auth/options", get(sign_in::options))
        .merge(user_routes("/user"))
}

/// What a user can do for themselves, under `prefix`.
fn user_routes(prefix: &str) -> Routes {
    Router::new()
        .route(&format!("{prefix}/api-key"), get(user::api_key))
        .route(&format!("{prefix}/api-key/refresh"), post(user::rotate_api_key))
        .route(&format!("{prefix}/password"), put(user::change_password))
        .route(&format!("{prefix}/usage"), get(user::usage))
        .route(&format!("{prefix}/quota"), get(user::quota))
        .route(&format!("{prefix}/config"), get(user::config))
}

/// The API, for clients signing with their key, which is checked before the body is read.
fn api_routes(state: &Arc<AppState>) -> Routes {
    Router::new()
        .route("/models", get(proxy::models::list))
        .route("/v1/models", get(proxy::models::list))
        .route("/v1/models/*id", get(proxy::models::get))
        .route("/v1/chat/completions", post(proxy::chat_completions))
        .route("/v1/responses", post(proxy::responses))
        .route("/v1/messages", post(proxy::messages))
        .route("/v1/messages/count_tokens", post(proxy::count_tokens))
        .route("/v1/images/generations", post(proxy::image_generations))
        .route("/v1/videos/generations", post(proxy::video_generations))
        .route("/v1/files", post(proxy::files::upload).get(proxy::files::list))
        .route("/v1/files/:file_id", get(proxy::files::get).delete(proxy::files::delete))
        .route("/v1/files/:file_id/content", get(proxy::files::content))
        // Everything the home page does, by key
        .route("/v1/quota", get(user::quota))
        .merge(user_routes("/v1/user"))
        .route_layer(middleware::from_fn_with_state(state.clone(), extract::require_api_key))
        .layer(DefaultBodyLimit::max(API_BODY_LIMIT))
        // Browser-based API clients need CORS; the pages and admin API don't
        .layer(CorsLayer::permissive())
}

async fn security_headers(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    resp
}
