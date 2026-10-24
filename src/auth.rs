use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::{Extension, extract::{Json, State}, http::{HeaderMap, StatusCode}};
use jsonwebtoken::{encode, decode, Header, Validation, EncodingKey, DecodingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{AppState, db, guard::PublicRequest};

#[derive(Serialize, Deserialize)]
pub struct Claims {
    pub sub: String, // username
    /// User id, so a token stops working if its user is deleted and the name reused
    #[serde(default)]
    pub uid: i64,
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

fn make_jwt(state: &AppState, user: &db::User, is_admin: bool) -> Result<String, StatusCode> {
    let claims = Claims {
        sub: user.username.clone(),
        uid: user.id,
        is_admin,
        exp: (chrono_now() + 86400 * 7) as usize, // 7 days
    };
    encode(&Header::default(), &claims, &EncodingKey::from_secret(state.jwt_secret.as_bytes()))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// A hash of a random password, checked against when the user doesn't exist.
fn dummy_hash() -> &'static str {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| hash_password(&uuid::Uuid::new_v4().to_string()).unwrap_or_default())
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

/// The user behind a request: API clients send their key (`/v1/user/…`), the pages a JWT.
pub async fn current_user(state: &AppState, headers: &HeaderMap) -> Result<db::User, StatusCode> {
    match crate::relay::extract_api_key(headers) {
        Some(key) if key.starts_with("sk-") => db::find_user_by_api_key(&state.db, &key).await.map_err(|_| StatusCode::UNAUTHORIZED),
        _ => signed_in_user(state, headers).await,
    }
}

/// The signed-in user behind a request's JWT.
async fn signed_in_user(state: &AppState, headers: &HeaderMap) -> Result<db::User, StatusCode> {
    let token = extract_bearer(headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let claims = decode_jwt(state, &token)?;
    let user = db::find_user_by_username(&state.db, &claims.sub).await.map_err(|_| StatusCode::UNAUTHORIZED)?;
    if user.id != claims.uid {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(user)
}

pub async fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<db::User, StatusCode> {
    // The database decides, so a revoked admin's unexpired token stops working.
    // Only a signed-in session: an API key alone never reaches the admin API.
    let user = signed_in_user(state, headers).await?;
    if !user.is_admin {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(user)
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
    // Argon2 is deliberately slow; keep it off the async workers
    let pw_hash = tokio::task::spawn_blocking(move || hash_password(&password)).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
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
    let token = make_jwt(&state, &user, user.is_admin)?;
    Ok(Json(AuthResp { token, api_key: user.api_key }))
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    public: Option<Extension<PublicRequest>>,
    Json(req): Json<AuthReq>,
) -> Result<Json<AuthResp>, StatusCode> {
    // On LazyCat the LAN side signs in through LazyCat; passwords are for the public address
    if state.lazycat && public.is_none() {
        return Err(StatusCode::FORBIDDEN);
    }
    let ip = crate::guard::client_ip(&headers, public.is_some());
    if ip.as_deref().is_some_and(|ip| state.guard.blocked(ip)) {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    // Try username first, then email
    let user = match db::find_user_by_username(&state.db, &req.username).await {
        Ok(u) => Some(u),
        Err(_) => db::find_user_by_email(&state.db, &req.username).await.ok(),
    };
    // Unknown users still pay for a hash, so response time doesn't reveal which names exist
    let hash = user.as_ref().map(|u| u.password_hash.clone());
    let password = req.password.clone();
    let ok = tokio::task::spawn_blocking(move || match &hash {
        Some(h) => verify_password(&password, h),
        None => verify_password(&password, dummy_hash()),
    }).await.unwrap_or(false);
    let Some(user) = user.filter(|_| ok) else {
        eprintln!("login failed for {:?} from {}", req.username, ip.as_deref().unwrap_or("unknown"));
        if let Some(ip) = &ip {
            state.guard.fail(ip);
        }
        return Err(StatusCode::UNAUTHORIZED);
    };
    let token = make_jwt(&state, &user, user.is_admin)?;
    Ok(Json(AuthResp { token, api_key: user.api_key }))
}

/// How the home page should offer sign-in on this address.
pub async fn options(
    State(state): State<Arc<AppState>>,
    public: Option<Extension<PublicRequest>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let public = public.is_some();
    let register = !public && !state.lazycat
        && db::user_count(&state.db).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)? == 0;
    Ok(Json(serde_json::json!({ "public": public, "register": register })))
}

const MIN_PASSWORD_LEN: usize = 8;

pub async fn set_password(pool: &sqlx::SqlitePool, user_id: i64, password: String) -> Result<(), StatusCode> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let pw_hash = tokio::task::spawn_blocking(move || hash_password(&password)).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)??;
    match db::update_user_password(pool, user_id, &pw_hash).await {
        Ok(0) => Err(StatusCode::NOT_FOUND),
        Ok(_) => Ok(()),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[derive(Deserialize)]
pub struct ChangePasswordReq {
    pub current_password: String,
    pub new_password: String,
}

/// A signed-in user changes their own password; a stolen session alone can't.
pub async fn change_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    public: Option<Extension<PublicRequest>>,
    Json(req): Json<ChangePasswordReq>,
) -> Result<StatusCode, StatusCode> {
    let ip = crate::guard::client_ip(&headers, public.is_some());
    if ip.as_deref().is_some_and(|ip| state.guard.blocked(ip)) {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    let user = current_user(&state, &headers).await?;
    let hash = user.password_hash.clone();
    let current = req.current_password;
    let ok = tokio::task::spawn_blocking(move || verify_password(&current, &hash)).await.unwrap_or(false);
    if !ok {
        if let Some(ip) = &ip {
            state.guard.fail(ip);
        }
        return Err(StatusCode::FORBIDDEN);
    }
    set_password(&state.db, user.id, req.new_password).await?;
    Ok(StatusCode::NO_CONTENT)
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
    let token = make_jwt(&state, &user, is_admin)?;
    Ok(Json(AuthResp { token, api_key: user.api_key }))
}

pub async fn get_api_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let user = current_user(&state, &headers).await?;
    Ok(Json(serde_json::json!({ "api_key": user.api_key })))
}

pub async fn refresh_api_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let user = current_user(&state, &headers).await?;
    let new_key = new_api_key(&user.username);
    db::update_user_api_key(&state.db, user.id, &new_key).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({ "api_key": new_key })))
}
