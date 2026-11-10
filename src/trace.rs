//! Request tracing for multi-agent clusters: who sent a request (X-Octo-* headers, stored
//! with its usage row) and, optionally, what it said (request and response bodies on disk).

use axum::http::HeaderMap;
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Longest actor / task / run value kept, in characters
const MAX_LEN: usize = 128;
/// A response is captured up to this size; the rest is cut off and marked as truncated
const MAX_CAPTURE: usize = 32 << 20;

/// Who and what a request is for, as the client labels it.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct Trace {
    pub actor: Option<String>,
    pub task: Option<String>,
    pub run: Option<String>,
}

impl Trace {
    /// Read `X-Octo-Actor`, `X-Octo-Task` and `X-Octo-Run`. They are only stored, never sent upstream:
    /// the relay builds upstream requests from scratch and copies no client headers but
    /// `anthropic-version` and `anthropic-beta`.
    pub fn from_headers(headers: &HeaderMap) -> Trace {
        let get = |name: &str| headers.get(name).and_then(|v| clean(v.as_bytes()));
        Trace { actor: get("x-octo-actor"), task: get("x-octo-task"), run: get("x-octo-run") }
    }
}

/// Printable ASCII and common Chinese characters, at most 128 of them; anything else is dropped.
/// None when nothing is left.
pub fn clean(raw: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(raw);
    let kept: String = s.chars().filter(|&c| allowed(c)).take(MAX_LEN).collect();
    let kept = kept.trim();
    (!kept.is_empty()).then(|| kept.to_string())
}

fn allowed(c: char) -> bool {
    matches!(c,
        ' '..='~'                       // printable ASCII
        | '\u{4E00}'..='\u{9FFF}'       // CJK unified ideographs
        | '\u{3400}'..='\u{4DBF}'       // CJK extension A
        | '\u{3001}'..='\u{3002}'       // 、。
        | '\u{300A}'..='\u{3011}'       // 《》「」『』【】
        | '\u{FF01}' | '\u{FF08}' | '\u{FF09}' | '\u{FF0C}' | '\u{FF1A}' | '\u{FF1B}' | '\u{FF1F}' // ！（），：；？
    )
}

// ── Body capture ──

/// `RELAY_CAPTURE_DIR` and `RELAY_CAPTURE_SAMPLE`; capture is off when the directory isn't set.
#[derive(Clone, Debug)]
pub struct CaptureConfig {
    pub dir: PathBuf,
    /// Fraction of requests captured, 0–1
    pub sample: f64,
}

impl CaptureConfig {
    pub fn from_env() -> Option<CaptureConfig> {
        let dir = std::env::var("RELAY_CAPTURE_DIR").ok().filter(|d| !d.trim().is_empty())?;
        let sample = parse_sample(std::env::var("RELAY_CAPTURE_SAMPLE").ok().as_deref());
        Some(CaptureConfig { dir: PathBuf::from(dir), sample })
    }

    /// Whether to capture this request.
    pub fn sampled(&self) -> bool {
        sampled(self.sample, rand::random::<f64>())
    }
}

/// The sample rate, clamped to 0–1; unset or unparseable means 1 (capture everything).
pub fn parse_sample(raw: Option<&str>) -> f64 {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => 1.0,
        Some(s) => match s.parse::<f64>() {
            Ok(v) if v.is_finite() => v.clamp(0.0, 1.0),
            _ => {
                eprintln!("RELAY_CAPTURE_SAMPLE={s:?} is not a number between 0 and 1; capturing everything");
                1.0
            }
        },
    }
}

/// `roll` is uniform in [0, 1): a rate of 1 always captures, 0 never does.
pub fn sampled(rate: f64, roll: f64) -> bool {
    roll < rate
}

/// One request being captured. Response bytes are added as they pass through; the file
/// is written when this is dropped, i.e. once the response is over or the client hung up.
pub struct Capture {
    dir: PathBuf,
    id: i64,
    meta: serde_json::Map<String, Value>,
    request: Value,
    response: Vec<u8>,
    truncated: bool,
    stream: bool,
}

impl Capture {
    /// `meta` holds what to file alongside the bodies (user, model, trace labels, …).
    /// It must never hold credentials: no request headers are captured at all.
    pub fn new(cfg: &CaptureConfig, id: i64, meta: serde_json::Map<String, Value>, request: Value, stream: bool) -> Capture {
        Capture { dir: cfg.dir.clone(), id, meta, request, response: Vec::new(), truncated: false, stream }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let room = MAX_CAPTURE.saturating_sub(self.response.len());
        if bytes.len() > room {
            self.truncated = true;
        }
        self.response.extend_from_slice(&bytes[..bytes.len().min(room)]);
    }

    fn document(&mut self) -> Value {
        let body = std::mem::take(&mut self.response);
        // A whole JSON response is kept as JSON; a stream is its raw SSE text
        let response = match self.stream {
            false => serde_json::from_slice(&body).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned())),
            true => Value::String(String::from_utf8_lossy(&body).into_owned()),
        };
        let mut doc = std::mem::take(&mut self.meta);
        doc.insert("usage_log_id".into(), self.id.into());
        doc.insert("stream".into(), self.stream.into());
        doc.insert("request".into(), std::mem::take(&mut self.request));
        doc.insert("response".into(), response);
        doc.insert("response_truncated".into(), self.truncated.into());
        Value::Object(doc)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let (dir, id, doc) = (self.dir.clone(), self.id, self.document());
        let write = move || {
            if let Err(e) = write_capture(&dir, id, &doc) {
                // Capture is best effort: the request itself already succeeded
                eprintln!("capture {id}: {e}");
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => { rt.spawn_blocking(write); }
            Err(_) => write(),
        }
    }
}

