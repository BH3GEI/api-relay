use serde::Serialize;
use sqlx::SqlitePool;

#[derive(Serialize, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub email: String,
    #[serde(skip)]
    pub password_hash: String,
    pub api_key: String,
    pub is_admin: bool,
    pub created_at: String,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct Provider {
    pub id: i64,
    pub name: String,
    pub base_url: String,
    pub user_agent: String,
    /// Legacy, unused; never sent to the browser
    #[serde(skip)]
    pub api_key: Option<String>,
    pub enabled: bool,
    pub anthropic_base_url: String,
    /// Model ids to offer in addition to what the upstream lists (some upstreams have no /models)
    pub models: String,
    /// Most requests in flight to this provider at once; None means no cap
    pub max_concurrency: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct ProviderKey {
    pub id: i64,
    pub provider_id: i64,
    /// Only the last characters ever leave the server
    #[serde(serialize_with = "serialize_key_tail")]
    pub api_key: String,
    pub label: String,
    pub enabled: bool,
    pub priority: i64,
}

fn serialize_key_tail<S: serde::Serializer>(key: &str, s: S) -> Result<S::Ok, S::Error> {
    let tail = key.char_indices().rev().nth(3).map_or(key, |(i, _)| &key[i..]);
    s.serialize_str(&format!("…{tail}"))
}

pub async fn migrate(pool: &SqlitePool) {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            username TEXT UNIQUE NOT NULL,
            email TEXT NOT NULL DEFAULT '',
            password_hash TEXT NOT NULL,
            api_key TEXT UNIQUE NOT NULL,
            is_admin BOOLEAN DEFAULT FALSE,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        )"
    ).execute(pool).await.unwrap();
    // Migration: add email column if missing
    let _ = sqlx::query("ALTER TABLE users ADD COLUMN email TEXT NOT NULL DEFAULT ''")
        .execute(pool).await;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS providers (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT UNIQUE NOT NULL,
            base_url TEXT NOT NULL,
            user_agent TEXT NOT NULL,
            api_key TEXT,
            enabled BOOLEAN DEFAULT TRUE
        )"
    ).execute(pool).await.unwrap();
    // Migration: base_url is the OpenAI-style base (…/v1); Anthropic endpoints often live elsewhere
    let _ = sqlx::query("ALTER TABLE providers ADD COLUMN anthropic_base_url TEXT NOT NULL DEFAULT ''")
        .execute(pool).await;
    let _ = sqlx::query("ALTER TABLE providers ADD COLUMN models TEXT NOT NULL DEFAULT ''")
        .execute(pool).await;
    // Migration: per-provider concurrency cap, NULL = no cap
    let _ = sqlx::query("ALTER TABLE providers ADD COLUMN max_concurrency INTEGER")
        .execute(pool).await;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS provider_keys (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            provider_id INTEGER NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
            api_key TEXT NOT NULL,
            label TEXT NOT NULL DEFAULT '',
            enabled BOOLEAN DEFAULT TRUE,
            priority INTEGER DEFAULT 0
        )"
    ).execute(pool).await.unwrap();

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS usage_logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL,
            model TEXT NOT NULL,
            input_tokens INTEGER DEFAULT 0,
            output_tokens INTEGER DEFAULT 0,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        )"
    ).execute(pool).await.unwrap();
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_usage_user_date ON usage_logs(user_id, created_at)")
        .execute(pool).await;
    // Migration: who sent a request and for what, from the client's X-Octo-* headers
    for col in ["actor", "task", "run"] {
        let _ = sqlx::query(&format!("ALTER TABLE usage_logs ADD COLUMN {col} TEXT"))
            .execute(pool).await;
    }
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_usage_actor_date ON usage_logs(actor, created_at)")
        .execute(pool).await;
    let _ = sqlx::query("CREATE INDEX IF NOT EXISTS idx_usage_task_date ON usage_logs(task, created_at)")
        .execute(pool).await;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS usage_limits (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL,
            model TEXT NOT NULL DEFAULT '*',
            window_secs INTEGER NOT NULL DEFAULT 86400,
            max_requests INTEGER DEFAULT -1,
            max_tokens INTEGER DEFAULT -1,
            UNIQUE(user_id, model, window_secs)
        )"
    ).execute(pool).await.unwrap();
    // Migration: limits used to be per calendar day only
    let (has_window,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pragma_table_info('usage_limits') WHERE name = 'window_secs'")
        .fetch_one(pool).await.unwrap();
    if has_window == 0 {
        let mut tx = pool.begin().await.unwrap();
        for stmt in [
            "CREATE TABLE usage_limits_new (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id INTEGER NOT NULL,
                model TEXT NOT NULL DEFAULT '*',
                window_secs INTEGER NOT NULL DEFAULT 86400,
                max_requests INTEGER DEFAULT -1,
                max_tokens INTEGER DEFAULT -1,
                UNIQUE(user_id, model, window_secs)
            )",
            "INSERT INTO usage_limits_new (id, user_id, model, window_secs, max_requests, max_tokens)
                SELECT id, user_id, model, 86400, max_requests_daily, max_tokens_daily FROM usage_limits",
            "DROP TABLE usage_limits",
            "ALTER TABLE usage_limits_new RENAME TO usage_limits",
        ] {
            sqlx::query(stmt).execute(&mut *tx).await.unwrap();
        }
        tx.commit().await.unwrap();
    }

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )"
    ).execute(pool).await.unwrap();

}

