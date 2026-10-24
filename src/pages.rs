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

const FILES: &[(&str, &str, &str)] = &[
    ("usage-chart.js", "text/javascript; charset=utf-8", include_str!("../static/usage-chart.js")),
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
    fn every_file_a_page_loads_is_versioned() {
        for page in [INDEX, ADMIN] {
            let served = versioned(page);
            assert!(!served.contains("/static/") || served.matches("/static/").count() == served.matches("?v=").count());
        }
    }
}
