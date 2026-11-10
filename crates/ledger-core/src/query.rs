//! Query schemas, action-categorized context packs (`EDIT_TARGETS` vs `REFERENCE_CONTEXT`),
//! Augment-style prompt enhancement blueprints, and idempotent expansion responses.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::ids::{ApproxTokens, ContentHash, LineSpan, LogicalSymbolKey, SnapshotHandle};
use crate::model::{
    CoChangePeer, EdgeConfidence, IndexState, RelationKind, SymbolKind, TestCandidate,
};

/// Explicit kind of anchor passed to `ledger_context` so punctuation-heavy error literals
/// are never misparsed as file paths or qualified symbol names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AnchorKind {
    /// Automatically classify as path, route, symbol, or literal.
    #[default]
    Auto,
    /// Exact literal substring (error code, log string, config key).
    Literal,
    /// Workspace-relative file or folder path.
    Path,
    /// Function, method, type, or constant name.
    Symbol,
    /// HTTP route path (`/api/orders`).
    Route,
}

/// Structured anchor with explicit kind and value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// Classification of `value`.
    #[serde(default)]
    pub kind: AnchorKind,
    /// Anchor string.
    pub value: String,
}

impl Anchor {
    /// Resolves `AnchorKind::Auto` into a concrete kind conservatively.
    #[must_use]
    pub fn resolved_kind(&self) -> AnchorKind {
        if self.kind != AnchorKind::Auto {
            return self.kind;
        }
        let v = self.value.trim();
        if v.starts_with('/') && v.len() > 1 && !v.contains(' ') {
            return AnchorKind::Route;
        }
        if !v.contains(' ')
            && (v.contains('/')
                || v.ends_with(".rs")
                || v.ends_with(".ts")
                || v.ends_with(".py")
                || v.ends_with(".go"))
        {
            return AnchorKind::Path;
        }
        if v.contains(' ')
            || v.chars()
                .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
        {
            return AnchorKind::Literal;
        }
        AnchorKind::Symbol
    }
}

/// Accepts either a plain string (`"ERR_INVALID_CURSOR"`) or a typed `{ kind, value }` object in JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnchorInput {
    /// Plain string anchor (`kind = Auto`).
    Plain(String),
    /// Typed anchor object.
    Typed(Anchor),
}

impl AnchorInput {
    /// Normalizes into a typed [`Anchor`].
    #[must_use]
    pub fn into_anchor(self) -> Anchor {
        match self {
            Self::Plain(value) => Anchor {
                kind: AnchorKind::Auto,
                value,
            },
            Self::Typed(a) => a,
        }
    }
}

/// Intent mode for `ledger_context`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum QueryMode {
    /// Automatically balance complete edit targets, reference contracts, and tests.
    #[default]
    Auto,
    /// Augment-style Prompt Enhancer mode: compact file-role & constraint blueprint (~400-900 tok).
    Enhance,
    /// Feature implementation focus (emphasizes edit targets + cross-boundary contracts).
    Implement,
    /// Bug-fixing focus (emphasizes literal/stack-frame fast path + callers + failing test targets).
    Debug,
    /// Blast-radius impact check (emphasizes callers, importers, contracts, and tests).
    BlastRadius,
    /// Historical context focus.
    History,
}

/// Whether `focus_paths` acts as a soft ranking boost or a strict path prefix filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FocusMode {
    /// Boost matching paths while still surfacing cross-boundary contracts outside `focus_paths`.
    #[default]
    Boost,
    /// Restrict primary `EDIT_TARGETS` strictly to `focus_paths`.
    Restrict,
}

/// Input parameters for `ledger_context`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextRequest {
    /// Concrete coding goal, bug description, or rough user prompt.
    pub task: String,
    /// Optional known file paths, symbols, HTTP routes, or exact error strings.
    #[serde(default)]
    pub anchors: Vec<AnchorInput>,
    /// Query intent mode.
    #[serde(default)]
    pub mode: QueryMode,
    /// Optional path prefixes to boost or restrict.
    #[serde(default)]
    pub focus_paths: Vec<String>,
    /// How `focus_paths` should be applied.
    #[serde(default)]
    pub focus_mode: FocusMode,
    /// Approximate token ceiling (`400..=12000`, default `2800`).
    #[serde(default = "ContextRequest::default_max_tokens")]
    pub max_approx_tokens: u32,
}

impl ContextRequest {
    /// Default ceiling (2,800 approx tokens).
    #[must_use]
    pub const fn default_max_tokens() -> u32 {
        2800
    }

