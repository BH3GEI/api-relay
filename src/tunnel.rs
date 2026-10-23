//! Runs `cloudflared` so the API is reachable from the internet through a Cloudflare Tunnel.
//!
//! The tunnel's public hostname must point at the public listener (PUBLIC_BIND_ADDR), which
//! serves only the API: the pages, /auth and /admin trust LazyCat's identity headers and must
//! never be reachable around LazyCat's gateway.

use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Notify;

use crate::{AppState, db};

const TOKEN_SETTING: &str = "cloudflare_tunnel_token";
const LOG_LINES: usize = 30;

#[derive(Clone, Serialize)]
pub struct TunnelStatus {
    /// cloudflared is bundled and a public listener is configured
    pub available: bool,
    pub configured: bool,
    /// "off", "starting", "connected" or "stopped"
    pub state: String,
    pub log: Vec<String>,
}

pub struct Tunnel {
    binary: Option<String>,
    /// Where the tunnel sends traffic: the public listener
    origin: String,
    status: Mutex<TunnelStatus>,
    changed: Notify,
}

impl Tunnel {
    pub fn new(binary: Option<String>, public_addr: Option<&str>) -> Self {
        let binary = binary.filter(|b| public_addr.is_some() && std::path::Path::new(b).exists());
        Tunnel {
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
        st.log.push(line);
        let excess = st.log.len().saturating_sub(LOG_LINES);
        st.log.drain(..excess);
    }
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
                .args(["tunnel", "--no-autoupdate", "--protocol", "http2", "run"])
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

            tokio::select! {
                exit = child.wait() => {
                    state.tunnel.log(format!("cloudflared exited ({})", exit.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string())));
                    state.tunnel.set_state("stopped");
                    // Back off before restarting, unless the token changes first
                    let _ = tokio::time::timeout(Duration::from_secs(10), state.tunnel.changed.notified()).await;
                }
                _ = state.tunnel.changed.notified() => {
                    let _ = child.kill().await;
                }
            }
        }
    });
}
