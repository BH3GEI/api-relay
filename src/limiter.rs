//! Per-provider concurrency caps.
//!
//! Coding plans throttle (or ban) accounts that open too many parallel requests. When a
//! provider has `max_concurrency`, requests beyond it wait for a slot instead of failing;
//! only a request that waits longer than the queue timeout is turned away.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

/// How often a waiting request re-reads the cap, so raising it in the admin page frees waiters
const RECHECK: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct Limiter {
    providers: Mutex<HashMap<i64, Arc<Counter>>>,
}

#[derive(Default)]
struct Counter {
    in_flight: Mutex<usize>,
    freed: Notify,
}

/// One in-flight request to a provider. The slot is given back when this is dropped:
/// after the response body has been sent, when the client hangs up, or on any error path.
pub struct Permit {
    counter: Arc<Counter>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut n = self.counter.in_flight.lock().unwrap();
        *n = n.saturating_sub(1);
        drop(n);
        self.counter.freed.notify_one();
    }
}

/// No slot came free within the queue timeout.
#[derive(Debug)]
pub struct QueueTimeout;

impl Limiter {
    fn counter(&self, provider_id: i64) -> Arc<Counter> {
        self.providers.lock().unwrap().entry(provider_id).or_default().clone()
    }

    /// Requests to the provider that currently hold a slot.
    pub fn in_flight(&self, provider_id: i64) -> usize {
        self.providers.lock().unwrap().get(&provider_id).map_or(0, |c| *c.in_flight.lock().unwrap())
    }

    /// Take a slot for `provider_id`, waiting up to `timeout` while `max` requests are in flight.
    /// `max` of None (or below 1) means no cap; the request is still counted.
    pub async fn acquire(&self, provider_id: i64, max: Option<i64>, timeout: Duration) -> Result<Permit, QueueTimeout> {
        let counter = self.counter(provider_id);
        let max = max.filter(|m| *m >= 1).map(|m| m as usize);
        let wait = async {
            loop {
                // Register for a wakeup before looking, so a slot freed in between isn't missed
                let freed = counter.freed.notified();
                tokio::pin!(freed);
                freed.as_mut().enable();
                {
                    let mut n = counter.in_flight.lock().unwrap();
                    if max.map_or(true, |m| *n < m) {
                        *n += 1;
                        return Permit { counter: counter.clone() };
                    }
                }
                let _ = tokio::time::timeout(RECHECK, freed).await;
            }
        };
        tokio::time::timeout(timeout, wait).await.map_err(|_| QueueTimeout)
    }
}

/// How long a request may wait for a slot: `RELAY_QUEUE_TIMEOUT_SECS`, default 120.
pub fn queue_timeout_from_env() -> Duration {
    let secs = std::env::var("RELAY_QUEUE_TIMEOUT_SECS").ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(120);
    Duration::from_secs(secs.clamp(1, 3600))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn drop_releases_slot() {
        let l = Limiter::default();
        let a = l.acquire(1, Some(2), LONG).await.unwrap();
        let b = l.acquire(1, Some(2), LONG).await.unwrap();
        assert_eq!(l.in_flight(1), 2);
        drop(a);
        assert_eq!(l.in_flight(1), 1);
        drop(b);
        assert_eq!(l.in_flight(1), 0);
    }

    #[tokio::test]
    async fn full_provider_times_out_and_others_are_unaffected() {
        let l = Limiter::default();
        let _held = l.acquire(1, Some(1), LONG).await.unwrap();
        assert!(l.acquire(1, Some(1), Duration::from_millis(50)).await.is_err());
        // A timed-out waiter leaves nothing behind
        assert_eq!(l.in_flight(1), 1);
        // Caps are per provider
        assert!(l.acquire(2, Some(1), Duration::from_millis(50)).await.is_ok());
    }

    #[tokio::test]
    async fn waiter_gets_slot_when_holder_drops() {
        let l = Arc::new(Limiter::default());
        let held = l.acquire(1, Some(1), LONG).await.unwrap();
        let l2 = l.clone();
        let waiter = tokio::spawn(async move { l2.acquire(1, Some(1), LONG).await.map(|_p| ()) });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        drop(held);
        let got = tokio::time::timeout(Duration::from_millis(500), waiter).await;
        assert!(matches!(got, Ok(Ok(Ok(())))));
        // The waiter's permit was dropped inside its task
        assert_eq!(l.in_flight(1), 0);
    }

    #[tokio::test]
    async fn released_when_holding_task_is_cancelled() {
        // Like a client hanging up mid-stream: the future holding the permit is dropped
        let l = Arc::new(Limiter::default());
        let l2 = l.clone();
        let task = tokio::spawn(async move {
            let _p = l2.acquire(1, Some(1), LONG).await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(l.in_flight(1), 1);
        task.abort();
        let _ = task.await;
        assert_eq!(l.in_flight(1), 0);
    }

    #[tokio::test]
    async fn no_cap_counts_without_waiting() {
        let l = Limiter::default();
        let permits: Vec<_> = futures::future::join_all((0..50).map(|_| l.acquire(1, None, LONG))).await;
        assert!(permits.iter().all(|p| p.is_ok()));
        assert_eq!(l.in_flight(1), 50);
        // Zero or negative is treated as "no cap", never as "block everything"
        assert!(l.acquire(1, Some(0), Duration::from_millis(10)).await.is_ok());
        drop(permits);
        assert_eq!(l.in_flight(1), 0);
    }
}
