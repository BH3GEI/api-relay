//! Plan quota (5-hour / weekly windows) for keys whose upstream reports it.

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::{AppState, db::{self, providers::Provider}};

// Admin pages poll often; the upstream numbers don't move that fast
const CACHE_FOR: Duration = Duration::from_secs(60);

#[derive(Clone, Serialize)]
pub struct Window {
    /// "5h", "week", …
    pub label: String,
    pub used_percent: f64,
    /// Unix seconds
    pub resets_at: Option<u64>,
}

/// When a key's quota was fetched, and what came back (None when the upstream doesn't report it).
type Fetched = (Instant, Option<Vec<Window>>);

#[derive(Default)]
pub struct QuotaCache {
    entries: Mutex<HashMap<String, Fetched>>,
}

impl QuotaCache {
    /// The key's quota windows, or None when the upstream doesn't report quota.
    pub async fn get(&self, state: &AppState, provider: &Provider, key: &str) -> Option<Vec<Window>> {
        if let Some((at, cached)) = self.entries.lock().unwrap().get(key) {
            if at.elapsed() < CACHE_FOR {
                return cached.clone();
            }
        }
        let fresh = fetch(state, provider, key).await;
        self.entries.lock().unwrap().insert(key.to_string(), (Instant::now(), fresh.clone()));
        fresh
    }
}

/// One plan window summed over a provider's keys, as users see it.
#[derive(Clone, Serialize, PartialEq, Debug)]
pub struct PoolWindow {
    pub label: String,
    pub remaining_percent: f64,
    /// When the first key's window resets, i.e. when the pool next gains room
    pub resets_at: Option<u64>,
}

#[derive(Serialize)]
pub struct PoolQuota {
    pub provider: String,
    /// The provider's keys taken together
    pub windows: Vec<PoolWindow>,
    pub keys: Vec<KeyQuota>,
}

/// One key as users see it: its label, never the key itself.
#[derive(Serialize)]
pub struct KeyQuota {
    pub label: String,
    pub windows: Vec<PoolWindow>,
}

/// What's left of each enabled provider's plan, per enabled key and in total.
/// Providers whose upstream doesn't report quota are left out.
pub async fn pool(state: &AppState) -> Vec<PoolQuota> {
    let Ok(providers) = db::providers::list_enabled(&state.db).await else { return Vec::new() };
    let per_provider = futures::future::join_all(providers.iter().map(|p| async move {
        let keys = db::providers::keys(&state.db, p.id).await.unwrap_or_default();
        let keys: Vec<_> = keys.into_iter().filter(|k| k.enabled).collect();
        let quotas = futures::future::join_all(keys.iter().map(|k| state.quota.get(state, p, &k.api_key))).await;
        let reported: Vec<(&db::providers::ProviderKey, Vec<Window>)> = keys.iter().zip(quotas)
            .filter_map(|(k, q)| q.map(|q| (k, q))).collect();
        let windows = summarize(&reported.iter().map(|(_, q)| q.clone()).collect::<Vec<_>>());
        let keys = reported.iter().map(|(k, q)| KeyQuota {
            label: if k.label.is_empty() { format!("key {}", k.id) } else { k.label.clone() },
            windows: summarize(std::slice::from_ref(q)),
        }).collect();
        (!windows.is_empty()).then(|| PoolQuota { provider: p.name.clone(), windows, keys })
    })).await;
    per_provider.into_iter().flatten().collect()
}

/// Requests spread over the keys, so the pool has the average key's share left.
fn summarize(keys: &[Vec<Window>]) -> Vec<PoolWindow> {
    let mut labels: Vec<&str> = Vec::new();
    for w in keys.iter().flatten() {
        if !labels.contains(&w.label.as_str()) {
            labels.push(&w.label);
        }
    }
    labels.into_iter().map(|label| {
        let windows: Vec<&Window> = keys.iter().flatten().filter(|w| w.label == label).collect();
        let remaining = windows.iter().map(|w| (100.0 - w.used_percent).clamp(0.0, 100.0)).sum::<f64>() / windows.len() as f64;
        PoolWindow {
            label: label.to_string(),
            remaining_percent: (remaining * 10.0).round() / 10.0,
            resets_at: windows.iter().filter_map(|w| w.resets_at).min(),
        }
    }).collect()
}

async fn fetch(state: &AppState, provider: &Provider, key: &str) -> Option<Vec<Window>> {
    let base = provider.base_url.trim_end_matches('/');
    let host = reqwest::Url::parse(base).ok()?.host_str()?.to_string();
    let url = if host == "api.kimi.com" {
        format!("{base}/usages")
    } else if host.ends_with("z.ai") || host.ends_with("bigmodel.cn") {
        format!("https://{host}/api/monitor/usage/quota/limit")
    } else {
        return None;
    };

    let resp = state.http_client.get(&url)
        .timeout(Duration::from_secs(10))
        .header("Authorization", format!("Bearer {key}"))
        .header("User-Agent", &provider.user_agent)
        .send().await
        .inspect_err(|e| eprintln!("quota {url}: {e:?}")).ok()?;
    if !resp.status().is_success() {
        eprintln!("quota {url}: upstream {}", resp.status());
        return None;
    }
    let body: Value = resp.json().await.inspect_err(|e| eprintln!("quota {url}: bad body ({e})")).ok()?;
    if host == "api.kimi.com" { parse_kimi(&body) } else { parse_zhipu(&body) }
}

