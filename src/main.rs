use axum::{
    Router,
    extract::{Json, Path, State},
    http::{HeaderMap, StatusCode},
    response::Html,
    routing::{delete, get, post, put},
};
use serde::Deserialize;
use sqlx::SqlitePool;
use std::sync::Arc;
use tower_http::cors::CorsLayer;

mod auth;
mod db;
mod relay;

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub jwt_secret: String,
}

#[tokio::main]
async fn main() {
    let db_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:relay.db?mode=rwc".into());
    let jwt_secret = std::env::var("JWT_SECRET").unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:3000".into());

    let pool = SqlitePool::connect(&db_url).await.expect("Failed to connect to database");
    db::migrate(&pool).await;

    let state = Arc::new(AppState { db: pool, jwt_secret });

    let app = Router::new()
        .route("/", get(page_index))
        .route("/admin", get(page_admin))
        .route("/auth/register", post(auth::register))
        .route("/auth/login", post(auth::login))
        .route("/user/api-key", get(auth::get_api_key))
        .route("/user/api-key/refresh", post(auth::refresh_api_key))
        .route("/models", get(relay::list_models))
        .route("/v1/models", get(relay::list_models))
        .route("/v1/chat/completions", post(relay::chat_completions))
        .route("/admin/users", get(admin_list_users).post(admin_create_user))
        .route("/admin/users/{id}", delete(admin_delete_user))
        .route("/admin/providers", get(admin_list_providers).post(admin_create_provider))
        .route("/admin/providers/{id}", put(admin_update_provider).delete(admin_delete_provider))
        .route("/admin/models", get(admin_list_models).post(admin_create_model))
        .route("/admin/models/{id}", put(admin_update_model).delete(admin_delete_model))
        .layer(CorsLayer::permissive())
        .with_state(state);

    println!("API Relay listening on {bind_addr}");
    let listener = tokio::net::TcpListener::bind(&bind_addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
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
struct CreateUserReq { username: String, password: String, is_admin: Option<bool> }

async fn admin_create_user(State(state): State<Arc<AppState>>, headers: HeaderMap, Json(req): Json<CreateUserReq>) -> Result<Json<db::User>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    auth::create_user_internal(&state.db, &req.username, &req.password, req.is_admin.unwrap_or(false))
        .await.map(Json).map_err(|_| StatusCode::CONFLICT)
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
struct ProviderReq { name: String, base_url: String, user_agent: String, api_key: Option<String>, enabled: Option<bool> }

async fn admin_create_provider(State(state): State<Arc<AppState>>, headers: HeaderMap, Json(req): Json<ProviderReq>) -> Result<Json<db::Provider>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    db::create_provider(&state.db, &req.name, &req.base_url, &req.user_agent, req.api_key.as_deref(), req.enabled.unwrap_or(true))
        .await.map(Json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn admin_update_provider(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<i64>, Json(req): Json<ProviderReq>) -> Result<Json<db::Provider>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    db::update_provider(&state.db, id, &req.name, &req.base_url, &req.user_agent, req.api_key.as_deref(), req.enabled.unwrap_or(true))
        .await.map(Json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn admin_delete_provider(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<i64>) -> Result<StatusCode, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    db::delete_provider(&state.db, id).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn admin_list_models(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Json<Vec<db::Model>>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    db::list_models(&state.db).await.map(Json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Deserialize)]
struct ModelReq { public_name: String, provider_id: i64, upstream_model: String, upstream_path: Option<String> }

async fn admin_create_model(State(state): State<Arc<AppState>>, headers: HeaderMap, Json(req): Json<ModelReq>) -> Result<Json<db::Model>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    db::create_model(&state.db, &req.public_name, req.provider_id, &req.upstream_model, req.upstream_path.as_deref().unwrap_or("/v1/chat/completions"))
        .await.map(Json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn admin_update_model(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<i64>, Json(req): Json<ModelReq>) -> Result<Json<db::Model>, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    db::update_model(&state.db, id, &req.public_name, req.provider_id, &req.upstream_model, req.upstream_path.as_deref().unwrap_or("/v1/chat/completions"))
        .await.map(Json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn admin_delete_model(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<i64>) -> Result<StatusCode, StatusCode> {
    auth::require_admin(&state, &headers).await?;
    db::delete_model(&state.db, id).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}
