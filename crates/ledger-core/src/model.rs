//! Core repository graph entities, typed relationships, confidence classes, and contracts.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::ids::{ByteSpan, ContentHash, GenerationId, InterfaceHash, LineSpan, LogicalSymbolKey};

/// Programming or configuration language recognized by LEDGER.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    /// Rust (`.rs`).
    Rust,
    /// TypeScript / TSX (`.ts`, `.tsx`).
    TypeScript,
    /// JavaScript / JSX (`.js`, `.jsx`, `.mjs`, `.cjs`).
    JavaScript,
    /// Python (`.py`).
    Python,
    /// Go (`.go`).
    Go,
    /// GraphQL schemas and operations (`.graphql`, `.gql`).
    Graphql,
    /// SQL migrations and queries (`.sql`).
    Sql,
    /// Markdown / ADR documentation (`.md`, `.mdx`).
    Markdown,
    /// Structured configuration (`.toml`, `.yaml`, `.yml`, `.json`).
    Config,
    /// Any other text file covered by lexical search.
    Other,
}

impl Language {
    /// Detects language from a relative file path extension.
    #[must_use]
    pub fn from_path(path: &Path) -> Self {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "rs" => Self::Rust,
            "ts" | "tsx" => Self::TypeScript,
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "py" => Self::Python,
            "go" => Self::Go,
            "graphql" | "gql" => Self::Graphql,
            "sql" => Self::Sql,
            "md" | "mdx" => Self::Markdown,
            "toml" | "yaml" | "yml" | "json" => Self::Config,
            _ => Self::Other,
        }
    }

    /// Short canonical identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::JavaScript => "javascript",
            Self::Python => "python",
            Self::Go => "go",
            Self::Graphql => "graphql",
            Self::Sql => "sql",
            Self::Markdown => "markdown",
            Self::Config => "config",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Inferred architectural subsystem layer for semantic filtering (`"backend"`, `"frontend"`, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchitecturalLayer {
    /// Backend API, services, handlers, domain logic.
    Backend,
    /// Web UI, components, frontend hooks, client state.
    Frontend,
    /// SQL schemas, migrations, repositories, data layer.
    Database,
    /// CI/CD, Docker, Terraform, scripts, deployment config.
    Infra,
    /// Unit, integration, and end-to-end test suites.
    Tests,
    /// Architecture docs, READMEs, ADRs.
    Docs,
    /// Shared libraries or unclassified core packages.
    Shared,
}

impl ArchitecturalLayer {
    /// Canonical lowercase label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Backend => "backend",
            Self::Frontend => "frontend",
            Self::Database => "database",
            Self::Infra => "infra",
            Self::Tests => "tests",
            Self::Docs => "docs",
            Self::Shared => "shared",
        }
    }
}

impl fmt::Display for ArchitecturalLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Structural coverage depth achieved when indexing a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseCoverage {
    /// Full Tree-sitter AST + scoped symbol & contract extraction succeeded cleanly.
    ScopedAst,
    /// Tree-sitter recovered partial AST despite syntax errors (e.g., mid-edit buffer).
    PartialAstWithSyntaxErrors,
    /// Contract + lexical extraction (SQL, config, Markdown).
    ContractsAndLexical,
    /// Lexical search only.
    LexicalOnly,
}

impl ParseCoverage {
    /// Human-readable status token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ScopedAst => "scoped_ast",
            Self::PartialAstWithSyntaxErrors => "partial_ast(syntax_error)",
            Self::ContractsAndLexical => "contracts+lexical",
            Self::LexicalOnly => "lexical_only",
        }
    }
}

/// Indexed file metadata record pinned to a specific content digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRecord {
    /// Workspace-relative POSIX path (`src/orders/query.rs`).
    pub rel_path: String,
    /// Detected language.
    pub language: Language,
    /// Inferred architectural layer.
    pub layer: ArchitecturalLayer,
    /// BLAKE3 digest of full file bytes.
    pub content_hash: ContentHash,
    /// BLAKE3 digest of exported symbols, signatures, imports, and contracts.
    pub interface_hash: InterfaceHash,
    /// Parse coverage achieved on this file version.
    pub coverage: ParseCoverage,
    /// Total byte length.
    pub byte_len: usize,
    /// Total 1-indexed line count.
    pub line_count: u32,
    /// Extracted semantic concept tokens (path roles, split identifiers, doc headings).
    pub concepts: Vec<String>,
}

/// Category of AST symbol extracted from source code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    /// Free function.
    Function,
    /// Associated method on a struct/class/impl/receiver.
    Method,
    /// Struct or class definition.
    StructOrClass,
    /// Trait or interface definition.
    TraitOrInterface,
    /// Enum or union type.
    Enum,
    /// Type alias.
    TypeAlias,
    /// Constant or static configuration symbol.
    Constant,
    /// Module or namespace declaration.
    Module,
    /// Dedicated unit or integration test function.
    Test,
}

