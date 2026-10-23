use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Upstream statuses that mean "this key can't serve right now, try the next one"
const RETRY_STATUSES: &[u16] = &[401, 402, 403, 429, 502, 503, 504];

/// Tries a provider's keys in priority order, skipping keys that recently failed.
///
/// Keys are sticky rather than round-robin: upstream prompt caches are per account,
/// so staying on one key until it fails keeps an agent's cache warm.
#[derive(Default)]
pub struct KeyPool {
    cooldowns: Mutex<HashMap<String, Instant>>,
    stats: Mutex<HashMap<String, KeyStatus>>,
}

/// Every key was tried and none could serve the request.
#[derive(Debug)]
pub struct Exhausted {
    /// Status of the last upstream answer, None when there were no keys or no answer at all
    pub last_status: Option<u16>,
}

/// What the pool has seen of one key since the relay started.
#[derive(Default, Clone, Serialize)]
pub struct KeyStatus {
    pub ok: u64,
    pub failed: u64,
    /// Upstream status code of the last attempt, or "network error"
    pub last_result: Option<String>,
    /// Unix seconds of the last attempt
    pub last_used: Option<u64>,
    /// Seconds left before the key is tried first again; 0 when ready
    pub cooling_secs: u64,
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

    fn record(&self, key: &str, result: String, ok: bool) {
        let mut stats = self.stats.lock().unwrap();
        let s = stats.entry(key.to_string()).or_default();
        if ok { s.ok += 1 } else { s.failed += 1 }
        s.last_result = Some(result);
        s.last_used = SystemTime::now().duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
    }

    pub fn status(&self, key: &str) -> KeyStatus {
        let mut s = self.stats.lock().unwrap().get(key).cloned().unwrap_or_default();
        s.cooling_secs = self.cooldowns.lock().unwrap().get(key)
            .map_or(0, |until| until.saturating_duration_since(Instant::now()).as_secs());
        s
    }

    /// Send with each key in turn until one gets a response worth returning.
    /// `build` makes the request for a given key; it is called once per attempt.
    pub async fn send<F>(&self, keys: Vec<String>, build: F) -> Result<reqwest::Response, Exhausted>
    where
        F: Fn(&str) -> reqwest::RequestBuilder,
    {
        let keys = self.order(keys);
        let mut last_status = None;
        for key in &keys {
            let resp = match build(key).send().await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("key …{}: request failed ({e:?}), cooling 10s", tail(key));
                    self.record(key, "network error".into(), false);
                    self.cool(key, Duration::from_secs(10));
                    continue;
                }
            };
            let status = resp.status().as_u16();
            if !RETRY_STATUSES.contains(&status) {
                // Anything we don't retry (including a 400 for a bad request) means the key works
                self.record(key, status.to_string(), true);
                return Ok(resp);
            }
            self.record(key, status.to_string(), false);
            let dur = cooldown_for(&resp);
            eprintln!("key …{}: upstream {status}, cooling {}s", tail(key), dur.as_secs());
            self.cool(key, dur);
            // The body isn't passed on: upstream errors can name the account behind the key
            last_status = Some(status);
        }

        Err(Exhausted { last_status })
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
