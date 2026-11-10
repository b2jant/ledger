//! Core domain types, strong identifiers, security boundary policy, single-buffer
//! file reading, and token-budgeted context pack formatting for LEDGER.
//!
//! This crate intentionally performs no database or network I/O so every policy,
//! scoring heuristic, and formatter remains pure, deterministic, and composable.

pub mod buffer;
pub mod ids;
pub mod model;
pub mod query;
pub mod scope;
pub mod tokens;

pub use buffer::{FileBuffer, SingleBufferError, SingleBufferReader};
pub use ids::{
    ApproxTokens, ByteSpan, ContentHash, GenerationId, InterfaceHash, LineSpan, LogicalSymbolKey,
    SnapshotHandle, WorkspaceId,
};
pub use model::{
    ArchitecturalLayer, CoChangePeer, ContractEndpoint, ContractKind, EdgeConfidence, FileRecord,
    HistoryHit, HunkConfidence, IndexState, Language, ParseCoverage, RelationKind, SymbolKind,
    SymbolRecord, TestCandidate, TestProvenance, TypedEdge, VcsBackendKind, VcsFileTouch,
    VcsRevision,
};
pub use query::{
    Anchor, AnchorInput, AnchorKind, ContextPack, ContextRequest, ExpandDelivery, ExpandItemResult,
    ExpandRequest, ExpandResponse, ExpandStatus, FocusMode, PackFormatter, QueryMode,
    ReferenceItem, TargetSlice, TelemetryRecord,
};
pub use scope::{DefaultScopeGuard, ScopeDecision, ScopePolicy};
