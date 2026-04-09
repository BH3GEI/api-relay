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
    pub api_key: Option<String>,
    pub enabled: bool,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct Model {
    pub id: i64,
    pub public_name: String,
    pub provider_id: i64,
    pub upstream_model: String,
    pub upstream_path: String,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct ProviderKey {
    pub id: i64,
    pub provider_id: i64,
    pub api_key: String,
    pub label: String,
    pub enabled: bool,
    pub priority: i64,
}

#[derive(sqlx::FromRow)]
pub struct ModelWithProvider {
    pub upstream_model: String,
    pub upstream_path: String,
    pub base_url: String,
    pub user_agent: String,
    pub provider_id: i64,
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

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS usage_limits (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL,
            model TEXT NOT NULL DEFAULT '*',
            max_requests_daily INTEGER DEFAULT -1,
            max_tokens_daily INTEGER DEFAULT -1,
            UNIQUE(user_id, model)
        )"
    ).execute(pool).await.unwrap();

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS models (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            public_name TEXT UNIQUE NOT NULL,
            provider_id INTEGER NOT NULL REFERENCES providers(id),
            upstream_model TEXT NOT NULL,
            upstream_path TEXT NOT NULL DEFAULT '/v1/chat/completions'
        )"
    ).execute(pool).await.unwrap();
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

pub async fn create_provider(pool: &SqlitePool, name: &str, base_url: &str, user_agent: &str, api_key: Option<&str>, enabled: bool) -> Result<Provider, sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO providers (name, base_url, user_agent, api_key, enabled) VALUES (?, ?, ?, ?, ?) RETURNING *"
    ).bind(name).bind(base_url).bind(user_agent).bind(api_key).bind(enabled).fetch_one(pool).await
}

pub async fn update_provider(pool: &SqlitePool, id: i64, name: &str, base_url: &str, user_agent: &str, api_key: Option<&str>, enabled: bool) -> Result<Provider, sqlx::Error> {
    sqlx::query_as(
        "UPDATE providers SET name=?, base_url=?, user_agent=?, api_key=?, enabled=? WHERE id=? RETURNING *"
    ).bind(name).bind(base_url).bind(user_agent).bind(api_key).bind(enabled).bind(id).fetch_one(pool).await
}

pub async fn delete_provider(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM providers WHERE id = ?").bind(id).execute(pool).await?;
    Ok(())
}

// ── Models ──

pub async fn list_models(pool: &SqlitePool) -> Result<Vec<Model>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM models ORDER BY id").fetch_all(pool).await
}

pub async fn create_model(pool: &SqlitePool, public_name: &str, provider_id: i64, upstream_model: &str, upstream_path: &str) -> Result<Model, sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO models (public_name, provider_id, upstream_model, upstream_path) VALUES (?, ?, ?, ?) RETURNING *"
    ).bind(public_name).bind(provider_id).bind(upstream_model).bind(upstream_path).fetch_one(pool).await
}

pub async fn update_model(pool: &SqlitePool, id: i64, public_name: &str, provider_id: i64, upstream_model: &str, upstream_path: &str) -> Result<Model, sqlx::Error> {
    sqlx::query_as(
        "UPDATE models SET public_name=?, provider_id=?, upstream_model=?, upstream_path=? WHERE id=? RETURNING *"
    ).bind(public_name).bind(provider_id).bind(upstream_model).bind(upstream_path).bind(id).fetch_one(pool).await
}

pub async fn delete_model(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM models WHERE id = ?").bind(id).execute(pool).await?;
    Ok(())
}

pub async fn resolve_model(pool: &SqlitePool, public_name: &str) -> Result<ModelWithProvider, sqlx::Error> {
    sqlx::query_as(
        "SELECT m.upstream_model, m.upstream_path, p.base_url, p.user_agent, p.id as provider_id
         FROM models m JOIN providers p ON m.provider_id = p.id
         WHERE m.public_name = ? AND p.enabled = TRUE"
    ).bind(public_name).fetch_one(pool).await
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

pub async fn log_usage(pool: &SqlitePool, user_id: i64, model: &str, input_tokens: i64, output_tokens: i64) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO usage_logs (user_id, model, input_tokens, output_tokens) VALUES (?, ?, ?, ?)")
        .bind(user_id).bind(model).bind(input_tokens).bind(output_tokens).execute(pool).await?;
    Ok(())
}

