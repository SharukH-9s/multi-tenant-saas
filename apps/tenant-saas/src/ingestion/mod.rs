//! Document ingestion stages. Everything here is pure and synchronous; the
//! ingestion service runs it inside `spawn_blocking`.

pub mod chunker;
pub mod normalize;
pub mod parser;
