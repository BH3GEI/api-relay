//! The model list, model groups included, in a shape both OpenAI and Anthropic model-list clients accept.

use axum::{Json, extract::{Path, State}};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};

use crate::{AppState, db, error::{OrNotFound, Result}};

async fn model_objects(state: &AppState) -> Result<Vec<Value>> {
    let providers: HashMap<i64, String> = db::providers::list(&state.db).await?.into_iter().map(|p| (p.id, p.name)).collect();
    let models: Vec<Value> = state.catalog.entries().into_iter().map(|e| {
        // Keep the upstream's metadata (context length, modalities, …) under our id
        let mut m = match e.meta {
            Value::Object(o) => o,
            _ => Default::default(),
        };
        m.entry("display_name").or_insert_with(|| Value::String(e.id.clone()));
        m.insert("id".into(), Value::String(e.id));
        m.insert("object".into(), "model".into());
        // Anthropic-style field, for clients that list models in that format
        m.insert("type".into(), "model".into());
        m.insert("owned_by".into(), providers.get(&e.provider_id).cloned().unwrap_or_default().into());
        Value::Object(m)
    }).collect();
    let by_id: HashMap<&str, &Value> = models.iter().filter_map(|m| Some((m["id"].as_str()?, m))).collect();
    // Model groups first: a client picks one like any model, and the relay picks what serves it
    let groups: Vec<Value> = db::groups::list(&state.db).await?.into_iter().map(|g| {
        let members = g.members();
        let mut group = json!({
            "id": g.name,
            "display_name": format!("{} (group: {})", g.name, members.join(" → ")),
            "object": "model",
            "type": "model",
            "owned_by": "relay",
        });
        // Any member may serve a request, so the group promises only what every member can do
        let found: Vec<&Value> = members.iter().filter_map(|id| by_id.get(id.as_str()).copied()).collect();
        if !members.is_empty() && found.len() == members.len() {
            if let Some(n) = smallest(&found, context_length) {
                group["context_length"] = n.into();
            }
            if let Some(n) = smallest(&found, max_tokens) {
                group["max_tokens"] = n.into();
            }
        }
        group["models"] = members.into();
        group
    }).collect();
    Ok(groups.into_iter().chain(models).collect())
}

/// Upstreams name the context length differently: OpenAI-style lists, DeepSeek and Kimi each have their own field
fn context_length(m: &Value) -> Option<u64> {
    m["context_length"].as_u64().or(m["context_window"].as_u64()).or(m["limit"]["context"].as_u64())
}

fn max_tokens(m: &Value) -> Option<u64> {
    m["max_tokens"].as_u64().or(m["max_output_tokens"].as_u64()).or(m["limit"]["output"].as_u64())
}

/// The least of the members' values, or `None` if any member doesn't say
fn smallest(members: &[&Value], field: fn(&Value) -> Option<u64>) -> Option<u64> {
    members.iter().map(|m| field(m)).collect::<Option<Vec<_>>>()?.into_iter().min()
}

/// `GET /v1/models`
pub async fn list(State(state): State<Arc<AppState>>) -> Result<Json<Value>> {
    let data = model_objects(&state).await?;
    let id = |m: Option<&Value>| m.map_or(Value::Null, |m| m["id"].clone());
    Ok(Json(json!({ "object": "list", "first_id": id(data.first()), "last_id": id(data.last()), "has_more": false, "data": data })))
}

/// `GET /v1/models/{id}`; ids contain a slash (`kimi/k3`), so this takes the rest of the path.
pub async fn get(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Result<Json<Value>> {
    model_objects(&state).await?.into_iter().find(|m| m["id"] == id.as_str()).map(Json).or_not_found()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_promises_the_smallest_of_its_members() {
        let (kimi, deepseek, ark) = (
            json!({ "limit": { "context": 1048576 } }),
            json!({ "context_window": 1048576, "max_output_tokens": 393216 }),
            json!({ "context_length": 1000000, "max_tokens": 131072 }),
        );
        assert_eq!(smallest(&[&kimi, &deepseek, &ark], context_length), Some(1000000));
        // Kimi doesn't say how much it writes, so neither does the group
        assert_eq!(smallest(&[&kimi, &deepseek, &ark], max_tokens), None);
        assert_eq!(smallest(&[&deepseek, &ark], max_tokens), Some(131072));
        assert_eq!(smallest(&[&ark, &json!({})], context_length), None);
    }
}