    /// Clamped token budget ceiling respecting protocol bounds.
    #[must_use]
    pub fn clamped_budget(&self) -> u32 {
        let default_cap = if self.mode == QueryMode::Enhance {
            self.max_approx_tokens.min(1200)
        } else {
            self.max_approx_tokens
        };
        default_cap.clamp(400, 12_000)
    }
}

/// Primary edit target slice included in `EDIT_TARGETS`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetSlice {
    /// Workspace-relative file path.
    pub rel_path: String,
    /// Symbol key, if slice corresponds to an AST symbol.
    pub symbol_key: Option<LogicalSymbolKey>,
    /// Symbol kind (`fn`, `struct`, ...).
    pub symbol_kind: Option<SymbolKind>,
    /// Qualified symbol name (`decode_cursor` or `OrderFilter::apply_predicates`).
    pub symbol_name: String,
    /// Signature summary.
    pub signature: String,
    /// 1-indexed line span rendered in `numbered_source`.
    pub line_span: LineSpan,
    /// BLAKE3 digest of the underlying file buffer.
    pub file_hash: ContentHash,
    /// Concise provenance explanation (`EXACT_LITERAL "ERR_..."`, `LEX_CODE + LAYER(backend)`).
    pub match_reason: String,
    /// Verbatim numbered source lines (`"  14| pub fn ..."`).
    pub numbered_source: String,
    /// Whether lines inside this symbol were truncated due to size (`> 80` lines), with expansion ID.
    pub omitted_expand_id: Option<String>,
}

/// Secondary caller, imported type, or cross-boundary contract in `REFERENCE_CONTEXT`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReferenceItem {
    /// Relationship type (`CALLS`, `HTTP_ENDPOINT`, `CONFIG_ENV_KEY`, `SQL_TABLE_ACCESS`).
    pub relation: RelationKind,
    /// Resolver certainty (`EXACT`, `SCOPED_HEURISTIC`, `VERIFIED_PAIR`, `STALE_AFTER_EDIT`).
    pub confidence: EdgeConfidence,
    /// Peer file path.
    pub rel_path: String,
    /// 1-indexed line in `rel_path`.
    pub line: u32,
    /// Peer symbol name, if known.
    pub peer_symbol_name: Option<String>,
    /// Summary of how the peer connects (`list_orders -> decode_cursor(...)`).
    pub summary: String,
    /// Stable expansion ID (`"body:sym_..."` or `"slice:path:start-end"`).
    pub expand_id: String,
}

/// Compiled evidence pack returned by `ledger_context`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPack {
    /// Snapshot-bound handle for `ledger_expand`.
    pub handle: SnapshotHandle,
    /// Canonical workspace root display string.
    pub workspace_root: String,
    /// Current index readiness & freshness state.
    pub index_state: IndexState,
    /// Optional active VCS revision badge (`git:88f1c2a` or `jj:qpvuntsm(88f1c2)`).
    #[serde(default)]
    pub vcs_revision: Option<String>,
    /// Query route chosen (`literal_fast_path`, `hybrid_lexical_ast_contracts`, `cold_start_live`).
    pub route: String,
    /// Requested mode.
    pub mode: QueryMode,
    /// Primary files & complete functions to modify.
    pub edit_targets: Vec<TargetSlice>,
    /// Callers, shared types, and cross-boundary contracts to reference without breaking.
    pub reference_context: Vec<ReferenceItem>,
    /// Non-transitive 1-hop VCS co-change peers (`BLAST_RADIUS_AND_COCHANGE`).
    #[serde(default)]
    pub cochange_peers: Vec<CoChangePeer>,
    /// Manifest-derived candidate test commands.
    pub test_candidates: Vec<TestCandidate>,
    /// Covered languages summary (`rust(scoped_ast), typescript(scoped_ast)`).
    pub language_coverage: String,
    /// Explicit unknowns, omitted candidate counts, excluded secret file counts, or stale warnings.
    pub unknowns: Vec<String>,
    /// Approximate token ceiling requested.
    pub budget_ceiling: ApproxTokens,
}

/// Delivery mode for `ledger_expand`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExpandDelivery {
    /// Always return the complete requested body/slice so it is directly edit-ready.
    #[default]
    Complete,
    /// Omit unchanged slices already delivered with the exact same `ContentHash` under this handle.
    Delta,
}

/// Input parameters for `ledger_expand`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpandRequest {
    /// Snapshot-bound handle returned by `ledger_context`.
    pub handle: String,
    /// Expansion IDs (`body:sym_...`, `callers:sym_...`, `slice:path:10-50`, `diff:rev:path`).
    pub items: Vec<String>,
    /// Complete vs delta delivery.
    #[serde(default)]
    pub delivery: ExpandDelivery,
    /// Approximate token ceiling (`300..=12000`, default `2400`).
    #[serde(default = "ExpandRequest::default_max_tokens")]
    pub max_approx_tokens: u32,
}

