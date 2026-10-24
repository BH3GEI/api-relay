//! Settings from the environment.

use std::path::PathBuf;

pub struct Config {
    pub database_url: String,
    /// The main listener: pages, admin and API, behind LazyCat's gateway when on LazyCat
    pub bind_addr: String,
    /// Optional second listener for internet traffic (a Cloudflare Tunnel)
    pub public_bind_addr: Option<String>,
    /// Signing secret for sessions; generated and kept in the database when unset
    pub jwt_secret: Option<String>,
    /// Running as a LazyCat app: trust the identity headers its gateway sets
    pub lazycat: bool,
    pub cloudflared_path: Option<String>,
    /// Where tunnel connection events are kept across restarts
    pub tunnel_log: Option<PathBuf>,
}

impl Config {
    pub fn from_env() -> Config {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        Config {
            database_url: var("DATABASE_URL").unwrap_or_else(|| "sqlite:relay.db?mode=rwc".into()),
            bind_addr: var("BIND_ADDR").unwrap_or_else(|| "0.0.0.0:3000".into()),
            public_bind_addr: var("PUBLIC_BIND_ADDR"),
            jwt_secret: var("JWT_SECRET"),
            lazycat: var("LAZYCAT").as_deref() == Some("1"),
            cloudflared_path: var("CLOUDFLARED_PATH"),
            tunnel_log: var("TUNNEL_LOG").map(PathBuf::from),
        }
    }
}
