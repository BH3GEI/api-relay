//! Token usage from upstream responses, including streamed ones.

use serde_json::Value;

/// (input, output) tokens from a `usage` object in any of the supported formats.
/// Input includes cached tokens, since they count against plan quotas too.
fn tokens(u: &Value) -> Option<(i64, i64)> {
    if !u.is_object() {
        return None;
    }
    let n = |f: &str| u[f].as_i64().unwrap_or(0);
    // OpenAI: prompt_tokens already includes cached tokens.
    // Anthropic: input_tokens excludes them, so add the cache fields.
    let input = u["prompt_tokens"].as_i64()
        .unwrap_or_else(|| n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens"));
    let output = u["completion_tokens"].as_i64().unwrap_or_else(|| n("output_tokens"));
    Some((input, output))
}

/// Usage from a JSON value: a whole response, or one streamed event.
fn tokens_in(v: &Value) -> Option<(i64, i64)> {
    // Chat Completions / Messages put it at the top; Anthropic's message_start nests it in
    // `message`, and Responses events nest it in `response`
    tokens(&v["usage"]).or_else(|| tokens(&v["message"]["usage"])).or_else(|| tokens(&v["response"]["usage"]))
}

/// (input, output) tokens of a non-streamed response body.
pub fn from_body(body: &[u8]) -> (i64, i64) {
    serde_json::from_slice::<Value>(body).ok().and_then(|v| tokens_in(&v)).unwrap_or((0, 0))
}

/// Picks usage out of a server-sent event stream as it passes through.
#[derive(Default)]
pub struct StreamScanner {
    line: Vec<u8>,
    pub input: i64,
    pub output: i64,
}

impl StreamScanner {
    pub fn feed(&mut self, chunk: &[u8]) {
        for &b in chunk {
            if b == b'\n' {
                self.line_done();
            } else if self.line.len() < 4 << 20 {
                // A runaway line is dropped rather than buffered without bound
                self.line.push(b);
            }
        }
    }

    fn line_done(&mut self) {
        let line = std::mem::take(&mut self.line);
        let Some(data) = line.strip_prefix(b"data:") else { return };
        let Ok(v) = serde_json::from_slice::<Value>(data.trim_ascii()) else { return };
        // Counts arrive in pieces (Anthropic sends input first, output at the end) and are cumulative
        if let Some((i, o)) = tokens_in(&v) {
            self.input = self.input.max(i);
            self.output = self.output.max(o);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_stream() {
        let mut s = StreamScanner::default();
        s.feed(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":90,\"output_tokens\":1}}}\n\n");
        s.feed(b"event: message_delta\ndata: {\"type\":\"message_de");
        s.feed(b"lta\",\"usage\":{\"output_tokens\":42}}\n\n");
        assert_eq!((s.input, s.output), (100, 42));
    }

    #[test]
    fn openai_and_responses_streams() {
        let mut s = StreamScanner::default();
        s.feed(b"data: {\"choices\":[]}\n\ndata: {\"usage\":{\"prompt_tokens\":50,\"completion_tokens\":7}}\n\ndata: [DONE]\n\n");
        assert_eq!((s.input, s.output), (50, 7));

        let mut s = StreamScanner::default();
        s.feed(b"data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":30,\"output_tokens\":5}}}\n");
        assert_eq!((s.input, s.output), (30, 5));
    }

    #[test]
    fn whole_bodies() {
        assert_eq!(from_body(br#"{"usage":{"input_tokens":3,"cache_creation_input_tokens":4,"output_tokens":2}}"#), (7, 2));
        assert_eq!(from_body(b"not json"), (0, 0));
    }
}