impl ExpandRequest {
    /// Default expansion token ceiling.
    #[must_use]
    pub const fn default_max_tokens() -> u32 {
        2400
    }
}

/// Per-item resolution status in [`ExpandResponse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpandStatus {
    /// Item resolved at the same content digest as `handle`.
    Ok,
    /// File changed on disk after `handle`; live updated content returned with notice.
    UpdatedSinceHandle,
    /// Unchanged content omitted because `delivery = "delta"` and digest already delivered.
    UnchangedDeltaOmitted,
    /// Symbol no longer exists in the current file buffer.
    SymbolDeleted,
    /// Path blocked by `ScopeGuard`.
    DeniedByPolicy,
    /// Item ID malformed or unknown.
    InvalidItemId,
    /// Truncated due to `max_approx_tokens`; `continuation_id` provided.
    TruncatedWithContinuation,
}

/// Result for one requested expansion item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpandItemResult {
    /// Requested item ID.
    pub item_id: String,
    /// Status code.
    pub status: ExpandStatus,
    /// File path, if applicable.
    pub rel_path: Option<String>,
    /// Current file `ContentHash`, if read.
    pub file_hash: Option<ContentHash>,
    /// Rendered source code or caller table.
    pub content: String,
    /// Optional continuation ID if truncated.
    pub continuation_id: Option<String>,
}

/// Batched response returned by `ledger_expand`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpandResponse {
    /// Handle echoed or upgraded if workspace advanced.
    pub handle: String,
    /// Current workspace index state.
    pub index_state: IndexState,
    /// Ordered results matching `ExpandRequest.items`.
    pub results: Vec<ExpandItemResult>,
}

/// Observability telemetry record logged for every MCP/CLI query (powering `ledger status` & Phase 4 Web UI).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryRecord {
    /// Unix epoch milliseconds.
    pub timestamp_ms: u64,
    /// Tool or prompt name (`ledger_context`, `ledger_expand`, `prompt:enhance`).
    pub tool_name: String,
    /// Query mode.
    pub mode: String,
    /// Sanitized task/query summary.
    pub query_summary: String,
    /// Route taken (`literal_fast_path`, `hybrid_lexical_ast_contracts`, `cold_start_live`).
    pub route: String,
    /// Snapshot generation served.
    pub generation: u64,
    /// Primary `EDIT_TARGETS` file paths returned.
    pub edit_target_files: Vec<String>,
    /// Secondary `REFERENCE_CONTEXT` file paths returned.
    pub reference_files: Vec<String>,
    /// End-to-end execution latency in microseconds.
    pub latency_us: u64,
    /// Approximate output tokens emitted.
    pub approx_tokens: u32,
}

/// Pure deterministic formatter rendering [`ContextPack`] and [`ExpandResponse`] into
/// compact, single-representation blocks optimized for LLM editing accuracy.
#[derive(Debug)]
pub struct PackFormatter;

impl PackFormatter {
    /// Renders a [`ContextPack`] respecting the complete response budget.
    #[must_use]
    pub fn render_context_pack(pack: &ContextPack) -> String {
        Self::render_context_pack_with_delivery(pack).0
    }

