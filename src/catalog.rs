use serde_json::Value;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use crate::{AppState, db::{self, providers::Provider}};

const REFRESH_EVERY: Duration = Duration::from_secs(600);
// A request for an unknown model refreshes the catalog, but not more often than this
const MISS_REFRESH_AFTER: Duration = Duration::from_secs(30);

/// One model a client can ask for, published as `<provider>/<upstream id>`.
#[derive(Clone)]
pub struct Entry {
    pub id: String,
    pub provider_id: i64,
    pub upstream_model: String,
    /// Whatever the upstream said about the model (context length, modalities, …)
    pub meta: Value,
}

/// Models offered by all enabled providers, synced from the upstreams.
pub struct Catalog {
    entries: RwLock<Vec<Entry>>,
    refreshed_at: RwLock<Option<Instant>>,
    refreshing: tokio::sync::Mutex<()>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
            refreshed_at: RwLock::new(None),
            refreshing: tokio::sync::Mutex::new(()),
        }
    }
}

impl Catalog {
    pub fn entries(&self) -> Vec<Entry> {
        self.entries.read().unwrap().clone()
    }

    pub fn ids(&self) -> Vec<String> {
        self.entries.read().unwrap().iter().map(|e| e.id.clone()).collect()
    }

    pub fn has(&self, id: &str) -> bool {
        self.entries.read().unwrap().iter().any(|e| e.id == id)
    }

    fn find(&self, name: &str) -> Option<Entry> {
        let entries = self.entries.read().unwrap();
        if let Some(e) = entries.iter().find(|e| e.id == name) {
            return Some(e.clone());
        }
        // A bare upstream id works when exactly one provider offers it
        let mut bare = entries.iter().filter(|e| e.upstream_model == name);
        match (bare.next(), bare.next()) {
            (Some(e), None) => Some(e.clone()),
            _ => None,
        }
    }

    /// The catalog entry and its provider for a requested model name.
    pub async fn resolve(&self, state: &AppState, name: &str) -> Option<(Provider, Entry)> {
        let entry = match self.find(name) {
            Some(e) => e,
            None => {
                if !self.stale() {
                    return None;
                }
                let _guard = self.refreshing.lock().await;
                // Requests that waited for someone else's refresh shouldn't each start another
                if self.stale() {
                    self.rebuild(state).await;
                }
                drop(_guard);
                self.find(name)?
            }
        };
        // Read the provider fresh so admin edits apply to the very next request
        let provider = db::providers::get_enabled(&state.db, entry.provider_id).await.ok()??;
        Some((provider, entry))
    }

    fn stale(&self) -> bool {
        self.refreshed_at.read().unwrap().is_none_or(|t| t.elapsed() > MISS_REFRESH_AFTER)
    }

    /// Rebuild the catalog from every enabled provider's model list.
    /// A provider whose list can't be fetched keeps the models it had before.
    pub async fn refresh(&self, state: &AppState) {
        let _guard = self.refreshing.lock().await;
        self.rebuild(state).await;
    }

    async fn rebuild(&self, state: &AppState) {
        let Ok(providers) = db::providers::list_enabled(&state.db).await else { return };

        let previous: HashMap<i64, Vec<Entry>> = self.entries().into_iter().fold(HashMap::new(), |mut m, e| {
            m.entry(e.provider_id).or_default().push(e);
            m
        });

        let mut entries = Vec::new();
        for p in &providers {
            let mut models: Vec<(String, Value)> = match fetch_models(state, p).await {
                Some(listed) => listed,
                None => previous.get(&p.id).map(|old| {
                    old.iter().map(|e| (e.upstream_model.clone(), e.meta.clone())).collect()
                }).unwrap_or_default(),
            };
            for id in p.models.split(|c: char| c == ',' || c.is_whitespace()).filter(|s| valid_model_id(s)) {
                if !models.iter().any(|(m, _)| m == id) {
                    models.push((id.to_string(), Value::Object(Default::default())));
                }
            }
            entries.extend(models.into_iter().map(|(upstream_model, meta)| Entry {
                id: format!("{}/{}", p.name, upstream_model),
                provider_id: p.id,
                upstream_model,
                meta,
            }));
        }

        *self.entries.write().unwrap() = entries;
        *self.refreshed_at.write().unwrap() = Some(Instant::now());
    }
}

/// `GET {base_url}/models` from the upstream, as (id, metadata) pairs.
async fn fetch_models(state: &AppState, p: &Provider) -> Option<Vec<(String, Value)>> {
    let keys = db::providers::usable_keys(&state.db, p.id).await.ok()?;
    let url = format!("{}/models", p.base_url.trim_end_matches('/'));

    let resp = state.keys.send(keys, |key| {
        state.http_client
            .get(&url)
            .timeout(Duration::from_secs(20))
            .header("User-Agent", &p.user_agent)
            .header("x-app", "cli")
            .header("Authorization", format!("Bearer {key}"))
    }).await.ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let body: Value = resp.json().await.ok()?;
    let listed = body["data"].as_array()?.iter()
        .filter_map(|m| Some((m["id"].as_str()?.to_string(), m.clone())))
        // Ids end up in URLs and pages; an upstream shouldn't be able to slip markup into them
        .filter(|(id, _)| valid_model_id(id))
        .collect();
    Some(listed)
}

fn valid_model_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128
        && id.chars().all(|c| c.is_ascii_alphanumeric() || "._:/@+-[]".contains(c))
}

/// Keep the catalog fresh in the background.
pub fn spawn_refresher(state: std::sync::Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            state.catalog.refresh(&state).await;
            tokio::time::sleep(REFRESH_EVERY).await;
        }
    });
}

/// Refresh soon after an admin change, without making the admin request wait.
pub fn refresh_in_background(state: &std::sync::Arc<AppState>) {
    let state = state.clone();
    tokio::spawn(async move { state.catalog.refresh(&state).await });
}
