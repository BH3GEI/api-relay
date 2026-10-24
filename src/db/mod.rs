//! Storage: SQLite, one module per kind of record.

pub mod limits;
pub mod providers;
mod schema;
pub mod settings;
pub mod usage;
pub mod users;

pub use schema::migrate;

pub type Db = sqlx::SqlitePool;
