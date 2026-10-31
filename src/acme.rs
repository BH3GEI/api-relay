//! A Let's Encrypt certificate for the frp listener, proven over DNS with a Cloudflare API token.
//!
//! frps only forwards high ports, so the HTTP challenge (port 80) is out; the DNS challenge
//! puts a TXT record in the domain's Cloudflare zone instead.

use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, BodyWrapper, BytesResponse, ChallengeType, HttpClient, Identifier,
    LetsEncrypt, NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{future::Future, pin::Pin, time::Duration};

use crate::db::{self, Db};

const CF_API: &str = "https://api.cloudflare.com/client/v4";
/// How long to wait for the TXT record to be visible before letting Let's Encrypt look
const PROPAGATION: Duration = Duration::from_secs(120);

/// A certificate chain and its key, as PEM.
pub struct Issued {
    pub cert_pem: String,
    pub key_pem: String,
}

/// Get a certificate for `domain`, reusing the stored ACME account or registering one.
pub async fn issue(db: &Db, http: &reqwest::Client, dns_token: &str, domain: &str) -> Result<Issued, String> {
    let account = account(db, http).await?;
    let zone = zone_id(http, dns_token, domain).await?;

    let ids = [Identifier::Dns(domain.to_string())];
    let mut order = account.new_order(&NewOrder::new(&ids)).await.map_err(|e| format!("new order: {e}"))?;
    let mut records = Vec::new();
    let result = async {
        let mut authorizations = order.authorizations();
        while let Some(authz) = authorizations.next().await {
            let mut authz = authz.map_err(|e| format!("authorization: {e}"))?;
            match authz.status {
                AuthorizationStatus::Pending => {}
                AuthorizationStatus::Valid => continue,
                s => return Err(format!("authorization is {s:?}")),
            }
            let mut challenge = authz.challenge(ChallengeType::Dns01).ok_or("no DNS challenge offered")?;
            let value = challenge.key_authorization().dns_value();
            let name = format!("_acme-challenge.{domain}");
            records.push(create_txt(http, dns_token, &zone, &name, &value).await?);
            wait_visible(http, &name, &value).await;
            challenge.set_ready().await.map_err(|e| format!("challenge: {e}"))?;
        }
        let retry = RetryPolicy::new().timeout(Duration::from_secs(180));
        let status = order.poll_ready(&retry).await.map_err(|e| format!("validation: {e}"))?;
        if status != OrderStatus::Ready {
            return Err(format!("order is {status:?}"));
        }
        let key_pem = order.finalize().await.map_err(|e| format!("finalize: {e}"))?;
        let cert_pem = order.poll_certificate(&retry).await.map_err(|e| format!("certificate: {e}"))?;
        Ok(Issued { cert_pem, key_pem })
    }.await;
    for id in records {
        let _ = cf(http, dns_token, reqwest::Method::DELETE, &format!("/zones/{zone}/dns_records/{id}"), None).await;
    }
    result
}

/// The relay's Let's Encrypt account, registered once and kept in the database.
async fn account(db: &Db, http: &reqwest::Client) -> Result<Account, String> {
    let builder = || Account::builder_with_http(Box::new(Reqwest(http.clone())));
    if let Some(saved) = db::settings::get_nonempty(db, db::settings::ACME_ACCOUNT).await.map_err(|e| e.to_string())? {
        let creds: AccountCredentials = serde_json::from_str(&saved).map_err(|e| format!("stored account: {e}"))?;
        return builder().from_credentials(creds).await.map_err(|e| format!("account: {e}"));
    }
    let new = NewAccount { contact: &[], terms_of_service_agreed: true, only_return_existing: false };
    let (account, creds) = builder().create(&new, LetsEncrypt::Production.url().to_owned(), None).await
        .map_err(|e| format!("register account: {e}"))?;
    let saved = serde_json::to_string(&creds).map_err(|e| e.to_string())?;
    db::settings::set(db, db::settings::ACME_ACCOUNT, &saved).await.map_err(|e| e.to_string())?;
    Ok(account)
}

