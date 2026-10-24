//! The model list, in a shape both OpenAI and Anthropic model-list clients accept.

use axum::{Json, extract::{Path, State}};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};

use crate::{AppState, db, error::{OrNotFound, Result}};

async fn model_objects(state: &AppState) -> Result<Vec<Value>> {
    let providers: HashMap<i64, String> = db::providers::list(&state.db).await?.into_iter().map(|p| (p.id, p.name)).collect();
    Ok(state.catalog.entries().into_iter().map(|e| {
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
    }).collect())
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
