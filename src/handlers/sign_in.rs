//! Getting a session: LazyCat sign-in on the LAN side, a password on the public side.

use axum::{Json, extract::State, http::{HeaderMap, StatusCode}};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{AppState, auth, db, error::Result, extract::Origin};

#[derive(Deserialize)]
pub struct Credentials {
    /// Username or email
    username: String,
    password: String,
    #[serde(default)]
    email: String,
}

#[derive(Serialize)]
pub struct Signed {
    token: String,
    api_key: String,
}

fn signed(state: &AppState, user: db::users::User) -> Result<Json<Signed>> {
    Ok(Json(Signed { token: auth::issue_session(&state.jwt_secret, &user)?, api_key: user.api_key }))
}

/// How the home page should offer sign-in on this address.
pub async fn options(State(state): State<Arc<AppState>>, origin: Origin) -> Result<Json<serde_json::Value>> {
    let register = !origin.public && !state.lazycat && db::users::count(&state.db).await? == 0;
    Ok(Json(serde_json::json!({ "public": origin.public, "register": register })))
}

/// Registration only bootstraps the first (admin) user of a non-LazyCat install; the admin adds everyone else.
pub async fn register(State(state): State<Arc<AppState>>, Json(req): Json<Credentials>) -> Result<Json<Signed>> {
    if state.lazycat {
        return Err(StatusCode::FORBIDDEN.into());
    }
    if !req.email.contains('@') {
        return Err(StatusCode::BAD_REQUEST.into());
    }
    if db::users::count(&state.db).await? > 0 {
        return Err(StatusCode::FORBIDDEN.into());
    }
    let user = auth::create_user(&state.db, &req.username, &req.email, &req.password, true).await?;
    signed(&state, user)
}

pub async fn login(State(state): State<Arc<AppState>>, origin: Origin, Json(req): Json<Credentials>) -> Result<Json<Signed>> {
    // On LazyCat the LAN side signs in through LazyCat; passwords are for the public address
    if state.lazycat && !origin.public {
        return Err(StatusCode::FORBIDDEN.into());
    }
    origin.check(&state)?;
    let user = match db::users::by_username(&state.db, &req.username).await? {
        Some(u) => Some(u),
        None => db::users::by_email(&state.db, &req.username).await?,
    };
    let ok = auth::verify_password(req.password, user.as_ref().map(|u| u.password_hash.clone())).await;
    match user.filter(|_| ok) {
        Some(user) => signed(&state, user),
        None => {
            eprintln!("login failed for {:?} from {}", req.username, origin.ip.as_deref().unwrap_or("unknown"));
            origin.failed(&state);
            Err(StatusCode::UNAUTHORIZED.into())
        }
    }
}

/// Sign in as the LazyCat user making the request.
///
/// LazyCat's gateway authenticates every request outside `public_path` and sets `X-HC-User-ID`
/// and `X-HC-User-Role`, so they can be trusted — but only behind that gateway, which is why
/// this is off unless running on LazyCat and never served on the public listener. LazyCat
/// admins get an account automatically; other LazyCat users need an admin to add them first.
pub async fn lazycat(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Result<Json<Signed>> {
    if !state.lazycat {
        return Err(StatusCode::NOT_FOUND.into());
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).filter(|s| !s.is_empty());
    let uid = header("x-hc-user-id").ok_or(StatusCode::UNAUTHORIZED)?;
    let is_admin = header("x-hc-user-role") == Some("ADMIN");

    let mut user = match db::users::by_username(&state.db, uid).await? {
        Some(user) => user,
        None if is_admin => auth::create_user(&state.db, uid, "", "", true).await?,
        None => return Err(StatusCode::FORBIDDEN.into()),
    };
    // LazyCat decides who is an admin
    if user.is_admin != is_admin {
        db::users::set_admin(&state.db, user.id, is_admin).await?;
        user.is_admin = is_admin;
    }
    signed(&state, user)
}