impl SymbolKind {
    /// Short display tag.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Function => "fn",
            Self::Method => "method",
            Self::StructOrClass => "struct",
            Self::TraitOrInterface => "trait",
            Self::Enum => "enum",
            Self::TypeAlias => "type",
            Self::Constant => "const",
            Self::Module => "mod",
            Self::Test => "test",
        }
    }
}

impl fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Versioned AST symbol record extracted from a `FileRecord`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolRecord {
    /// Stable logical slot identifier (`sym_...`).
    pub logical_key: LogicalSymbolKey,
    /// Workspace-relative file path containing the symbol.
    pub rel_path: String,
    /// Symbol kind (`fn`, `method`, `struct`, `test`, ...).
    pub kind: SymbolKind,
    /// Simple identifier name (`apply_predicates`).
    pub name: String,
    /// Enclosing scope/receiver (`OrderFilter` or module chain), if any.
    pub scope: Option<String>,
    /// Cleaned declaration signature (`pub fn apply_predicates(qb: &mut QueryBuilder, f: &OrderFilter)`).
    pub signature: String,
    /// Leading docstring / comment summary, if present.
    pub docstring: Option<String>,
    /// Whether the symbol is exported / public outside its file.
    pub exported: bool,
    /// Byte span in the file buffer.
    pub byte_span: ByteSpan,
    /// 1-indexed inclusive line span in the file buffer.
    pub line_span: LineSpan,
    /// BLAKE3 digest of the signature text.
    pub signature_hash: ContentHash,
    /// BLAKE3 digest of the complete symbol body bytes.
    pub body_hash: ContentHash,
}

/// Kind of cross-boundary non-AST contract extracted from code or SQL/config files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ContractKind {
    /// Normalized HTTP method + route template (`GET /api/orders/{param}`).
    HttpEndpoint,
    /// GraphQL schema field or query/mutation operation (`gql:mutation:archiveOrder` or `gql:op:ArchiveOrder`).
    GraphqlSchemaOp,
    /// SQL table or view referenced by migration/DDL or query (`sql:table:orders`).
    SqlTableAccess,
    /// Environment variable / configuration key (`env:DATABASE_URL`, never secret values).
    ConfigEnvKey,
    /// Pub/sub or event bus topic (`event:order.archived`).
    EventChannel,
}

impl fmt::Display for ContractKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::HttpEndpoint => "HTTP_ENDPOINT",
            Self::GraphqlSchemaOp => "GRAPHQL_SCHEMA_OP",
            Self::SqlTableAccess => "SQL_TABLE_ACCESS",
            Self::ConfigEnvKey => "CONFIG_ENV_KEY",
            Self::EventChannel => "EVENT_CHANNEL",
        };
        f.write_str(s)
    }
}

/// Producer or consumer endpoint participating in a cross-boundary contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractEndpoint {
    /// Contract domain.
    pub kind: ContractKind,
    /// Normalized namespaced key (`GET /api/orders/{param}`, `env:ORDER_PAGE_LIMIT`).
    pub normalized_key: String,
    /// Role (`"provider"` such as route handler/DDL, or `"consumer"` such as `fetch`/query/reader).
    pub role: String,
    /// File containing the contract reference.
    pub rel_path: String,
    /// Enclosing symbol, if inside a function/method.
    pub enclosing_symbol: Option<LogicalSymbolKey>,
    /// Line span where the contract appears.
    pub line_span: LineSpan,
    /// Compact evidence snippet (with secret values stripped).
    pub snippet: String,
    /// Whether the contract path/topic had dynamic interpolation (`DYNAMIC_PREFIX`).
    pub is_dynamic: bool,
}

/// Semantic/structural relationship kind (strictly separated from resolver confidence).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RelationKind {
    /// File imports target file or exported symbol.
    Imports,
    /// Source symbol invokes target symbol.
    Calls,
    /// Type implements trait/interface or references type.
    ImplementsOrRefsType,
    /// Cross-boundary HTTP route handler <-> client call.
    HttpEndpoint,
    /// Cross-boundary GraphQL schema/resolver <-> client query/mutation operation.
    GraphqlSchemaOp,
    /// Cross-boundary SQL DDL/migration <-> query/ORM access.
    SqlTableAccess,
    /// Cross-boundary environment/config key shared across modules.
    ConfigEnvKey,
    /// Cross-boundary event producer <-> consumer.
    EventChannel,
    /// Test function covers target symbol or route.
    Tests,
}