/// Kimi: `usage` is the weekly window; `limits` holds shorter rolling windows.
fn parse_kimi(body: &Value) -> Option<Vec<Window>> {
    let window = |label: String, d: &Value| -> Option<Window> {
        let num = |f: &str| d[f].as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| d[f].as_f64());
        let limit = num("limit")?;
        // An untouched window omits `used`
        let used = num("used").or_else(|| num("remaining").map(|r| limit - r)).unwrap_or(0.0);
        Some(Window {
            label,
            used_percent: if limit > 0.0 { used * 100.0 / limit } else { 0.0 },
            resets_at: d["resetTime"].as_str().and_then(parse_rfc3339),
        })
    };
    let mut out: Vec<Window> = body["limits"].as_array().into_iter().flatten().filter_map(|l| {
        let minutes = l["window"]["duration"].as_f64()?
            * match l["window"]["timeUnit"].as_str()? {
                "TIME_UNIT_MINUTE" => 1.0,
                "TIME_UNIT_HOUR" => 60.0,
                "TIME_UNIT_DAY" => 1440.0,
                _ => return None,
            };
        window(duration_label(minutes), &l["detail"])
    }).collect();
    out.extend(window("week".into(), &body["usage"]));
    (!out.is_empty()).then_some(out)
}

/// Zhipu: TOKENS_LIMIT is the rolling token window (unit 3 = hours); TIME_LIMIT counts tool calls.
fn parse_zhipu(body: &Value) -> Option<Vec<Window>> {
    let out: Vec<Window> = body["data"]["limits"].as_array()?.iter().filter_map(|l| {
        let label = match (l["type"].as_str()?, l["unit"].as_i64(), l["number"].as_i64()) {
            ("TOKENS_LIMIT", Some(3), Some(n)) => format!("{n}h"),
            ("TOKENS_LIMIT", _, _) => "tokens".into(),
            ("TIME_LIMIT", _, _) => "tools".into(),
            _ => return None,
        };
        Some(Window {
            label,
            used_percent: l["percentage"].as_f64()?,
            resets_at: l["nextResetTime"].as_u64().map(|ms| ms / 1000),
        })
    }).collect();
    (!out.is_empty()).then_some(out)
}

fn duration_label(minutes: f64) -> String {
    if minutes >= 1440.0 && minutes % 1440.0 == 0.0 {
        format!("{}d", minutes / 1440.0)
    } else if minutes >= 60.0 && minutes % 60.0 == 0.0 {
        format!("{}h", minutes / 60.0)
    } else {
        format!("{minutes}m")
    }
}

/// Seconds since the epoch for an RFC 3339 UTC timestamp like 2026-10-26T02:22:41.26Z.
fn parse_rfc3339(s: &str) -> Option<u64> {
    let date = s.get(0..10)?;
    let time = s.get(11..19)?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.split(':').map(|p| p.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    // Days from civil date (Howard Hinnant's algorithm)
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    u64::try_from(days * 86400 + hh * 3600 + mm * 60 + ss).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kimi_usages() {
        let body: Value = serde_json::from_str(r#"{"usage":{"limit":"100","used":"29","remaining":"71","resetTime":"2026-10-26T02:22:41.264494Z"},
            "limits":[{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},"detail":{"limit":"100","used":"41","remaining":"59","resetTime":"2026-10-23T22:22:41.264494Z"}}]}"#).unwrap();
        let w = parse_kimi(&body).unwrap();
        assert_eq!((w[0].label.as_str(), w[0].used_percent), ("5h", 41.0));
        assert_eq!((w[1].label.as_str(), w[1].used_percent), ("week", 29.0));
        assert_eq!(w[1].resets_at, Some(1792981361));
    }

    #[test]
    fn parses_unused_kimi_key() {
        let body: Value = serde_json::from_str(r#"{"usage":{"limit":"100","remaining":"100","resetTime":"2026-10-28T17:54:01.755670Z"},
            "limits":[{"window":{"duration":300,"timeUnit":"TIME_UNIT_MINUTE"},"detail":{"limit":"100","remaining":"100"}}]}"#).unwrap();
        let w = parse_kimi(&body).unwrap();
        assert_eq!((w[0].label.as_str(), w[0].used_percent, w[0].resets_at), ("5h", 0.0, None));
        assert_eq!((w[1].label.as_str(), w[1].used_percent), ("week", 0.0));
    }

    #[test]
    fn pools_keys() {
        let w = |label: &str, used: f64, reset: Option<u64>| Window { label: label.into(), used_percent: used, resets_at: reset };
        let keys = vec![
            vec![w("5h", 100.0, Some(200)), w("week", 40.0, Some(900))],
            vec![w("5h", 20.0, Some(100)), w("week", 120.0, None)],
            vec![w("5h", 0.0, None)],
        ];
        assert_eq!(summarize(&keys), vec![
            PoolWindow { label: "5h".into(), remaining_percent: 60.0, resets_at: Some(100) },
            PoolWindow { label: "week".into(), remaining_percent: 30.0, resets_at: Some(900) },
        ]);
        assert!(summarize(&[]).is_empty());
    }

    #[test]
    fn parses_zhipu_quota() {
        let body: Value = serde_json::from_str(r#"{"data":{"limits":[
            {"type":"TIME_LIMIT","unit":5,"number":1,"percentage":2,"nextResetTime":1793494156983},
            {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":70,"nextResetTime":1792789270960}]}}"#).unwrap();
        let w = parse_zhipu(&body).unwrap();
        assert_eq!((w[0].label.as_str(), w[0].used_percent), ("tools", 2.0));
        assert_eq!((w[1].label.as_str(), w[1].used_percent, w[1].resets_at), ("5h", 70.0, Some(1792789270)));
    }
}
