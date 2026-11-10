//! Scoped cross-file symbol, import, contract, and manifest-backed test resolver.
//!
//! Assigns honest [`EdgeConfidence`] (`Exact`, `ScopedHeuristic`, `VerifiedPair`,
//! `DynamicPrefix`, or `Ambiguous { candidate_count }`) and caps ambiguous fan-out.

use std::collections::{HashMap, HashSet};

use ledger_core::{
    ArchitecturalLayer, ContractEndpoint, ContractKind, EdgeConfidence, FileRecord,
    LogicalSymbolKey, RelationKind, SymbolKind, SymbolRecord, TestCandidate, TestProvenance,
    TypedEdge,
};

use crate::extractors::ExtractedFile;

/// Discovered project runner manifest (`Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestInfo {
    /// Workspace-relative directory containing the manifest (`"."` or `"crates/acme-api"`).
    pub dir: String,
    /// Workspace-relative manifest path (`"crates/acme-api/Cargo.toml"`).
    pub manifest_path: String,
    /// Runner kind (`"cargo"`, `"npm"`, `"pytest"`, `"go"`).
    pub runner: String,
    /// Package name extracted from `Cargo.toml` (`name = "acme-api"`), if present.
    pub package_name: Option<String>,
}

/// Complete resolved workspace graph output ready for atomic SQLite + `petgraph` publication.
#[derive(Debug, Clone)]
pub struct ResolvedWorkspaceGraph {
    /// All indexed files.
    pub files: Vec<FileRecord>,
    /// All extracted AST symbols across `files`.
    pub symbols: Vec<SymbolRecord>,
    /// Resolved typed structural, contract, and test edges.
    pub edges: Vec<TypedEdge>,
    /// Candidate test commands keyed by target file path.
    pub tests_by_target_file: HashMap<String, Vec<TestCandidate>>,
}

/// Stateless workspace resolver transforming per-file [`ExtractedFile`] records into
/// a [`ResolvedWorkspaceGraph`].
#[derive(Debug, Default)]
pub struct ScopedResolver;

/// Identity of a resolved pair, retaining distinct enclosing symbols and contract keys.
#[derive(Debug, PartialEq, Eq, Hash)]
struct ContractPair {
    consumer_file: String,
    consumer_symbol: Option<LogicalSymbolKey>,
    provider_file: String,
    provider_symbol: Option<LogicalSymbolKey>,
    consumer_key: String,
    provider_key: String,
}

impl ScopedResolver {
    /// Maximum edges emitted for an `AMBIGUOUS` call site to prevent quadratic graph bloat.
    const MAX_AMBIGUOUS_FANOUT: usize = 3;

