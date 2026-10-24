//! The two pages and the files they load, built into the binary.
//!
//! Pages refer to their files as `/static/<name>`; when served, each such URL gets the file's
//! content hash appended, because Cloudflare caches scripts and styles for hours and a changed
//! file must not be served stale under its old URL.

use axum::{
    extract::Path,
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use std::sync::OnceLock;

const JS: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";

/// (name, content type, content)
const FILES: &[(&str, &str, &str)] = &[
    ("base.css", CSS, include_str!("../static/base.css")),
    ("index.css", CSS, include_str!("../static/index.css")),
    ("admin.css", CSS, include_str!("../static/admin.css")),
    ("common.js", JS, include_str!("../static/common.js")),
    ("index.js", JS, include_str!("../static/index.js")),
    ("admin.js", JS, include_str!("../static/admin.js")),
    ("usage-chart.js", JS, include_str!("../static/usage-chart.js")),
];

const INDEX: &str = include_str!("../static/index.html");
const ADMIN: &str = include_str!("../static/admin.html");

fn hash(content: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut h);
    format!("{:x}", h.finish())
}

fn versioned(html: &str) -> String {
    FILES.iter().fold(html.to_string(), |html, (name, _, content)| {
        html.replace(&format!("\"/static/{name}\""), &format!("\"/static/{name}?v={}\"", hash(content)))
    })
}

pub async fn index() -> Html<&'static str> {
    static PAGE: OnceLock<String> = OnceLock::new();
    Html(PAGE.get_or_init(|| versioned(INDEX)))
}

pub async fn admin() -> Html<&'static str> {
    static PAGE: OnceLock<String> = OnceLock::new();
    Html(PAGE.get_or_init(|| versioned(ADMIN)))
}

pub async fn file(Path(name): Path<String>) -> Response {
    match FILES.iter().find(|(n, _, _)| *n == name) {
        Some((_, content_type, content)) => ([(header::CONTENT_TYPE, *content_type)], *content).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_file_a_page_loads_is_served_and_versioned() {
        for page in [INDEX, ADMIN] {
            for name in page.split("\"/static/").skip(1).map(|rest| rest.split('"').next().unwrap()) {
                assert!(FILES.iter().any(|(n, _, _)| *n == name), "{name} is not in FILES");
            }
            let served = versioned(page);
            assert_eq!(served.matches("\"/static/").count(), served.matches("?v=").count());
        }
    }
}
