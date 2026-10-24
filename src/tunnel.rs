//! Runs `cloudflared` so the API is reachable from the internet through a Cloudflare Tunnel.
//!
//! The tunnel's public hostname must point at the public listener (PUBLIC_BIND_ADDR), which
//! serves the API and the password-login home page only: /auth/sso and /admin trust LazyCat's
//! identity headers and must never be reachable around LazyCat's gateway.

use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Notify;

use crate::{AppState, db};

const TOKEN_SETTING: &str = "cloudflare_tunnel_token";
const LOG_LINES: usize = 30;

// The watchdog: cloudflared can end up with no edge connection and stay that way, so it is
// restarted when it reports none for a while, whatever the cause.
/// cloudflared's metrics server; `/ready` answers 200 while it has a live edge connection.
/// Off cloudflared's default range (20241–20245), which another cloudflared on the host may hold.
const METRICS_ADDR: &str = "127.0.0.1:20290";
const CHECK_EVERY: Duration = Duration::from_secs(15);
/// Time to connect after a start before the watchdog may act
const STARTUP_GRACE: Duration = Duration::from_secs(60);
/// How long without a ready connection before cloudflared is restarted
const NOT_READY_LIMIT: Duration = Duration::from_secs(60);

#[derive(Clone, Serialize)]
pub struct TunnelStatus {
    /// cloudflared is bundled and a public listener is configured
    pub available: bool,
    pub configured: bool,
    /// "off", "starting", "connected", "reconnecting" or "stopped"
    pub state: String,
    pub log: Vec<String>,
}

pub struct Tunnel {
    binary: Option<String>,
    /// Connection events kept across restarts (TUNNEL_LOG), so an outage can be looked into later
    history: Option<std::path::PathBuf>,
    /// Where the tunnel sends traffic: the public listener
    origin: String,
    status: Mutex<TunnelStatus>,
    changed: Notify,
}

impl Tunnel {
    pub fn new(binary: Option<String>, public_addr: Option<&str>, history: Option<std::path::PathBuf>) -> Self {
        let binary = binary.filter(|b| public_addr.is_some() && std::path::Path::new(b).exists());
        Tunnel {
            history,
            origin: format!("http://{}", public_addr.unwrap_or_default()),
            status: Mutex::new(TunnelStatus {
                available: binary.is_some(),
                configured: false,
                state: "off".into(),
                log: Vec::new(),
            }),
            binary,
            changed: Notify::new(),
        }
    }

    pub fn status(&self) -> TunnelStatus {
        self.status.lock().unwrap().clone()
    }

    pub async fn set_token(&self, state: &AppState, token: &str) -> Result<(), sqlx::Error> {
        db::set_setting(&state.db, TOKEN_SETTING, token.trim()).await?;
        self.changed.notify_one();
        Ok(())
    }

    fn set_state(&self, s: &str) {
        self.status.lock().unwrap().state = s.into();
    }

    fn log(&self, line: String) {
        let mut st = self.status.lock().unwrap();
        if line.contains("Registered tunnel connection") {
            st.state = "connected".into();
        }
        drop(st);
        if is_event(&line) {
            self.record(&line);
        }
        let mut st = self.status.lock().unwrap();
        st.log.push(line);
        let excess = st.log.len().saturating_sub(LOG_LINES);
        st.log.drain(..excess);
    }

    fn record(&self, line: &str) {
        use std::io::Write;
        let Some(path) = &self.history else { return };
        // Keep the file small: past the cap, start over from the most recent half
        if std::fs::metadata(path).is_ok_and(|m| m.len() > HISTORY_MAX) {
            if let Ok(old) = std::fs::read(path) {
                let keep = &old[old.len() - (HISTORY_MAX / 2) as usize..];
                let start = keep.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
                let _ = std::fs::write(path, &keep[start..]);
            }
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            // cloudflared's own lines carry a timestamp; ours (exits, watchdog) get one here
            let stamped = line.as_bytes().first().is_some_and(u8::is_ascii_digit);
            let _ = if stamped { writeln!(f, "{line}") } else { writeln!(f, "{} {line}", now_utc()) };
        }
    }
}

const HISTORY_MAX: u64 = 1024 * 1024;

/// Lines worth keeping: the connection's life, not per-request errors.
fn is_event(line: &str) -> bool {
    const EVENTS: [&str; 10] = ["Registered tunnel connection", "Unregistered tunnel connection", "Retrying connection",
        "Connection terminated", "edge discovery", "lookup", "Initial protocol", "cloudflared exited", "watchdog", "failed to start"];
    !line.contains("failed to serve incoming request") && !line.contains("Unable to reach the origin")
        && (EVENTS.iter().any(|e| line.contains(e)) || line.contains(" ERR ") && line.contains("connIndex"))
}

