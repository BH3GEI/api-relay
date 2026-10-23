use axum::http::StatusCode;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// Upstream statuses that mean "this key can't serve right now, try the next one"
const RETRY_STATUSES: &[u16] = &[401, 402, 403, 429, 502, 503, 504];

/// Tries a provider's keys in priority order, skipping keys that recently failed.
///
/// Keys are sticky rather than round-robin: upstream prompt caches are per account,
/// so staying on one key until it fails keeps an agent's cache warm.
#[derive(Default)]
pub struct KeyPool {
    cooldowns: Mutex<HashMap<String, Instant>>,
}

impl KeyPool {
    /// Keys in try order: ready keys first, keys still cooling down last.
    fn order(&self, keys: Vec<String>) -> Vec<String> {
        let now = Instant::now();
        let cooldowns = self.cooldowns.lock().unwrap();
        let (ready, cooling): (Vec<String>, Vec<String>) = keys.into_iter()
            .partition(|k| cooldowns.get(k).map_or(true, |until| *until <= now));
        ready.into_iter().chain(cooling).collect()
    }

    fn cool(&self, key: &str, dur: Duration) {
        self.cooldowns.lock().unwrap().insert(key.to_string(), Instant::now() + dur);
    }

    /// Send with each key in turn until one gets a response worth returning.
    /// `build` makes the request for a given key; it is called once per attempt.
    pub async fn send<F>(&self, keys: Vec<String>, build: F) -> Result<reqwest::Response, StatusCode>
    where
        F: Fn(&str) -> reqwest::RequestBuilder,
    {
        let keys = self.order(keys);
        if keys.is_empty() {
            return Err(StatusCode::BAD_REQUEST);
        }

        let mut last_resp = None;
        for key in &keys {
            let resp = match build(key).send().await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("key …{}: request failed ({e:?}), cooling 10s", tail(key));
                    self.cool(key, Duration::from_secs(10));
                    continue;
                }
            };
            let status = resp.status().as_u16();
            if !RETRY_STATUSES.contains(&status) {
                return Ok(resp);
            }
            let dur = cooldown_for(&resp);
            eprintln!("key …{}: upstream {status}, cooling {}s", tail(key), dur.as_secs());
            self.cool(key, dur);
            last_resp = Some(resp);
        }

        last_resp.ok_or(StatusCode::BAD_GATEWAY)
    }
}

// Last few characters of a key, enough to tell keys apart in logs
fn tail(key: &str) -> &str {
    key.char_indices().rev().nth(3).map_or(key, |(i, _)| &key[i..])
}

fn cooldown_for(resp: &reqwest::Response) -> Duration {
    match resp.status().as_u16() {
        429 => {
            let retry_after = resp.headers().get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            Duration::from_secs(retry_after.unwrap_or(60).clamp(1, 600))
        }
        401 | 402 | 403 => Duration::from_secs(600),
        _ => Duration::from_secs(10),
    }
}
