//! Credentials: password hashes, session tokens, API keys, accounts.

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use axum::http::{HeaderMap, StatusCode};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};

use crate::db::{self, Db, users::User};
use crate::error::{Error, Result};

pub const MIN_PASSWORD_LEN: usize = 8;
const SESSION_SECS: u64 = 7 * 86400;

// ── Passwords ──

fn hash_now(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default().hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| Error::Internal(format!("hashing a password: {e}")))
}

/// Argon2 is deliberately slow, so it runs off the async workers.
pub async fn hash_password(password: String) -> Result<String> {
    tokio::task::spawn_blocking(move || hash_now(&password)).await.map_err(|e| Error::Internal(e.to_string()))?
}

/// Whether `password` matches `hash`. With no hash (an unknown user) a dummy one is checked,
/// so the time taken doesn't reveal which names exist.
pub async fn verify_password(password: String, hash: Option<String>) -> bool {
    tokio::task::spawn_blocking(move || {
        let hash = hash.unwrap_or_else(|| dummy_hash().to_string());
        PasswordHash::new(&hash)
            .and_then(|h| Argon2::default().verify_password(password.as_bytes(), &h))
            .is_ok()
    }).await.unwrap_or(false)
}

fn dummy_hash() -> &'static str {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| hash_now(&uuid::Uuid::new_v4().to_string()).unwrap_or_default())
}

/// Set a user's password; 422 when it is too short, 404 when there is no such user.
pub async fn set_password(db: &Db, user_id: i64, password: String) -> Result<()> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(StatusCode::UNPROCESSABLE_ENTITY.into());
    }
    let hash = hash_password(password).await?;
    if db::users::set_password_hash(db, user_id, &hash).await? { Ok(()) } else { Err(StatusCode::NOT_FOUND.into()) }
}

// ── Accounts ──

/// `sk-<username>-relay-<uuid>`; the name is cosmetic, the uuid carries all the secrecy.
pub fn new_api_key(username: &str) -> String {
    let name: String = username.chars().filter(|c| c.is_ascii_alphanumeric()).take(16).collect::<String>().to_ascii_lowercase();
    let random = uuid::Uuid::new_v4().simple();
    if name.is_empty() { format!("sk-relay-{random}") } else { format!("sk-{name}-relay-{random}") }
}

/// Whether a credential is one of our API keys rather than a session token.
pub fn is_api_key(credential: &str) -> bool {
    credential.starts_with("sk-")
}

/// A new account; 409 when the name is taken. Without a password (LazyCat sign-in users)
/// it gets a random one nobody knows.
pub async fn create_user(db: &Db, username: &str, email: &str, password: &str, is_admin: bool) -> Result<User> {
    let password = if password.is_empty() { uuid::Uuid::new_v4().to_string() } else { password.to_string() };
    let hash = hash_password(password).await?;
    let new = db::users::NewUser { username, email, password_hash: &hash, api_key: &new_api_key(username), is_admin };
    db::users::insert(db, &new).await?.ok_or(Error::Status(StatusCode::CONFLICT))
}

// ── Sessions ──

#[derive(Serialize, Deserialize)]
struct Claims {
    /// Username
    sub: String,
    /// User id, so a token stops working if its user is deleted and the name reused
    #[serde(default)]
    uid: i64,
    is_admin: bool,
    exp: usize,
}

pub fn issue_session(secret: &str, user: &User) -> Result<String> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let claims = Claims { sub: user.username.clone(), uid: user.id, is_admin: user.is_admin, exp: (now + SESSION_SECS) as usize };
    encode(&Header::default(), &claims, &EncodingKey::from_secret(secret.as_bytes())).map_err(|e| Error::Internal(e.to_string()))
}

/// The user a session token was issued to, if it is valid and they still exist.
pub async fn session_user(db: &Db, secret: &str, token: &str) -> Result<Option<User>> {
    let Ok(data) = decode::<Claims>(token, &DecodingKey::from_secret(secret.as_bytes()), &Validation::default()) else {
        return Ok(None);
    };
    Ok(db::users::by_username(db, &data.claims.sub).await?.filter(|u| u.id == data.claims.uid))
}

/// The credential a request carries: `Authorization: Bearer …`, or `x-api-key` (Anthropic clients).
pub fn credential(headers: &HeaderMap) -> Option<&str> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    header("authorization").and_then(|v| v.strip_prefix("Bearer ")).or_else(|| header("x-api-key"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_keys_carry_a_cleaned_name() {
        assert!(new_api_key("Ana.The-Party").starts_with("sk-anatheparty-relay-"));
        assert!(new_api_key("中文").starts_with("sk-relay-"));
        assert!(is_api_key(&new_api_key("x")));
    }

    #[test]
    fn credentials_come_from_either_header() {
        let mut h = HeaderMap::new();
        assert_eq!(credential(&h), None);
        h.insert("x-api-key", "sk-a".parse().unwrap());
        assert_eq!(credential(&h), Some("sk-a"));
        h.insert("authorization", "Bearer sk-b".parse().unwrap());
        assert_eq!(credential(&h), Some("sk-b"));
    }
}
