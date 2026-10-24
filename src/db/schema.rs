//! The schema, brought up to date at start. `PRAGMA user_version` records how far a database
//! has come; each step runs once, in order.

use super::Db;

/// Steps after the first, each moving the schema from version N to N + 1.
const STEPS: &[&[&str]] = &[];

pub async fn migrate(db: &Db) {
    let (version,): (i64,) = sqlx::query_as("PRAGMA user_version").fetch_one(db).await.unwrap();
    if version < 1 {
        baseline(db).await;
        set_version(db, 1).await;
    }
    for (i, step) in STEPS.iter().enumerate() {
        let to = i as i64 + 2;
        if version >= to {
            continue;
        }
        let mut tx = db.begin().await.unwrap();
        for stmt in *step {
            sqlx::query(stmt).execute(&mut *tx).await.unwrap();
        }
        tx.commit().await.unwrap();
        set_version(db, to).await;
    }
}

async fn set_version(db: &Db, version: i64) {
    // PRAGMA takes no bound parameters
    sqlx::query(&format!("PRAGMA user_version = {version}")).execute(db).await.unwrap();
}

/// Version 1: the schema as it stood before versioning. Databases from then may be at any
/// earlier stage, so every change of that time is applied if it is missing.
async fn baseline(db: &Db) {
    let run = |sql: &'static str| async move { sqlx::query(sql).execute(db).await.unwrap() };
    let has_column = |table: &'static str, column: &'static str| async move {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM pragma_table_info(?) WHERE name = ?")
            .bind(table).bind(column).fetch_one(db).await.unwrap();
        n > 0
    };

    run("CREATE TABLE IF NOT EXISTS users (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        username TEXT UNIQUE NOT NULL,
        email TEXT NOT NULL DEFAULT '',
        password_hash TEXT NOT NULL,
        api_key TEXT UNIQUE NOT NULL,
        is_admin BOOLEAN DEFAULT FALSE,
        created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
    )").await;
    if !has_column("users", "email").await {
        run("ALTER TABLE users ADD COLUMN email TEXT NOT NULL DEFAULT ''").await;
    }

    // `api_key` is a leftover from before key pools; it is no longer read
    run("CREATE TABLE IF NOT EXISTS providers (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        name TEXT UNIQUE NOT NULL,
        base_url TEXT NOT NULL,
        user_agent TEXT NOT NULL,
        api_key TEXT,
        enabled BOOLEAN DEFAULT TRUE
    )").await;
    // base_url is the OpenAI-style base (…/v1); Anthropic endpoints often live elsewhere
    if !has_column("providers", "anthropic_base_url").await {
        run("ALTER TABLE providers ADD COLUMN anthropic_base_url TEXT NOT NULL DEFAULT ''").await;
    }
    if !has_column("providers", "models").await {
        run("ALTER TABLE providers ADD COLUMN models TEXT NOT NULL DEFAULT ''").await;
    }

    run("CREATE TABLE IF NOT EXISTS provider_keys (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        provider_id INTEGER NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
        api_key TEXT NOT NULL,
        label TEXT NOT NULL DEFAULT '',
        enabled BOOLEAN DEFAULT TRUE,
        priority INTEGER DEFAULT 0
    )").await;

    run("CREATE TABLE IF NOT EXISTS usage_logs (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        user_id INTEGER NOT NULL,
        model TEXT NOT NULL,
        input_tokens INTEGER DEFAULT 0,
        output_tokens INTEGER DEFAULT 0,
        created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
    )").await;
    run("CREATE INDEX IF NOT EXISTS idx_usage_user_date ON usage_logs(user_id, created_at)").await;

    const LIMITS: &str = "CREATE TABLE IF NOT EXISTS usage_limits (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        user_id INTEGER NOT NULL,
        model TEXT NOT NULL DEFAULT '*',
        window_secs INTEGER NOT NULL DEFAULT 86400,
        max_requests INTEGER DEFAULT -1,
        max_tokens INTEGER DEFAULT -1,
        UNIQUE(user_id, model, window_secs)
    )";
    run(LIMITS).await;
    // Limits used to be per calendar day only
    if !has_column("usage_limits", "window_secs").await {
        let mut tx = db.begin().await.unwrap();
        for stmt in [
            "ALTER TABLE usage_limits RENAME TO usage_limits_old",
            LIMITS,
            "INSERT INTO usage_limits (id, user_id, model, window_secs, max_requests, max_tokens)
                SELECT id, user_id, model, 86400, max_requests_daily, max_tokens_daily FROM usage_limits_old",
            "DROP TABLE usage_limits_old",
        ] {
            sqlx::query(stmt).execute(&mut *tx).await.unwrap();
        }
        tx.commit().await.unwrap();
    }

    run("CREATE TABLE IF NOT EXISTS settings (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    )").await;
}