    /// Creates a new `ScopedResolver`.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Resolves imports, calls, cross-boundary contracts, and test commands across `extracted`.
    #[must_use]
    pub fn resolve(
        &self,
        extracted: &[ExtractedFile],
        manifests: &[ManifestInfo],
    ) -> ResolvedWorkspaceGraph {
        let mut files = Vec::with_capacity(extracted.len());
        let mut symbols = Vec::new();
        let mut edges = Vec::new();

        // Indexes for fast resolution
        let mut file_paths: HashSet<String> = HashSet::with_capacity(extracted.len());
        let mut symbols_by_file: HashMap<&str, Vec<&SymbolRecord>> = HashMap::new();
        let mut exported_by_name: HashMap<&str, Vec<&SymbolRecord>> = HashMap::new();
        let mut sym_by_key: HashMap<&LogicalSymbolKey, &SymbolRecord> = HashMap::new();

        for ef in extracted {
            files.push(ef.file.clone());
            file_paths.insert(ef.file.rel_path.clone());
            for sym in &ef.symbols {
                symbols.push(sym.clone());
                symbols_by_file
                    .entry(ef.file.rel_path.as_str())
                    .or_default()
                    .push(sym);
                sym_by_key.insert(&sym.logical_key, sym);
                if sym.exported && sym.kind != SymbolKind::Test {
                    exported_by_name
                        .entry(sym.name.as_str())
                        .or_default()
                        .push(sym);
                }
            }
        }

        // 1. Resolve file-to-file IMPORTS and build per-file imported-file set
        let mut imported_files_map: HashMap<String, HashSet<String>> = HashMap::new();
        for ef in extracted {
            let src_file = &ef.file.rel_path;
            for (line, imp_text) in &ef.imports {
                for target_file in Self::match_import_to_files(src_file, imp_text, &file_paths) {
                    if &target_file != src_file {
                        imported_files_map
                            .entry(src_file.clone())
                            .or_default()
                            .insert(target_file.clone());
                        edges.push(TypedEdge {
                            src_file: src_file.clone(),
                            src_symbol: None,
                            dst_file: target_file,
                            dst_symbol: None,
                            relation: RelationKind::Imports,
                            confidence: EdgeConfidence::ScopedHeuristic,
                            evidence_line: *line,
                            evidence_snippet: imp_text.clone(),
                        });
                    }
                }
            }
        }

        // 2. Resolve CALLS & TEST_REFERENCES_SYMBOL
        for ef in extracted {
            let src_file = ef.file.rel_path.as_str();
            let local_syms = symbols_by_file.get(src_file).cloned().unwrap_or_default();
            let imported_set = imported_files_map.get(src_file);

            for call in &ef.calls {
                let caller_is_test = call
                    .caller_symbol
                    .as_ref()
                    .and_then(|k| sym_by_key.get(k))
                    .is_some_and(|s| s.kind == SymbolKind::Test)
                    || ef.file.layer == ArchitecturalLayer::Tests
                    || Self::is_test_path(src_file);

                let is_generic = Self::is_generic_method_name(&call.callee_name);
                let qual_is_self = call
                    .qualifier
                    .as_deref()
                    .is_some_and(|q| q == "self" || q == "Self");

                // Tier A: Unambiguous same-file definition -> EXACT
                let same_file_matches: Vec<&SymbolRecord> = local_syms
                    .iter()
                    .copied()
                    .filter(|s| {
                        if s.name != call.callee_name
                            || s.kind == SymbolKind::Test
                            || Some(&s.logical_key) == call.caller_symbol.as_ref()
                        {
                            return false;
                        }
                        if s.kind == SymbolKind::Method && is_generic {
                            return qual_is_self
                                || call
                                    .qualifier
                                    .as_deref()
                                    .is_some_and(|q| s.scope.as_deref().is_some_and(|sc| sc == q));
                        }
                        true
                    })
                    .collect();

                if same_file_matches.len() == 1 {
                    let target = same_file_matches[0];
                    let rel = if caller_is_test {
                        RelationKind::Tests
                    } else {
                        RelationKind::Calls
                    };
                    edges.push(TypedEdge {
                        src_file: src_file.to_string(),
                        src_symbol: call.caller_symbol.clone(),
                        dst_file: target.rel_path.clone(),
                        dst_symbol: Some(target.logical_key.clone()),
                        relation: rel,
                        confidence: EdgeConfidence::Exact,
                        evidence_line: call.line,
                        evidence_snippet: call.snippet.clone(),
                    });
                    continue;
                }

                // Tier B: Exported symbol from an explicitly imported file or matching type qualifier -> SCOPED_HEURISTIC
                let Some(global_candidates) = exported_by_name.get(call.callee_name.as_str())
                else {
                    continue;
                };

                let import_scoped: Vec<&SymbolRecord> = global_candidates
                    .iter()
                    .copied()
                    .filter(|s| {
                        let qual_matches_scope = call.qualifier.as_ref().is_some_and(|q| {
                            s.scope
                                .as_ref()
                                .is_some_and(|sc| sc.eq_ignore_ascii_case(q))
                        });
                        if is_generic && s.kind == SymbolKind::Method {
                            // Generic method names (`new`, `insert`, `get`, `open`) require either explicit type qualifier
                            // (`ParserRegistry::new`) or an imported file whose scope/receiver plausibly matches.
                            return qual_matches_scope;
                        }
                        let qual_matches_path = call.qualifier.as_ref().is_some_and(|q| {
                            let q_low = q.to_ascii_lowercase();
                            q_low.len() >= 3 && s.rel_path.to_ascii_lowercase().contains(&q_low)
                        });
                        imported_set.is_some_and(|set| set.contains(&s.rel_path))
                            || qual_matches_scope
                            || qual_matches_path
                    })
                    .collect();

                if import_scoped.len() == 1 {
                    let target = import_scoped[0];
                    let rel = if caller_is_test {
                        RelationKind::Tests
                    } else {
                        RelationKind::Calls
                    };
                    edges.push(TypedEdge {
                        src_file: src_file.to_string(),
                        src_symbol: call.caller_symbol.clone(),
                        dst_file: target.rel_path.clone(),
                        dst_symbol: Some(target.logical_key.clone()),
                        relation: rel,
                        confidence: EdgeConfidence::ScopedHeuristic,
                        evidence_line: call.line,
                        evidence_snippet: call.snippet.clone(),
                    });
                    continue;
                }

                // Skip unimported global fallback for ubiquitous stdlib/collection method names (`insert`, `get`, `new`, etc.)
                // so `HashMap::insert` or `slice::get` never creates false edges to unrelated domain types.
                if is_generic {
                    continue;
                }

                // Tier C: Unique workspace-wide exported domain symbol -> SCOPED_HEURISTIC; else AMBIGUOUS(n)
                if global_candidates.len() == 1 {
                    let target = global_candidates[0];
                    if target.rel_path != src_file {
                        let rel = if caller_is_test {
                            RelationKind::Tests
                        } else {
                            RelationKind::Calls
                        };
                        edges.push(TypedEdge {
                            src_file: src_file.to_string(),
                            src_symbol: call.caller_symbol.clone(),
                            dst_file: target.rel_path.clone(),
                            dst_symbol: Some(target.logical_key.clone()),
                            relation: rel,
                            confidence: EdgeConfidence::ScopedHeuristic,
                            evidence_line: call.line,
                            evidence_snippet: call.snippet.clone(),
                        });
                    }
                } else if global_candidates.len() <= Self::MAX_AMBIGUOUS_FANOUT {
                    let candidate_count =
                        u16::try_from(global_candidates.len()).unwrap_or(u16::MAX);
                    for target in global_candidates {
                        if target.rel_path != src_file {
                            edges.push(TypedEdge {
                                src_file: src_file.to_string(),
                                src_symbol: call.caller_symbol.clone(),
                                dst_file: target.rel_path.clone(),
                                dst_symbol: Some(target.logical_key.clone()),
                                relation: RelationKind::Calls,
                                confidence: EdgeConfidence::Ambiguous { candidate_count },
                                evidence_line: call.line,
                                evidence_snippet: call.snippet.clone(),
                            });
                        }
                    }
                }
            }
        }

        // 3. Contract pairs require both endpoint roles; HTTP methods remain compatible.
        Self::resolve_contracts(extracted, &mut edges);

        // 4. Derive manifest-backed candidates in evidence-priority order.
        let tests_by_target_file = self.derive_test_candidates(extracted, &edges, manifests);

        ResolvedWorkspaceGraph {
            files,
            symbols,
            edges,
            tests_by_target_file,
        }
    }

