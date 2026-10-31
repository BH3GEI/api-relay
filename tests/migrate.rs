//! Databases from before schema versioning open and keep their data.

mod common;

use api_relay::{App, config::Config};
use axum::http::StatusCode;
use serde_json::json;

#[tokio::test]
async fn the_oldest_layout_is_brought_up_to_date() {
    let path = std::env::temp_dir().join(format!("api-relay-migrate-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let url = format!("sqlite:{}?mode=rwc", path.display());
    {
        let db = sqlx::SqlitePool::connect(&url).await.unwrap();
        for stmt in [
            "CREATE TABLE users (id INTEGER PRIMARY KEY AUTOINCREMENT, username TEXT UNIQUE NOT NULL, password_hash TEXT NOT NULL,
                api_key TEXT UNIQUE NOT NULL, is_admin BOOLEAN DEFAULT FALSE, created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP)",
            "INSERT INTO users (username, password_hash, api_key) VALUES ('old', 'x', 'sk-old-relay-1')",
            "CREATE TABLE providers (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT UNIQUE NOT NULL, base_url TEXT NOT NULL,
                user_agent TEXT NOT NULL, api_key TEXT, enabled BOOLEAN DEFAULT TRUE)",
            "INSERT INTO providers (name, base_url, user_agent, api_key) VALUES ('kimi', 'http://127.0.0.1:9/v1', 'ua', 'legacy')",
            "CREATE TABLE usage_limits (id INTEGER PRIMARY KEY AUTOINCREMENT, user_id INTEGER NOT NULL, model TEXT NOT NULL DEFAULT '*',
                max_requests_daily INTEGER DEFAULT -1, max_tokens_daily INTEGER DEFAULT -1)",
            "INSERT INTO usage_limits (user_id, model, max_requests_daily, max_tokens_daily) VALUES (1, '*', 5, 1000)",
        ] {
            sqlx::query(stmt).execute(&db).await.unwrap();
        }
        db.close().await;
    }

    let config = || Config {
        database_url: url.clone(),
        bind_addr: "unused".into(),
        public_bind_addr: None,
        jwt_secret: Some("s".into()),
        lazycat: true,
        cloudflared_path: None,
        tunnel_log: None,
        frpc_path: None,
        frp_bind_addr: None,
    };
    // Twice: the second start finds the schema current
    for _ in 0..2 {
        let app = App::new(&config()).await;
        let (version,): (i64,) = sqlx::query_as("PRAGMA user_version").fetch_one(&app.state.db).await.unwrap();
        assert_eq!(version, 1);

        let relay = common::Relay { app, upstream: common::Upstream::start().await, admin: String::new() };
        let sso = relay.lan("GET", "/auth/sso", &[("x-hc-user-id", "yao"), ("x-hc-user-role", "ADMIN")], None).await;
        assert_eq!(sso.status, StatusCode::OK);
        let relay = common::Relay { admin: sso.json()["token"].as_str().unwrap().to_string(), ..relay };

        let users = relay.admin("GET", "/admin/users", None).await.json();
        assert_eq!((users[0]["username"].clone(), users[0]["email"].clone()), (json!("old"), json!("")));
        let providers = relay.admin("GET", "/admin/providers", None).await.json();
        assert_eq!((providers[0]["anthropic_base_url"].clone(), providers[0]["models"].clone()), (json!(""), json!("")));
        let limits = relay.admin("GET", "/admin/limits", None).await.json();
        assert_eq!((limits[0]["window_secs"].clone(), limits[0]["max_requests"].clone(), limits[0]["max_tokens"].clone()), (json!(86400), json!(5), json!(1000)));
        // The old key still works
        assert_eq!(relay.lan("GET", "/v1/models", &[("authorization", "Bearer sk-old-relay-1")], None).await.status, StatusCode::OK);
    }
    let _ = std::fs::remove_file(&path);
}
