//! A second way in from the internet beside the Cloudflare Tunnel: `frpc` forwards a port on
//! an frp server to the frp listener, which serves the public routes over HTTPS.
//!
//! frp forwards raw TCP, so the relay terminates TLS itself, with a Let's Encrypt certificate
//! for a domain pointing at the frp server (see `acme`). frpc puts a PROXY protocol header in
//! front of each connection, which is where the caller's address comes from: on this listener
//! nothing a client sends can be trusted for it.

use axum::{Router, extract::Request};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::Notify;
use tower::ServiceExt;

use crate::{AppState, acme, db};

const LOG_LINES: usize = 30;
/// Renew when the certificate has less than this left
const RENEW_BEFORE: i64 = 30 * 86400;
const CERT_CHECK_EVERY: Duration = Duration::from_secs(12 * 3600);
/// After a failed issuance; Let's Encrypt limits failed validations per hour
const CERT_RETRY: Duration = Duration::from_secs(3600);

/// What the admin sets. Secrets are write-only through the admin API.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct FrpSettings {
    /// frps address, `host:port`
    #[serde(default)]
    pub server: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
    /// Port opened on the frp server
    #[serde(default)]
    pub remote_port: u16,
    /// The name the certificate is for; resolves to the frp server
    #[serde(default)]
    pub domain: String,
    /// Cloudflare API token with DNS edit on the domain's zone
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub dns_token: String,
}

impl FrpSettings {
    pub fn configured(&self) -> bool {
        !self.server.is_empty() && self.remote_port != 0 && !self.domain.is_empty() && !self.dns_token.is_empty()
    }

    /// The address clients use
    pub fn url(&self) -> Option<String> {
        self.configured().then(|| format!("https://{}:{}", self.domain, self.remote_port))
    }
}

#[derive(Clone, Serialize)]
pub struct FrpStatus {
    /// frpc is bundled and the frp listener is configured
    pub available: bool,
    pub configured: bool,
    /// "off", "starting", "connected", "reconnecting" or "stopped"
    pub state: String,
    pub url: Option<String>,
    /// "none", "issuing", "valid" or "failed"
    pub cert: String,
    /// When the certificate expires, seconds since the epoch
    pub cert_expires: Option<i64>,
    pub server: String,
    pub remote_port: u16,
    pub domain: String,
    pub has_token: bool,
    pub has_dns_token: bool,
    pub log: Vec<String>,
}

/// The caller's address on the frp listener, from frpc's PROXY protocol header.
#[derive(Clone)]
pub struct FrpClient(pub Option<String>);

pub struct Frp {
    binary: Option<String>,
    /// The frp listener frpc forwards to
    local: Option<String>,
    status: Mutex<FrpStatus>,
    certs: Arc<Certs>,
    changed: Notify,
}

impl Frp {
    pub fn new(binary: Option<String>, local: Option<String>) -> Self {
        let binary = binary.filter(|b| local.is_some() && std::path::Path::new(b).exists());
        Frp {
            local,
            status: Mutex::new(FrpStatus {
                available: binary.is_some(),
                configured: false,
                state: "off".into(),
                url: None,
                cert: "none".into(),
                cert_expires: None,
                server: String::new(),
                remote_port: 0,
                domain: String::new(),
                has_token: false,
                has_dns_token: false,
                log: Vec::new(),
            }),
            binary,
            certs: Arc::default(),
            changed: Notify::new(),
        }
    }

    pub fn status(&self) -> FrpStatus {
        self.status.lock().unwrap().clone()
    }

    /// Save new settings; a secret left empty keeps the saved one. An empty server turns frp off.
    pub async fn set(&self, state: &AppState, mut new: FrpSettings) -> Result<(), sqlx::Error> {
        let old = settings(&state.db).await;
        if new.token.is_empty() {
            new.token = old.token;
        }
        if new.dns_token.is_empty() {
            new.dns_token = old.dns_token;
        }
        new.server = new.server.trim().into();
        new.domain = new.domain.trim().trim_end_matches('.').to_ascii_lowercase();
        db::settings::set(&state.db, db::settings::FRP, &serde_json::to_string(&new).unwrap()).await?;
        self.changed.notify_waiters();
        Ok(())
    }