    /// Derives test commands from direct references, shared contracts, and local tests.
    /// Stronger provenance wins before command deduplication and the per-file limit.
    #[must_use]
    pub fn derive_test_candidates(
        &self,
        extracted: &[ExtractedFile],
        edges: &[TypedEdge],
        manifests: &[ManifestInfo],
    ) -> HashMap<String, Vec<TestCandidate>> {
        let symbols = extracted
            .iter()
            .flat_map(|file| &file.symbols)
            .map(|symbol| (&symbol.logical_key, symbol))
            .collect();
        Self::build_test_candidates(extracted, edges, &symbols, manifests)
    }

    fn resolve_contracts(extracted: &[ExtractedFile], edges: &mut Vec<TypedEdge>) {
        let contracts: Vec<_> = extracted.iter().flat_map(|file| &file.contracts).collect();
        let mut by_key: HashMap<(ContractKind, &str), Vec<&ContractEndpoint>> = HashMap::new();
        let mut http_providers = Vec::new();
        for endpoint in &contracts {
            by_key
                .entry((endpoint.kind, endpoint.normalized_key.as_str()))
                .or_default()
                .push(endpoint);
            if endpoint.kind == ContractKind::HttpEndpoint && endpoint.role == "provider" {
                http_providers.push(*endpoint);
            }
        }
        let mut seen = HashSet::new();
        for consumer in &contracts {
            if consumer.role != "consumer" {
                continue;
            }
            let exact = by_key.get(&(consumer.kind, consumer.normalized_key.as_str()));
            for provider in exact
                .into_iter()
                .flatten()
                .filter(|endpoint| endpoint.role == "provider")
            {
                Self::push_contract_pair(consumer, provider, edges, &mut seen, false);
            }
            if consumer.kind == ContractKind::HttpEndpoint {
                for provider in &http_providers {
                    if Self::shared_contract(consumer, provider) {
                        let dynamic = consumer.normalized_key != provider.normalized_key
                            && consumer.is_dynamic;
                        Self::push_contract_pair(consumer, provider, edges, &mut seen, dynamic);
                    }
                }
            }
        }
        // Preserve existing shared environment-reader evidence without a template provider.
        for consumer in contracts.iter().filter(|endpoint| {
            endpoint.kind == ContractKind::ConfigEnvKey && endpoint.role == "consumer"
        }) {
            let Some(endpoints) = by_key.get(&(consumer.kind, consumer.normalized_key.as_str()))
            else {
                continue;
            };
            if endpoints.iter().any(|endpoint| endpoint.role == "provider") {
                continue;
            }
            if let Some(first) = endpoints.first()
                && first.rel_path != consumer.rel_path
            {
                edges.push(Self::contract_edge(
                    consumer,
                    first,
                    EdgeConfidence::VerifiedPair,
                ));
            }
        }
    }

