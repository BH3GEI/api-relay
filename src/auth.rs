use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::{extract::{Json, State}, http::{HeaderMap, StatusCode}};
use jsonwebtoken::{encode, decode, Header, Validation, EncodingKey, DecodingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{AppState, db};

#[derive(Serialize, Deserialize)]
pub struct Claims {
    pub sub: String, // username
    pub is_admin: bool,
    pub exp: usize,
}

#[derive(Deserialize)]
pub struct AuthReq {
    pub username: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct AuthResp {
    pub token: String,
    pub api_key: String,
}

pub fn hash_password(password: &str) -> Result<String, StatusCode> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .and_then(|h| Argon2::default().verify_password(password.as_bytes(), &h))
        .is_ok()
}

fn make_jwt(state: &AppState, username: &str, is_admin: bool) -> Result<String, StatusCode> {
    let claims = Claims {
        sub: username.to_string(),
        is_admin,
        exp: (chrono_now() + 86400 * 7) as usize, // 7 days
    };
    encode(&Header::default(), &claims, &EncodingKey::from_secret(state.jwt_secret.as_bytes()))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

fn chrono_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

pub fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    headers.get("authorization")?
        .to_str().ok()?
        .strip_prefix("Bearer ")
        .map(|s| s.to_string())
}

pub fn decode_jwt(state: &AppState, token: &str) -> Result<Claims, StatusCode> {
    decode::<Claims>(token, &DecodingKey::from_secret(state.jwt_secret.as_bytes()), &Validation::default())
        .map(|d| d.claims)
        .map_err(|_| StatusCode::UNAUTHORIZED)
}

pub async fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<Claims, StatusCode> {
    let token = extract_bearer(headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let claims = decode_jwt(state, &token)?;
    if !claims.is_admin {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(claims)
}

pub async fn create_user_internal(pool: &sqlx::SqlitePool, username: &str, password: &str, is_admin: bool) -> Result<db::User, StatusCode> {
    let pw_hash = hash_password(password)?;
    let api_key = format!("sk-relay-{}", uuid::Uuid::new_v4());
    db::insert_user(pool, username, &pw_hash, &api_key, is_admin)
        .await
        .map_err(|_| StatusCode::CONFLICT)
}

// ── Handlers ──

pub async fn register(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AuthReq>,
) -> Result<Json<AuthResp>, StatusCode> {
    // First user is auto-admin
    let is_admin = db::user_count(&state.db).await.unwrap_or(0) == 0;
    let user = create_user_internal(&state.db, &req.username, &req.password, is_admin).await?;
    let token = make_jwt(&state, &user.username, user.is_admin)?;
    Ok(Json(AuthResp { token, api_key: user.api_key }))
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AuthReq>,
) -> Result<Json<AuthResp>, StatusCode> {
    let user = db::find_user_by_username(&state.db, &req.username).await.map_err(|_| StatusCode::UNAUTHORIZED)?;
    if !verify_password(&req.password, &user.password_hash) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let token = make_jwt(&state, &user.username, user.is_admin)?;
    Ok(Json(AuthResp { token, api_key: user.api_key }))
}

pub async fn get_api_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let token = extract_bearer(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let claims = decode_jwt(&state, &token)?;
    let user = db::find_user_by_username(&state.db, &claims.sub).await.map_err(|_| StatusCode::NOT_FOUND)?;
    Ok(Json(serde_json::json!({ "api_key": user.api_key })))
}

pub async fn refresh_api_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let token = extract_bearer(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let claims = decode_jwt(&state, &token)?;
    let user = db::find_user_by_username(&state.db, &claims.sub).await.map_err(|_| StatusCode::NOT_FOUND)?;
    let new_key = format!("sk-relay-{}", uuid::Uuid::new_v4());
    db::update_user_api_key(&state.db, user.id, &new_key).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "api_key": new_key })))
}
