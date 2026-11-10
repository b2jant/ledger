//! Hunk-to-AST symbol overlap attribution (`HunkVerifiedSymbol` vs `FileLevel`)
//! and historical co-change test candidate discovery.

use ledger_core::{CoChangePeer, HunkConfidence, LineSpan, SymbolKind, SymbolRecord};

/// Evaluates whether historical unified-diff hunk spans overlap tracked AST symbol spans.
#[derive(Debug, Default, Clone, Copy)]
pub struct HunkSymbolAttributor;

impl HunkSymbolAttributor {
    /// Returns [`HunkConfidence::HunkVerifiedSymbol`] if any hunk in `hunks` overlaps any
    /// non-module symbol span in `symbols`, otherwise [`HunkConfidence::FileLevel`].
    #[must_use]
    pub fn classify_touch(hunks: &[LineSpan], symbols: &[SymbolRecord]) -> HunkConfidence {
        if symbols.iter().any(|symbol| symbol_overlaps(hunks, symbol)) {
            HunkConfidence::HunkVerifiedSymbol
        } else {
            HunkConfidence::FileLevel
        }
    }

    /// Returns overlapping non-module symbols once each, in their original order.
    #[must_use]
    pub fn overlapping_symbols<'a>(
        hunks: &[LineSpan],
        symbols: &'a [SymbolRecord],
    ) -> Vec<&'a SymbolRecord> {
        symbols
            .iter()
            .filter(|symbol| symbol_overlaps(hunks, symbol))
            .collect()
    }
}

fn symbol_overlaps(hunks: &[LineSpan], symbol: &SymbolRecord) -> bool {
    let span = symbol.line_span;
    symbol.kind != SymbolKind::Module
        && span.start_line > 0
        && span.start_line <= span.end_line
        && hunks.iter().any(|hunk| {
            hunk.start_line > 0
                && hunk.start_line <= hunk.end_line
                && hunk.start_line <= span.end_line
                && span.start_line <= hunk.end_line
        })
}

/// Filters 1-hop [`CoChangePeer`] entries down to test files (`tests/`, `*_test.*`, `*.spec.*`, `test_*`).
#[must_use]
pub fn derive_cochange_test_files(peers: &[CoChangePeer]) -> Vec<&CoChangePeer> {
    peers
        .iter()
        .filter(|peer| is_test_path(&peer.rel_path))
        .collect()
}

fn is_test_path(path: &str) -> bool {
    let (directories, filename) = path.rsplit_once('/').unwrap_or(("", path));
    directories
        .split('/')
        .any(|part| matches!(part, "tests" | "__tests__"))
        || [
            "_test.rs",
            "_test.go",
            "_test.py",
            ".test.ts",
            ".test.tsx",
            ".spec.ts",
            ".spec.tsx",
            ".test.js",
            ".spec.js",
            ".test.jsx",
            ".spec.jsx",
        ]
        .iter()
        .any(|suffix| filename.ends_with(suffix))
        || (filename.starts_with("test_") && filename.ends_with(".py"))
}
