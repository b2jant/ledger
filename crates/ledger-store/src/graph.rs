//! Compact in-memory `petgraph` snapshot (`GraphGeneration`) and lightweight handle registry.
//!
//! At most 2 `GraphGeneration` instances exist in RAM simultaneously (active + building).
//! Snapshot handles (`HandleRegistry`) store bounded hashes and symbol descriptors,
//! never `Arc<GraphGeneration>` clones.

use std::collections::HashMap;
use std::num::NonZeroUsize;

use ledger_core::{
    ArchitecturalLayer, ContentHash, EdgeConfidence, FileRecord, GenerationId, InterfaceHash,
    LogicalSymbolKey, ParseCoverage, RelationKind, SymbolRecord, TestCandidate, TypedEdge,
};
use ledger_parser::ResolvedWorkspaceGraph;
use lru::LruCache;
use parking_lot::Mutex;
use petgraph::Direction;
use petgraph::stable_graph::{NodeIndex, StableDiGraph};
use petgraph::visit::EdgeRef;

/// Node payload inside [`GraphGeneration`]'s `petgraph`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphNode {
    /// File node index into `GraphGeneration::files`.
    File(usize),
    /// Symbol node index into `GraphGeneration::symbols`.
    Symbol(usize),
}

/// Edge payload inside [`GraphGeneration`]'s `petgraph`.
#[derive(Debug, Clone)]
pub struct GraphEdge {
    /// Index into `GraphGeneration::edges`.
    pub edge_idx: usize,
    /// Structural/contract propagation weight for spreading activation (`0.0..=1.0`).
    pub weight: f32,
}

/// Immutable in-memory snapshot generation matching an atomic SQLite commit.
#[derive(Debug)]
pub struct GraphGeneration {
    /// Generation ID matching SQLite `meta.current_gen_id`.
    pub gen_id: GenerationId,
    /// All indexed file records.
    pub files: Vec<FileRecord>,
    /// All indexed symbol records.
    pub symbols: Vec<SymbolRecord>,
    /// All typed edges.
    pub edges: Vec<TypedEdge>,
    /// Manifest-derived test candidates keyed by target `rel_path`.
    pub tests_by_file: HashMap<String, Vec<TestCandidate>>,

    file_idx_by_path: HashMap<String, usize>,
    symbol_idx_by_key: HashMap<LogicalSymbolKey, usize>,
    symbols_by_file: HashMap<String, Vec<usize>>,
    symbol_idxs_by_lower_name: HashMap<String, Vec<usize>>,

    graph: StableDiGraph<GraphNode, GraphEdge, u32>,
    file_nodes: Vec<NodeIndex<u32>>,
    symbol_nodes: Vec<NodeIndex<u32>>,
}

impl GraphGeneration {
    /// Builds an immutable `GraphGeneration` from a [`ResolvedWorkspaceGraph`] and `gen_id`.
    #[must_use]
    pub fn build(gen_id: GenerationId, resolved: ResolvedWorkspaceGraph) -> Self {
        let mut graph = StableDiGraph::<GraphNode, GraphEdge, u32>::with_capacity(
            resolved.files.len() + resolved.symbols.len(),
            resolved.edges.len() + resolved.symbols.len(),
        );

        let mut file_idx_by_path = HashMap::with_capacity(resolved.files.len());
        let mut file_nodes = Vec::with_capacity(resolved.files.len());
        for (idx, f) in resolved.files.iter().enumerate() {
            file_idx_by_path.insert(f.rel_path.clone(), idx);
            file_nodes.push(graph.add_node(GraphNode::File(idx)));
        }

        let mut symbol_idx_by_key = HashMap::with_capacity(resolved.symbols.len());
        let mut symbols_by_file: HashMap<String, Vec<usize>> = HashMap::new();
        let mut symbol_idxs_by_lower_name: HashMap<String, Vec<usize>> = HashMap::new();
        let mut symbol_nodes = Vec::with_capacity(resolved.symbols.len());

        for (idx, sym) in resolved.symbols.iter().enumerate() {
            symbol_idx_by_key.insert(sym.logical_key.clone(), idx);
            symbols_by_file
                .entry(sym.rel_path.clone())
                .or_default()
                .push(idx);
            symbol_idxs_by_lower_name
                .entry(sym.name.to_ascii_lowercase())
                .or_default()
                .push(idx);
            let sym_node = graph.add_node(GraphNode::Symbol(idx));
            symbol_nodes.push(sym_node);
        }

        for (edge_idx, edge) in resolved.edges.iter().enumerate() {
            let src_node = edge
                .src_symbol
                .as_ref()
                .and_then(|k| symbol_idx_by_key.get(k))
                .map(|&i| symbol_nodes[i])
                .or_else(|| file_idx_by_path.get(&edge.src_file).map(|&i| file_nodes[i]));

            let dst_node = edge
                .dst_symbol
                .as_ref()
                .and_then(|k| symbol_idx_by_key.get(k))
                .map(|&i| symbol_nodes[i])
                .or_else(|| file_idx_by_path.get(&edge.dst_file).map(|&i| file_nodes[i]));

            if let (Some(u), Some(v)) = (src_node, dst_node) {
                let weight = Self::edge_propagation_weight(edge.relation, edge.confidence);
                graph.add_edge(u, v, GraphEdge { edge_idx, weight });
            }
        }

        Self {
            gen_id,
            files: resolved.files,
            symbols: resolved.symbols,
            edges: resolved.edges,
            tests_by_file: resolved.tests_by_target_file,
            file_idx_by_path,
            symbol_idx_by_key,
            symbols_by_file,
            symbol_idxs_by_lower_name,
            graph,
            file_nodes,
            symbol_nodes,
        }
    }