    fn push_contract_pair(
        consumer: &ContractEndpoint,
        provider: &ContractEndpoint,
        edges: &mut Vec<TypedEdge>,
        seen: &mut HashSet<ContractPair>,
        dynamic_prefix: bool,
    ) {
        if consumer.rel_path == provider.rel_path {
            return;
        }
        let key = ContractPair {
            consumer_file: consumer.rel_path.clone(),
            consumer_symbol: consumer.enclosing_symbol.clone(),
            provider_file: provider.rel_path.clone(),
            provider_symbol: provider.enclosing_symbol.clone(),
            consumer_key: consumer.normalized_key.clone(),
            provider_key: provider.normalized_key.clone(),
        };
        if seen.insert(key) {
            let confidence = if dynamic_prefix || consumer.is_dynamic || provider.is_dynamic {
                EdgeConfidence::DynamicPrefix
            } else {
                EdgeConfidence::VerifiedPair
            };
            edges.push(Self::contract_edge(consumer, provider, confidence));
        }
    }

    fn contract_edge(
        consumer: &ContractEndpoint,
        provider: &ContractEndpoint,
        confidence: EdgeConfidence,
    ) -> TypedEdge {
        let relation = match provider.kind {
            ContractKind::HttpEndpoint => RelationKind::HttpEndpoint,
            ContractKind::SqlTableAccess => RelationKind::SqlTableAccess,
            ContractKind::ConfigEnvKey => RelationKind::ConfigEnvKey,
            ContractKind::EventChannel => RelationKind::EventChannel,
            ContractKind::GraphqlSchemaOp => RelationKind::GraphqlSchemaOp,
        };
        TypedEdge {
            src_file: consumer.rel_path.clone(),
            src_symbol: consumer.enclosing_symbol.clone(),
            dst_file: provider.rel_path.clone(),
            dst_symbol: provider.enclosing_symbol.clone(),
            relation,
            confidence,
            evidence_line: consumer.line_span.start_line,
            evidence_snippet: format!("{} ({})", provider.normalized_key, consumer.snippet),
        }
    }

    fn shared_contract(left: &ContractEndpoint, right: &ContractEndpoint) -> bool {
        if left.kind != right.kind {
            return false;
        }
        if left.normalized_key == right.normalized_key {
            return true;
        }
        if left.kind != ContractKind::HttpEndpoint {
            return false;
        }
        let Some((left_method, left_path)) = left.normalized_key.split_once(' ') else {
            return false;
        };
        let Some((right_method, right_path)) = right.normalized_key.split_once(' ') else {
            return false;
        };
        if left_method != right_method && left_method != "ANY" && right_method != "ANY" {
            return false;
        }
        left_path == right_path
            || (left.is_dynamic
                && left.role == "consumer"
                && Self::http_prefix_matches(left_path, right_path))
            || (right.is_dynamic
                && right.role == "consumer"
                && Self::http_prefix_matches(right_path, left_path))
    }

    fn http_prefix_matches(prefix: &str, route: &str) -> bool {
        // Require a concrete segment so a variable origin alone cannot match every route.
        let prefix: Vec<_> = prefix
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        let route: Vec<_> = route
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect();
        !prefix.is_empty()
            && prefix.iter().any(|segment| *segment != "{param}")
            && prefix.len() <= route.len()
            && prefix
                .iter()
                .zip(route)
                .all(|(left, right)| *left == "{param}" || *left == right)
    }

