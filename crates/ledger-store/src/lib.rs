//! SQLite WAL authoritative snapshot store, separate FTS5 retrieval channels,
//! `petgraph` generation topology, coherent cross-store snapshot pinning, and
//! lightweight LRU handle tracking.

pub mod graph;
pub mod sqlite;

pub use graph::{GraphGeneration, HandleMetadata, HandleRegistry};
pub use sqlite::{FtsHit, SqliteStore, StoreError, StoreStats};
