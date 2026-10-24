//! What a handler returns when it can't do what was asked.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub enum Error {
    /// A bare status, as the pages and admin API expect
    Status(StatusCode),
    /// An error body API clients understand, OpenAI-style and Anthropic-style alike
    Api { status: StatusCode, kind: &'static str, message: String },
    /// Something broke on our side; logged, and reported as a bare 500
    Internal(String),
}

impl Error {
    pub fn api(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Error {
        Error::Api { status, kind, message: message.into() }
    }
}

impl From<StatusCode> for Error {
    fn from(status: StatusCode) -> Error {
        Error::Status(status)
    }
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Error {
        Error::Internal(format!("database: {e}"))
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        match self {
            Error::Status(status) => status.into_response(),
            Error::Api { status, kind, message } => {
                (status, Json(serde_json::json!({ "type": "error", "error": { "type": kind, "message": message } }))).into_response()
            }
            Error::Internal(what) => {
                eprintln!("internal error: {what}");
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            }
        }
    }
}

/// Turns "no such row" into a 404.
pub trait OrNotFound<T> {
    fn or_not_found(self) -> Result<T>;
}

impl<T> OrNotFound<T> for Option<T> {
    fn or_not_found(self) -> Result<T> {
        self.ok_or(Error::Status(StatusCode::NOT_FOUND))
    }
}