    fn is_test_contract(kind: ContractKind) -> bool {
        matches!(
            kind,
            ContractKind::HttpEndpoint | ContractKind::EventChannel | ContractKind::GraphqlSchemaOp
        )
    }

    fn is_test_path(path: &str) -> bool {
        path.split('/')
            .any(|segment| matches!(segment, "test" | "tests" | "__tests__"))
            || path.rsplit('/').next().is_some_and(|name| {
                name.starts_with("test_")
                    || name.contains("_test.")
                    || name.contains(".test.")
                    || name.contains(".spec.")
            })
    }

    const fn test_priority(provenance: TestProvenance) -> u8 {
        match provenance {
            TestProvenance::DirectSymbolRef => 0,
            TestProvenance::ContractRouteRef => 1,
            TestProvenance::HistoricalCochange => 2,
            TestProvenance::ModuleOrNamingProximity => 3,
        }
    }

    fn is_generic_method_name(name: &str) -> bool {
        matches!(
            name,
            "new"
                | "default"
                | "from"
                | "into"
                | "try_from"
                | "insert"
                | "get"
                | "get_mut"
                | "remove"
                | "contains"
                | "contains_key"
                | "clear"
                | "len"
                | "is_empty"
                | "push"
                | "pop"
                | "extend"
                | "append"
                | "iter"
                | "into_iter"
                | "clone"
                | "as_str"
                | "as_bytes"
                | "to_string"
                | "open"
                | "close"
                | "read"
                | "write"
                | "flush"
                | "status"
                | "build"
                | "run"
                | "walk"
                | "parse"
                | "fmt"
        )
    }

    fn match_import_to_files(
        src_file: &str,
        imp_text: &str,
        all_files: &HashSet<String>,
    ) -> Vec<String> {
        let mut matched = Vec::new();

        // Extract clean path/module tokens from Rust `crate::a::b`, TS `'./a/b'`, Python `from a.b import c`, Go `pkg/a/b`.
        let mut candidate_stems = Vec::new();
        if let Some(q_start) = imp_text.find(['\'', '"'])
            && let Some(q_end) = imp_text[q_start + 1..].find(['\'', '"'])
        {
            let spec = &imp_text[q_start + 1..q_start + 1 + q_end];
            candidate_stems.push(
                spec.trim_start_matches("./")
                    .trim_start_matches("../")
                    .to_string(),
            );
        } else {
            let normalized = imp_text
                .split('{')
                .next()
                .unwrap_or(imp_text)
                .replace("::", "/")
                .replace('.', "/");
            for tok in normalized.split_whitespace() {
                if !matches!(tok, "from" | "import" | "as" | "crate" | "self" | "super") {
                    let clean = tok
                        .trim_start_matches("crate/")
                        .trim_start_matches("super/")
                        .trim_matches('/');
                    if !clean.is_empty() {
                        candidate_stems.push(clean.to_string());
                    }
                }
            }
        }

        let src_dir = src_file.rsplit_once('/').map_or("", |(d, _)| d);

        for stem in candidate_stems {
            for ext in [
                ".rs",
                "/mod.rs",
                ".ts",
                ".tsx",
                "/index.ts",
                ".js",
                ".py",
                "/__init__.py",
            ] {
                let rel_try = if src_dir.is_empty() {
                    format!("{stem}{ext}")
                } else {
                    format!("{src_dir}/{stem}{ext}")
                };
                if all_files.contains(&rel_try) {
                    matched.push(rel_try);
                }
                let root_try = format!("src/{stem}{ext}");
                if all_files.contains(&root_try) {
                    matched.push(root_try);
                }
                let direct_try = format!("{stem}{ext}");
                if all_files.contains(&direct_try) {
                    matched.push(direct_try);
                }
            }
            // Strip trailing symbol segment (`crate/orders/query/apply_predicates` -> `src/orders/query.rs`)
            if let Some((parent_mod, _sym)) = stem.rsplit_once('/') {
                for prefix in ["src/", ""] {
                    for ext in [".rs", "/mod.rs", ".ts", ".tsx", ".py"] {
                        let p = format!("{prefix}{parent_mod}{ext}");
                        if all_files.contains(&p) {
                            matched.push(p);
                        }
                    }
                }
            }
        }

        matched.sort();
        matched.dedup();
        matched
    }