// ── Settings ──

/// The JWT signing secret, generated on first start so admin logins survive restarts.
pub async fn jwt_secret(pool: &SqlitePool) -> Result<String, sqlx::Error> {
    sqlx::query("INSERT OR IGNORE INTO settings (key, value) VALUES ('jwt_secret', ?)")
        .bind(format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple()))
        .execute(pool).await?;
    let row: (String,) = sqlx::query_as("SELECT value FROM settings WHERE key = 'jwt_secret'")
        .fetch_one(pool).await?;
    Ok(row.0)
}

pub async fn get_setting(pool: &SqlitePool, key: &str) -> Result<Option<String>, sqlx::Error> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = ?").bind(key).fetch_optional(pool).await?;
    Ok(row.map(|r| r.0))
}

pub async fn set_setting(pool: &SqlitePool, key: &str, value: &str) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(key).bind(value).execute(pool).await?;
    Ok(())
}

// ── Users ──

pub async fn list_users(pool: &SqlitePool) -> Result<Vec<User>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM users ORDER BY id").fetch_all(pool).await
}

pub async fn find_user_by_username(pool: &SqlitePool, username: &str) -> Result<User, sqlx::Error> {
    sqlx::query_as("SELECT * FROM users WHERE username = ?").bind(username).fetch_one(pool).await
}

pub async fn find_user_by_email(pool: &SqlitePool, email: &str) -> Result<User, sqlx::Error> {
    sqlx::query_as("SELECT * FROM users WHERE email = ?").bind(email).fetch_one(pool).await
}

pub async fn find_user_by_api_key(pool: &SqlitePool, api_key: &str) -> Result<User, sqlx::Error> {
    sqlx::query_as("SELECT * FROM users WHERE api_key = ?").bind(api_key).fetch_one(pool).await
}

pub async fn insert_user(pool: &SqlitePool, username: &str, email: &str, password_hash: &str, api_key: &str, is_admin: bool) -> Result<User, sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO users (username, email, password_hash, api_key, is_admin) VALUES (?, ?, ?, ?, ?) RETURNING *"
    ).bind(username).bind(email).bind(password_hash).bind(api_key).bind(is_admin).fetch_one(pool).await
}

pub async fn update_user_api_key(pool: &SqlitePool, user_id: i64, new_key: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET api_key = ? WHERE id = ?").bind(new_key).bind(user_id).execute(pool).await?;
    Ok(())
}

pub async fn set_user_admin(pool: &SqlitePool, user_id: i64, is_admin: bool) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET is_admin = ? WHERE id = ?").bind(is_admin).bind(user_id).execute(pool).await?;
    Ok(())
}

pub async fn delete_user(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM users WHERE id = ?").bind(id).execute(pool).await?;
    Ok(())
}

pub async fn user_count(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users").fetch_one(pool).await?;
    Ok(row.0)
}

// ── Providers ──

pub async fn list_providers(pool: &SqlitePool) -> Result<Vec<Provider>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM providers ORDER BY id").fetch_all(pool).await
}

pub struct ProviderInput<'a> {
    pub name: &'a str,
    pub base_url: &'a str,
    pub anthropic_base_url: &'a str,
    pub user_agent: &'a str,
    pub api_key: Option<&'a str>,
    pub models: &'a str,
    pub enabled: bool,
    pub max_concurrency: Option<i64>,
}

