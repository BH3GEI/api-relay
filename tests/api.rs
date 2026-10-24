//! The relay's behavior as its users and admin see it, over HTTP.

mod common;

use axum::http::StatusCode;
use common::{Relay, bearer};
use serde_json::json;

// ── Pages and what each listener serves ──

#[tokio::test]
async fn listeners_serve_their_own_surfaces() {
    let r = Relay::start().await;
    assert_eq!(r.lan("GET", "/healthz", &[], None).await.status, StatusCode::OK);
    assert_eq!(r.lan("GET", "/", &[], None).await.status, StatusCode::OK);
    assert_eq!(r.lan("GET", "/admin", &[], None).await.status, StatusCode::OK);
    assert_eq!(r.public("GET", "/", &[], None).await.status, StatusCode::OK);
    // LazyCat sign-in trusts gateway headers anyone could forge on the public side
    let forged = [("x-hc-user-id", "yao"), ("x-hc-user-role", "ADMIN")];
    assert_eq!(r.public("GET", "/auth/sso", &forged, None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(r.public("GET", "/admin", &[], None).await.status, StatusCode::NOT_FOUND);
    let auth = bearer(&r.admin);
    assert_eq!(r.public("GET", "/admin/users", &[("authorization", &auth)], None).await.status, StatusCode::NOT_FOUND);

    let lan = r.lan("GET", "/auth/options", &[], None).await.json();
    assert_eq!(lan, json!({ "public": false, "register": false }));
    let public = r.public("GET", "/auth/options", &[], None).await.json();
    assert_eq!(public, json!({ "public": true, "register": false }));
}

#[tokio::test]
async fn pages_reference_their_scripts_by_content_hash() {
    let r = Relay::start().await;
    for (side_page, admin) in [("/", false), ("/admin", true)] {
        let page = if admin { r.lan("GET", side_page, &[], None).await } else { r.public("GET", side_page, &[], None).await };
        let start = page.text.find("/usage-chart.js?v=").unwrap_or_else(|| panic!("{side_page} has no hashed script URL"));
        let url: String = page.text[start..].chars().take_while(|c| *c != '"').collect();
        let js = r.lan("GET", &url, &[], None).await;
        assert_eq!(js.status, StatusCode::OK);
        assert!(js.headers["content-type"].to_str().unwrap().contains("javascript"));
        assert!(js.text.contains("UsageChart"));
    }
}

// ── Signing in ──

#[tokio::test]
async fn lazycat_sign_in() {
    let r = Relay::start().await;
    assert_eq!(r.lan("GET", "/auth/sso", &[], None).await.status, StatusCode::UNAUTHORIZED);
    // A LazyCat user the admin hasn't added
    assert_eq!(r.lan("GET", "/auth/sso", &[("x-hc-user-id", "stranger")], None).await.status, StatusCode::FORBIDDEN);
    r.user("added", "password-1").await;
    let sso = r.lan("GET", "/auth/sso", &[("x-hc-user-id", "added")], None).await;
    assert_eq!(sso.status, StatusCode::OK);
    assert!(sso.json()["api_key"].as_str().unwrap().starts_with("sk-added-relay-"));
}

#[tokio::test]
async fn passwords_sign_in_on_the_public_side_only() {
    let r = Relay::start().await;
    let (_, key) = r.user("bob", "bobpassword1").await;
    let creds = json!({ "username": "bob", "password": "bobpassword1" });
    assert_eq!(r.lan("POST", "/auth/login", &[], Some(creds.clone())).await.status, StatusCode::FORBIDDEN);
    let ok = r.public("POST", "/auth/login", &[], Some(creds)).await;
    assert_eq!(ok.status, StatusCode::OK);
    assert_eq!(ok.json()["api_key"], key.as_str());
    let wrong = r.public("POST", "/auth/login", &[], Some(json!({ "username": "bob", "password": "nope" }))).await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    let nobody = r.public("POST", "/auth/login", &[], Some(json!({ "username": "nobody", "password": "nope" }))).await;
    assert_eq!(nobody.status, StatusCode::UNAUTHORIZED);
    // Registration only bootstraps a non-LazyCat install
    let reg = r.public("POST", "/auth/register", &[], Some(json!({ "username": "x", "password": "y", "email": "x@y" }))).await;
    assert_eq!(reg.status, StatusCode::NOT_FOUND);
    assert_eq!(r.lan("POST", "/auth/register", &[], Some(json!({ "username": "x", "password": "y", "email": "x@y" }))).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn repeated_failures_shut_an_address_out() {
    let r = Relay::start().await;
    let (_, key) = r.user("bob", "bobpassword1").await;
    let attacker = [("cf-connecting-ip", "203.0.113.9")];
    for _ in 0..10 {
        let w = r.public("POST", "/auth/login", &attacker, Some(json!({ "username": "bob", "password": "guess" }))).await;
        assert_eq!(w.status, StatusCode::UNAUTHORIZED);
    }
    let right = r.public("POST", "/auth/login", &attacker, Some(json!({ "username": "bob", "password": "bobpassword1" }))).await;
    assert_eq!(right.status, StatusCode::TOO_MANY_REQUESTS);
    let auth = bearer(&key);
    assert_eq!(r.public("GET", "/v1/models", &[attacker[0], ("authorization", &auth)], None).await.status, StatusCode::TOO_MANY_REQUESTS);
    // Others are unaffected
    assert_eq!(r.public("GET", "/v1/models", &[("authorization", &auth)], None).await.status, StatusCode::OK);
    // Bad API keys count too
    let other = [("cf-connecting-ip", "203.0.113.10"), ("authorization", "Bearer sk-nope")];
    for _ in 0..10 {
        assert_eq!(r.public("GET", "/v1/models", &other, None).await.status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(r.public("GET", "/v1/models", &other, None).await.status, StatusCode::TOO_MANY_REQUESTS);
}

// ── Admin ──

#[tokio::test]
async fn admin_api_takes_only_an_admin_session() {
    let r = Relay::start().await;
    let (_, key) = r.user("bob", "bobpassword1").await;
    let session = r.login("bob", "bobpassword1").await;
    assert_eq!(r.lan("GET", "/admin/users", &[], None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.lan("GET", "/admin/users", &[("authorization", &bearer(&key))], None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.lan("GET", "/admin/users", &[("authorization", &bearer(&session))], None).await.status, StatusCode::FORBIDDEN);
    let users = r.admin("GET", "/admin/users", None).await;
    assert_eq!(users.status, StatusCode::OK);
    let names: Vec<_> = users.json().as_array().unwrap().iter().map(|u| u["username"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, ["yao", "bob"]);
    assert!(!users.text.contains("password"), "password hashes never leave the server");
}

#[tokio::test]
async fn admin_manages_users() {
    let r = Relay::start().await;
    let (id, key) = r.user("bob", "bobpassword1").await;
    assert_eq!(r.admin("POST", "/admin/users", Some(json!({ "username": "bob" }))).await.status, StatusCode::CONFLICT);
    assert_eq!(r.admin("PUT", &format!("/admin/users/{id}/password"), Some(json!({ "password": "short" }))).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.admin("PUT", "/admin/users/999/password", Some(json!({ "password": "long-enough" }))).await.status, StatusCode::NOT_FOUND);
    assert_eq!(r.admin("DELETE", &format!("/admin/users/{id}"), None).await.status, StatusCode::NO_CONTENT);
    assert_eq!(r.lan("GET", "/v1/models", &[("authorization", &bearer(&key))], None).await.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn admin_sees_provider_keys_by_their_tail_only() {
    let r = Relay::start().await;
    let id = r.provider("fake", &["secret-key-abcd"]).await;
    let keys = r.admin("GET", &format!("/admin/keys/{id}"), None).await;
    assert_eq!(keys.status, StatusCode::OK);
    assert!(!keys.text.contains("secret-key"));
    assert_eq!(keys.json()[0]["api_key"], "…abcd");
    let providers = r.admin("GET", "/admin/providers", None).await.json();
    assert_eq!(providers[0]["name"], "fake");
    assert!(providers[0].get("api_key").is_none());
    assert_eq!(r.admin("GET", "/admin/catalog", None).await.json(), json!(["fake/m1", "fake/m2"]));
}

#[tokio::test]
async fn public_url_is_validated() {
    let r = Relay::start().await;
    for bad in ["ftp://x.y", "https://x.y/path", "not a url"] {
        assert_eq!(r.admin("PUT", "/admin/public-url", Some(json!({ "url": bad }))).await.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    assert_eq!(r.admin("PUT", "/admin/public-url", Some(json!({ "url": "https://relay.example.com/" }))).await.status, StatusCode::NO_CONTENT);
    let (_, key) = r.user("bob", "bobpassword1").await;
    let cfg = r.public("GET", "/v1/user/config", &[("authorization", &bearer(&key))], None).await.json();
    assert_eq!(cfg, json!({ "public_url": "https://relay.example.com" }));
}

#[tokio::test]
async fn tunnel_is_unavailable_without_cloudflared() {
    let r = Relay::start().await;
    let st = r.admin("GET", "/admin/tunnel", None).await.json();
    assert_eq!((st["available"].clone(), st["state"].clone()), (json!(false), json!("off")));
    assert_eq!(r.admin("PUT", "/admin/tunnel", Some(json!({ "token": "x" }))).await.status, StatusCode::NOT_IMPLEMENTED);
}

// ── What a user can do, on the page (session) and over the API (key) ──

#[tokio::test]
async fn user_features_work_by_session_and_by_key() {
    let r = Relay::start().await;
    let (_, key) = r.user("bob", "bobpassword1").await;
    let session = r.login("bob", "bobpassword1").await;
    for (prefix, token) in [("/user", session.clone()), ("/v1/user", key.clone())] {
        let auth = bearer(&token);
        let h = [("authorization", auth.as_str())];
        assert_eq!(r.public("GET", &format!("{prefix}/api-key"), &h, None).await.json()["api_key"], key.as_str());
        assert_eq!(r.public("GET", &format!("{prefix}/config"), &h, None).await.json(), json!({ "public_url": null }));
        let q = r.public("GET", &format!("{prefix}/quota"), &h, None).await.json();
        assert_eq!(q, json!({ "limits": [], "providers": [] }));
        let u = r.public("GET", &format!("{prefix}/usage?period=month&tz=480"), &h, None).await.json();
        assert_eq!(u, json!({ "series": [], "models": [] }));
    }
    assert_eq!(r.public("GET", "/v1/quota", &[("authorization", &bearer(&key))], None).await.json(), json!({ "limits": [], "providers": [] }));
    // x-api-key works like a bearer key
    assert_eq!(r.public("GET", "/v1/user/api-key", &[("x-api-key", &key)], None).await.status, StatusCode::OK);
    assert_eq!(r.public("GET", "/v1/user/usage", &[], None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.public("GET", "/user/usage", &[("authorization", "Bearer sk-nope")], None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.public("GET", "/user/usage?period=week", &[("authorization", &bearer(&session))], None).await.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn users_change_their_password_with_the_current_one() {
    let r = Relay::start().await;
    let (_, key) = r.user("bob", "bobpassword1").await;
    let auth = bearer(&key);
    let h = [("authorization", auth.as_str())];
    let change = |cur: &str, new: &str| json!({ "current_password": cur, "new_password": new });
    assert_eq!(r.public("PUT", "/v1/user/password", &h, Some(change("wrong", "newpassword9"))).await.status, StatusCode::FORBIDDEN);
    assert_eq!(r.public("PUT", "/v1/user/password", &h, Some(change("bobpassword1", "short"))).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.public("PUT", "/v1/user/password", &h, Some(change("bobpassword1", "newpassword9"))).await.status, StatusCode::NO_CONTENT);
    let old = r.public("POST", "/auth/login", &[], Some(json!({ "username": "bob", "password": "bobpassword1" }))).await;
    assert_eq!(old.status, StatusCode::UNAUTHORIZED);
    let session = r.login("bob", "newpassword9").await;
    assert_eq!(r.public("PUT", "/user/password", &[("authorization", &bearer(&session))], Some(change("newpassword9", "bobpassword1"))).await.status, StatusCode::NO_CONTENT);
    r.login("bob", "bobpassword1").await;
}

#[tokio::test]
async fn users_rotate_their_key() {
    let r = Relay::start().await;
    let (_, key) = r.user("bob", "bobpassword1").await;
    let session = r.login("bob", "bobpassword1").await;
    let rotated = r.public("POST", "/v1/user/api-key/refresh", &[("authorization", &bearer(&key))], None).await;
    assert_eq!(rotated.status, StatusCode::OK);
    let new = rotated.json()["api_key"].as_str().unwrap().to_string();
    assert!(new.starts_with("sk-bob-relay-") && new != key);
    assert_eq!(r.public("GET", "/v1/models", &[("authorization", &bearer(&key))], None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.public("GET", "/v1/models", &[("authorization", &bearer(&new))], None).await.status, StatusCode::OK);
    // The page's session keeps working and sees the new key
    assert_eq!(r.public("GET", "/user/api-key", &[("authorization", &bearer(&session))], None).await.json()["api_key"], new.as_str());
    let again = r.public("POST", "/user/api-key/refresh", &[("authorization", &bearer(&session))], None).await;
    assert_ne!(again.json()["api_key"], new.as_str());
}

// ── Models ──

#[tokio::test]
async fn models_are_listed_under_provider_names_with_upstream_metadata() {
    let r = Relay::start().await;
    r.provider("fake", &["good"]).await;
    let (_, key) = r.user("bob", "bobpassword1").await;
    let auth = bearer(&key);
    let h = [("authorization", auth.as_str())];
    let list = r.public("GET", "/v1/models", &h, None).await.json();
    assert_eq!((list["object"].clone(), list["first_id"].clone(), list["last_id"].clone(), list["has_more"].clone()), (json!("list"), json!("fake/m1"), json!("fake/m2"), json!(false)));
    let m1 = &list["data"][0];
    assert_eq!((m1["id"].clone(), m1["context_length"].clone(), m1["owned_by"].clone(), m1["type"].clone(), m1["display_name"].clone()),
        (json!("fake/m1"), json!(1000), json!("fake"), json!("model"), json!("fake/m1")));
    assert_eq!(r.public("GET", "/models", &h, None).await.json(), list);
    assert_eq!(r.public("GET", "/v1/models/fake/m1", &h, None).await.json(), *m1);
    assert_eq!(r.public("GET", "/v1/models/fake/nope", &h, None).await.status, StatusCode::NOT_FOUND);
}

// ── Relaying ──

#[tokio::test]
async fn chat_is_relayed_on_the_first_working_key_and_counted() {
    let r = Relay::start().await;
    r.provider("fake", &["bad", "good"]).await;
    let (_, key) = r.user("bob", "bobpassword1").await;
    let auth = bearer(&key);
    let h = [("authorization", auth.as_str())];
    let resp = r.public("POST", "/v1/chat/completions", &h, Some(json!({ "model": "fake/m1", "messages": [], "temperature": 0.2 }))).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text);
    assert_eq!(resp.json()["choices"][0]["message"]["content"], "ok");

    // Listing models tried the bad key first and put it in cooldown, so chat goes straight to the good one
    let auths = |path: &str| r.upstream.seen(path).iter().map(|s| s.headers["authorization"].to_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(auths("/v1/models")[..2], ["Bearer bad", "Bearer good"]);
    assert_eq!(auths("/v1/chat/completions"), ["Bearer good"]);
    let sent = &r.upstream.seen("/v1/chat/completions")[0];
    assert_eq!(sent.body, json!({ "model": "m1", "messages": [], "temperature": 0.2 }), "only the model name changes");
    assert_eq!(sent.headers["user-agent"], "test-agent");

    r.public("POST", "/v1/chat/completions", &h, Some(json!({ "model": "m1", "messages": [] }))).await;
    assert_eq!(auths("/v1/chat/completions"), ["Bearer good", "Bearer good"], "bare upstream ids resolve too");

    let usage = r.public("GET", "/v1/user/usage?period=day&tz=0", &h, None).await.json();
    assert_eq!(usage["models"], json!([{ "model": "fake/m1", "requests": 2, "total_tokens": 30 }]));
}

#[tokio::test]
async fn a_refused_key_hands_over_to_the_next() {
    let r = Relay::start().await;
    // A key that lists models fine but is refused for chat
    let id = r.provider("fake", &["chatbad", "good"]).await;
    let (_, key) = r.user("bob", "bobpassword1").await;
    let resp = r.public("POST", "/v1/chat/completions", &[("authorization", &bearer(&key))], Some(json!({ "model": "fake/m1", "messages": [] }))).await;
    assert_eq!(resp.status, StatusCode::OK);
    let auths: Vec<_> = r.upstream.seen("/v1/chat/completions").iter().map(|s| s.headers["authorization"].to_str().unwrap().to_string()).collect();
    assert_eq!(auths, ["Bearer chatbad", "Bearer good"]);
    let keys = r.admin("GET", &format!("/admin/keys/{id}"), None).await.json();
    let bad = keys.as_array().unwrap().iter().find(|k| k["api_key"] == "…tbad").unwrap();
    assert_eq!((bad["status"]["last_result"].clone(), bad["status"]["failed"].clone()), (json!("401"), json!(1)));
    assert!(bad["status"]["cooling_secs"].as_u64().unwrap() > 500);
}

#[tokio::test]
async fn streamed_chat_asks_for_usage_and_is_counted() {
    let r = Relay::start().await;
    r.provider("fake", &["good"]).await;
    let (_, key) = r.user("bob", "bobpassword1").await;
    let auth = bearer(&key);
    let h = [("authorization", auth.as_str())];
    let resp = r.public("POST", "/v1/chat/completions", &h, Some(json!({ "model": "fake/m1", "messages": [], "stream": true }))).await;
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.headers["content-type"], "text/event-stream");
    assert!(resp.text.contains("[DONE]"));
    assert_eq!(r.upstream.seen("/v1/chat/completions")[0].body["stream_options"], json!({ "include_usage": true }));
    // Tokens are written when the stream ends, off the request path
    let mut models = json!(null);
    for _ in 0..50 {
        models = r.public("GET", "/v1/user/usage?period=day", &h, None).await.json()["models"].clone();
        if models[0]["total_tokens"] == 24 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(models, json!([{ "model": "fake/m1", "requests": 1, "total_tokens": 24 }]));
}

#[tokio::test]
async fn anthropic_messages_go_to_the_anthropic_side_with_its_headers() {
    let r = Relay::start().await;
    r.provider("fake", &["good"]).await;
    let (_, key) = r.user("bob", "bobpassword1").await;
    let body = json!({ "model": "fake/m2", "max_tokens": 10, "messages": [] });
    let resp = r.public("POST", "/v1/messages", &[("x-api-key", &key), ("anthropic-beta", "tools-1")], Some(body)).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text);
    let seen = &r.upstream.seen("/v1/messages")[0];
    assert_eq!(seen.headers["x-api-key"], "good");
    assert_eq!(seen.headers["anthropic-version"], "2023-06-01");
    assert_eq!(seen.headers["anthropic-beta"], "tools-1");
    assert!(seen.headers.get("authorization").is_none());
    assert_eq!(seen.body["model"], "m2");
    let usage = r.public("GET", "/v1/user/usage", &[("x-api-key", &key)], None).await.json();
    assert_eq!(usage["models"], json!([{ "model": "fake/m2", "requests": 1, "total_tokens": 5 }]));
}

#[tokio::test]
async fn failures_are_reported_without_the_upstream_body_and_not_counted() {
    let r = Relay::start().await;
    r.provider("fake", &["good"]).await;
    let dead = r.admin("POST", "/admin/providers", Some(json!({ "name": "dead", "base_url": format!("{}/v1", r.upstream.base), "user_agent": "t", "models": "x1" }))).await.json();
    let dead_id = dead["id"].as_i64().unwrap();
    r.admin("POST", &format!("/admin/keys/{dead_id}"), Some(json!({ "api_key": "bad" }))).await;
    r.admin("POST", "/admin/catalog/refresh", None).await;
    let (_, key) = r.user("bob", "bobpassword1").await;
    let auth = bearer(&key);
    let h = [("authorization", auth.as_str())];

    let refused = r.public("POST", "/v1/chat/completions", &h, Some(json!({ "model": "dead/x1", "messages": [] }))).await;
    assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
    assert_eq!(refused.json()["error"]["type"], "api_error");
    assert!(!refused.text.contains("a@b.c"), "upstream errors can name the account");

    let unknown = r.public("POST", "/v1/chat/completions", &h, Some(json!({ "model": "nope", "messages": [] }))).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.json()["error"]["type"], "not_found_error");
    assert_eq!(r.public("POST", "/v1/chat/completions", &h, Some(json!({ "messages": [] }))).await.status, StatusCode::BAD_REQUEST);

    let usage = r.public("GET", "/v1/user/usage", &h, None).await.json();
    assert_eq!(usage["models"], json!([]));
}

#[tokio::test]
async fn limits_stop_requests_and_show_on_the_quota() {
    let r = Relay::start().await;
    r.provider("fake", &["good"]).await;
    let (id, key) = r.user("bob", "bobpassword1").await;
    let auth = bearer(&key);
    let h = [("authorization", auth.as_str())];
    assert_eq!(r.admin("POST", "/admin/limits", Some(json!({ "user_id": id, "model": "fake/nope", "max_requests": 0 }))).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.admin("POST", "/admin/limits", Some(json!({ "user_id": id, "window_secs": 0 }))).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    let blocked = r.admin("POST", "/admin/limits", Some(json!({ "user_id": id, "model": "fake/m2", "max_requests": 0 }))).await;
    assert_eq!(blocked.status, StatusCode::OK);
    r.admin("POST", "/admin/limits", Some(json!({ "user_id": id, "window_secs": 18000, "max_tokens": 20 }))).await;

    let no = r.public("POST", "/v1/chat/completions", &h, Some(json!({ "model": "fake/m2", "messages": [] }))).await;
    assert_eq!(no.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(no.json()["error"]["type"], "rate_limit_error");
    assert_eq!(r.public("POST", "/v1/chat/completions", &h, Some(json!({ "model": "fake/m1", "messages": [] }))).await.status, StatusCode::OK);
    assert_eq!(r.public("POST", "/v1/chat/completions", &h, Some(json!({ "model": "fake/m1", "messages": [] }))).await.status, StatusCode::OK);
    // 30 tokens used against a cap of 20
    assert_eq!(r.public("POST", "/v1/chat/completions", &h, Some(json!({ "model": "fake/m1", "messages": [] }))).await.status, StatusCode::TOO_MANY_REQUESTS);

    let limits = r.public("GET", "/v1/quota", &h, None).await.json()["limits"].clone();
    let shape: Vec<_> = limits.as_array().unwrap().iter().map(|l| (l["model"].clone(), l["window_secs"].clone(), l["max_requests"].clone(), l["max_tokens"].clone(), l["used"].clone())).collect();
    assert_eq!(shape, vec![
        (json!("*"), json!(18000), json!(-1), json!(20), json!({ "requests": 2, "total_tokens": 30 })),
        (json!("fake/m2"), json!(86400), json!(0), json!(-1), json!({ "requests": 0, "total_tokens": 0 })),
    ]);
    let all = r.admin("GET", "/admin/limits", None).await.json();
    assert_eq!(all.as_array().unwrap().len(), 2);
    let lid = all[0]["id"].as_i64().unwrap();
    assert_eq!(r.admin("DELETE", &format!("/admin/limits/{lid}"), None).await.status, StatusCode::NO_CONTENT);
    assert_eq!(r.admin("GET", "/admin/limits", None).await.json().as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn files_are_for_admins_only() {
    let r = Relay::start().await;
    let (_, key) = r.user("bob", "bobpassword1").await;
    let resp = r.public("GET", "/v1/files", &[("authorization", &bearer(&key))], None).await;
    assert_eq!(resp.status, StatusCode::FORBIDDEN);
    let yao = r.admin("GET", "/admin/users", None).await.json()[0]["api_key"].as_str().unwrap().to_string();
    let bad_id = r.lan("GET", "/v1/files/..%2Fsecrets", &[("authorization", &bearer(&yao))], None).await;
    assert_eq!(bad_id.status, StatusCode::BAD_REQUEST);
    // No Kimi provider here
    assert_eq!(r.lan("GET", "/v1/files", &[("authorization", &bearer(&yao))], None).await.status, StatusCode::NOT_FOUND);
}

// ── Usage over periods ──

#[tokio::test]
async fn usage_is_bucketed_by_the_viewers_calendar() {
    let r = Relay::start().await;
    let (id, key) = r.user("bob", "bobpassword1").await;
    // Times relative to "now" in UTC+8, written in UTC as the relay stores them
    let local = "datetime('now', '+480 minutes')";
    let at = |local_expr: &str| format!("datetime({local_expr}, '-480 minutes')");
    let row = |model: &str, tokens: i64, when: String| format!(
        "INSERT INTO usage_logs (user_id, model, input_tokens, output_tokens, created_at) VALUES ({id}, '{model}', {tokens}, 0, {when})");
    r.sql(&row("a/x", 100, at(local))).await;
    r.sql(&row("b/y", 10, at(&format!("datetime({local}, 'start of day', '+30 minutes')")))).await;
    for _ in 0..3 {
        r.sql(&row("a/x", 1000, at(&format!("datetime({local}, 'start of month', '+12 hours')")))).await;
    }
    r.sql(&row("a/x", 7, at(&format!("datetime({local}, 'start of year', '+12 hours')")))).await;
    r.sql(&row("a/x", 9, at(&format!("datetime({local}, '-1 year')")))).await;

    let auth = bearer(&key);
    let h = [("authorization", auth.as_str())];
    let requests = |v: &serde_json::Value| v["series"].as_array().unwrap().iter().map(|b| b["requests"].as_i64().unwrap()).sum::<i64>();
    let day = r.public("GET", "/v1/user/usage?period=day&tz=480", &h, None).await.json();
    let month = r.public("GET", "/v1/user/usage?period=month&tz=480", &h, None).await.json();
    let year = r.public("GET", "/v1/user/usage?period=year&tz=480", &h, None).await.json();
    // Early in a month or year the planted rows can coincide; the counts only grow outward
    assert_eq!(requests(&day), 2);
    assert!(requests(&month) >= 5 && requests(&year) >= requests(&month) && requests(&year) <= 6, "{month} {year}");
    assert_eq!(day["series"][0]["bucket"], "00");
    assert_eq!(day["models"], json!([{ "model": "a/x", "requests": 1, "total_tokens": 100 }, { "model": "b/y", "requests": 1, "total_tokens": 10 }]));
    // Viewed from UTC, 00:30 in UTC+8 belongs to yesterday (unless it's before 08:00 UTC… then it's still today there too)
    let utc_day = r.public("GET", "/v1/user/usage?period=day&tz=0", &h, None).await.json();
    assert!(requests(&utc_day) <= 2);

    let admin = r.admin("GET", "/admin/usage?period=day&tz=480", None).await.json();
    assert_eq!(admin["rows"].as_array().unwrap().len(), 2);
    assert_eq!(admin["rows"][0]["username"], "bob");
    assert_eq!(r.admin("GET", "/admin/usage", None).await.status, StatusCode::OK);
    assert_eq!(r.admin("GET", "/admin/usage?period=week", None).await.status, StatusCode::BAD_REQUEST);
}
