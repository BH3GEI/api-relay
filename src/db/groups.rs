//! Model groups: a name clients ask for, standing for an ordered list of catalog models that
//! are tried in turn until one answers.

use serde::{Deserialize, Serialize};

use super::Db;

#[derive(Clone, Serialize, sqlx::FromRow)]
pub struct Group {
    pub id: i64,
    pub name: String,
    /// Catalog ids, first tried first, one per line
    #[serde(serialize_with = "serialize_list")]
    pub models: String,
}

impl Group {
    pub fn members(&self) -> Vec<String> {
        split(&self.models)
    }
}

fn serialize_list<S: serde::Serializer>(models: &str, s: S) -> Result<S::Ok, S::Error> {
    s.collect_seq(split(models))
}

/// `a, b\nc` → [a, b, c]
pub fn split(list: &str) -> Vec<String> {
    list.split(|c: char| c == ',' || c.is_whitespace()).filter(|m| !m.is_empty()).map(str::to_string).collect()
}

/// A group as the admin page sends it; `models` is a list or comma/space/line-separated text.
#[derive(Deserialize)]
pub struct GroupInput {
    pub name: String,
    pub models: ModelList,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum ModelList {
    List(Vec<String>),
    Text(String),
}

impl ModelList {
    pub fn items(&self) -> Vec<String> {
        match self {
            ModelList::List(l) => l.iter().map(|m| m.trim().to_string()).filter(|m| !m.is_empty()).collect(),
            ModelList::Text(t) => split(t),
        }
    }
}

const COLUMNS: &str = "id, name, models";

pub async fn list(db: &Db) -> sqlx::Result<Vec<Group>> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM model_groups ORDER BY id")).fetch_all(db).await
}

pub async fn by_name(db: &Db, name: &str) -> sqlx::Result<Option<Group>> {
    sqlx::query_as(&format!("SELECT {COLUMNS} FROM model_groups WHERE name = ?")).bind(name).fetch_optional(db).await
}

pub async fn create(db: &Db, name: &str, models: &[String]) -> sqlx::Result<Group> {
    sqlx::query_as(&format!("INSERT INTO model_groups (name, models) VALUES (?, ?) RETURNING {COLUMNS}"))
        .bind(name).bind(models.join("\n")).fetch_one(db).await
}

pub async fn update(db: &Db, id: i64, name: &str, models: &[String]) -> sqlx::Result<Option<Group>> {
    sqlx::query_as(&format!("UPDATE model_groups SET name = ?, models = ? WHERE id = ? RETURNING {COLUMNS}"))
        .bind(name).bind(models.join("\n")).bind(id).fetch_optional(db).await
}

pub async fn delete(db: &Db, id: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM model_groups WHERE id = ?").bind(id).execute(db).await?;
    Ok(())
}
