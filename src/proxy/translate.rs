//! OpenAI Chat Completions on top of an upstream that speaks only Anthropic Messages: the request
//! is rewritten as a Messages request, and the answer (whole or streamed) back as a completion.

use axum::body::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{Map, Value, json};

/// Messages wants a cap on output; Chat Completions requests often leave it out.
const DEFAULT_MAX_TOKENS: u64 = 8192;

/// A Chat Completions request as a Messages request for `model`.
pub fn request(chat: &Value, model: &str) -> Value {
    let mut system = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    // Consecutive messages of one role become one, as tool results after several calls must
    let mut push = |role: &str, blocks: Vec<Value>| {
        if blocks.is_empty() {
            return;
        }
        match messages.last_mut() {
            Some(last) if last["role"] == role => last["content"].as_array_mut().unwrap().extend(blocks),
            _ => messages.push(json!({ "role": role, "content": blocks })),
        }
    };
    for m in chat["messages"].as_array().into_iter().flatten() {
        match m["role"].as_str().unwrap_or("user") {
            "system" | "developer" => system.extend(content_blocks(&m["content"]).into_iter().filter(|b| b["type"] == "text")),
            "assistant" => {
                let mut blocks = content_blocks(&m["content"]);
                for call in m["tool_calls"].as_array().into_iter().flatten() {
                    let args = call["function"]["arguments"].as_str().unwrap_or("{}");
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call["id"],
                        "name": call["function"]["name"],
                        "input": serde_json::from_str::<Value>(args).ok().filter(Value::is_object).unwrap_or_else(|| json!({})),
                    }));
                }
                push("assistant", blocks);
            }
            "tool" => {
                let text: String = content_blocks(&m["content"]).iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n");
                push("user", vec![json!({ "type": "tool_result", "tool_use_id": m["tool_call_id"], "content": text })]);
            }
            _ => push("user", content_blocks(&m["content"])),
        }
    }

    let max_tokens = chat["max_completion_tokens"].as_u64().or(chat["max_tokens"].as_u64()).unwrap_or(DEFAULT_MAX_TOKENS);
    let mut out = json!({ "model": model, "max_tokens": max_tokens, "messages": messages });
    if !system.is_empty() {
        out["system"] = Value::Array(system);
    }
    for field in ["temperature", "top_p", "stream"] {
        if !chat[field].is_null() {
            out[field] = chat[field].clone();
        }
    }
    match &chat["stop"] {
        Value::String(s) => out["stop_sequences"] = json!([s]),
        Value::Array(a) => out["stop_sequences"] = Value::Array(a.clone()),
        _ => {}
    }
    if let Some(tools) = chat["tools"].as_array() {
        out["tools"] = tools.iter().filter(|t| t["type"] == "function").map(|t| {
            let f = &t["function"];
            let mut tool = json!({ "name": f["name"], "input_schema": if f["parameters"].is_object() { f["parameters"].clone() } else { json!({ "type": "object" }) } });
            if let Some(d) = f["description"].as_str() {
                tool["description"] = d.into();
            }
            tool
        }).collect();
    }
    let choice = match &chat["tool_choice"] {
        Value::String(s) if s == "required" => Some(json!({ "type": "any" })),
        Value::String(s) if s == "none" => Some(json!({ "type": "none" })),
        Value::String(s) if s == "auto" => Some(json!({ "type": "auto" })),
        Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("function") => Some(json!({ "type": "tool", "name": chat["tool_choice"]["function"]["name"] })),
        _ => None,
    };
    if let Some(mut choice) = choice {
        if chat["parallel_tool_calls"] == false {
            choice["disable_parallel_tool_use"] = true.into();
        }
        out["tool_choice"] = choice;
    }
    out
}

/// A Chat Completions message's content (text, or a list of text and image parts) as Messages blocks.
fn content_blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::String(s) if !s.is_empty() => vec![json!({ "type": "text", "text": s })],
        Value::Array(parts) => parts.iter().filter_map(|p| match p["type"].as_str()? {
            "text" => Some(json!({ "type": "text", "text": p["text"] })),
            "image_url" => {
                let url = p["image_url"]["url"].as_str().or(p["image_url"].as_str())?;
                Some(match url.strip_prefix("data:").and_then(|d| d.split_once(";base64,")) {
                    Some((media_type, data)) => json!({ "type": "image", "source": { "type": "base64", "media_type": media_type, "data": data } }),
                    None => json!({ "type": "image", "source": { "type": "url", "url": url } }),
                })
            }
            _ => None,
        }).collect(),
        _ => Vec::new(),
    }
}

