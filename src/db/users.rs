use serde::Serialize;

use super::Db;

#[derive(Clone, Serialize, sqlx::FromRow)]
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

const COLUMNS: &str = "id, username, email, password_hash, api_key, is_admin, created_at";

pub async fn list(db: &Db) -> sqlx::Result<Vec<User>> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM users ORDER BY id")).fetch_all(db).await
}

pub async fn by_username(db: &Db, username: &str) -> sqlx::Result<Option<User>> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM users WHERE username = ?")).bind(username).fetch_optional(db).await
}

pub async fn by_email(db: &Db, email: &str) -> sqlx::Result<Option<User>> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM users WHERE email = ?")).bind(email).fetch_optional(db).await
}

pub async fn by_api_key(db: &Db, api_key: &str) -> sqlx::Result<Option<User>> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM users WHERE api_key = ?")).bind(api_key).fetch_optional(db).await
}

pub async fn count(db: &Db) -> sqlx::Result<i64> {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users").fetch_one(db).await?;
    Ok(n)
}

pub struct NewUser<'a> {
    pub username: &'a str,
    pub email: &'a str,
    pub password_hash: &'a str,
    pub api_key: &'a str,
    pub is_admin: bool,
}

/// None when the username (or key) is taken.
pub async fn insert(db: &Db, u: &NewUser<'_>) -> sqlx::Result<Option<User>> {
    let inserted = sqlx::query_as(&format!(
        "INSERT INTO users (username, email, password_hash, api_key, is_admin) VALUES (?, ?, ?, ?, ?) RETURNING {COLUMNS}"
    )).bind(u.username).bind(u.email).bind(u.password_hash).bind(u.api_key).bind(u.is_admin).fetch_one(db).await;
    match inserted {
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Ok(None),
        other => other.map(Some),
    }
}

pub async fn set_api_key(db: &Db, id: i64, api_key: &str) -> sqlx::Result<()> {
    sqlx::query("UPDATE users SET api_key = ? WHERE id = ?").bind(api_key).bind(id).execute(db).await?;
    Ok(())
}

/// Whether the user exists.
pub async fn set_password_hash(db: &Db, id: i64, password_hash: &str) -> sqlx::Result<bool> {
    let r = sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?").bind(password_hash).bind(id).execute(db).await?;
    Ok(r.rows_affected() > 0)
}

pub async fn set_admin(db: &Db, id: i64, is_admin: bool) -> sqlx::Result<()> {
    sqlx::query("UPDATE users SET is_admin = ? WHERE id = ?").bind(is_admin).bind(id).execute(db).await?;
    Ok(())
}

pub async fn delete(db: &Db, id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM users WHERE id = ?").bind(id).execute(db).await?;
    Ok(())
}
