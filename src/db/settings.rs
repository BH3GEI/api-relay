//! Single values the admin sets, and secrets the relay makes for itself.

use super::Db;

pub const PUBLIC_URL: &str = "public_url";
pub const TUNNEL_TOKEN: &str = "cloudflare_tunnel_token";
const JWT_SECRET: &str = "jwt_secret";

pub async fn get(db: &Db, key: &str) -> sqlx::Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = ?").bind(key).fetch_optional(db).await?;
    Ok(row.map(|(v,)| v))
}

/// The value, or None when it is unset or empty.
pub async fn get_nonempty(db: &Db, key: &str) -> sqlx::Result<Option<String>> {
    Ok(get(db, key).await?.filter(|v| !v.is_empty()))
}

pub async fn set(db: &Db, key: &str, value: &str) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(key).bind(value).execute(db).await?;
    Ok(())
}

/// The session signing secret, made on first start so sessions survive restarts.
pub async fn jwt_secret(db: &Db) -> sqlx::Result<String> {
    sqlx::query("INSERT OR IGNORE INTO settings (key, value) VALUES (?, ?)")
        .bind(JWT_SECRET)
        .bind(format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple()))
        .execute(db).await?;
    Ok(get(db, JWT_SECRET).await?.unwrap_or_default())
}
