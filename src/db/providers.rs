//! Upstream providers and their pools of keys.

use serde::{Deserialize, Serialize};

use super::Db;

#[derive(Clone, Serialize, sqlx::FromRow)]
pub struct Provider {
    pub id: i64,
    pub name: String,
    /// The OpenAI-style base, usually ending in /v1
    pub base_url: String,
    /// Where the Anthropic-style endpoints live; empty means the base without its /v1
    pub anthropic_base_url: String,
    pub user_agent: String,
    pub enabled: bool,
    /// Model ids to offer in addition to what the upstream lists (some upstreams have no /models)
    pub models: String,
}

const COLUMNS: &str = "id, name, base_url, anthropic_base_url, user_agent, enabled, models";

impl Provider {
    pub fn anthropic_base(&self) -> &str {
        let explicit = self.anthropic_base_url.trim_end_matches('/');
        if !explicit.is_empty() {
            return explicit;
        }
        let base = self.base_url.trim_end_matches('/');
        base.strip_suffix("/v1").unwrap_or(base)
    }

    pub fn host(&self) -> Option<String> {
        reqwest::Url::parse(&self.base_url).ok()?.host_str().map(str::to_string)
    }
}

/// A provider as the admin page sends it.
#[derive(Deserialize)]
pub struct ProviderInput {
    pub name: String,
    pub base_url: String,
    #[serde(default)]
    pub anthropic_base_url: String,
    pub user_agent: String,
    #[serde(default)]
    pub models: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

pub async fn list(db: &Db) -> sqlx::Result<Vec<Provider>> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM providers ORDER BY id")).fetch_all(db).await
}

pub async fn list_enabled(db: &Db) -> sqlx::Result<Vec<Provider>> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM providers WHERE enabled = TRUE ORDER BY id")).fetch_all(db).await
}

pub async fn get(db: &Db, id: i64) -> sqlx::Result<Option<Provider>> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM providers WHERE id = ?")).bind(id).fetch_optional(db).await
}

pub async fn get_enabled(db: &Db, id: i64) -> sqlx::Result<Option<Provider>> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM providers WHERE id = ? AND enabled = TRUE")).bind(id).fetch_optional(db).await
}

pub async fn create(db: &Db, p: &ProviderInput) -> sqlx::Result<Provider> {
    sqlx::query_as(&format!(
        "INSERT INTO providers (name, base_url, anthropic_base_url, user_agent, models, enabled) VALUES (?, ?, ?, ?, ?, ?) RETURNING {COLUMNS}"
    )).bind(&p.name).bind(&p.base_url).bind(&p.anthropic_base_url).bind(&p.user_agent).bind(&p.models).bind(p.enabled).fetch_one(db).await
}

pub async fn update(db: &Db, id: i64, p: &ProviderInput) -> sqlx::Result<Option<Provider>> {
    sqlx::query_as(&format!(
        "UPDATE providers SET name = ?, base_url = ?, anthropic_base_url = ?, user_agent = ?, models = ?, enabled = ? WHERE id = ? RETURNING {COLUMNS}"
    )).bind(&p.name).bind(&p.base_url).bind(&p.anthropic_base_url).bind(&p.user_agent).bind(&p.models).bind(p.enabled).bind(id).fetch_optional(db).await
}

pub async fn delete(db: &Db, id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM providers WHERE id = ?").bind(id).execute(db).await?;
    Ok(())
}

// ── Keys ──

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

const KEY_COLUMNS: &str = "id, provider_id, api_key, label, enabled, priority";

fn serialize_key_tail<S: serde::Serializer>(key: &str, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format!("…{}", crate::pool::tail(key)))
}

/// All of a provider's keys, in the order they are tried.
pub async fn keys(db: &Db, provider_id: i64) -> sqlx::Result<Vec<ProviderKey>> {
    sqlx::query_as(&format!("SELECT {KEY_COLUMNS} FROM provider_keys WHERE provider_id = ? ORDER BY priority, id"))
        .bind(provider_id).fetch_all(db).await
}

/// The keys requests may use, in the order they are tried.
pub async fn usable_keys(db: &Db, provider_id: i64) -> sqlx::Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT api_key FROM provider_keys WHERE provider_id = ? AND enabled = TRUE ORDER BY priority, id")
        .bind(provider_id).fetch_all(db).await?;
    Ok(rows.into_iter().map(|(k,)| k).collect())
}

pub async fn add_key(db: &Db, provider_id: i64, api_key: &str, label: &str, priority: i64) -> sqlx::Result<ProviderKey> {
    sqlx::query_as(&format!("INSERT INTO provider_keys (provider_id, api_key, label, priority) VALUES (?, ?, ?, ?) RETURNING {KEY_COLUMNS}"))
        .bind(provider_id).bind(api_key).bind(label).bind(priority).fetch_one(db).await
}

pub async fn delete_key(db: &Db, id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM provider_keys WHERE id = ?").bind(id).execute(db).await?;
    Ok(())
}