/// Write `<dir>/<YYYY-MM-DD>/<id>.json.gz` (UTC date), via a temporary file so readers never see half a file.
pub fn write_capture(dir: &Path, id: i64, doc: &Value) -> std::io::Result<PathBuf> {
    let day = dir.join(utc_date(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())));
    std::fs::create_dir_all(&day)?;
    let path = day.join(format!("{id}.json.gz"));
    let tmp = day.join(format!(".{id}.json.gz.tmp"));
    let file = std::fs::File::create(&tmp)?;
    let mut gz = flate2::write::GzEncoder::new(std::io::BufWriter::new(file), flate2::Compression::default());
    serde_json::to_writer(&mut gz, doc)?;
    gz.finish()?.flush()?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// `YYYY-MM-DD` for a Unix timestamp, in UTC (like the usage log's `created_at`).
pub fn utc_date(unix: u64) -> String {
    // Howard Hinnant's civil_from_days
    let z = (unix / 86400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    // Scratch space inside the build directory
    fn test_dir() -> PathBuf {
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn cleans_header_values() {
        assert_eq!(clean(b"lane-m07-a").as_deref(), Some("lane-m07-a"));
        // Control characters, newlines and other scripts are dropped
        assert_eq!(clean(b"lane\x00-m07\r\n\x1b[31m").as_deref(), Some("lane-m07[31m"));
        assert_eq!(clean("修复登录页 bug：第3步".as_bytes()).as_deref(), Some("修复登录页 bug：第3步"));
        assert_eq!(clean("emoji🚀 ok ünï".as_bytes()).as_deref(), Some("emoji ok n"));
        // Invalid UTF-8 doesn't sneak through as replacement characters
        assert_eq!(clean(b"ab\xff\xfecd").as_deref(), Some("abcd"));
        assert_eq!(clean(b"   ").as_deref(), None);
        assert_eq!(clean("\u{1F600}".as_bytes()), None);
    }

    #[test]
    fn truncates_to_128_chars() {
        assert_eq!(clean(&[b'a'; 500]).unwrap().len(), 128);
        let chinese = "测".repeat(300);
        assert_eq!(clean(chinese.as_bytes()).unwrap().chars().count(), 128);
    }

    #[test]
    fn trace_from_headers() {
        let mut h = HeaderMap::new();
        h.insert("x-octo-actor", HeaderValue::from_static("m07-ver"));
        h.insert("X-Octo-Run", HeaderValue::from_bytes("r-2026-11-10 第二轮".as_bytes()).unwrap());
        h.insert("authorization", HeaderValue::from_static("Bearer sk-secret"));
        let t = Trace::from_headers(&h);
        assert_eq!(t, Trace { actor: Some("m07-ver".into()), task: None, run: Some("r-2026-11-10 第二轮".into()) });
    }

    #[test]
    fn sample_rate() {
        assert_eq!(parse_sample(None), 1.0);
        assert_eq!(parse_sample(Some("")), 1.0);
        assert_eq!(parse_sample(Some("0.25")), 0.25);
        assert_eq!(parse_sample(Some("5")), 1.0);
        assert_eq!(parse_sample(Some("-1")), 0.0);
        assert_eq!(parse_sample(Some("NaN")), 1.0);
        assert_eq!(parse_sample(Some("half")), 1.0);

        assert!(sampled(1.0, 0.0) && sampled(1.0, 0.999_999));
        assert!(!sampled(0.0, 0.0) && !sampled(0.0, 0.5));
        assert!(sampled(0.3, 0.29) && !sampled(0.3, 0.3));
        // Over many rolls the rate is roughly honored
        let hits = (0..10_000).filter(|_| sampled(0.2, rand::random())).count();
        assert!((1_500..2_500).contains(&hits), "{hits}");
    }

    #[test]
    fn dates() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(951_782_400), "2000-02-29");
        assert_eq!(utc_date(1_793_577_600), "2026-11-02");
    }

    #[test]
    fn capture_writes_gzip_without_secrets() {
        use std::io::Read;
        let dir = test_dir().join(format!("relay-capture-test-{}", uuid::Uuid::new_v4().simple()));
        let cfg = CaptureConfig { dir: dir.clone(), sample: 1.0 };
        let mut meta = serde_json::Map::new();
        meta.insert("actor".into(), "lane-m07-a".into());
        let mut c = Capture::new(&cfg, 42, meta, serde_json::json!({ "model": "kimi/k3" }), true);
        c.feed(b"data: {\"a\":1}\n\n");
        c.feed(b"data: [DONE]\n\n");
        drop(c); // no runtime here, so this writes synchronously

        let path = dir.join(utc_date(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs())).join("42.json.gz");
        let mut text = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(&path).unwrap()).read_to_string(&mut text).unwrap();
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["usage_log_id"], 42);
        assert_eq!(doc["actor"], "lane-m07-a");
        assert_eq!(doc["request"]["model"], "kimi/k3");
        assert_eq!(doc["response"], "data: {\"a\":1}\n\ndata: [DONE]\n\n");
        assert!(!text.to_lowercase().contains("authorization") && !text.contains("x-api-key"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn capture_write_failure_is_not_fatal() {
        // A file where the directory should be: the write fails, and dropping must not panic
        let file = test_dir().join(format!("relay-capture-file-{}", uuid::Uuid::new_v4().simple()));
        std::fs::write(&file, b"x").unwrap();
        let cfg = CaptureConfig { dir: file.clone(), sample: 1.0 };
        drop(Capture::new(&cfg, 1, Default::default(), Value::Null, false));
        std::fs::remove_file(&file).unwrap();
    }
}