/// The Cloudflare zone holding `domain`: the longest of its suffixes that is a zone.
async fn zone_id(http: &reqwest::Client, token: &str, domain: &str) -> Result<String, String> {
    let labels: Vec<&str> = domain.split('.').collect();
    for i in 0..labels.len().saturating_sub(1) {
        let name = labels[i..].join(".");
        let found = cf(http, token, reqwest::Method::GET, &format!("/zones?name={name}"), None).await?;
        if let Some(id) = found[0]["id"].as_str() {
            return Ok(id.to_string());
        }
    }
    Err(format!("no Cloudflare zone this token can see holds {domain}"))
}

async fn create_txt(http: &reqwest::Client, token: &str, zone: &str, name: &str, value: &str) -> Result<String, String> {
    let body = json!({ "type": "TXT", "name": name, "content": format!("\"{value}\""), "ttl": 60 });
    let rec = cf(http, token, reqwest::Method::POST, &format!("/zones/{zone}/dns_records"), Some(body)).await?;
    rec["id"].as_str().map(str::to_string).ok_or_else(|| "Cloudflare returned no record id".into())
}

/// Wait until Cloudflare's resolver returns the record, so Let's Encrypt won't look too early.
async fn wait_visible(http: &reqwest::Client, name: &str, value: &str) {
    let deadline = tokio::time::Instant::now() + PROPAGATION;
    while tokio::time::Instant::now() < deadline {
        let answer = http.get("https://1.1.1.1/dns-query").query(&[("name", name), ("type", "TXT")])
            .header("accept", "application/dns-json").timeout(Duration::from_secs(10))
            .send().await.ok();
        if let Some(r) = answer {
            if r.text().await.is_ok_and(|t| t.contains(value)) {
                return;
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// One Cloudflare API call; its `result`, or the errors it reported.
async fn cf(http: &reqwest::Client, token: &str, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, String> {
    let mut req = http.request(method, format!("{CF_API}{path}")).bearer_auth(token).timeout(Duration::from_secs(30));
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp: Value = req.send().await.map_err(|e| format!("Cloudflare: {e}"))?.json().await.map_err(|e| format!("Cloudflare: {e}"))?;
    if resp["success"].as_bool() != Some(true) {
        return Err(format!("Cloudflare: {}", resp["errors"]));
    }
    Ok(resp["result"].clone())
}

/// When the first certificate in a PEM chain expires, as seconds since the epoch.
pub fn expires_at(cert_pem: &str) -> Option<i64> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes()).ok()?;
    Some(pem.parse_x509().ok()?.validity().not_after.timestamp())
}

/// ACME over the relay's own HTTP client, which carries its own root certificates; the
/// container may have none of its own.
struct Reqwest(reqwest::Client);

impl HttpClient for Reqwest {
    fn request(&self, req: http::Request<BodyWrapper<bytes::Bytes>>) -> Pin<Box<dyn Future<Output = Result<BytesResponse, instant_acme::Error>> + Send>> {
        let client = self.0.clone();
        Box::pin(async move {
            let (parts, body) = req.into_parts();
            let body = body.collect().await.map(|b| b.to_bytes()).unwrap_or_default();
            let resp = client.request(parts.method, parts.uri.to_string()).headers(parts.headers).body(body)
                .timeout(Duration::from_secs(60))
                .send().await.map_err(|e| instant_acme::Error::Other(Box::new(e)))?;
            let mut builder = http::Response::builder().status(resp.status());
            for (k, v) in resp.headers() {
                builder = builder.header(k, v);
            }
            let bytes = resp.bytes().await.map_err(|e| instant_acme::Error::Other(Box::new(e)))?;
            let (parts, _) = builder.body(()).map_err(|e| instant_acme::Error::Other(Box::new(e)))?.into_parts();
            Ok(BytesResponse { parts, body: Box::new(bytes) })
        })
    }
}