pub async fn create_provider(pool: &SqlitePool, p: &ProviderInput<'_>) -> Result<Provider, sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO providers (name, base_url, anthropic_base_url, user_agent, api_key, models, enabled, max_concurrency) VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING *"
    ).bind(p.name).bind(p.base_url).bind(p.anthropic_base_url).bind(p.user_agent).bind(p.api_key).bind(p.models).bind(p.enabled).bind(p.max_concurrency).fetch_one(pool).await
}

pub async fn update_provider(pool: &SqlitePool, id: i64, p: &ProviderInput<'_>) -> Result<Provider, sqlx::Error> {
    sqlx::query_as(
        "UPDATE providers SET name=?, base_url=?, anthropic_base_url=?, user_agent=?, api_key=?, models=?, enabled=?, max_concurrency=? WHERE id=? RETURNING *"
    ).bind(p.name).bind(p.base_url).bind(p.anthropic_base_url).bind(p.user_agent).bind(p.api_key).bind(p.models).bind(p.enabled).bind(p.max_concurrency).bind(id).fetch_one(pool).await
}

pub async fn get_provider(pool: &SqlitePool, id: i64) -> Result<Provider, sqlx::Error> {
    sqlx::query_as("SELECT * FROM providers WHERE id = ?").bind(id).fetch_one(pool).await
}

pub async fn get_enabled_provider(pool: &SqlitePool, id: i64) -> Result<Provider, sqlx::Error> {
    sqlx::query_as("SELECT * FROM providers WHERE id = ? AND enabled = TRUE").bind(id).fetch_one(pool).await
}

pub async fn list_enabled_providers(pool: &SqlitePool) -> Result<Vec<Provider>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM providers WHERE enabled = TRUE ORDER BY id").fetch_all(pool).await
}

pub async fn delete_provider(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM providers WHERE id = ?").bind(id).execute(pool).await?;
    Ok(())
}

pub async fn get_provider_keys(pool: &SqlitePool, provider_id: i64) -> Result<Vec<ProviderKey>, sqlx::Error> {
    sqlx::query_as(
        "SELECT * FROM provider_keys WHERE provider_id = ? AND enabled = TRUE ORDER BY priority ASC, id ASC"
    ).bind(provider_id).fetch_all(pool).await
}

// ── Provider Keys CRUD ──

pub async fn list_provider_keys(pool: &SqlitePool, provider_id: i64) -> Result<Vec<ProviderKey>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM provider_keys WHERE provider_id = ? ORDER BY priority ASC, id ASC")
        .bind(provider_id).fetch_all(pool).await
}

pub async fn create_provider_key(pool: &SqlitePool, provider_id: i64, api_key: &str, label: &str, priority: i64) -> Result<ProviderKey, sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO provider_keys (provider_id, api_key, label, priority) VALUES (?, ?, ?, ?) RETURNING *"
    ).bind(provider_id).bind(api_key).bind(label).bind(priority).fetch_one(pool).await
}

pub async fn delete_provider_key(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM provider_keys WHERE id = ?").bind(id).execute(pool).await?;
    Ok(())
}

// ── Usage Logging ──

/// Record a request as it starts, so concurrent requests count against limits; tokens are filled in later.
pub async fn start_usage(pool: &SqlitePool, user_id: i64, model: &str, trace: &crate::trace::Trace) -> Result<i64, sqlx::Error> {
    let (id,): (i64,) = sqlx::query_as("INSERT INTO usage_logs (user_id, model, actor, task, run) VALUES (?, ?, ?, ?, ?) RETURNING id")
        .bind(user_id).bind(model).bind(&trace.actor).bind(&trace.task).bind(&trace.run).fetch_one(pool).await?;
    Ok(id)
}

pub async fn finish_usage(pool: &SqlitePool, id: i64, input_tokens: i64, output_tokens: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE usage_logs SET input_tokens = ?, output_tokens = ? WHERE id = ?")
        .bind(input_tokens).bind(output_tokens).bind(id).execute(pool).await?;
    Ok(())
}

/// Forget a request the upstream refused, so it doesn't count.
pub async fn drop_usage(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM usage_logs WHERE id = ?").bind(id).execute(pool).await?;
    Ok(())
}

#[derive(Serialize, sqlx::FromRow)]
pub struct WindowUsage {
    pub requests: i64,
    pub total_tokens: i64,
}