impl fmt::Display for RelationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Imports => "IMPORTS",
            Self::Calls => "CALLS",
            Self::ImplementsOrRefsType => "TYPE_REF",
            Self::HttpEndpoint => "HTTP_ENDPOINT",
            Self::GraphqlSchemaOp => "GRAPHQL_SCHEMA_OP",
            Self::SqlTableAccess => "SQL_TABLE_ACCESS",
            Self::ConfigEnvKey => "CONFIG_ENV_KEY",
            Self::EventChannel => "EVENT_CHANNEL",
            Self::Tests => "TESTS",
        };
        f.write_str(s)
    }
}

/// Explicit resolver confidence attached to every graph edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeConfidence {
    /// Unambiguous same-file or unique scoped resolution.
    Exact,
    /// Resolved via file import table + exported symbol matching.
    ScopedHeuristic,
    /// Verified producer + consumer pair within the same contract namespace.
    VerifiedPair,
    /// Dynamic prefix/interpolated contract match.
    DynamicPrefix,
    /// Multiple plausible targets exist (`candidate_count` > 1).
    Ambiguous {
        /// Number of plausible candidate symbols with the same name.
        candidate_count: u16,
    },
    /// Target file's `InterfaceHash` changed on disk after `GenerationId` without full re-index yet.
    StaleAfterEdit,
}

impl fmt::Display for EdgeConfidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact => f.write_str("EXACT"),
            Self::ScopedHeuristic => f.write_str("SCOPED_HEURISTIC"),
            Self::VerifiedPair => f.write_str("VERIFIED_PAIR"),
            Self::DynamicPrefix => f.write_str("DYNAMIC_PREFIX"),
            Self::Ambiguous { candidate_count } => write!(f, "AMBIGUOUS({candidate_count})"),
            Self::StaleAfterEdit => f.write_str("STALE_AFTER_EDIT"),
        }
    }
}

/// Directed typed relationship edge between two files or symbols.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedEdge {
    /// Source file path.
    pub src_file: String,
    /// Optional source symbol key.
    pub src_symbol: Option<LogicalSymbolKey>,
    /// Destination file path.
    pub dst_file: String,
    /// Optional destination symbol key.
    pub dst_symbol: Option<LogicalSymbolKey>,
    /// Relationship category.
    pub relation: RelationKind,
    /// Resolver certainty.
    pub confidence: EdgeConfidence,
    /// 1-indexed line where the call/import/contract occurs in `src_file`.
    pub evidence_line: u32,
    /// Compact 1-line call-site or contract snippet.
    pub evidence_snippet: String,
}

/// Basis on which a candidate test command was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TestProvenance {
    /// Test body directly calls or references the target symbol.
    DirectSymbolRef,
    /// Test exercises the same HTTP route or contract key.
    ContractRouteRef,
    /// Test historically co-changes with the target file across VCS revisions.
    HistoricalCochange,
    /// Test resides in the same module (`#[cfg(test)]`) or filename convention (`*_test.go`, `test_*.py`).
    ModuleOrNamingProximity,
}

impl fmt::Display for TestProvenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::DirectSymbolRef => "DIRECT_SYMBOL_REF",
            Self::ContractRouteRef => "CONTRACT_ROUTE_REF",
            Self::HistoricalCochange => "HISTORICAL_COCHANGE",
            Self::ModuleOrNamingProximity => "MODULE_OR_NAME_PROXIMITY",
        };
        f.write_str(s)
    }
}

/// Candidate test command derived from project manifests + graph edges (never auto-executed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestCandidate {
    /// Suggested CLI command (`cargo test -p acme-api orders::cursor`).
    pub command: String,
    /// Workspace-relative working directory to run `command` in.
    pub cwd: String,
    /// Manifest file used to derive the runner (`Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`).
    pub manifest_path: String,
    /// Test file path.
    pub test_file: String,
    /// Specific test symbol key, if applicable.
    pub test_symbol: Option<LogicalSymbolKey>,
    /// Evidence basis linking this test to the target.
    pub provenance: TestProvenance,
}

/// Detected Version Control System backing the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VcsBackendKind {
    /// Standard Git repository (`.git`).
    Git,
    /// Standalone Jujutsu repository (`.jj` without colocated `.git`).
    Jujutsu,
    /// Colocated Jujutsu + Git repository (`.jj` + `.git`).
    JujutsuColocatedGit,
    /// No VCS repository detected.
    #[default]
    None,
}

impl fmt::Display for VcsBackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Git => "git",
            Self::Jujutsu => "jj",
            Self::JujutsuColocatedGit => "jj+git",
            Self::None => "none",
        };
        f.write_str(s)
    }
}

/// Attribution granularity of a historical VCS touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HunkConfidence {
    /// Historical diff hunk overlapped a tracked AST symbol's line span.
    HunkVerifiedSymbol,
    /// File-level revision touch without hunk-to-symbol overlap.
    FileLevel,
}

