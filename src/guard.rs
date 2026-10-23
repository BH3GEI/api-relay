//! Slows down key guessing: an address that fails authentication too often is shut out for a while.

use axum::http::HeaderMap;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(600);
const MAX_FAILURES: u32 = 10;

#[derive(Default)]
pub struct AuthGuard {
    failures: Mutex<HashMap<String, (u32, Instant)>>,
}

impl AuthGuard {
    pub fn blocked(&self, ip: &str) -> bool {
        let failures = self.failures.lock().unwrap();
        failures.get(ip).is_some_and(|(n, since)| *n >= MAX_FAILURES && since.elapsed() < WINDOW)
    }

    pub fn fail(&self, ip: &str) {
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

/// The caller's address as reported by the reverse proxy in front of the relay.
/// None when there is no proxy header: throttling everyone behind one address would let
/// an attacker lock out legitimate users.
pub fn client_ip(headers: &HeaderMap) -> Option<String> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    // The last hop is the one our proxy appended; earlier entries come from the client and can be forged
    header("x-forwarded-for")
        .and_then(|v| v.rsplit(',').next())
        .or_else(|| header("x-real-ip"))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
