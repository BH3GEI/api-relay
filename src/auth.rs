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
    pub email: Option<String>,
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
    // Check the database too, so a revoked admin's unexpired token stops working
    let user = db::find_user_by_username(&state.db, &claims.sub).await.map_err(|_| StatusCode::UNAUTHORIZED)?;
    if !claims.is_admin || !user.is_admin {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(claims)
}

/// `sk-<username>-relay-<uuid>`; the name is cosmetic, the uuid carries all the secrecy.
fn new_api_key(username: &str) -> String {
    let name: String = username.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(16)
        .collect::<String>()
        .to_ascii_lowercase();
    let random = uuid::Uuid::new_v4().simple();
    if name.is_empty() {
        format!("sk-relay-{random}")
    } else {
        format!("sk-{name}-relay-{random}")
    }
}

pub async fn create_user_internal(pool: &sqlx::SqlitePool, username: &str, email: &str, password: &str, is_admin: bool) -> Result<db::User, StatusCode> {
    // No password (LazyCat sign-in users): a random one nobody knows
    let password = if password.is_empty() { uuid::Uuid::new_v4().to_string() } else { password.to_string() };
    let pw_hash = hash_password(&password)?;
    let api_key = new_api_key(username);
    db::insert_user(pool, username, email, &pw_hash, &api_key, is_admin)
        .await
        .map_err(|_| StatusCode::CONFLICT)
}

// ── Handlers ──

pub async fn register(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AuthReq>,
) -> Result<Json<AuthResp>, StatusCode> {
    // On LazyCat, accounts come from LazyCat sign-in instead
    if state.lazycat {
        return Err(StatusCode::FORBIDDEN);
    }
    let email = req.email.as_deref().unwrap_or("");
    if email.is_empty() || !email.contains('@') {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Registration only bootstraps the first (admin) user; the admin adds everyone else
    let count = db::user_count(&state.db).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if count > 0 {
        return Err(StatusCode::FORBIDDEN);
    }
    let user = create_user_internal(&state.db, &req.username, email, &req.password, true).await?;
    let token = make_jwt(&state, &user.username, user.is_admin)?;
    Ok(Json(AuthResp { token, api_key: user.api_key }))
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AuthReq>,
) -> Result<Json<AuthResp>, StatusCode> {
    if state.lazycat {
        return Err(StatusCode::FORBIDDEN);
    }
    // Try username first, then email
    let user = match db::find_user_by_username(&state.db, &req.username).await {
        Ok(u) => u,
        Err(_) => db::find_user_by_email(&state.db, &req.username).await.map_err(|_| StatusCode::UNAUTHORIZED)?,
    };
    if !verify_password(&req.password, &user.password_hash) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let token = make_jwt(&state, &user.username, user.is_admin)?;
    Ok(Json(AuthResp { token, api_key: user.api_key }))
}

/// Sign in as the LazyCat user making the request.
///
/// LazyCat's gateway authenticates every request outside `public_path` and sets `X-HC-User-ID`
/// and `X-HC-User-Role`, so they can be trusted — but only when running on LazyCat, which is
/// why this is off unless `LAZYCAT` is set. LazyCat admins get an account automatically;
/// other LazyCat users need an admin to add their username first.
pub async fn sso(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<AuthResp>, StatusCode> {
    if !state.lazycat {
        return Err(StatusCode::NOT_FOUND);
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).filter(|s| !s.is_empty());
    let uid = header("x-hc-user-id").ok_or(StatusCode::UNAUTHORIZED)?;
    let is_admin = header("x-hc-user-role") == Some("ADMIN");

    let user = match db::find_user_by_username(&state.db, uid).await {
        Ok(user) => {
            // LazyCat decides who is an admin
            if user.is_admin != is_admin {
                db::set_user_admin(&state.db, user.id, is_admin).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            }
            user
        }
        Err(_) if is_admin => create_user_internal(&state.db, uid, "", "", true).await?,
        Err(_) => return Err(StatusCode::FORBIDDEN),
    };
    let token = make_jwt(&state, &user.username, is_admin)?;
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
    let new_key = new_api_key(&user.username);
    db::update_user_api_key(&state.db, user.id, &new_key).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "api_key": new_key })))
}