impl fmt::Display for HunkConfidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HunkVerifiedSymbol => f.write_str("HUNK_VERIFIED_SYMBOL"),
            Self::FileLevel => f.write_str("FILE_LEVEL"),
        }
    }
}

/// ScopeGuard-filtered file modification inside a single VCS revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcsFileTouch {
    /// Workspace-relative POSIX path after the revision.
    pub rel_path: String,
    /// Previous workspace-relative POSIX path if renamed/moved in this revision.
    pub old_rel_path: Option<String>,
    /// Lines added in this revision.
    pub added_lines: u32,
    /// Lines deleted in this revision.
    pub deleted_lines: u32,
    /// Optional 1-indexed modified line ranges in the new file version (`@@ -a,b +start,count @@`).
    #[serde(default)]
    pub hunk_new_ranges: Vec<LineSpan>,
}

/// Sanitized, `ScopePolicy`-filtered VCS revision record from Git or Jujutsu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcsRevision {
    /// Canonical commit SHA or `jj` commit ID.
    pub rev_id: String,
    /// Compact display badge (`88f1c2a` or `qpvuntsm(88f1c2)`).
    pub short_id: String,
    /// Author/committer Unix timestamp in seconds.
    pub timestamp_secs: i64,
    /// Sanitized author identifier.
    pub author: String,
    /// Sanitized single-line commit summary (untrusted repository evidence).
    pub summary: String,
    /// Allowed files modified in this revision (`ScopeGuard`-denied paths already excluded).
    pub touched_files: Vec<VcsFileTouch>,
}

/// Non-transitive 1-hop historical co-change peer surfaced in `BLAST_RADIUS_AND_COCHANGE`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoChangePeer {
    /// Co-changing peer file path.
    pub rel_path: String,
    /// Primary `EDIT_TARGETS` file that triggered this 1-hop co-change link.
    pub anchor_file: String,
    /// Raw number of non-sweep commits touching both `anchor_file` and `rel_path`.
    pub cochange_commits: u32,
    /// Raw total non-sweep commits touching `anchor_file`.
    pub anchor_total_commits: u32,
    /// Regularized conditional probability `P_reg(peer | anchor)` (`alpha = 3.0`).
    pub regularized_prob: f32,
    /// Lift suppressing high-churn global files (`P_reg(peer | anchor) / P(peer)`).
    pub lift: f32,
    /// Attribution granularity (`HUNK_VERIFIED_SYMBOL` or `FILE_LEVEL`).
    pub confidence: HunkConfidence,
    /// Most recent shared revision short ID.
    pub recent_rev_short: String,
    /// Sanitized summary of the most recent shared revision.
    pub recent_summary: String,
    /// Actionable expansion ID (`"diff:<rev_id>:<rel_path>"` or `"slice:<rel_path>:1-40"`).
    pub expand_id: String,
}

/// Provenance-only historical commit hit from `fts_lex_history` (weight `0.35`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryHit {
    /// Canonical revision ID.
    pub rev_id: String,
    /// Short revision badge.
    pub short_id: String,
    /// Revision timestamp in Unix seconds.
    pub timestamp_secs: i64,
    /// Sanitized commit summary.
    pub summary: String,
    /// Allowed touched files in this revision.
    pub touched_files: Vec<String>,
    /// Positive `0.35`-weighted relevance score.
    pub score: f32,
}

/// Current readiness state of the workspace index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum IndexState {
    /// Cold start: serving live lexical + on-demand AST while background graph builds.
    Warming {
        /// Number of files indexed so far.
        indexed_files: usize,
        /// Total allowed files discovered.
        total_files: usize,
    },
    /// Full snapshot generation published and verified.
    Ready {
        /// Active generation identifier.
        generation: GenerationId,
        /// Number of files in the published snapshot.
        indexed_files: usize,
    },
    /// Published snapshot supplemented with live on-disk buffer re-parses for recently edited files.
    ReadyWithLiveOverlay {
        /// Base generation identifier.
        base_generation: GenerationId,
        /// Workspace-relative paths re-parsed live from disk after `base_generation`.
        dirty_files: Vec<String>,
    },
}

impl fmt::Display for IndexState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Warming {
                indexed_files,
                total_files,
            } => write!(
                f,
                "warming(live_lexical+local_ast; graph {indexed_files}/{total_files} files)"
            ),
            Self::Ready { generation, .. } => write!(f, "ready({generation})"),
            Self::ReadyWithLiveOverlay {
                base_generation,
                dirty_files,
            } => write!(
                f,
                "live_overlay({base_generation} + {} dirty file(s))",
                dirty_files.len()
            ),
        }
    }
}