    const fn edge_propagation_weight(rel: RelationKind, conf: EdgeConfidence) -> f32 {
        let base = match rel {
            RelationKind::Calls => 0.85,
            RelationKind::HttpEndpoint
            | RelationKind::SqlTableAccess
            | RelationKind::GraphqlSchemaOp => 0.90,
            RelationKind::ConfigEnvKey | RelationKind::EventChannel => 0.75,
            RelationKind::ImplementsOrRefsType => 0.70,
            RelationKind::Imports => 0.45,
            RelationKind::Tests => 0.50,
        };
        let mult = match conf {
            EdgeConfidence::Exact | EdgeConfidence::VerifiedPair => 1.0,
            EdgeConfidence::ScopedHeuristic => 0.85,
            EdgeConfidence::DynamicPrefix => 0.65,
            EdgeConfidence::Ambiguous { .. } => 0.35,
            EdgeConfidence::StaleAfterEdit => 0.25,
        };
        base * mult
    }

    /// Looks up a [`FileRecord`] by relative path.
    #[must_use]
    pub fn file_by_path(&self, rel_path: &str) -> Option<&FileRecord> {
        self.file_idx_by_path.get(rel_path).map(|&i| &self.files[i])
    }

    /// Looks up a [`SymbolRecord`] by [`LogicalSymbolKey`].
    #[must_use]
    pub fn symbol_by_key(&self, key: &LogicalSymbolKey) -> Option<&SymbolRecord> {
        self.symbol_idx_by_key.get(key).map(|&i| &self.symbols[i])
    }

    /// Looks up symbols matching `name` case-insensitively.
    #[must_use]
    pub fn symbols_by_name_ci(&self, name: &str) -> Vec<&SymbolRecord> {
        let lower = name.to_ascii_lowercase();
        self.symbol_idxs_by_lower_name
            .get(&lower)
            .map(|idxs| idxs.iter().map(|&i| &self.symbols[i]).collect())
            .unwrap_or_default()
    }

    /// Returns all symbols inside `rel_path`.
    #[must_use]
    pub fn symbols_in_file(&self, rel_path: &str) -> Vec<&SymbolRecord> {
        self.symbols_by_file
            .get(rel_path)
            .map(|idxs| idxs.iter().map(|&i| &self.symbols[i]).collect())
            .unwrap_or_default()
    }

    /// Finds the innermost symbol enclosing `line` in `rel_path`.
    #[must_use]
    pub fn symbol_at_line(&self, rel_path: &str, line: u32) -> Option<&SymbolRecord> {
        self.symbols_in_file(rel_path)
            .into_iter()
            .filter(|s| line >= s.line_span.start_line && line <= s.line_span.end_line)
            .min_by_key(|s| s.line_span.line_count())
    }