/// A user's usage over the last `window_secs`, for one model or all (`*`).
pub async fn window_usage(pool: &SqlitePool, user_id: i64, model: &str, window_secs: i64) -> Result<WindowUsage, sqlx::Error> {
    sqlx::query_as(
        "SELECT COUNT(*) as requests, COALESCE(SUM(input_tokens + output_tokens), 0) as total_tokens
         FROM usage_logs WHERE user_id = ? AND (? = '*' OR model = ?) AND created_at >= datetime('now', ?)"
    ).bind(user_id).bind(model).bind(model).bind(format!("-{window_secs} seconds")).fetch_one(pool).await
}

#[derive(Serialize, sqlx::FromRow)]
pub struct UsageLimit {
    pub id: i64,
    pub user_id: i64,
    pub model: String,
    pub window_secs: i64,
    pub max_requests: i64,
    pub max_tokens: i64,
}

/// Whether the user may make one more request to `model` under every limit that applies.
pub async fn check_limit(pool: &SqlitePool, user_id: i64, model: &str) -> Result<bool, sqlx::Error> {
    let limits: Vec<UsageLimit> = sqlx::query_as(
        "SELECT * FROM usage_limits WHERE user_id = ? AND (model = ? OR model = '*')"
    ).bind(user_id).bind(model).fetch_all(pool).await?;

    for limit in &limits {
        let usage = window_usage(pool, user_id, &limit.model, limit.window_secs).await?;
        if limit.max_requests >= 0 && usage.requests >= limit.max_requests {
            return Ok(false);
        }
        if limit.max_tokens >= 0 && usage.total_tokens >= limit.max_tokens {
            return Ok(false);
        }
    }
    // No limits configured means unlimited
    Ok(true)
}

#[derive(Serialize, sqlx::FromRow)]
pub struct UsageStats {
    pub model: String,
    pub requests: i64,
    pub total_tokens: i64,
}

pub async fn get_user_usage_today(pool: &SqlitePool, user_id: i64) -> Result<Vec<UsageStats>, sqlx::Error> {
    sqlx::query_as(
        "SELECT model, COUNT(*) as requests, COALESCE(SUM(input_tokens + output_tokens), 0) as total_tokens
         FROM usage_logs WHERE user_id = ? AND date(created_at) = date('now')
         GROUP BY model ORDER BY requests DESC"
    ).bind(user_id).fetch_all(pool).await
}

#[derive(Serialize, sqlx::FromRow)]
pub struct AdminUsageStats {
    pub user_id: i64,
    pub username: String,
    pub model: String,
    pub requests: i64,
    pub total_tokens: i64,
}

/// Today's usage per user and model; `actor` / `task` narrow it to requests carrying that label.
pub async fn get_all_usage_today(pool: &SqlitePool, actor: Option<&str>, task: Option<&str>) -> Result<Vec<AdminUsageStats>, sqlx::Error> {
    sqlx::query_as(
        "SELECT u.id as user_id, u.username, l.model, COUNT(*) as requests, COALESCE(SUM(l.input_tokens + l.output_tokens), 0) as total_tokens
         FROM usage_logs l JOIN users u ON l.user_id = u.id
         WHERE date(l.created_at) = date('now') AND (? IS NULL OR l.actor = ?) AND (? IS NULL OR l.task = ?)
         GROUP BY u.id, l.model ORDER BY requests DESC"
    ).bind(actor).bind(actor).bind(task).bind(task).fetch_all(pool).await
}

/// One request in the usage log, for export.
#[derive(Serialize, sqlx::FromRow, Debug)]
pub struct UsageRow {
    pub id: i64,
    pub created_at: String,
    pub username: String,
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub actor: Option<String>,
    pub task: Option<String>,
    pub run: Option<String>,
}

#[derive(Default)]
pub struct UsageFilter<'a> {
    /// Anything SQLite's datetime() understands (`2026-11-10`, `2026-11-10T08:00:00Z`, …), UTC
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    pub username: Option<&'a str>,
    pub actor: Option<&'a str>,
    pub task: Option<&'a str>,
    pub run: Option<&'a str>,
    pub limit: i64,
}

/// Whether SQLite can read `s` as a date and time.
pub async fn valid_datetime(pool: &SqlitePool, s: &str) -> Result<bool, sqlx::Error> {
    let (v,): (Option<String>,) = sqlx::query_as("SELECT datetime(?)").bind(s).fetch_one(pool).await?;
    Ok(v.is_some())
}

