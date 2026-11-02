use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Upstream statuses that mean "this key can't serve right now, try the next one"
const RETRY_STATUSES: &[u16] = &[401, 402, 403, 429, 502, 503, 504];

/// Tries a provider's keys in priority order, putting keys that recently failed last.
///
/// Keys are sticky rather than round-robin: upstream prompt caches are per account,
/// so staying on one key until it fails keeps an agent's cache warm.
#[derive(Default)]
pub struct KeyPool {
    /// Until when each failed key is held back, and whether it's set aside entirely meanwhile
    cooldowns: Mutex<HashMap<String, (Instant, bool)>>,
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

/// How long a busy provider is waited for before the request gives up
const BUSY_WAIT: Duration = Duration::from_secs(60);

impl KeyPool {
    /// Keys in try order: ready keys in priority order, then cooling keys, soonest ready first.
    /// Keys set aside (spent or refused) are left out while they cool, unless nothing else is left.
    fn order(&self, keys: Vec<String>) -> Vec<String> {
        let now = Instant::now();
        let cooldowns = self.cooldowns.lock().unwrap();
        let held = |k: &String| cooldowns.get(k).filter(|(until, _)| *until > now).copied();
        let (ready, mut cooling): (Vec<String>, Vec<String>) = keys.into_iter().partition(|k| held(k).is_none());
        cooling.sort_by_key(|k| held(k).map(|(until, _)| until));
        let (aside, cooling): (Vec<String>, Vec<String>) = cooling.into_iter().partition(|k| held(k).is_some_and(|(_, aside)| aside));
        if ready.is_empty() && cooling.is_empty() {
            return aside;
        }
        ready.into_iter().chain(cooling).collect()
    }

    fn cool(&self, key: &str, dur: Duration, aside: bool) {
        self.cooldowns.lock().unwrap().insert(key.to_string(), (Instant::now() + dur, aside));
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
            .map_or(0, |(until, _)| until.saturating_duration_since(Instant::now()).as_secs());
        s
    }

    /// Send with each key in turn until one gets a response worth returning.
    /// `build` makes the request for a given key; it is called once per attempt.
    ///
    /// Keys that were only busy (at their concurrency limit) are tried again for a while
    /// before giving up, so a burst waits its turn instead of failing.
    pub async fn send<F>(&self, keys: Vec<String>, build: F) -> Result<reqwest::Response, Exhausted>
    where
        F: Fn(&str) -> reqwest::RequestBuilder,
    {
        let started = Instant::now();
        let mut keys = self.order(keys);
        let mut last_status = None;
        for round in 1u32.. {
            let mut busy = Vec::new();
            for key in &keys {
                let resp = match build(key).send().await {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("key …{}: request failed ({e:?}), cooling 10s", tail(key));
                        self.record(key, "network error".into(), false);
                        self.cool(key, Duration::from_secs(10), false);
                        continue;
                    }
                };
                let status = resp.status().as_u16();
                if !RETRY_STATUSES.contains(&status) {
                    // Anything we don't retry (including a 400 for a bad request) means the key works
                    self.record(key, status.to_string(), true);
                    self.cooldowns.lock().unwrap().remove(key);
                    return Ok(resp);
                }
                self.record(key, status.to_string(), false);
                let retry_after = retry_after(&resp);
                // The body isn't passed on: upstream errors can name the account behind the key
                let failure = Failure::of(status, &resp.text().await.unwrap_or_default());
                let dur = failure.cooldown(retry_after);
                eprintln!("key …{}: upstream {status} ({failure:?}), cooling {}s", tail(key), dur.as_secs());
                self.cool(key, dur, matches!(failure, Failure::OutOfQuota | Failure::Refused));
                if failure == Failure::Busy {
                    busy.push(key.clone());
                }
                last_status = Some(status);
            }
            let pause = Duration::from_secs(round.min(5) as u64);
            if busy.is_empty() || started.elapsed() + pause > BUSY_WAIT {
                break;
            }
            tokio::time::sleep(pause).await;
            keys = busy;
        }

        Err(Exhausted { last_status })
    }
}

/// Why a key couldn't serve a request, read from the upstream's answer.
#[derive(Debug, PartialEq)]
enum Failure {
    /// At its concurrency limit: free again in moments (Kimi says so with a 403)
    Busy,
    /// Its plan's quota is used up until the window resets
    OutOfQuota,
    /// Rejected: a bad, expired or suspended key
    Refused,
    /// Rate limited, or the upstream is struggling
    Other,
}

impl Failure {
    fn of(status: u16, body: &str) -> Failure {
        let body = body.to_lowercase();
        let says = |words: &[&str]| words.iter().any(|w| body.contains(w));
        if says(&["concurren", "并发"]) {
            Failure::Busy
        } else if status == 429 && says(&["quota", "usage limit", "使用上限", "额度", "余额"]) {
            Failure::OutOfQuota
        } else if (401..=403).contains(&status) {
            Failure::Refused
        } else {
            Failure::Other
        }
    }

    fn cooldown(&self, retry_after: Option<u64>) -> Duration {
        Duration::from_secs(match self {
            Failure::Busy => 3,
            // Checked again every half hour, so a key comes back soon after its window resets
            Failure::OutOfQuota => retry_after.unwrap_or(1800).clamp(60, 1800),
            Failure::Refused => 600,
            Failure::Other => retry_after.unwrap_or(30).clamp(1, 600),
        })
    }
}

fn retry_after(resp: &reqwest::Response) -> Option<u64> {
    resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|s| s.parse().ok())
}

/// The last few characters of a key, enough to tell keys apart in logs and on the admin page.
pub fn tail(key: &str) -> &str {
    key.char_indices().rev().nth(3).map_or(key, |(i, _)| &key[i..])
}