/// "2026-10-24T10:41:15Z"
fn now_utc() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Civil date from days since the epoch (Howard Hinnant's algorithm)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

/// Keep cloudflared running with the configured token; restart it when the token changes or it exits.
pub fn spawn(state: Arc<AppState>) {
    let Some(binary) = state.tunnel.binary.clone() else { return };
    tokio::spawn(async move {
        loop {
            let token = db::get_setting(&state.db, TOKEN_SETTING).await.ok().flatten().unwrap_or_default();
            state.tunnel.status.lock().unwrap().configured = !token.is_empty();
            if token.is_empty() {
                state.tunnel.set_state("off");
                state.tunnel.changed.notified().await;
                continue;
            }

            state.tunnel.set_state("starting");
            // The token goes in the environment, not argv, so it doesn't show up in process listings.
            // HTTP/2 rather than QUIC, which is often blocked in mainland China.
            let child = Command::new(&binary)
                .args(["tunnel", "--no-autoupdate", "--protocol", "http2", "--metrics", METRICS_ADDR, "run"])
                .env("TUNNEL_TOKEN", &token)
                // For tunnels created from the CLI; a tunnel configured in the dashboard uses its own routes
                .env("TUNNEL_URL", &state.tunnel.origin)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn();
            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    state.tunnel.log(format!("failed to start cloudflared: {e}"));
                    state.tunnel.set_state("stopped");
                    let _ = tokio::time::timeout(Duration::from_secs(30), state.tunnel.changed.notified()).await;
                    continue;
                }
            };

            if let Some(stderr) = child.stderr.take() {
                let st = state.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        st.tunnel.log(line);
                    }
                });
            }

            let started = Instant::now();
            let mut last_ready = Instant::now();
            let mut tick = tokio::time::interval(CHECK_EVERY);
            loop {
                tokio::select! {
                    exit = child.wait() => {
                        state.tunnel.log(format!("cloudflared exited ({})", exit.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string())));
                        state.tunnel.set_state("stopped");
                        // Back off before restarting, unless the token changes first
                        let _ = tokio::time::timeout(Duration::from_secs(10), state.tunnel.changed.notified()).await;
                        break;
                    }
                    _ = state.tunnel.changed.notified() => {
                        let _ = child.kill().await;
                        break;
                    }
                    _ = tick.tick() => {
                        if let Some(reason) = watchdog(&state, started, &mut last_ready).await {
                            state.tunnel.log(format!("watchdog: {reason}; restarting cloudflared"));
                            state.tunnel.set_state("reconnecting");
                            let _ = child.kill().await;
                            break;
                        }
                    }
                }
            }
        }
    });
}

/// Why cloudflared should be restarted, if it should.
async fn watchdog(state: &AppState, started: Instant, last_ready: &mut Instant) -> Option<String> {
    if ready(state).await {
        *last_ready = Instant::now();
        state.tunnel.set_state("connected");
    } else if started.elapsed() > STARTUP_GRACE {
        state.tunnel.set_state("reconnecting");
        if last_ready.elapsed() > NOT_READY_LIMIT {
            return Some(format!("no edge connection for {}s", last_ready.elapsed().as_secs()));
        }
    }
    None
}

async fn ready(state: &AppState) -> bool {
    state.http_client.get(format!("http://{METRICS_ADDR}/ready"))
        .timeout(Duration::from_secs(5))
        .send().await
        .is_ok_and(|r| r.status().is_success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_connection_events_only() {
        assert!(is_event("2026-10-24T10:32:53Z INF Registered tunnel connection connIndex=0"));
        assert!(is_event("cloudflared exited (exit status: 1)"));
        assert!(is_event("watchdog: no edge connection for 75s; restarting cloudflared"));
        assert!(is_event("2026-10-24T10:32:53Z ERR Serve tunnel error error=\"dial tcp: i/o timeout\" connIndex=1"));
        assert!(!is_event("2026-10-24T10:41:15Z ERR failed to serve incoming request error=\"Failed to proxy HTTP\""));
        assert!(!is_event("2026-10-24T10:41:15Z ERR  error=\"Unable to reach the origin service\" connIndex=2"));
    }

    #[test]
    fn formats_utc() {
        let s = now_utc();
        assert_eq!((s.len(), &s[4..5], &s[10..11]), (20, "-", "T"));
    }
}