/// Individual requests matching `f`, oldest first.
pub async fn list_usage(pool: &SqlitePool, f: &UsageFilter<'_>) -> Result<Vec<UsageRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT l.id, CAST(l.created_at AS TEXT) as created_at, COALESCE(u.username, '') as username, l.model,
                COALESCE(l.input_tokens, 0) as input_tokens, COALESCE(l.output_tokens, 0) as output_tokens, l.actor, l.task, l.run
         FROM usage_logs l LEFT JOIN users u ON l.user_id = u.id
         WHERE (?1 IS NULL OR l.created_at >= datetime(?1)) AND (?2 IS NULL OR l.created_at < datetime(?2))
           AND (?3 IS NULL OR u.username = ?3) AND (?4 IS NULL OR l.actor = ?4)
           AND (?5 IS NULL OR l.task = ?5) AND (?6 IS NULL OR l.run = ?6)
         ORDER BY l.id LIMIT ?7"
    ).bind(f.since).bind(f.until).bind(f.username).bind(f.actor).bind(f.task).bind(f.run).bind(f.limit)
        .fetch_all(pool).await
}

// ── Usage Limits CRUD ──

pub async fn list_limits(pool: &SqlitePool) -> Result<Vec<UsageLimit>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM usage_limits ORDER BY user_id, model, window_secs").fetch_all(pool).await
}

pub async fn create_limit(pool: &SqlitePool, user_id: i64, model: &str, window_secs: i64, max_requests: i64, max_tokens: i64) -> Result<UsageLimit, sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO usage_limits (user_id, model, window_secs, max_requests, max_tokens) VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(user_id, model, window_secs) DO UPDATE SET max_requests=excluded.max_requests, max_tokens=excluded.max_tokens
         RETURNING *"
    ).bind(user_id).bind(model).bind(window_secs).bind(max_requests).bind(max_tokens).fetch_one(pool).await
}

pub async fn delete_limit(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM usage_limits WHERE id = ?").bind(id).execute(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::Trace;

    async fn memory_db() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
        migrate(&pool).await;
        // Running it again on a migrated database is harmless
        migrate(&pool).await;
        pool
    }

    #[tokio::test]
    async fn provider_concurrency_round_trips() {
        let pool = memory_db().await;
        let mut input = ProviderInput { name: "kimi", base_url: "https://example.invalid/v1", anthropic_base_url: "", user_agent: "ua", api_key: None, models: "", enabled: true, max_concurrency: None };
        let p = create_provider(&pool, &input).await.unwrap();
        assert_eq!(p.max_concurrency, None);
        input.max_concurrency = Some(8);
        assert_eq!(update_provider(&pool, p.id, &input).await.unwrap().max_concurrency, Some(8));
    }

    #[tokio::test]
    async fn usage_carries_trace_and_filters() {
        let pool = memory_db().await;
        let user = insert_user(&pool, "lane-m07-a", "", "x", "k1", false).await.unwrap();
        let t1 = Trace { actor: Some("lane-m07-a".into()), task: Some("M07".into()), run: Some("r1".into()) };
        let a = start_usage(&pool, user.id, "kimi/k3", &t1).await.unwrap();
        finish_usage(&pool, a, 10, 2).await.unwrap();
        start_usage(&pool, user.id, "kimi/k3", &Trace::default()).await.unwrap();

        let all = list_usage(&pool, &UsageFilter { limit: 100, ..Default::default() }).await.unwrap();
        assert_eq!(all.len(), 2);
        let by_task = list_usage(&pool, &UsageFilter { task: Some("M07"), limit: 100, ..Default::default() }).await.unwrap();
        assert_eq!(by_task.len(), 1);
        assert_eq!((by_task[0].actor.as_deref(), by_task[0].run.as_deref(), by_task[0].input_tokens), (Some("lane-m07-a"), Some("r1"), 10));
        assert_eq!(by_task[0].username, "lane-m07-a");
        let future = list_usage(&pool, &UsageFilter { since: Some("2999-01-01"), limit: 100, ..Default::default() }).await.unwrap();
        assert!(future.is_empty());
        assert!(valid_datetime(&pool, "2026-11-10T08:00:00Z").await.unwrap());
        assert!(!valid_datetime(&pool, "yesterday").await.unwrap());

        let today = get_all_usage_today(&pool, Some("lane-m07-a"), None).await.unwrap();
        assert_eq!(today[0].requests, 1);
        assert_eq!(get_all_usage_today(&pool, None, None).await.unwrap()[0].requests, 2);
    }
}
