//! Composable Tree-sitter AST extractors, cross-boundary contract extractors,
//! import-scoped symbol resolution, and manifest-backed test command derivation.

pub mod contracts;
pub mod extractors;
pub mod resolver;

pub use contracts::ContractExtractor;
pub use extractors::{
    AstExtraction, ExtractedFile, LanguageExtractor, ParserError, ParserRegistry, RawCallSite,
};
pub use resolver::{ManifestInfo, ResolvedWorkspaceGraph, ScopedResolver, detect_manifest};
