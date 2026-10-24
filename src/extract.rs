//! Who is asking, and from where — pulled out of a request before a handler runs.

use axum::{
    async_trait,
    extract::{FromRequestParts, Request, State},
    http::{StatusCode, request::Parts},
    middleware::Next,
    response::Response,
};
use std::sync::Arc;

use crate::{AppState, auth, db, error::{Error, Result}, guard};

/// Marks requests that came in on the public listener (the Cloudflare Tunnel).
#[derive(Clone, Copy)]
pub struct PublicListener;

/// Which listener a request came in on, and the caller's address as its proxy reports it.
pub struct Origin {
    pub public: bool,
    pub ip: Option<String>,
}

impl Origin {
    /// 429 when this address is shut out for failing too often.
    pub fn check(&self, state: &AppState) -> Result<()> {
        state.guard.check(self.ip.as_deref())
    }

    pub fn failed(&self, state: &AppState) {
        state.guard.fail(self.ip.as_deref());
    }

    fn from_parts(parts: &Parts) -> Origin {
        let public = parts.extensions.get::<PublicListener>().is_some();
        Origin { public, ip: guard::client_ip(&parts.headers, public) }
    }
}

#[async_trait]
impl<S: Send + Sync> FromRequestParts<S> for Origin {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Origin, Self::Rejection> {
        Ok(Origin::from_parts(parts))
    }
}

/// A signed-in session (the pages). An API key is not enough.
pub struct Session(pub db::users::User);

#[async_trait]
impl FromRequestParts<Arc<AppState>> for Session {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<AppState>) -> Result<Session> {
        let token = auth::credential(&parts.headers).ok_or(StatusCode::UNAUTHORIZED)?;
        // The database decides, so a deleted user's or revoked admin's unexpired token stops working
        let user = auth::session_user(&state.db, &state.jwt_secret, token).await?.ok_or(StatusCode::UNAUTHORIZED)?;
        Ok(Session(user))
    }
}

/// An admin's signed-in session.
pub struct Admin;

#[async_trait]
impl FromRequestParts<Arc<AppState>> for Admin {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<AppState>) -> Result<Admin> {
        let Session(user) = Session::from_request_parts(parts, state).await?;
        if !user.is_admin {
            return Err(StatusCode::FORBIDDEN.into());
        }
        Ok(Admin)
    }
}

/// A user by API key or by session: what the home page can do, an API client can too.
pub struct Caller(pub db::users::User);

#[async_trait]
impl FromRequestParts<Arc<AppState>> for Caller {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<AppState>) -> Result<Caller> {
        if let Some(KeyHolder(user)) = parts.extensions.get::<KeyHolder>() {
            return Ok(Caller(user.clone()));
        }
        match auth::credential(&parts.headers) {
            Some(key) if auth::is_api_key(key) => {
                db::users::by_api_key(&state.db, key).await?.map(Caller).ok_or(StatusCode::UNAUTHORIZED.into())
            }
            _ => Session::from_request_parts(parts, state).await.map(|Session(u)| Caller(u)),
        }
    }
}

/// The user whose API key `require_api_key` accepted.
#[derive(Clone)]
struct KeyHolder(db::users::User);

/// Reject API calls without a valid key before their (possibly large) body is read,
/// counting failures against the caller's address.
pub async fn require_api_key(State(state): State<Arc<AppState>>, mut req: Request, next: Next) -> Result<Response> {
    let (mut parts, body) = req.into_parts();
    let origin = Origin::from_parts(&parts);
    origin.check(&state)?;
    let user = match auth::credential(&parts.headers) {
        Some(key) => db::users::by_api_key(&state.db, key).await?,
        None => None,
    };
    let Some(user) = user else {
        eprintln!("auth failed: {} {} from {}", parts.method, parts.uri.path(), origin.ip.as_deref().unwrap_or("unknown"));
        origin.failed(&state);
        return Err(StatusCode::UNAUTHORIZED.into());
    };
    parts.extensions.insert(KeyHolder(user));
    req = Request::from_parts(parts, body);
    Ok(next.run(req).await)
}
