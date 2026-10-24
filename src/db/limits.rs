//! Caps on what a user may use: requests and/or tokens over a rolling window, for one model or all.

use serde::Serialize;

use super::{Db, usage};

/// No cap, for `max_requests` or `max_tokens`.
pub const UNLIMITED: i64 = -1;
/// Applies to every model.
pub const ALL_MODELS: &str = "*";

#[derive(Serialize, sqlx::FromRow)]
pub struct Limit {
    pub id: i64,
    pub user_id: i64,
    pub model: String,
    pub window_secs: i64,
    pub max_requests: i64,
    pub max_tokens: i64,
}

const COLUMNS: &str = "id, user_id, model, window_secs, max_requests, max_tokens";

impl Limit {
    fn allows(&self, used: &usage::Totals) -> bool {
        // Any negative cap is no cap
        let under = |max: i64, n: i64| max < 0 || n < max;
        under(self.max_requests, used.requests) && under(self.max_tokens, used.total_tokens)
    }
}

/// Whether the user may make one more request to `model` under every limit that applies.
/// No limits means unlimited.
pub async fn allows(db: &Db, user_id: i64, model: &str) -> sqlx::Result<bool> {
    let limits: Vec<Limit> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM usage_limits WHERE user_id = ? AND (model = ? OR model = ?)"))
        .bind(user_id).bind(model).bind(ALL_MODELS).fetch_all(db).await?;
    for limit in &limits {
        if !limit.allows(&usage::in_window(db, user_id, &limit.model, limit.window_secs).await?) {
            return Ok(false);
        }
    }
    Ok(true)
}

pub async fn list(db: &Db) -> sqlx::Result<Vec<Limit>> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM usage_limits ORDER BY user_id, model, window_secs")).fetch_all(db).await
}

pub async fn for_user(db: &Db, user_id: i64) -> sqlx::Result<Vec<Limit>> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM usage_limits WHERE user_id = ? ORDER BY model, window_secs")).bind(user_id).fetch_all(db).await
}

/// Set a limit; one for the same user, model and window is replaced.
pub async fn set(db: &Db, user_id: i64, model: &str, window_secs: i64, max_requests: i64, max_tokens: i64) -> sqlx::Result<Limit> {
    sqlx::query_as(&format!(
        "INSERT INTO usage_limits (user_id, model, window_secs, max_requests, max_tokens) VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(user_id, model, window_secs) DO UPDATE SET max_requests = excluded.max_requests, max_tokens = excluded.max_tokens
         RETURNING {COLUMNS}"
    )).bind(user_id).bind(model).bind(window_secs).bind(max_requests).bind(max_tokens).fetch_one(db).await
}

pub async fn delete(db: &Db, id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM usage_limits WHERE id = ?").bind(id).execute(db).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps() {
        let limit = |max_requests, max_tokens| Limit { id: 0, user_id: 0, model: "*".into(), window_secs: 1, max_requests, max_tokens };
        let used = |requests, total_tokens| usage::Totals { requests, total_tokens };
        assert!(limit(UNLIMITED, UNLIMITED).allows(&used(1000, 1000)));
        assert!(!limit(0, UNLIMITED).allows(&used(0, 0)), "0 blocks the model");
        assert!(limit(2, UNLIMITED).allows(&used(1, 0)));
        assert!(!limit(2, UNLIMITED).allows(&used(2, 0)));
        assert!(!limit(UNLIMITED, 20).allows(&used(1, 30)));
    }
}