fn finish_reason(stop_reason: &Value) -> Value {
    match stop_reason.as_str() {
        Some("max_tokens") => "length".into(),
        Some("tool_use") => "tool_calls".into(),
        Some(_) => "stop".into(),
        None => Value::Null,
    }
}

/// Anthropic usage as OpenAI usage; input counts cached tokens, as OpenAI's does.
fn usage(input: i64, output: i64) -> Value {
    json!({ "prompt_tokens": input, "completion_tokens": output, "total_tokens": input + output })
}

fn input_tokens(u: &Value) -> i64 {
    ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"].iter().map(|f| u[f].as_i64().unwrap_or(0)).sum()
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// A whole Messages answer as a Chat Completions answer; what isn't a message is passed on as it is.
pub fn response(body: &[u8], model: &str) -> Vec<u8> {
    let Ok(msg) = serde_json::from_slice::<Value>(body) else { return body.to_vec() };
    if msg["type"] != "message" {
        return body.to_vec();
    }
    let (mut text, mut reasoning, mut calls) = (String::new(), String::new(), Vec::new());
    for b in msg["content"].as_array().into_iter().flatten() {
        match b["type"].as_str() {
            Some("text") => text.push_str(b["text"].as_str().unwrap_or("")),
            Some("thinking") => reasoning.push_str(b["thinking"].as_str().unwrap_or("")),
            Some("tool_use") => calls.push(json!({
                "id": b["id"], "type": "function",
                "function": { "name": b["name"], "arguments": b["input"].to_string() },
            })),
            _ => {}
        }
    }
    let mut message = json!({ "role": "assistant", "content": if text.is_empty() && !calls.is_empty() { Value::Null } else { text.into() } });
    if !reasoning.is_empty() {
        message["reasoning_content"] = reasoning.into();
    }
    if !calls.is_empty() {
        message["tool_calls"] = calls.into();
    }
    let u = &msg["usage"];
    json!({
        "id": msg["id"], "object": "chat.completion", "created": now(), "model": model,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason(&msg["stop_reason"]) }],
        "usage": usage(input_tokens(u), u["output_tokens"].as_i64().unwrap_or(0)),
    }).to_string().into_bytes()
}

/// Turns a Messages event stream into a Chat Completions one, event by event.
struct StreamState {
    model: String,
    include_usage: bool,
    line: Vec<u8>,
    id: Value,
    input: i64,
    output: i64,
    /// Anthropic content block index → OpenAI tool call index
    tools: Map<String, Value>,
    done: bool,
}

