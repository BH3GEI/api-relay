//! Plan quota (5-hour / weekly windows) for keys whose upstream reports it.

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::{AppState, db};

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

#[derive(Default)]
pub struct QuotaCache {
    entries: Mutex<HashMap<String, (Instant, Option<Vec<Window>>)>>,
}

impl QuotaCache {
    /// The key's quota windows, or None when the upstream doesn't report quota.
    pub async fn get(&self, state: &AppState, provider: &db::Provider, key: &str) -> Option<Vec<Window>> {
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

async fn fetch(state: &AppState, provider: &db::Provider, key: &str) -> Option<Vec<Window>> {
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
    fn parses_zhipu_quota() {
        let body: Value = serde_json::from_str(r#"{"data":{"limits":[
            {"type":"TIME_LIMIT","unit":5,"number":1,"percentage":2,"nextResetTime":1793494156983},
            {"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":70,"nextResetTime":1792789270960}]}}"#).unwrap();
        let w = parse_zhipu(&body).unwrap();
        assert_eq!((w[0].label.as_str(), w[0].used_percent), ("tools", 2.0));
        assert_eq!((w[1].label.as_str(), w[1].used_percent, w[1].resets_at), ("5h", 70.0, Some(1792789270)));
    }
}