    /// Returns rendered text and the immutable baseline of complete source actually shown.
    /// Truncated pack slices, formatter omissions and enhance blueprints are never delivered.
    #[must_use]
    pub fn render_context_pack_with_delivery(
        pack: &ContextPack,
    ) -> (String, HashMap<String, ContentHash>) {
        let enhance = pack.mode == QueryMode::Enhance;
        let max_bytes =
            usize::try_from(u64::from(pack.budget_ceiling.0) * 36 / 10).unwrap_or(usize::MAX);
        let label = if enhance {
            "LEDGER_ENHANCE_BLUEPRINT"
        } else {
            "LEDGER"
        };
        let revision = pack.vcs_revision.as_ref().map_or_else(String::new, |rev| {
            format!(" | rev={}", display_metadata(rev, 48))
        });
        let header = format!(
            "[{label} handle={} | ws={} | index={}{} | route={} | ceiling={} approx_tok]\n",
            pack.handle,
            display_metadata(&pack.workspace_root, 120),
            pack.index_state,
            revision,
            display_metadata(&pack.route, 48),
            pack.budget_ceiling,
        );
        let mut footer = BudgetedText::new(max_bytes / 4);
        footer.push(if enhance {
            "COVERAGE_NOTES:\n"
        } else {
            "COVERAGE_AND_UNKNOWNS:\n"
        });
        footer.push(&format!(
            "  - languages: {}\n",
            display_metadata(&pack.language_coverage, 160)
        ));
        for unknown in &pack.unknowns {
            if !footer.push(&format!("  - {}\n", display_metadata(unknown, 240))) {
                break;
            }
        }
        let footer = footer.finish();
        let Some(body_limit) = max_bytes.checked_sub(header.len() + footer.len()) else {
            let mut error = BudgetedText::new(max_bytes);
            error.push("Context metadata exceeds budget; retry with a larger token ceiling.\n");
            return (error.finish(), HashMap::new());
        };
        let mut body = BudgetedText::new(body_limit);
        let mut delivered = HashMap::new();
        body.push(if enhance {
            "FILES_TO_EDIT:\n"
        } else {
            "EDIT_TARGETS:\n"
        });
        if pack.edit_targets.is_empty() {
            body.push("  (none matched; inspect coverage notes)\n");
        }
        for target in &pack.edit_targets {
            let expand_id = target.omitted_expand_id.clone().unwrap_or_else(|| {
                target.symbol_key.as_ref().map_or_else(
                    || format!("slice:{}:{}", target.rel_path, target.line_span),
                    |key| format!("body:{key}"),
                )
            });
            let kind = target.symbol_kind.map_or("slice", |kind| kind.as_str());
            let location = format!("{}:{}", target.rel_path, target.line_span);
            let metadata = format!(
                "  - [{location} | {kind} {} | blake3:{} | match: {}] [expand: \"{expand_id}\"]\n",
                display_metadata(&target.symbol_name, 96),
                target.file_hash,
                display_metadata(&target.match_reason, 120),
            );
            if enhance {
                let entry = format!(
                    "{metadata}    sig: {}\n",
                    display_metadata(&target.signature, 160)
                );
                if !body.push(&entry) {
                    let compact =
                        format!("  - [{location}] [expand: \"{expand_id}\"] [details omitted]\n");
                    if !body.push(&compact) {
                        body.push(&format!(
                            "  - [location/details omitted; expand: \"{expand_id}\"]\n"
                        ));
                    }
                }
                continue;
            }
            // Never shorten a source body to make it fit. Include its complete numbered
            // representation and partial-source warning atomically, or advertise expansion.
            let mut entry = metadata;
            if target.omitted_expand_id.is_some() {
                entry.push_str("    [partial source; remaining lines omitted; use expansion]\n");
            }
            if target.numbered_source.len() <= body.remaining() {
                for line in target.numbered_source.lines() {
                    entry.push_str("   ");
                    entry.push_str(line);
                    entry.push('\n');
                }
                if body.push(&entry) {
                    if target.omitted_expand_id.is_none() && !target.numbered_source.is_empty() {
                        delivered.insert(
                            format!("slice:{}:{}", target.rel_path, target.line_span),
                            target.file_hash.clone(),
                        );
                        if let Some(key) = &target.symbol_key {
                            delivered.insert(format!("body:{key}"), target.file_hash.clone());
                        }
                    }
                    continue;
                }
            }
            let compact = format!(
                "  - [{location}] [body omitted for token ceiling; expand: \"{expand_id}\"]\n"
            );
            if !body.push(&compact) {
                body.push(&format!(
                    "  - [location/body omitted for token ceiling; expand: \"{expand_id}\"]\n"
                ));
            }
        }
        if !pack.reference_context.is_empty() {
            body.push(if enhance {
                "FILES_TO_REFERENCE_AND_RESPECT:\n"
            } else {
                "REFERENCE_CONTEXT:\n"
            });
            for reference in &pack.reference_context {
                if !body.push(&format!(
                    "  - [{} | {}] {}:{} {} [expand: \"{}\"]\n",
                    reference.relation,
                    reference.confidence,
                    reference.rel_path,
                    reference.line,
                    display_metadata(&reference.summary, 160),
                    reference.expand_id,
                )) && !body.push(&format!(
                    "  - [{} | {}] [details omitted; expand: \"{}\"]\n",
                    reference.relation, reference.confidence, reference.expand_id,
                )) {
                    break;
                }
            }
        } else if enhance {
            body.push("FILES_TO_REFERENCE_AND_RESPECT:\n  (none detected)\n");
        }
        if !pack.cochange_peers.is_empty() {
            body.push(if enhance {
                "CO_CHANGE_BLAST_RADIUS:\n"
            } else {
                "BLAST_RADIUS_AND_COCHANGE:\n"
            });
            for peer in &pack.cochange_peers {
                if !body.push(&format!(
                    "  - [COCHANGE {}/{} commits ({:.0}%, lift={:.1}x) | {}] `{}` <-> `{}` (latest {}: \"{}\") [expand: \"{}\"]\n",
                    peer.cochange_commits,
                    peer.anchor_total_commits,
                    peer.regularized_prob * 100.0,
                    peer.lift,
                    peer.confidence,
                    peer.rel_path,
                    peer.anchor_file,
                    display_metadata(&peer.recent_rev_short, 32),
                    display_metadata(&peer.recent_summary, 120),
                    peer.expand_id,
                )) && !body.push(&format!(
                    "  - [COCHANGE details omitted; expand: \"{}\"]\n",
                    peer.expand_id,
                )) {
                    break;
                }
            }
        }
        body.push(if enhance {
            "VERIFICATION_COMMANDS:\n"
        } else {
            "TEST_CANDIDATES:\n"
        });
        if pack.test_candidates.is_empty() {
            body.push("  (no direct test candidate mapped; check project test suite)\n");
        }
        for test in &pack.test_candidates {
            let expansion = test
                .test_symbol
                .as_ref()
                .map_or_else(String::new, |key| format!(" [expand: \"body:{key}\"]"));
            // Commands and paths remain exact: an abbreviated command is not runnable.
            if !body.push(&format!(
                "  - `{}` (cwd: `{}`, provenance: {} + {} in `{}`){}\n",
                test.command,
                test.cwd,
                test.manifest_path,
                test.provenance,
                test.test_file,
                expansion,
            )) {
                break;
            }
        }
        (format!("{header}{}{footer}", body.finish()), delivered)
    }