    fn set_state(&self, s: &str) {
        self.status.lock().unwrap().state = s.into();
    }

    fn log(&self, line: String) {
        let mut st = self.status.lock().unwrap();
        if line.contains("start proxy success") {
            st.state = "connected".into();
        } else if line.contains("login to the server failed") || line.contains("connect to server error") || line.contains("start error") {
            st.state = "reconnecting".into();
        }
        st.log.push(line);
        let excess = st.log.len().saturating_sub(LOG_LINES);
        st.log.drain(..excess);
    }

    fn show(&self, s: &FrpSettings) {
        let mut st = self.status.lock().unwrap();
        st.configured = s.configured();
        st.url = s.url();
        st.server = s.server.clone();
        st.remote_port = s.remote_port;
        st.domain = s.domain.clone();
        st.has_token = !s.token.is_empty();
        st.has_dns_token = !s.dns_token.is_empty();
    }
}

async fn settings(db: &db::Db) -> FrpSettings {
    db::settings::get_nonempty(db, db::settings::FRP).await.ok().flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Start everything frp needs: the certificate, frpc, and the HTTPS listener serving `public`.
pub fn spawn(state: Arc<AppState>, public: Option<Router>) {
    let (Some(binary), Some(local), Some(public)) = (state.frp.binary.clone(), state.frp.local.clone(), public) else { return };
    tokio::spawn(certificates(state.clone()));
    tokio::spawn(frpc(state.clone(), binary, local.clone()));
    tokio::spawn(async move {
        let listener = match TcpListener::bind(&local).await {
            Ok(l) => l,
            Err(e) => return state.frp.log(format!("frp listener on {local}: {e}")),
        };
        serve(listener, state.frp.certs.clone(), public).await;
    });
}

/// Keep a valid certificate for the configured domain: load the saved one, issue or renew as needed.
async fn certificates(state: Arc<AppState>) {
    let frp = &state.frp;
    loop {
        // Before reading, so a change made meanwhile isn't missed
        let changed = frp.changed.notified();
        let s = settings(&state.db).await;
        frp.show(&s);
        let wait = if !s.configured() {
            frp.certs.set(None);
            frp.status.lock().unwrap().cert = "none".into();
            None
        } else {
            let saved = saved_cert(&state.db, &s.domain).await;
            let now = now();
            match saved {
                Some((issued, expires)) if expires - now > RENEW_BEFORE => {
                    frp.certs.set(load(&issued));
                    let mut st = frp.status.lock().unwrap();
                    (st.cert, st.cert_expires) = ("valid".into(), Some(expires));
                    Some(CERT_CHECK_EVERY)
                }
                saved => {
                    // Keep serving the old one while renewing, if it still works
                    if let Some((issued, expires)) = &saved {
                        if *expires > now {
                            frp.certs.set(load(issued));
                        }
                    }
                    frp.status.lock().unwrap().cert = "issuing".into();
                    frp.log(format!("certificate: requesting one for {}", s.domain));
                    match acme::issue(&state.db, &state.http_client, &s.dns_token, &s.domain).await {
                        Ok(issued) => {
                            let expires = acme::expires_at(&issued.cert_pem);
                            let value = serde_json::json!({ "domain": s.domain, "cert": issued.cert_pem, "key": issued.key_pem });
                            let _ = db::settings::set(&state.db, db::settings::FRP_CERT, &value.to_string()).await;
                            frp.certs.set(load(&(issued.cert_pem, issued.key_pem)));
                            frp.log("certificate: issued".into());
                            let mut st = frp.status.lock().unwrap();
                            (st.cert, st.cert_expires) = ("valid".into(), expires);
                            Some(CERT_CHECK_EVERY)
                        }
                        Err(e) => {
                            frp.log(format!("certificate: failed: {e}"));
                            frp.status.lock().unwrap().cert = "failed".into();
                            Some(CERT_RETRY)
                        }
                    }
                }
            }
        };
        match wait {
            Some(d) => { let _ = tokio::time::timeout(d, changed).await; }
            None => changed.await,
        }
    }
}

/// The saved certificate and key for `domain`, and when it expires.
async fn saved_cert(db: &db::Db, domain: &str) -> Option<((String, String), i64)> {
    let v: serde_json::Value = serde_json::from_str(&db::settings::get_nonempty(db, db::settings::FRP_CERT).await.ok()??).ok()?;
    if v["domain"].as_str() != Some(domain) {
        return None;
    }
    let (cert, key) = (v["cert"].as_str()?.to_string(), v["key"].as_str()?.to_string());
    let expires = acme::expires_at(&cert)?;
    Some(((cert, key), expires))
}

fn load((cert, key): &(String, String)) -> Option<Arc<CertifiedKey>> {
    let chain = rustls_pemfile::certs(&mut cert.as_bytes()).collect::<Result<Vec<_>, _>>().ok()?;
    let key = rustls_pemfile::private_key(&mut key.as_bytes()).ok()??;
    let signer = rustls::crypto::ring::sign::any_supported_type(&key).ok()?;
    Some(Arc::new(CertifiedKey::new(chain, signer)))
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Keep frpc running with the saved settings; restart it when they change or it exits.
async fn frpc(state: Arc<AppState>, binary: String, local: String) {
    let frp = &state.frp;
    let config = std::env::temp_dir().join("api-relay-frpc.toml");
    loop {
        let changed = frp.changed.notified();
        let s = settings(&state.db).await;
        frp.show(&s);
        if !s.configured() {
            frp.set_state("off");
            changed.await;
            continue;
        }
        frp.set_state("starting");
        let mut child = match start(&state, &binary, &config, &local, &s) {
            Ok(c) => c,
            Err(e) => {
                frp.log(format!("failed to start frpc: {e}"));
                frp.set_state("stopped");
                let _ = tokio::time::timeout(Duration::from_secs(30), changed).await;
                continue;
            }
        };
        tokio::pin!(changed);
        tokio::select! {
            exit = child.wait() => {
                frp.log(format!("frpc exited ({})", exit.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string())));
                frp.set_state("stopped");
                let _ = tokio::time::timeout(Duration::from_secs(10), changed).await;
            }
            _ = &mut changed => {
                let _ = child.kill().await;
            }
        }
    }
}

fn start(state: &Arc<AppState>, binary: &str, config: &std::path::Path, local: &str, s: &FrpSettings) -> std::io::Result<Child> {
    let invalid = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, what.to_string());
    let (host, port) = s.server.rsplit_once(':').ok_or_else(|| invalid("server must be host:port"))?;
    let port: u16 = port.parse().map_err(|_| invalid("bad server port"))?;
    let (local_ip, local_port) = local.rsplit_once(':').ok_or_else(|| invalid("bad frp listener address"))?;
    // The token stays out of the file and argv: frpc reads it from its environment
    let toml = format!(
        r#"serverAddr = {host:?}
serverPort = {port}
loginFailExit = false
auth.method = "token"
auth.token = "{{{{ .Envs.FRP_AUTH_TOKEN }}}}"
log.to = "console"
log.disablePrintColor = true

[[proxies]]
name = "api-relay-{remote}"
type = "tcp"
localIP = {local_ip:?}
localPort = {local_port}
remotePort = {remote}
transport.proxyProtocolVersion = "v2"
"#,
        remote = s.remote_port,
    );
    std::fs::write(config, toml)?;
    let mut child = Command::new(binary)
        .arg("-c").arg(config)
        .env("FRP_AUTH_TOKEN", &s.token)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    for out in [child.stdout.take().map(|o| Box::new(o) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
                child.stderr.take().map(|e| Box::new(e) as Box<dyn tokio::io::AsyncRead + Send + Unpin>)].into_iter().flatten() {
        let st = state.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                st.frp.log(line);
            }
        });
    }
    Ok(child)
}