#[derive(Serialize, sqlx::FromRow)]
pub struct DailyUsage {
    pub requests: i64,
    pub total_tokens: i64,
}

pub async fn get_daily_usage(pool: &SqlitePool, user_id: i64, model: &str) -> Result<DailyUsage, sqlx::Error> {
    let row: DailyUsage = if model == "*" {
        sqlx::query_as(
            "SELECT COUNT(*) as requests, COALESCE(SUM(input_tokens + output_tokens), 0) as total_tokens
             FROM usage_logs WHERE user_id = ? AND date(created_at) = date('now')"
        ).bind(user_id).fetch_one(pool).await?
    } else {
        sqlx::query_as(
            "SELECT COUNT(*) as requests, COALESCE(SUM(input_tokens + output_tokens), 0) as total_tokens
             FROM usage_logs WHERE user_id = ? AND model = ? AND date(created_at) = date('now')"
        ).bind(user_id).bind(model).fetch_one(pool).await?
    };
    Ok(row)
}

#[derive(Serialize, sqlx::FromRow)]
pub struct UsageLimit {
    pub id: i64,
    pub user_id: i64,
    pub model: String,
    pub max_requests_daily: i64,
    pub max_tokens_daily: i64,
}

pub async fn check_limit(pool: &SqlitePool, user_id: i64, model: &str) -> Result<bool, sqlx::Error> {
    // Check model-specific limit first, then wildcard
    let limits: Vec<UsageLimit> = sqlx::query_as(
        "SELECT * FROM usage_limits WHERE user_id = ? AND (model = ? OR model = '*') ORDER BY CASE WHEN model = '*' THEN 1 ELSE 0 END"
    ).bind(user_id).bind(model).fetch_all(pool).await?;

    for limit in &limits {
        let usage = get_daily_usage(pool, user_id, &limit.model).await?;
        if limit.max_requests_daily >= 0 && usage.requests >= limit.max_requests_daily {
            return Ok(false); // Over limit
        }
        if limit.max_tokens_daily >= 0 && usage.total_tokens >= limit.max_tokens_daily {
            return Ok(false);
        }
    }
    Ok(true) // Within limits
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

pub async fn get_all_usage_today(pool: &SqlitePool) -> Result<Vec<AdminUsageStats>, sqlx::Error> {
    sqlx::query_as(
        "SELECT u.id as user_id, u.username, l.model, COUNT(*) as requests, COALESCE(SUM(l.input_tokens + l.output_tokens), 0) as total_tokens
         FROM usage_logs l JOIN users u ON l.user_id = u.id
         WHERE date(l.created_at) = date('now')
         GROUP BY u.id, l.model ORDER BY requests DESC"
    ).fetch_all(pool).await
}

// ── Usage Limits CRUD ──

pub async fn list_limits(pool: &SqlitePool) -> Result<Vec<UsageLimit>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM usage_limits ORDER BY user_id, model").fetch_all(pool).await
}

pub async fn create_limit(pool: &SqlitePool, user_id: i64, model: &str, max_requests: i64, max_tokens: i64) -> Result<UsageLimit, sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO usage_limits (user_id, model, max_requests_daily, max_tokens_daily) VALUES (?, ?, ?, ?)
         ON CONFLICT(user_id, model) DO UPDATE SET max_requests_daily=excluded.max_requests_daily, max_tokens_daily=excluded.max_tokens_daily
         RETURNING *"
    ).bind(user_id).bind(model).bind(max_requests).bind(max_tokens).fetch_one(pool).await
}

pub async fn delete_limit(pool: &SqlitePool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM usage_limits WHERE id = ?").bind(id).execute(pool).await?;
    Ok(())
}