    fn build_test_candidates(
        extracted: &[ExtractedFile],
        edges: &[TypedEdge],
        sym_by_key: &HashMap<&LogicalSymbolKey, &SymbolRecord>,
        manifests: &[ManifestInfo],
    ) -> HashMap<String, Vec<TestCandidate>> {
        let mut map: HashMap<String, Vec<TestCandidate>> = HashMap::new();

        for edge in edges {
            if edge.relation == RelationKind::Tests {
                let test_sym = edge
                    .src_symbol
                    .as_ref()
                    .and_then(|key| sym_by_key.get(key).copied());
                if let Some(candidate) = Self::format_test_command(
                    &edge.src_file,
                    test_sym,
                    TestProvenance::DirectSymbolRef,
                    manifests,
                ) {
                    map.entry(edge.dst_file.clone())
                        .or_default()
                        .push(candidate);
                }
            }
        }

        // Test endpoints can be providers or consumers, and two clients can exercise
        // the same contract. Candidate evidence therefore does not depend on edge direction.
        let contracts: Vec<_> = extracted
            .iter()
            .flat_map(|file| &file.contracts)
            .filter(|endpoint| Self::is_test_contract(endpoint.kind))
            .collect();
        for test in &contracts {
            let file = extracted
                .iter()
                .find(|file| file.file.rel_path == test.rel_path);
            let symbol = test
                .enclosing_symbol
                .as_ref()
                .and_then(|key| sym_by_key.get(key).copied());
            let is_test = symbol.is_some_and(|symbol| symbol.kind == SymbolKind::Test)
                || Self::is_test_path(&test.rel_path)
                || file.is_some_and(|file| file.file.layer == ArchitecturalLayer::Tests);
            if !is_test {
                continue;
            }
            for target in &contracts {
                if target.rel_path == test.rel_path
                    && target.enclosing_symbol == test.enclosing_symbol
                {
                    continue;
                }
                if Self::shared_contract(test, target)
                    && let Some(candidate) = Self::format_test_command(
                        &test.rel_path,
                        symbol,
                        TestProvenance::ContractRouteRef,
                        manifests,
                    )
                {
                    map.entry(target.rel_path.clone())
                        .or_default()
                        .push(candidate);
                }
            }
        }

        // Same-file unit tests (e.g., Rust `#[cfg(test)] mod tests { #[test] fn ... }`)
        for ef in extracted {
            for sym in &ef.symbols {
                if sym.kind == SymbolKind::Test
                    && let Some(tc) = Self::format_test_command(
                        &ef.file.rel_path,
                        Some(sym),
                        TestProvenance::ModuleOrNamingProximity,
                        manifests,
                    )
                {
                    map.entry(ef.file.rel_path.clone()).or_default().push(tc);
                }
            }
        }

        // Deduplicate commands per target file
        for list in map.values_mut() {
            list.sort_by(|left, right| {
                Self::test_priority(left.provenance)
                    .cmp(&Self::test_priority(right.provenance))
                    .then_with(|| left.command.cmp(&right.command))
            });
            let mut seen = HashSet::new();
            list.retain(|tc| seen.insert(tc.command.clone()));
            list.truncate(4);
        }
        map
    }

