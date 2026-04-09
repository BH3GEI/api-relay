use serde::Serialize;
use sqlx::SqlitePool;

#[derive(Serialize, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub username: String,
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

#[derive(sqlx::FromRow)]
pub struct ModelWithProvider {
    pub upstream_model: String,
    pub upstream_path: String,
    pub base_url: String,
    pub user_agent: String,
    pub provider_api_key: Option<String>,
}

pub async fn migrate(pool: &SqlitePool) {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            username TEXT UNIQUE NOT NULL,
            password_hash TEXT NOT NULL,
            api_key TEXT UNIQUE NOT NULL,
            is_admin BOOLEAN DEFAULT FALSE,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        )"
    ).execute(pool).await.unwrap();

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

pub async fn find_user_by_api_key(pool: &SqlitePool, api_key: &str) -> Result<User, sqlx::Error> {
    sqlx::query_as("SELECT * FROM users WHERE api_key = ?").bind(api_key).fetch_one(pool).await
}

pub async fn insert_user(pool: &SqlitePool, username: &str, password_hash: &str, api_key: &str, is_admin: bool) -> Result<User, sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO users (username, password_hash, api_key, is_admin) VALUES (?, ?, ?, ?) RETURNING *"
    ).bind(username).bind(password_hash).bind(api_key).bind(is_admin).fetch_one(pool).await
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
        "SELECT m.upstream_model, m.upstream_path, p.base_url, p.user_agent, p.api_key as provider_api_key
         FROM models m JOIN providers p ON m.provider_id = p.id
         WHERE m.public_name = ? AND p.enabled = TRUE"
    ).bind(public_name).fetch_one(pool).await
}