    /// Runs bounded 2-step spreading activation from seed symbol scores `(LogicalSymbolKey -> seed_score)`,
    /// boosting structurally/contract-connected symbols and matching architectural layers.
    #[must_use]
    pub fn spread_symbol_scores(
        &self,
        seeds: &HashMap<LogicalSymbolKey, f32>,
        preferred_layer: Option<ArchitecturalLayer>,
    ) -> Vec<(&SymbolRecord, f32)> {
        let mut scores: HashMap<usize, f32> = HashMap::with_capacity(seeds.len() * 3);

        for (key, &seed_score) in seeds {
            if let Some(&sym_idx) = self.symbol_idx_by_key.get(key) {
                *scores.entry(sym_idx).or_insert(0.0) += seed_score;
                let node = self.symbol_nodes[sym_idx];

                // 1-hop outgoing & incoming structural/contract diffusion (damping = 0.28)
                for dir in [Direction::Outgoing, Direction::Incoming] {
                    for edge_ref in self.graph.edges_directed(node, dir) {
                        let neighbor = match dir {
                            Direction::Outgoing => edge_ref.target(),
                            Direction::Incoming => edge_ref.source(),
                        };
                        match self.graph[neighbor] {
                            GraphNode::Symbol(n_idx) => {
                                let sym = &self.symbols[n_idx];
                                // Keep test symbols in TEST_CANDIDATES rather than primary EDIT_TARGETS unless directly seeded.
                                if sym.kind != ledger_core::SymbolKind::Test {
                                    let delta = seed_score * 0.28 * edge_ref.weight().weight;
                                    *scores.entry(n_idx).or_insert(0.0) += delta;
                                }
                            }
                            GraphNode::File(f_idx) => {
                                let peer_path = &self.files[f_idx].rel_path;
                                if let Some(peer_syms) = self.symbols_by_file.get(peer_path) {
                                    for &n_idx in peer_syms.iter().take(3) {
                                        let sym = &self.symbols[n_idx];
                                        if sym.kind != ledger_core::SymbolKind::Test {
                                            let delta =
                                                seed_score * 0.16 * edge_ref.weight().weight;
                                            *scores.entry(n_idx).or_insert(0.0) += delta;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                // Also propagate file-level contract edges originating on the seed symbol's file node.
                if let Some(&f_idx) = self.file_idx_by_path.get(&self.symbols[sym_idx].rel_path) {
                    let f_node = self.file_nodes[f_idx];
                    for dir in [Direction::Outgoing, Direction::Incoming] {
                        for edge_ref in self.graph.edges_directed(f_node, dir) {
                            let neighbor = match dir {
                                Direction::Outgoing => edge_ref.target(),
                                Direction::Incoming => edge_ref.source(),
                            };
                            if let GraphNode::Symbol(n_idx) = self.graph[neighbor] {
                                let sym = &self.symbols[n_idx];
                                if sym.kind != ledger_core::SymbolKind::Test {
                                    let delta = seed_score * 0.20 * edge_ref.weight().weight;
                                    *scores.entry(n_idx).or_insert(0.0) += delta;
                                }
                            }
                        }
                    }
                }
            }
        }

        if let Some(layer) = preferred_layer {
            for (&sym_idx, score) in &mut scores {
                let sym = &self.symbols[sym_idx];
                if let Some(f) = self.file_by_path(&sym.rel_path)
                    && f.layer == layer
                {
                    *score *= 1.35;
                }
            }
        }

        let mut ranked: Vec<(&SymbolRecord, f32)> = scores
            .into_iter()
            .map(|(idx, sc)| (&self.symbols[idx], sc))
            .collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.rel_path.cmp(&b.0.rel_path))
                .then_with(|| a.0.line_span.start_line.cmp(&b.0.line_span.start_line))
        });
        ranked
    }

    /// Collects 1-hop incoming callers, outgoing callees, and cross-boundary contracts
    /// connected to `target_sym` or `target_file`, downgrading confidence to
    /// `EdgeConfidence::StaleAfterEdit` if a peer file's `InterfaceHash` changed on disk.
    #[must_use]
    pub fn collect_reference_edges(
        &self,
        target_files: &[String],
        target_symbols: &[LogicalSymbolKey],
        changed_interfaces: &HashMap<String, InterfaceHash>,
    ) -> Vec<TypedEdge> {
        let mut collected = Vec::new();
        let file_set: std::collections::HashSet<&str> =
            target_files.iter().map(String::as_str).collect();
        let sym_set: std::collections::HashSet<&LogicalSymbolKey> = target_symbols.iter().collect();

        for edge in &self.edges {
            if edge.relation == RelationKind::Tests {
                continue;
            }
            let touches_sym = edge
                .src_symbol
                .as_ref()
                .is_some_and(|k| sym_set.contains(k))
                || edge
                    .dst_symbol
                    .as_ref()
                    .is_some_and(|k| sym_set.contains(k));

            let touches_contract_file = matches!(
                edge.relation,
                RelationKind::HttpEndpoint
                    | RelationKind::SqlTableAccess
                    | RelationKind::ConfigEnvKey
                    | RelationKind::EventChannel
                    | RelationKind::GraphqlSchemaOp
            ) && (file_set.contains(edge.src_file.as_str())
                || file_set.contains(edge.dst_file.as_str()));

            let touches_incoming_file =
                edge.relation == RelationKind::Calls && file_set.contains(edge.dst_file.as_str());

            if touches_sym || touches_contract_file || touches_incoming_file {
                let mut e = edge.clone();
                // Conservative invalidation when either endpoint's exported interface changed after gen_id.
                if changed_interfaces.contains_key(&e.src_file)
                    || changed_interfaces.contains_key(&e.dst_file)
                {
                    e.confidence = EdgeConfidence::StaleAfterEdit;
                }
                collected.push(e);
            }
        }

        collected
    }

    /// Summarizes per-language coverage for the context pack header.
    #[must_use]
    pub fn summarize_language_coverage(&self) -> String {
        let mut counts: HashMap<(&'static str, &'static str), usize> = HashMap::new();
        for f in &self.files {
            if f.coverage != ParseCoverage::LexicalOnly
                || f.language != ledger_core::Language::Other
            {
                *counts
                    .entry((f.language.as_str(), f.coverage.as_str()))
                    .or_insert(0) += 1;
            }
        }
        if counts.is_empty() {
            return "lexical_only".to_string();
        }
        let mut items: Vec<_> = counts.into_iter().collect();
        items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.0.cmp(b.0.0)));
        items
            .into_iter()
            .take(5)
            .map(|((lang, cov), n)| format!("{lang}({cov}:{n})"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Immutable evidence baseline and bounded descriptors, with zero graph retention.
#[derive(Debug, Clone)]
pub struct HandleMetadata {
    /// Generation active when the handle was issued.
    pub generation: GenerationId,
    /// Map of `item_id -> ContentHash` delivered under this handle for idempotent `delta` checks.
    pub delivered_digests: HashMap<String, ContentHash>,
    /// Symbol identities needed to resolve an old or cold handle against live source.
    pub symbols: HashMap<LogicalSymbolKey, SymbolRecord>,
}

/// Thread-safe bounded LRU cache of issued [`SnapshotHandle`]s.
#[derive(Debug)]
pub struct HandleRegistry {
    cache: Mutex<LruCache<String, HandleMetadata>>,
}

impl Default for HandleRegistry {
    fn default() -> Self {
        Self::new(128)
    }
}

impl HandleRegistry {
    /// Maximum descriptors retained per handle. Callers should select semantic priority first.
    pub const MAX_SYMBOLS_PER_HANDLE: usize = 128;
    /// Maximum retained UTF-8 metadata bytes per handle, excluding fixed-size record fields.
    pub const MAX_SYMBOL_METADATA_BYTES: usize = 65_536;

    /// Creates a bounded `HandleRegistry` retaining at most `capacity` lightweight handles.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity.max(1)).unwrap_or(NonZeroUsize::MIN);
        Self {
            cache: Mutex::new(LruCache::new(cap)),
        }
    }

    /// Registers a newly issued handle and its initial delivered item digests.
    pub fn insert(
        &self,
        handle_str: String,
        generation: GenerationId,
        delivered_digests: HashMap<String, ContentHash>,
    ) {
        self.insert_with_symbols(handle_str, generation, delivered_digests, HashMap::new());
    }

    /// Registers an immutable baseline, retaining deterministic, bounded symbol metadata.
    ///
    /// Existing handles are never overwritten. Callers issuing a different response must
    /// use a new handle. Docstrings are discarded; unusually large signatures are omitted,
    /// while identity fields are preserved exactly or the descriptor is skipped entirely.
    pub fn insert_with_symbols(
        &self,
        handle_str: String,
        generation: GenerationId,
        delivered_digests: HashMap<String, ContentHash>,
        symbols: HashMap<LogicalSymbolKey, SymbolRecord>,
    ) {
        let mut ordered: Vec<_> = symbols.into_iter().collect();
        ordered.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        let mut bounded = HashMap::new();
        let mut retained_bytes = 0usize;
        for (key, mut symbol) in ordered {
            if bounded.len() == Self::MAX_SYMBOLS_PER_HANDLE {
                break;
            }
            if key != symbol.logical_key {
                continue;
            }
            symbol.docstring = None;
            if symbol.signature.len() > 2048 {
                symbol.signature = String::new();
            }
            let bytes = key.as_str().len()
                + symbol.logical_key.as_str().len()
                + symbol.rel_path.len()
                + symbol.name.len()
                + symbol.scope.as_ref().map_or(0, String::len)
                + symbol.signature.len()
                + symbol.signature_hash.as_str().len()
                + symbol.body_hash.as_str().len();
            if bytes > Self::MAX_SYMBOL_METADATA_BYTES.saturating_sub(retained_bytes) {
                continue;
            }
            retained_bytes += bytes;
            bounded.insert(key, symbol);
        }
        let mut cache = self.cache.lock();
        if !cache.contains(&handle_str) {
            cache.put(
                handle_str,
                HandleMetadata {
                    generation,
                    delivered_digests,
                    symbols: bounded,
                },
            );
        }
    }

    /// Looks up handle metadata without mutating delivered state.
    #[must_use]
    pub fn get(&self, handle_str: &str) -> Option<HandleMetadata> {
        self.cache.lock().get(handle_str).cloned()
    }
}
