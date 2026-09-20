pub mod facts;
mod fts;
mod knowledge;
mod migrations;
mod query_log;
mod tasks;

pub use migrations::{migrate, CURRENT_SCHEMA_VERSION};
