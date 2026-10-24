//! Slows down guessing: an address that fails authentication too often is shut out for a while.

use axum::http::{HeaderMap, StatusCode};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::Result;

const WINDOW: Duration = Duration::from_secs(600);
const MAX_FAILURES: u32 = 10;

#[derive(Default)]
pub struct AuthGuard {
    failures: Mutex<HashMap<String, (u32, Instant)>>,
}

impl AuthGuard {
    /// 429 when the address is shut out. An unknown address never is: throttling everyone
    /// behind one would let an attacker lock out legitimate users.
    pub fn check(&self, ip: Option<&str>) -> Result<()> {
        let Some(ip) = ip else { return Ok(()) };
        let failures = self.failures.lock().unwrap();
        let blocked = failures.get(ip).is_some_and(|(n, since)| *n >= MAX_FAILURES && since.elapsed() < WINDOW);
        if blocked { Err(StatusCode::TOO_MANY_REQUESTS.into()) } else { Ok(()) }
    }

    pub fn fail(&self, ip: Option<&str>) {
        let Some(ip) = ip else { return };
        let mut failures = self.failures.lock().unwrap();
        if failures.len() > 10_000 {
            failures.retain(|_, (_, since)| since.elapsed() < WINDOW);
        }
        let entry = failures.entry(ip.to_string()).or_insert((0, Instant::now()));
        if entry.1.elapsed() >= WINDOW {
            *entry = (0, Instant::now());
        }
        entry.0 += 1;
    }
}

/// The caller's address as reported by the proxy in front of the listener it came in on.
pub fn client_ip(headers: &HeaderMap, public: bool) -> Option<String> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let ip = if public {
        // Cloudflare sets this itself; through the tunnel nobody else can reach us
        header("cf-connecting-ip")
    } else {
        // The last hop is the one our proxy appended; earlier entries come from the client and can be forged
        header("x-forwarded-for").and_then(|v| v.rsplit(',').next()).or_else(|| header("x-real-ip"))
    };
    ip.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shuts_out_after_too_many_failures() {
        let g = AuthGuard::default();
        for _ in 0..MAX_FAILURES {
            assert!(g.check(Some("a")).is_ok());
            g.fail(Some("a"));
        }
        assert!(g.check(Some("a")).is_err());
        assert!(g.check(Some("b")).is_ok());
        g.fail(None);
        assert!(g.check(None).is_ok());
    }

    #[test]
    fn trusts_only_the_proxy_hop() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "6.6.6.6, 10.0.0.1".parse().unwrap());
        h.insert("cf-connecting-ip", "1.2.3.4".parse().unwrap());
        assert_eq!(client_ip(&h, false).as_deref(), Some("10.0.0.1"));
        assert_eq!(client_ip(&h, true).as_deref(), Some("1.2.3.4"));
        assert_eq!(client_ip(&HeaderMap::new(), true), None);
    }
}
