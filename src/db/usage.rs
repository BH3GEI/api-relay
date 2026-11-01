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

/// A calendar period in the viewer's time zone, for callers that don't give their own range.
#[derive(Clone, Copy, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    #[default]
    Day,
    /// Monday to Sunday
    Week,
    Month,
    Year,
}

/// How finely a report is cut over time.
#[derive(Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Step {
    Hour,
    Day,
    Month,
}

impl Step {
    /// The strftime of a bucket: "2026-11-01 13", "2026-11-01", "2026-11"
    fn format(self) -> &'static str {
        match self {
            Step::Hour => "%Y-%m-%d %H",
            Step::Day => "%Y-%m-%d",
            Step::Month => "%Y-%m",
        }
    }

    /// The step a chart of `secs` reads best at.
    pub fn for_span(secs: i64) -> Step {
        match secs {
            s if s <= 2 * 86400 => Step::Hour,
            s if s <= 93 * 86400 => Step::Day,
            _ => Step::Month,
        }
    }

    /// The longest span a report may cover at this step, so a chart stays a chart.
    pub fn max_span(self) -> i64 {
        match self {
            Step::Hour => 31 * 86400,
            Step::Day => 400 * 86400,
            Step::Month => 30 * 366 * 86400,
        }
    }
}

/// What the series is split into, one stack segment per value.
#[derive(Clone, Copy, PartialEq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Group {
    #[default]
    None,
    Model,
    /// The part of the model name before the `/`
    Provider,
    User,
}

impl Group {
    fn expr(self) -> &'static str {
        match self {
            Group::None => "''",
            Group::Model => "l.model",
            Group::Provider => "CASE WHEN instr(l.model, '/') > 0 THEN substr(l.model, 1, instr(l.model, '/') - 1) ELSE l.model END",
            Group::User => "u.username",
        }
    }
}

/// Which requests a report covers: `[from, to)` in seconds since the epoch, optionally one
/// user's and one model's (`kimi/m1`) or provider's (`kimi/*`).
#[derive(Clone, Copy)]
pub struct Filter<'a> {
    pub from: i64,
    pub to: i64,
    pub user_id: Option<i64>,
    pub model: Option<&'a str>,
}

/// `[from, to)` of the current `period` in a time zone `tz_minutes` east of UTC (timestamps are stored in UTC).
pub async fn period_bounds(db: &Db, period: Period, tz_minutes: i64) -> sqlx::Result<(i64, i64)> {
    let (start, next): (&[&str], &str) = match period {
        Period::Day => (&["start of day"], "+1 day"),
        // weekday 0 moves on to Sunday (or stays on it); six days back is that week's Monday
        Period::Week => (&["start of day", "weekday 0", "-6 days"], "+7 days"),
        Period::Month => (&["start of month"], "+1 month"),
        Period::Year => (&["start of year"], "+1 year"),
    };
    let shift = tz_minutes.clamp(-14 * 60, 14 * 60);
    let start = start.iter().map(|m| format!(", '{m}'")).collect::<String>();
    sqlx::query_as(&format!(
        "SELECT CAST(strftime('%s', 'now', ?1{start}, ?2) AS INTEGER), CAST(strftime('%s', 'now', ?1{start}, '{next}', ?2) AS INTEGER)"
    )).bind(format!("{shift:+} minutes")).bind(format!("{:+} minutes", -shift)).fetch_one(db).await
}

/// The rows a filter selects; binds ?1 from, ?2 to, ?3 user, ?4 model. Comparing the stored
/// text lets the (user_id, created_at) index narrow the scan.
const MATCHING: &str = "FROM usage_logs l JOIN users u ON l.user_id = u.id
    WHERE l.created_at >= datetime(?1, 'unixepoch') AND l.created_at < datetime(?2, 'unixepoch')
      AND (?3 IS NULL OR l.user_id = ?3)
      AND (?4 IS NULL OR l.model = ?4
        OR (?4 LIKE '%/*' AND substr(l.model, 1, length(?4) - 1) = substr(?4, 1, length(?4) - 1)))";

const SUMS: &str = "COUNT(*) AS requests, COALESCE(SUM(l.input_tokens), 0) AS input_tokens,
    COALESCE(SUM(l.output_tokens), 0) AS output_tokens, COALESCE(SUM(l.input_tokens + l.output_tokens), 0) AS total_tokens";

fn bind<'q>(q: &'q str, f: Filter<'q>) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    sqlx::query(q).bind(f.from).bind(f.to).bind(f.user_id).bind(f.model)
}

#[derive(Serialize, sqlx::FromRow, Default)]
pub struct Sums {
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct Bucket {
    /// When, in the viewer's time zone, as `Step::format` writes it
    pub bucket: String,
    /// The model, provider or user this segment is; empty when not split
    pub group: String,
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub sums: Sums,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct Row {
    pub user_id: i64,
    pub username: String,
    pub model: String,
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub sums: Sums,
}

/// Usage per bucket of `step` (in a zone `tz_minutes` east of UTC) and per `group`.
pub async fn series(db: &Db, f: Filter<'_>, step: Step, tz_minutes: i64, group: Group) -> sqlx::Result<Vec<Bucket>> {
    use sqlx::FromRow;
    let q = format!(
        "SELECT strftime(?5, l.created_at, ?6) AS bucket, {} AS \"group\", {SUMS} {MATCHING} GROUP BY 1, 2 ORDER BY 1, 2",
        group.expr()
    );
    let shift = format!("{:+} minutes", tz_minutes.clamp(-14 * 60, 14 * 60));
    let rows = bind(&q, f).bind(step.format()).bind(shift).fetch_all(db).await?;
    rows.iter().map(Bucket::from_row).collect()
}

/// Usage per user and model, biggest first.
pub async fn rows(db: &Db, f: Filter<'_>) -> sqlx::Result<Vec<Row>> {
    use sqlx::FromRow;
    let q = format!("SELECT u.id AS user_id, u.username, l.model, {SUMS} {MATCHING} GROUP BY u.id, l.model ORDER BY total_tokens DESC");
    bind(&q, f).fetch_all(db).await?.iter().map(Row::from_row).collect()
}

pub async fn sums(db: &Db, f: Filter<'_>) -> sqlx::Result<Sums> {
    use sqlx::FromRow;
    let q = format!("SELECT {SUMS} {MATCHING}");
    Sums::from_row(&bind(&q, f).fetch_one(db).await?)
}

/// The models, and the users, that have any usage in the filter's time range, whatever else it picks.
pub async fn present(db: &Db, f: Filter<'_>) -> sqlx::Result<(Vec<String>, Vec<(i64, String)>)> {
    let f = Filter { model: None, ..f };
    let q = format!("SELECT DISTINCT l.model {MATCHING} ORDER BY 1");
    let models = bind(&q, f).fetch_all(db).await?.iter().map(|r| sqlx::Row::get(r, 0)).collect();
    let q = format!("SELECT DISTINCT u.id, u.username {MATCHING} ORDER BY 2");
    let users = bind(&q, Filter { user_id: None, ..f }).fetch_all(db).await?.iter()
        .map(|r| (sqlx::Row::get(r, 0), sqlx::Row::get(r, 1))).collect();
    Ok((models, users))
}