    fn nearest_manifest<'a>(
        test_file: &str,
        runner: &str,
        manifests: &'a [ManifestInfo],
    ) -> Option<&'a ManifestInfo> {
        manifests
            .iter()
            .filter(|m| {
                m.runner == runner
                    && (m.dir == "." || test_file.starts_with(&format!("{}/", m.dir)))
            })
            .max_by_key(|m| m.dir.len())
    }

    /// Formats a runnable test command for `test_file` using the nearest enclosing [`ManifestInfo`].
    #[must_use]
    pub fn format_test_command(
        test_file: &str,
        test_sym: Option<&SymbolRecord>,
        provenance: TestProvenance,
        manifests: &[ManifestInfo],
    ) -> Option<TestCandidate> {
        if test_file.ends_with(".rs") {
            let m = Self::nearest_manifest(test_file, "cargo", manifests)?;
            let pkg_flag = m
                .package_name
                .as_ref()
                .map_or_else(String::new, |p| format!(" -p {p}"));
            let cmd = if test_file.starts_with("tests/") || test_file.contains("/tests/") {
                let it_name = test_file
                    .rsplit('/')
                    .next()
                    .unwrap_or(test_file)
                    .trim_end_matches(".rs");
                if let Some(sym) = test_sym {
                    format!("cargo test{pkg_flag} --test {it_name} {}", sym.name)
                } else {
                    format!("cargo test{pkg_flag} --test {it_name}")
                }
            } else if let Some(sym) = test_sym {
                format!("cargo test{pkg_flag} {}", sym.name)
            } else {
                format!("cargo test{pkg_flag}")
            };
            return Some(TestCandidate {
                command: cmd,
                cwd: ".".to_string(),
                manifest_path: m.manifest_path.clone(),
                test_file: test_file.to_string(),
                test_symbol: test_sym.map(|s| s.logical_key.clone()),
                provenance,
            });
        }

        if test_file.ends_with(".py") {
            let m = Self::nearest_manifest(test_file, "pytest", manifests);
            let manifest_path =
                m.map_or_else(|| "pyproject.toml".to_string(), |x| x.manifest_path.clone());
            let cwd = m.map_or_else(|| ".".to_string(), |x| x.dir.clone());
            let cmd = if let Some(sym) = test_sym {
                format!("pytest {test_file} -k {}", sym.name)
            } else {
                format!("pytest {test_file}")
            };
            return Some(TestCandidate {
                command: cmd,
                cwd,
                manifest_path,
                test_file: test_file.to_string(),
                test_symbol: test_sym.map(|s| s.logical_key.clone()),
                provenance,
            });
        }

        if test_file.ends_with(".go") {
            let m = Self::nearest_manifest(test_file, "go", manifests)?;
            let pkg_dir = test_file.rsplit_once('/').map_or(".", |(d, _)| d);
            let cmd = if let Some(sym) = test_sym {
                format!("go test ./{pkg_dir} -run ^{}$", sym.name)
            } else {
                format!("go test ./{pkg_dir}")
            };
            return Some(TestCandidate {
                command: cmd,
                cwd: m.dir.clone(),
                manifest_path: m.manifest_path.clone(),
                test_file: test_file.to_string(),
                test_symbol: test_sym.map(|s| s.logical_key.clone()),
                provenance,
            });
        }

        if test_file.ends_with(".ts") || test_file.ends_with(".tsx") || test_file.ends_with(".js") {
            let m = Self::nearest_manifest(test_file, "npm", manifests)?;
            return Some(TestCandidate {
                command: format!("npm test -- {test_file}"),
                cwd: m.dir.clone(),
                manifest_path: m.manifest_path.clone(),
                test_file: test_file.to_string(),
                test_symbol: test_sym.map(|s| s.logical_key.clone()),
                provenance,
            });
        }

        None
    }
}

/// Parses lightweight manifest metadata (`Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`)
/// from an allowed file buffer.
#[must_use]
pub fn detect_manifest(rel_path: &str, content: &str) -> Option<ManifestInfo> {
    let (dir, filename) = match rel_path.rsplit_once('/') {
        Some((d, f)) => (d.to_string(), f),
        None => (".".to_string(), rel_path),
    };
    match filename {
        "Cargo.toml" => {
            // Extract `[package] name = "..."` if not a virtual workspace root.
            let mut in_pkg = false;
            let mut pkg_name = None;
            for line in content.lines() {
                let t = line.trim();
                if t.starts_with('[') {
                    in_pkg = t == "[package]";
                } else if in_pkg
                    && let Some(rest) = t.strip_prefix("name")
                    && let Some((_, val)) = rest.split_once('=')
                {
                    let clean = val.trim().trim_matches('"').trim_matches('\'');
                    if !clean.is_empty() {
                        pkg_name = Some(clean.to_string());
                        break;
                    }
                }
            }
            if pkg_name.is_some() || content.contains("[workspace]") {
                Some(ManifestInfo {
                    dir,
                    manifest_path: rel_path.to_string(),
                    runner: "cargo".to_string(),
                    package_name: pkg_name,
                })
            } else {
                None
            }
        }
        "package.json" => Some(ManifestInfo {
            dir,
            manifest_path: rel_path.to_string(),
            runner: "npm".to_string(),
            package_name: None,
        }),
        "pyproject.toml" | "pytest.ini" => Some(ManifestInfo {
            dir,
            manifest_path: rel_path.to_string(),
            runner: "pytest".to_string(),
            package_name: None,
        }),
        "go.mod" => Some(ManifestInfo {
            dir,
            manifest_path: rel_path.to_string(),
            runner: "go".to_string(),
            package_name: None,
        }),
        _ => None,
    }
}