    /// Renders a source-free prompt enhancement blueprint within the complete budget.
    #[must_use]
    pub fn render_enhance_blueprint(pack: &ContextPack) -> String {
        let mut blueprint = pack.clone();
        blueprint.mode = QueryMode::Enhance;
        Self::render_context_pack_with_delivery(&blueprint).0
    }

    /// Renders an [`ExpandResponse`] compactly with explicit per-item statuses.
    #[must_use]
    pub fn render_expand_response(resp: &ExpandResponse) -> String {
        let mut out = format!(
            "[LEDGER_EXPAND handle={} | index={} | items={}]\n",
            resp.handle,
            resp.index_state,
            resp.results.len()
        );
        for item in &resp.results {
            let loc = item.rel_path.as_deref().unwrap_or("-");
            let hash = item.file_hash.as_ref().map_or("-", |h| h.short());
            out.push_str(&format!(
                "\nITEM [{} | status={:?} | file={} | blake3:{}]\n",
                item.item_id, item.status, loc, hash
            ));
            if !item.content.is_empty() {
                out.push_str(&item.content);
                if !item.content.ends_with('\n') {
                    out.push('\n');
                }
            }
            if let Some(cont) = &item.continuation_id {
                out.push_str(&format!(
                    "  // [truncated; continue with expand item: \"{cont}\"]\n"
                ));
            }
        }
        out
    }
}

/// Atomic-block writer. Reserve an explicit omission marker before accepting any block.
struct BudgetedText {
    text: String,
    max_bytes: usize,
    omitted: bool,
}

impl BudgetedText {
    const OMISSION: &str = "  ... [additional evidence omitted for token ceiling]\n";

    fn new(max_bytes: usize) -> Self {
        Self {
            text: String::new(),
            max_bytes,
            omitted: false,
        }
    }

    fn remaining(&self) -> usize {
        self.max_bytes
            .saturating_sub(self.text.len() + Self::OMISSION.len())
    }

    fn push(&mut self, block: &str) -> bool {
        if block.len() > self.remaining() {
            self.omitted = true;
            return false;
        }
        self.text.push_str(block);
        true
    }

    fn finish(mut self) -> String {
        if self.omitted {
            if Self::OMISSION.len() <= self.max_bytes.saturating_sub(self.text.len()) {
                self.text.push_str(Self::OMISSION);
            } else {
                // Supports even malformed, sub-protocol budgets without UTF-8 slicing.
                self.text
                    .push_str(&"omitted"[..self.max_bytes.saturating_sub(self.text.len()).min(7)]);
            }
        }
        self.text
    }
}

/// Abbreviate descriptive prose only; source, commands and expansion IDs stay exact.
fn display_metadata(raw: &str, max_bytes: usize) -> String {
    let sanitized: String = raw.chars().filter(|ch| !ch.is_control()).collect();
    if sanitized.len() <= max_bytes {
        return sanitized;
    }
    let mut end = max_bytes.saturating_sub(3).min(sanitized.len());
    while !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &sanitized[..end])
}
