//! One row per relayed request, and reports over them.

use serde::{Deserialize, Serialize};

use super::Db;

/// Record a request as it starts, so concurrent requests count against limits; tokens are filled in later.
pub async fn start(db: &Db, user_id: i64, model: &str) -> sqlx::Result<i64> {
    let (id,): (i64,) = sqlx::query_as("INSERT INTO usage_logs (user_id, model) VALUES (?, ?) RETURNING id")
        .bind(user_id).bind(model).fetch_one(db).await?;
    Ok(id)
}

pub async fn finish(db: &Db, id: i64, input_tokens: i64, output_tokens: i64) -> sqlx::Result<()> {
    sqlx::query("UPDATE usage_logs SET input_tokens = ?, output_tokens = ? WHERE id = ?")
        .bind(input_tokens).bind(output_tokens).bind(id).execute(db).await?;
    Ok(())
}

/// Forget a request the upstream refused, so it doesn't count.
pub async fn discard(db: &Db, id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM usage_logs WHERE id = ?").bind(id).execute(db).await?;
    Ok(())
}

#[derive(Serialize, sqlx::FromRow)]
pub struct Totals {
    pub requests: i64,
    pub total_tokens: i64,
}

/// A user's usage over the last `window_secs`, for one model, a provider's (`kimi/*`) or all (`*`).
pub async fn in_window(db: &Db, user_id: i64, model: &str, window_secs: i64) -> sqlx::Result<Totals> {
    // `kimi/*` matches on the `kimi/` prefix, compared as a string so `_` and `%` stay literal
    sqlx::query_as(
        "SELECT COUNT(*) AS requests, COALESCE(SUM(input_tokens + output_tokens), 0) AS total_tokens
         FROM usage_logs WHERE user_id = ?1 AND created_at >= datetime('now', ?3) AND (?2 = '*' OR model = ?2
           OR (?2 LIKE '%/*' AND substr(model, 1, length(?2) - 1) = substr(?2, 1, length(?2) - 1)))"
    ).bind(user_id).bind(model).bind(format!("-{window_secs} seconds")).fetch_one(db).await
}

/// A calendar period in the viewer's time zone, bucketed for a chart.
#[derive(Clone, Copy, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    /// Today, by hour
    #[default]
    Day,
    /// This month, by day
    Month,
    /// This year, by month
    Year,
}

impl Period {
    /// (strftime of the period, strftime of a bucket within it)
    fn formats(self) -> (&'static str, &'static str) {
        match self {
            Period::Day => ("%Y-%m-%d", "%H"),
            Period::Month => ("%Y-%m", "%d"),
            Period::Year => ("%Y", "%m"),
        }
    }
}

/// The current `period` as seen from a time zone `tz_minutes` east of UTC (timestamps are stored in UTC).
#[derive(Clone, Copy)]
pub struct Range {
    pub period: Period,
    pub tz_minutes: i64,
}

impl Range {
    fn shift(self) -> String {
        format!("{:+} minutes", self.tz_minutes.clamp(-14 * 60, 14 * 60))
    }
}

/// `column` falls in the viewer's current period; `?{period}` binds the period's strftime and
/// `?{shift}` the zone shift. The plain bound lets the index narrow the scan before the
/// local-time comparison, which it can't use.
fn in_range(column: &str, period: u8, shift: u8) -> String {
    format!("{column} >= datetime('now', '-367 days') AND strftime(?{period}, {column}, ?{shift}) = strftime(?{period}, 'now', ?{shift})")
}

#[derive(Serialize, sqlx::FromRow)]
pub struct Bucket {
    /// "00"–"23" for a day, "01"–"31" for a month, "01"–"12" for a year
    pub bucket: String,
    pub requests: i64,
    pub total_tokens: i64,
}

/// Usage over the range, per bucket; one user's, or everyone's.
pub async fn series(db: &Db, user_id: Option<i64>, range: Range) -> sqlx::Result<Vec<Bucket>> {
    let (period, bucket) = range.period.formats();
    sqlx::query_as(&format!(
        "SELECT strftime(?1, created_at, ?2) AS bucket, COUNT(*) AS requests, COALESCE(SUM(input_tokens + output_tokens), 0) AS total_tokens
         FROM usage_logs WHERE (?3 IS NULL OR user_id = ?3) AND {}
         GROUP BY bucket ORDER BY bucket", in_range("created_at", 4, 2)
    )).bind(bucket).bind(range.shift()).bind(user_id).bind(period).fetch_all(db).await
}

#[derive(Serialize, sqlx::FromRow)]
pub struct ModelTotals {
    pub model: String,
    pub requests: i64,
    pub total_tokens: i64,
}

/// One user's usage over the range, per model.
pub async fn by_model(db: &Db, user_id: i64, range: Range) -> sqlx::Result<Vec<ModelTotals>> {
    let (period, _) = range.period.formats();
    sqlx::query_as(&format!(
        "SELECT model, COUNT(*) AS requests, COALESCE(SUM(input_tokens + output_tokens), 0) AS total_tokens
         FROM usage_logs WHERE user_id = ?1 AND {}
         GROUP BY model ORDER BY total_tokens DESC", in_range("created_at", 2, 3)
    )).bind(user_id).bind(period).bind(range.shift()).fetch_all(db).await
}

#[derive(Serialize, sqlx::FromRow)]
pub struct UserModelTotals {
    pub user_id: i64,
    pub username: String,
    pub model: String,
    pub requests: i64,
    pub total_tokens: i64,
}

/// Everyone's usage over the range, per user and model.
pub async fn by_user_and_model(db: &Db, range: Range) -> sqlx::Result<Vec<UserModelTotals>> {
    let (period, _) = range.period.formats();
    sqlx::query_as(&format!(
        "SELECT u.id AS user_id, u.username, l.model, COUNT(*) AS requests, COALESCE(SUM(l.input_tokens + l.output_tokens), 0) AS total_tokens
         FROM usage_logs l JOIN users u ON l.user_id = u.id WHERE {}
         GROUP BY u.id, l.model ORDER BY total_tokens DESC", in_range("l.created_at", 1, 2)
    )).bind(period).bind(range.shift()).fetch_all(db).await
}