impl StreamState {
    fn chunk(&self, delta: Value, finish: Value) -> String {
        let c = json!({ "id": self.id, "object": "chat.completion.chunk", "created": now(), "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] });
        format!("data: {c}\n\n")
    }

    fn feed(&mut self, bytes: &[u8], out: &mut String) {
        for &b in bytes {
            if b == b'\n' {
                let line = std::mem::take(&mut self.line);
                if let Some(data) = line.strip_prefix(b"data:") {
                    if let Ok(event) = serde_json::from_slice::<Value>(data.trim_ascii()) {
                        self.event(&event, out);
                    }
                }
            } else {
                self.line.push(b);
            }
        }
    }

    fn event(&mut self, e: &Value, out: &mut String) {
        match e["type"].as_str() {
            Some("message_start") => {
                self.id = e["message"]["id"].clone();
                self.input = input_tokens(&e["message"]["usage"]);
                out.push_str(&self.chunk(json!({ "role": "assistant", "content": "" }), Value::Null));
            }
            Some("content_block_start") if e["content_block"]["type"] == "tool_use" => {
                let n = self.tools.len();
                self.tools.insert(e["index"].to_string(), n.into());
                let b = &e["content_block"];
                out.push_str(&self.chunk(json!({ "tool_calls": [{ "index": n, "id": b["id"], "type": "function", "function": { "name": b["name"], "arguments": "" } }] }), Value::Null));
            }
            Some("content_block_delta") => {
                let d = &e["delta"];
                let delta = match d["type"].as_str() {
                    Some("text_delta") => json!({ "content": d["text"] }),
                    Some("thinking_delta") => json!({ "reasoning_content": d["thinking"] }),
                    Some("input_json_delta") => {
                        let n = self.tools.get(&e["index"].to_string()).cloned().unwrap_or(0.into());
                        json!({ "tool_calls": [{ "index": n, "function": { "arguments": d["partial_json"] } }] })
                    }
                    _ => return,
                };
                out.push_str(&self.chunk(delta, Value::Null));
            }
            Some("message_delta") => {
                if let Some(o) = e["usage"]["output_tokens"].as_i64() {
                    self.output = o;
                }
                if e["usage"]["input_tokens"].is_i64() {
                    self.input = input_tokens(&e["usage"]);
                }
                out.push_str(&self.chunk(json!({}), finish_reason(&e["delta"]["stop_reason"])));
            }
            Some("message_stop") => self.finish(out),
            Some("error") => out.push_str(&format!("data: {}\n\n", json!({ "error": e["error"] }))),
            _ => {}
        }
    }

    fn finish(&mut self, out: &mut String) {
        if self.done {
            return;
        }
        self.done = true;
        if self.include_usage {
            let c = json!({ "id": self.id, "object": "chat.completion.chunk", "created": now(), "model": self.model,
                "choices": [], "usage": usage(self.input, self.output) });
            out.push_str(&format!("data: {c}\n\n"));
        }
        out.push_str("data: [DONE]\n\n");
    }
}

/// A Messages event stream as a Chat Completions event stream.
pub fn stream<S>(upstream: S, model: String, include_usage: bool) -> impl Stream<Item = Result<Bytes, reqwest::Error>>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let state = StreamState { model, include_usage, line: Vec::new(), id: Value::Null, input: 0, output: 0, tools: Map::new(), done: false };
    upstream.map(Some).chain(futures::stream::once(async { None })).scan(state, |state, item| {
        let mut out = String::new();
        let result = match item {
            Some(Ok(bytes)) => {
                state.feed(&bytes, &mut out);
                Ok(Bytes::from(out))
            }
            Some(Err(e)) => Err(e),
            None => Ok(Bytes::new()),
        };
        futures::future::ready(Some(result))
    }).filter(|r| futures::future::ready(!matches!(r, Ok(b) if b.is_empty())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_become_messages_requests() {
        let chat = json!({
            "model": "x", "max_tokens": 50, "stop": "END", "tool_choice": "required",
            "messages": [
                { "role": "system", "content": "be brief" },
                { "role": "user", "content": [{ "type": "text", "text": "hi" }, { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }] },
                { "role": "assistant", "content": null, "tool_calls": [{ "id": "c1", "type": "function", "function": { "name": "move", "arguments": "{\"x\":1}" } }] },
                { "role": "tool", "tool_call_id": "c1", "content": "moved" },
            ],
            "tools": [{ "type": "function", "function": { "name": "move", "description": "Move", "parameters": { "type": "object" } } }],
        });
        let m = request(&chat, "up");
        assert_eq!(m["system"], json!([{ "type": "text", "text": "be brief" }]));
        assert_eq!(m["messages"][0]["content"][1]["source"], json!({ "type": "base64", "media_type": "image/png", "data": "AAAA" }));
        assert_eq!(m["messages"][1]["content"][0], json!({ "type": "tool_use", "id": "c1", "name": "move", "input": { "x": 1 } }));
        assert_eq!(m["messages"][2], json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "c1", "content": "moved" }] }));
        assert_eq!((m["model"].clone(), m["max_tokens"].clone(), m["stop_sequences"].clone(), m["tool_choice"].clone()), (json!("up"), json!(50), json!(["END"]), json!({ "type": "any" })));
        assert_eq!(m["tools"][0], json!({ "name": "move", "description": "Move", "input_schema": { "type": "object" } }));
    }

    #[test]
    fn streams_become_chunks() {
        let events = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"usage\":{\"input_tokens\":7}}}\n\n\
            data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\"}}\n\n\
            data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hm\"}}\n\n\
            data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"move\"}}\n\n\
            data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"x\\\":1}\"}}\n\n\
            data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":3}}\n\n\
            data: {\"type\":\"message_stop\"}\n\n";
        let mut s = StreamState { model: "inkling".into(), include_usage: true, line: Vec::new(), id: Value::Null, input: 0, output: 0, tools: Map::new(), done: false };
        let mut out = String::new();
        // Split mid-line, as the network may
        let (a, b) = events.as_bytes().split_at(100);
        s.feed(a, &mut out);
        s.feed(b, &mut out);
        let chunks: Vec<Value> = out.split("\n\n").filter_map(|l| l.strip_prefix("data: ")).filter(|l| *l != "[DONE]").map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(chunks[1]["choices"][0]["delta"], json!({ "reasoning_content": "hm" }));
        assert_eq!(chunks[2]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"], "move");
        assert_eq!(chunks[3]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"], "{\"x\":1}");
        assert_eq!(chunks[4]["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(chunks[5]["usage"], json!({ "prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10 }));
        assert!(out.ends_with("data: [DONE]\n\n"));
    }
}
