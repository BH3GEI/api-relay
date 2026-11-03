//! The relay's behavior as its users and admin see it, over HTTP.

mod common;

use axum::http::StatusCode;
use common::{Relay, bearer};
use serde_json::{Value, json};

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
    // Behind the gateway its headers alone make an admin (a plain download link); not a LazyCat user's
    assert_eq!(r.lan("GET", "/admin/users", &forged, None).await.status, StatusCode::OK);
    assert_eq!(r.lan("GET", "/admin/users", &[("x-hc-user-id", "someone")], None).await.status, StatusCode::UNAUTHORIZED);

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
        let start = page.text.find("/static/usage-chart.js?v=").unwrap_or_else(|| panic!("{side_page} has no hashed script URL"));
        let url: String = page.text[start..].chars().take_while(|c| *c != '"').collect();
        let js = r.lan("GET", &url, &[], None).await;
        assert_eq!(js.status, StatusCode::OK);
        assert!(js.headers["content-type"].to_str().unwrap().contains("javascript"));
        assert!(js.text.contains("UsageView"));
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
    let (other, _) = r.user("carol", "carolpassword").await;
    for user in [id, other] {
        r.admin("POST", "/admin/limits", Some(json!({ "user_id": user, "max_requests": 5 }))).await;
    }
    assert_eq!(r.admin("DELETE", &format!("/admin/users/{id}"), None).await.status, StatusCode::NO_CONTENT);
    assert_eq!(r.lan("GET", "/v1/models", &[("authorization", &bearer(&key))], None).await.status, StatusCode::UNAUTHORIZED);
    // Their limits go with them; others' stay
    let left: Vec<_> = r.admin("GET", "/admin/limits", None).await.json().as_array().unwrap().iter().map(|l| l["user_id"].as_i64().unwrap()).collect();
    assert_eq!(left, [other]);
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
    assert_eq!(cfg, json!({ "public_url": "https://relay.example.com", "backup_url": null }));
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
        assert_eq!(r.public("GET", &format!("{prefix}/config"), &h, None).await.json(), json!({ "public_url": null, "backup_url": null }));
        let q = r.public("GET", &format!("{prefix}/quota"), &h, None).await.json();
        assert_eq!(q, json!({ "limits": [], "providers": [] }));
        let u = r.public("GET", &format!("{prefix}/usage?period=month&tz=480"), &h, None).await.json();
        assert_eq!((&u["series"], &u["models"], &u["totals"]["total_tokens"], &u["previous"]), (&json!([]), &json!([]), &json!(0), &json!(null)));
    }
    assert_eq!(r.public("GET", "/v1/quota", &[("authorization", &bearer(&key))], None).await.json(), json!({ "limits": [], "providers": [] }));
    // x-api-key works like a bearer key
    assert_eq!(r.public("GET", "/v1/user/api-key", &[("x-api-key", &key)], None).await.status, StatusCode::OK);
    assert_eq!(r.public("GET", "/v1/user/usage", &[], None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.public("GET", "/user/usage", &[("authorization", "Bearer sk-nope")], None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.public("GET", "/user/usage?period=decade", &[("authorization", &bearer(&session))], None).await.status, StatusCode::BAD_REQUEST);
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

#[tokio::test]
async fn admins_add_models_and_declare_context_lengths() {
    let r = Relay::start().await;
    let id = r.provider("fake", &["good"]).await;
    let (_, key) = r.user("bob", "bobpassword1").await;
    let auth = bearer(&key);
    let h = [("authorization", auth.as_str())];
    let p = r.admin("PUT", &format!("/admin/providers/{id}"), Some(json!({
        "name": "fake", "base_url": format!("{}/v1", r.upstream.base), "user_agent": "test-agent", "models": "m1=5000, m3=7000 m4",
    }))).await;
    assert_eq!(p.status, StatusCode::OK, "{}", p.text);
    r.admin("POST", "/admin/catalog/refresh", None).await;

    let list = r.public("GET", "/v1/models", &h, None).await.json();
    let context = |id: &str| list["data"].as_array().unwrap().iter().find(|m| m["id"] == id).map(|m| m["context_length"].clone());
    // Declared lengths win over the upstream's; models it doesn't list are added
    assert_eq!([context("fake/m1"), context("fake/m2"), context("fake/m3"), context("fake/m4")],
        [Some(json!(5000)), Some(json!(null)), Some(json!(7000)), Some(json!(null))]);
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
    assert_eq!(brief(&usage["models"]), json!([{ "model": "fake/m1", "requests": 2, "total_tokens": 30 }]));
}

#[tokio::test]
async fn a_busy_key_is_waited_for_and_a_spent_one_set_aside() {
    let r = Relay::start().await;
    let id = r.provider("fake", &["spent", "busy2"]).await;
    let (_, key) = r.user("bob", "bobpassword1").await;
    let resp = r.public("POST", "/v1/chat/completions", &[("authorization", &bearer(&key))], Some(json!({ "model": "fake/m1", "messages": [] }))).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text);
    let auths: Vec<_> = r.upstream.seen("/v1/chat/completions").iter().map(|s| s.headers["authorization"].to_str().unwrap().to_string()).collect();
    assert_eq!(auths, ["Bearer spent", "Bearer busy2", "Bearer busy2", "Bearer busy2"], "only the busy key is tried again");
    let keys = r.admin("GET", &format!("/admin/keys/{id}"), None).await.json();
    let cooling = |tail: &str| keys.as_array().unwrap().iter().find(|k| k["api_key"] == tail).unwrap()["status"]["cooling_secs"].as_u64().unwrap();
    assert!(cooling("…pent") > 1000, "a spent key waits for its quota");
    assert_eq!(cooling("…usy2"), 0);
    // While it cools, the spent key is left out
    r.public("POST", "/v1/chat/completions", &[("authorization", &bearer(&key))], Some(json!({ "model": "fake/m1", "messages": [] }))).await;
    assert_eq!(r.upstream.seen("/v1/chat/completions").len(), 5);

    // The admin reorders keys by priority
    let spent = keys.as_array().unwrap().iter().find(|k| k["api_key"] == "…pent").unwrap()["id"].as_i64().unwrap();
    let set = r.admin("PUT", &format!("/admin/keys/priority/{spent}"), Some(json!({ "priority": 9 }))).await;
    assert_eq!(set.json()["priority"], 9);
    let keys = r.admin("GET", &format!("/admin/keys/{id}"), None).await.json();
    assert_eq!(keys[1]["api_key"], "…pent");
    assert_eq!(r.admin("PUT", "/admin/keys/priority/999", Some(json!({ "priority": 1 }))).await.status, StatusCode::NOT_FOUND);
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
    assert_eq!(models, json!([{ "model": "fake/m1", "requests": 1, "input_tokens": 20, "output_tokens": 4, "total_tokens": 24 }]));
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
    assert_eq!(brief(&usage["models"]), json!([{ "model": "fake/m2", "requests": 1, "total_tokens": 5 }]));
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
async fn a_provider_limit_covers_all_its_models() {
    let r = Relay::start().await;
    r.provider("fake", &["good"]).await;
    let (id, key) = r.user("bob", "bobpassword1").await;
    let auth = bearer(&key);
    let h = [("authorization", auth.as_str())];
    assert_eq!(r.admin("POST", "/admin/limits", Some(json!({ "user_id": id, "model": "nope/*", "max_requests": 0 }))).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    // Two requests a day across every fake model, whichever one they go to
    assert_eq!(r.admin("POST", "/admin/limits", Some(json!({ "user_id": id, "model": "fake/*", "max_requests": 2 }))).await.status, StatusCode::OK);
    for (model, status) in [("fake/m1", StatusCode::OK), ("fake/m2", StatusCode::OK), ("fake/m1", StatusCode::TOO_MANY_REQUESTS), ("fake/m2", StatusCode::TOO_MANY_REQUESTS)] {
        assert_eq!(r.public("POST", "/v1/chat/completions", &h, Some(json!({ "model": model, "messages": [] }))).await.status, status, "{model}");
    }
    let limits = r.public("GET", "/v1/quota", &h, None).await.json()["limits"].clone();
    assert_eq!((limits[0]["model"].clone(), limits[0]["used"]["requests"].clone()), (json!("fake/*"), json!(2)));
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
    // On the 1st, the month's planted rows are today's too: today counts what the month's bucket for today does
    let today = &day["series"][0]["bucket"].as_str().unwrap()[..10];
    let month_today = month["series"].as_array().unwrap().iter().filter(|b| b["bucket"] == today).map(|b| b["requests"].as_i64().unwrap()).sum::<i64>();
    assert!(requests(&day) >= 2 && requests(&day) == month_today, "{day} {month}");
    assert!(requests(&month) >= 5 && requests(&year) >= requests(&month) && requests(&year) <= 6, "{month} {year}");
    assert!(day["series"][0]["bucket"].as_str().unwrap().ends_with(" 00"), "{day}");
    assert!(brief(&day["models"]).as_array().unwrap().contains(&json!({ "model": "b/y", "requests": 1, "total_tokens": 10 })), "{day}");
    // Viewed from UTC, 00:30 in UTC+8 is 16:30 the day before: in UTC's today only while UTC is
    // still on that day (00:00–08:00 in UTC+8)
    let utc_day = r.public("GET", "/v1/user/usage?period=day&tz=0", &h, None).await.json();
    let in_utc_today = day["from"].as_i64().unwrap() + 30 * 60 >= utc_day["from"].as_i64().unwrap();
    assert_eq!(utc_day["models"].as_array().unwrap().iter().any(|m| m["model"] == "b/y"), in_utc_today, "{utc_day}");

    let admin = r.admin("GET", "/admin/usage?period=day&tz=480", None).await.json();
    assert!(admin["rows"].as_array().unwrap().len() == 2, "{admin}");
    assert_eq!(admin["rows"][0]["username"], "bob");
    assert_eq!(r.admin("GET", "/admin/usage", None).await.status, StatusCode::OK);
    let week = r.admin("GET", "/admin/usage?period=week&tz=480", None).await.json();
    assert!(requests(&week) >= 2 && requests(&week) <= requests(&year), "{week}");
    assert_eq!(week["to"].as_i64().unwrap() - week["from"].as_i64().unwrap(), 7 * 86400);
    assert_eq!(week["step"], "day");
}

/// `models` entries without the input/output split
fn brief(models: &serde_json::Value) -> serde_json::Value {
    models.as_array().unwrap().iter()
        .map(|m| json!({ "model": m["model"], "requests": m["requests"], "total_tokens": m["total_tokens"] }))
        .collect()
}

#[tokio::test]
async fn usage_ranges_split_filter_and_compare() {
    let r = Relay::start().await;
    let (bob, key) = r.user("bob", "bobpassword1").await;
    let (amy, _) = r.user("amy", "amypassword1").await;
    // 2026-01-01 00:00 UTC, and rows an hour, a day and a month after it
    let t0: i64 = 1767225600;
    let row = |user: i64, model: &str, input: i64, output: i64, at: i64| format!(
        "INSERT INTO usage_logs (user_id, model, input_tokens, output_tokens, created_at) VALUES ({user}, '{model}', {input}, {output}, datetime({at}, 'unixepoch'))");
    r.sql(&row(bob, "kimi/k3", 100, 10, t0 + 3600)).await;
    r.sql(&row(bob, "kimi/k2", 200, 20, t0 + 3600)).await;
    r.sql(&row(bob, "ark/a", 300, 30, t0 + 86400 + 60)).await;
    r.sql(&row(amy, "kimi/k3", 1000, 100, t0 + 3600)).await;
    r.sql(&row(bob, "kimi/k3", 5, 5, t0 - 3600)).await;

    let auth = bearer(&key);
    let h = [("authorization", auth.as_str())];
    let get = |q: String| { let h = h; let r = &r; async move { r.public("GET", &format!("/v1/user/usage?{q}"), &h, None).await } };

    // Two days at UTC: picked by hour, split by provider
    let u = get(format!("from={t0}&to={}&tz=0&group=provider", t0 + 2 * 86400)).await.json();
    assert_eq!(u["step"], "hour");
    assert_eq!(u["totals"], json!({ "requests": 3, "input_tokens": 600, "output_tokens": 60, "total_tokens": 660 }));
    let series: Vec<_> = u["series"].as_array().unwrap().iter().map(|b| (b["bucket"].as_str().unwrap(), b["group"].as_str().unwrap(), b["total_tokens"].as_i64().unwrap())).collect();
    assert_eq!(series, [("2026-01-01 01", "kimi", 330), ("2026-01-02 00", "ark", 330)]);
    assert_eq!(u["present_models"], json!(["ark/a", "kimi/k2", "kimi/k3"]));
    assert_eq!(u["present_users"], json!([]), "a user sees no one else");
    assert_eq!(u["models"][0]["model"], "ark/a");

    // By day in UTC+8; one provider; against the day before
    let u = get(format!("from={}&to={}&tz=480&step=day&model=kimi/*&compare_from={}&compare_to={}", t0, t0 + 2 * 86400, t0 - 86400, t0)).await.json();
    let series: Vec<_> = u["series"].as_array().unwrap().iter().map(|b| (b["bucket"].as_str().unwrap(), b["group"].as_str().unwrap(), b["requests"].as_i64().unwrap())).collect();
    assert_eq!(series, [("2026-01-01", "", 2)]);
    assert_eq!(u["previous"]["total_tokens"], 10);
    assert_eq!(u["present_models"], json!(["ark/a", "kimi/k2", "kimi/k3"]), "choices ignore the model filter");
    // One model exactly
    let u = get(format!("from={t0}&to={}&model=kimi/k3", t0 + 86400)).await.json();
    assert_eq!(u["totals"]["total_tokens"], 110);

    // Users can't look at others, nor split by user
    for q in [format!("from={t0}&to={}&group=user", t0 + 86400), format!("from={t0}&to={}&user={amy}", t0 + 86400)] {
        assert_eq!(get(q).await.status, StatusCode::BAD_REQUEST);
    }
    // Ranges must be sane and charts bounded
    for q in [format!("from={t0}"), format!("from={t0}&to={t0}"), format!("from={t0}&to={}&step=hour", t0 + 40 * 86400), format!("from={t0}&to={}&compare_from=1", t0 + 86400)] {
        assert_eq!(get(q.clone()).await.status, StatusCode::BAD_REQUEST, "{q}");
    }
    assert_eq!(get(format!("from={t0}&to={}", t0 + 40 * 86400)).await.json()["step"], "day");
    assert_eq!(get(format!("from={t0}&to={}", t0 + 400 * 86400)).await.json()["step"], "month");

    // The admin sees everyone, split by user, or one user
    let a = r.admin("GET", &format!("/admin/usage?from={t0}&to={}&group=user", t0 + 86400), None).await.json();
    let groups: Vec<_> = a["series"].as_array().unwrap().iter().map(|b| (b["group"].as_str().unwrap(), b["total_tokens"].as_i64().unwrap())).collect();
    assert_eq!(groups, [("amy", 1100), ("bob", 330)]);
    assert_eq!(a["present_users"], json!([{ "id": amy, "username": "amy" }, { "id": bob, "username": "bob" }]));
    assert_eq!(a["rows"][0]["username"], "amy");
    let a = r.admin("GET", &format!("/admin/usage?from={t0}&to={}&user={amy}", t0 + 86400), None).await.json();
    assert_eq!(a["totals"]["total_tokens"], 1100);
    assert_eq!(a["present_users"].as_array().unwrap().len(), 2, "user choices ignore the user filter");
}

/// The admin's list of recorded calls, once it holds `n` (records are written in the background).
async fn recorded(r: &Relay, n: usize) -> Value {
    for _ in 0..100 {
        let list = r.admin("GET", "/admin/requests", None).await.json();
        if list.as_array().unwrap().len() >= n {
            return list;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("fewer than {n} calls recorded");
}

#[tokio::test]
async fn every_call_is_recorded_in_full_unless_the_user_opts_out() {
    let r = Relay::start().await;
    r.provider("fake", &["spent", "good"]).await;
    let (user_id, key) = r.user("bob", "bobpassword1").await;
    let auth = [("authorization", bearer(&key)), ("user-agent", "pi/1".to_string())];
    let auth: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let system = "You are a careful agent. ".repeat(20);
    let question = json!({ "role": "user", "content": "Explain the key pool. ".repeat(10) });
    let turn1 = json!({ "model": "fake/m1", "messages": [{ "role": "system", "content": system }, question] });
    let resp = r.public("POST", "/v1/chat/completions", &auth, Some(turn1)).await;
    assert_eq!(resp.status, StatusCode::OK);

    let list = recorded(&r, 1).await;
    let call = &list[0];
    assert_eq!((call["user_id"].as_i64(), call["username"].clone(), call["model"].clone(), call["provider"].clone()), (Some(user_id), json!("bob"), json!("fake/m1"), json!("fake")));
    assert_eq!((call["status"].clone(), call["key"].clone(), call["entry"].clone(), call["client_ip"].clone()), (json!(200), json!("good"), json!("tunnel"), json!("198.51.100.1")));
    assert_eq!((call["input_tokens"].clone(), call["output_tokens"].clone(), call["complete"].clone(), call["content"].clone()), (json!(10), json!(5), json!(true), json!(true)));
    let detail = r.admin("GET", &format!("/admin/requests/{}", call["id"]), None).await.json();
    // The request as sent upstream, with its shared parts back in place
    assert_eq!(detail["request"], r.upstream.seen("/v1/chat/completions").last().unwrap().body);
    assert!(detail["response"].as_str().unwrap().contains("\"ok\""));
    assert_eq!(detail["user_agent"], "pi/1");
    assert!(detail["headers"].get("authorization").is_none(), "credentials aren't kept");
    let attempts: Vec<_> = detail["attempts"].as_array().unwrap().iter().map(|a| (a["key"].clone(), a["status"].clone(), a["failure"].clone())).collect();
    assert_eq!(attempts, [(json!("pent"), json!(429), json!("OutOfQuota")), (json!("good"), json!(200), Value::Null)]);

    // The next turn resends the conversation; only the new message is stored anew
    let parts = r.admin("GET", "/admin/requests/stats", None).await.json()["parts"].as_i64().unwrap();
    assert_eq!(parts, 2);
    let answer = json!({ "role": "assistant", "content": "The pool tries keys in priority order. ".repeat(5) });
    let turn2 = json!({ "model": "fake/m1", "stream": true, "messages": [{ "role": "system", "content": system }, question, answer] });
    assert_eq!(r.public("POST", "/v1/chat/completions", &auth, Some(turn2)).await.status, StatusCode::OK);
    let list = recorded(&r, 2).await;
    assert_eq!((list[0]["stream"].clone(), list[0]["input_tokens"].clone(), list[0]["complete"].clone()), (json!(true), json!(20), json!(true)));
    assert_eq!(r.admin("GET", "/admin/requests/stats", None).await.json()["parts"], 3);
    let detail = r.admin("GET", &format!("/admin/requests/{}", list[0]["id"]), None).await.json();
    assert_eq!(detail["request"]["messages"][2], answer);
    assert!(detail["response"].as_str().unwrap().starts_with("data: "));

    // The relay's own refusals are recorded too
    let unknown = r.public("POST", "/v1/chat/completions", &auth, Some(json!({ "model": "nope", "messages": [] }))).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    let list = recorded(&r, 3).await;
    assert_eq!((list[0]["status"].clone(), list[0]["error"].clone()), (json!(404), json!("Unknown model nope")));

    // Opted out: what a call was is still recorded, not what it said
    assert_eq!(r.public("GET", "/v1/user/recording", &auth, None).await.json()["content"], true);
    let off = r.public("PUT", "/v1/user/recording", &auth, Some(json!({ "content": false }))).await;
    assert_eq!(off.json()["content"], false);
    r.public("POST", "/v1/chat/completions", &auth, Some(json!({ "model": "fake/m1", "messages": [question] }))).await;
    let list = recorded(&r, 4).await;
    assert_eq!((list[0]["content"].clone(), list[0]["input_tokens"].clone()), (json!(false), json!(10)));
    let detail = r.admin("GET", &format!("/admin/requests/{}", list[0]["id"]), None).await.json();
    assert_eq!((detail["request"].clone(), detail["response"].clone()), (Value::Null, Value::Null));

    // Filters, paging and the export
    let errors = r.admin("GET", "/admin/requests?status=error", None).await.json();
    assert_eq!(errors.as_array().unwrap().len(), 1);
    let page = r.admin("GET", &format!("/admin/requests?limit=2&before={}", list[1]["id"]), None).await.json();
    assert_eq!(page.as_array().unwrap().iter().map(|c| c["id"].clone()).collect::<Vec<_>>(), [list[2]["id"].clone(), list[3]["id"].clone()]);
    let export = r.admin("GET", &format!("/admin/requests/export?user={user_id}&model=fake/*"), None).await;
    let lines: Vec<Value> = export.text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[1]["request"]["messages"][2], answer);
    // The estimate counts the requests and responses, which the export adds its metadata to
    let size = r.admin("GET", &format!("/admin/requests/export/size?user={user_id}&model=fake/*"), None).await.json();
    let bytes = size["bytes"].as_u64().unwrap() as usize;
    assert!(bytes > 0 && bytes < export.text.len(), "{bytes} vs {}", export.text.len());
    // Users can't see the record
    assert_eq!(r.lan("GET", "/admin/requests", &auth, None).await.status, StatusCode::UNAUTHORIZED);
}