/// The certificate the listener presents; none until one is issued, and handshakes fail meanwhile.
#[derive(Default, Debug)]
struct Certs(RwLock<Option<Arc<CertifiedKey>>>);

impl Certs {
    fn set(&self, c: Option<Arc<CertifiedKey>>) {
        *self.0.write().unwrap() = c;
    }
}

impl ResolvesServerCert for Certs {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.0.read().unwrap().clone()
    }
}

async fn serve(listener: TcpListener, certs: Arc<Certs>, public: Router) {
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions().unwrap()
        .with_no_client_auth()
        .with_cert_resolver(certs);
    tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let (acceptor, router) = (acceptor.clone(), public.clone());
        tokio::spawn(async move {
            let Ok(Ok((stream, client))) = tokio::time::timeout(Duration::from_secs(10), proxy_header(stream)).await else { return };
            let Ok(Ok(tls)) = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream)).await else { return };
            let client = FrpClient(client);
            let service = hyper::service::service_fn(move |mut req: Request<hyper::body::Incoming>| {
                req.extensions_mut().insert(client.clone());
                router.clone().oneshot(req.map(axum::body::Body::new))
            });
            let _ = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(TokioIo::new(tls), service).await;
        });
    }
}

const PROXY_V2: [u8; 12] = *b"\r\n\r\n\0\r\nQUIT\n";

/// Read frpc's PROXY protocol v2 header; the source address, if it carries one. A connection
/// without the header didn't come through frpc and is refused.
async fn proxy_header(mut stream: TcpStream) -> std::io::Result<(TcpStream, Option<String>)> {
    let mut head = [0u8; 16];
    stream.read_exact(&mut head).await?;
    if head[..12] != PROXY_V2 || head[12] >> 4 != 2 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "no PROXY v2 header"));
    }
    let mut addr = vec![0u8; u16::from_be_bytes([head[14], head[15]]) as usize];
    stream.read_exact(&mut addr).await?;
    Ok((stream, source(head[12] & 0x0f, head[13], &addr)))
}

/// The source address in a PROXY v2 address block: `command` 1 is PROXY, family 0x11 TCP over
/// IPv4 and 0x21 TCP over IPv6.
fn source(command: u8, family: u8, addr: &[u8]) -> Option<String> {
    if command != 1 {
        return None;
    }
    match family {
        0x11 if addr.len() >= 12 => Some(std::net::Ipv4Addr::new(addr[0], addr[1], addr[2], addr[3]).to_string()),
        0x21 if addr.len() >= 36 => Some(std::net::Ipv6Addr::from(<[u8; 16]>::try_from(&addr[..16]).ok()?).to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_proxy_source() {
        assert_eq!(source(1, 0x11, &[1, 2, 3, 4, 10, 0, 0, 1, 0, 80, 1, 187]).as_deref(), Some("1.2.3.4"));
        let mut v6 = [0u8; 36];
        v6[15] = 1;
        assert_eq!(source(1, 0x21, &v6).as_deref(), Some("::1"));
        // LOCAL (health checks) and unknown families carry no client
        assert_eq!(source(0, 0x11, &[1, 2, 3, 4, 10, 0, 0, 1, 0, 80, 1, 187]), None);
        assert_eq!(source(1, 0x00, &[]), None);
    }

    #[tokio::test]
    async fn refuses_connections_without_the_header() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let mut s = TcpStream::connect(addr).await.unwrap();
            s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
            let mut s2 = TcpStream::connect(addr).await.unwrap();
            let mut good = PROXY_V2.to_vec();
            good.extend([0x21, 0x11, 0, 12, 5, 6, 7, 8, 127, 0, 0, 1, 0, 1, 0, 2]);
            s2.write_all(&good).await.unwrap();
            (s, s2)
        });
        let (bad, _) = listener.accept().await.unwrap();
        assert!(proxy_header(bad).await.is_err());
        let (good, _) = listener.accept().await.unwrap();
        assert_eq!(proxy_header(good).await.unwrap().1.as_deref(), Some("5.6.7.8"));
        drop(client.await);
    }

    #[test]
    fn settings_need_everything_but_the_frp_token() {
        let mut s = FrpSettings { server: "frp.example.com:7000".into(), remote_port: 40001, domain: "frp.example.com".into(), ..Default::default() };
        assert!(!s.configured());
        s.dns_token = "t".into();
        assert_eq!(s.url().as_deref(), Some("https://frp.example.com:40001"));
        // Secrets never go out through the admin API
        let shown = serde_json::to_value(FrpSettings { token: String::new(), dns_token: String::new(), ..s }).unwrap();
        assert!(shown.get("token").is_none() && shown.get("dns_token").is_none());
    }
}
