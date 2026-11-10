//! Snapshot-consistent query, live dirty-buffer overlay, and indexing coordinator (`WorkspaceEngine`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use parking_lot::{Condvar, Mutex, RwLock};
use std::time::Instant;

use arc_swap::ArcSwapOption;
use ledger_core::tokens::{detect_requested_layer, extract_query_terms};
use ledger_core::{
    Anchor, AnchorKind, ApproxTokens, ContentHash, ContextPack, ContextRequest, DefaultScopeGuard,
    EdgeConfidence, ExpandDelivery, ExpandItemResult, ExpandRequest, ExpandResponse, ExpandStatus,
    FocusMode, GenerationId, IndexState, InterfaceHash, LineSpan, LogicalSymbolKey, PackFormatter,
    QueryMode, ReferenceItem, ScopePolicy, SingleBufferError, SingleBufferReader, SnapshotHandle,
    SymbolKind, SymbolRecord, TargetSlice, TelemetryRecord, TestCandidate, TestProvenance,
    WorkspaceId,
};
use ledger_parser::{ExtractedFile, ManifestInfo, ParserRegistry, ScopedResolver, detect_manifest};
use ledger_search::{LexicalSearcher, RipgrepSearcher, SearchBudget};
use ledger_store::{GraphGeneration, HandleRegistry, SqliteStore, StoreError, StoreStats};
use ledger_vcs::{
    BipartiteCoChangeIndex, CoChangeConfig, SystemCommandRunner, VcsBackend, VcsError,
    derive_cochange_test_files, detect_vcs_backend,
};
use thiserror::Error;

/// Immutable VCS history & co-change snapshot paired with a [`GraphGeneration`].
#[derive(Debug)]
struct VcsSnapshot {
    head_badge: Option<String>,
    head_digest: Option<String>,
    cochange: BipartiteCoChangeIndex,
    symbols_by_file: HashMap<String, Vec<SymbolRecord>>,
    manifests: Vec<ManifestInfo>,
}

/// Graph and history are always published and pinned together.
#[derive(Debug)]
struct PublishedSnapshot {
    graph: Arc<GraphGeneration>,
    vcs: VcsSnapshot,
}

#[derive(Debug, Clone)]
struct CachedFile {
    extracted: ExtractedFile,
    manifest: Option<ManifestInfo>,
}

#[derive(Debug, Default)]
struct IndexWaiter {
    result: Mutex<Option<Result<GenerationId, String>>>,
    ready: Condvar,
}

#[derive(Debug, Default)]
struct IndexCoordinator {
    closed: bool,
    running: bool,
    refresh_pending: bool,
    waiters: Vec<Arc<IndexWaiter>>,
    last_error: Option<String>,
}

#[derive(Debug, Default)]
struct HandleEvidence {
    symbols: HashMap<LogicalSymbolKey, SymbolRecord>,
    // Positional duplicate disambiguators are safe only while the original file is unchanged.
    ambiguous_files: HashMap<LogicalSymbolKey, ContentHash>,
    file_hashes: HashMap<String, ContentHash>,
}

#[derive(Debug, Default, Clone)]
struct HandleSourceBaseline {
    ambiguous_files: HashMap<LogicalSymbolKey, ContentHash>,
    file_hashes: HashMap<String, ContentHash>,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct IndexTestHooks {
    after_scan: Mutex<Option<(Arc<std::sync::Barrier>, Arc<std::sync::Barrier>)>>,
    passes: AtomicUsize,
    parses: AtomicUsize,
    panic_next: std::sync::atomic::AtomicBool,
}

/// Errors produced by [`WorkspaceEngine`].
#[derive(Debug, Error)]
pub enum EngineError {
    /// I/O error initializing workspace directories.
    #[error("workspace I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// SQLite store error.
    #[error("storage error: {0}")]
    Store(#[from] StoreError),
    /// Lexical search error.
    #[error("search error: {0}")]
    Search(#[from] ledger_search::SearchError),
    /// Invalid or expired snapshot handle passed to `ledger_expand`.
    #[error("snapshot handle expired or belongs to another workspace: '{0}'")]
    InvalidOrExpiredHandle(String),
    /// Index admission closed, a worker failed, or source could not be read safely.
    #[error("indexing error: {0}")]
    Indexing(String),
    /// Request metadata cannot fit within the rendered expansion budget.
    #[error("expansion budget: {0}")]
    ExpansionBudget(String),
    /// VCS evidence could not be loaded.
    #[error("VCS error: {0}")]
    Vcs(#[from] VcsError),
}

/// Core per-workspace intelligence engine shared across connections inside the Owner Daemon.
#[derive(Debug)]
pub struct WorkspaceEngine {
    canonical_root: PathBuf,
    workspace_id: WorkspaceId,
    guard: DefaultScopeGuard,
    reader: SingleBufferReader<DefaultScopeGuard>,
    searcher: RipgrepSearcher<DefaultScopeGuard>,
    parsers: ParserRegistry,
    resolver: ScopedResolver,
    store: SqliteStore,
    published: ArcSwapOption<PublishedSnapshot>,
    publication: RwLock<()>,
    index_cache: Mutex<HashMap<String, CachedFile>>,
    coordinator: Mutex<IndexCoordinator>,
    index_idle: Condvar,
    handle_namespace: String,
    query_serial: AtomicU64,
    #[cfg(test)]
    test_hooks: IndexTestHooks,
    handles: HandleRegistry,
    handle_baselines: Mutex<HashMap<String, HandleSourceBaseline>>,
    warming_indexed_files: AtomicUsize,
    warming_total_files: AtomicUsize,
}

impl WorkspaceEngine {
    /// Maximum lines included inline for a single primary target function before emitting an `expand` continuation.
    const MAX_INLINE_SYMBOL_LINES: u32 = 80;
    /// Maximum recent VCS revisions loaded into the bounded CSR co-change index.
    const MAX_VCS_REVISIONS: usize = 500;

    /// Opens or initializes `.ledger/` inside `workspace_root` with `0700` runtime permissions.
    pub fn open(workspace_root: &Path) -> Result<Arc<Self>, EngineError> {
        let guard = DefaultScopeGuard::new(workspace_root)?;
        let canonical_root = guard.workspace_root().to_path_buf();
        let workspace_id = WorkspaceId::from_canonical_path(&canonical_root);

        let ledger_dir = canonical_root.join(".ledger");
        let runtime_dir = ledger_dir.join("runtime");
        create_private_dir(&ledger_dir)?;
        create_private_dir(&runtime_dir)?;

        let db_path = ledger_dir.join("ledger.db");
        let store = SqliteStore::open(&db_path, workspace_id.clone())?;
        // Cold handles use generation zero on every startup. Give each engine lifetime
        // its own namespace so an old CLI handle cannot alias a new cold response.
        static ENGINE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let handle_namespace = format!(
            "{}:{:?}:{}",
            std::process::id(),
            std::time::SystemTime::now(),
            ENGINE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );

        Ok(Arc::new(Self {
            canonical_root,
            workspace_id,
            reader: SingleBufferReader::new(guard.clone()),
            searcher: RipgrepSearcher::new(guard.clone()),
            guard,
            parsers: ParserRegistry::new(),
            resolver: ScopedResolver::new(),
            store,
            published: ArcSwapOption::empty(),
            publication: RwLock::new(()),
            index_cache: Mutex::new(HashMap::new()),
            coordinator: Mutex::new(IndexCoordinator::default()),
            index_idle: Condvar::new(),
            handle_namespace,
            query_serial: AtomicU64::new(0),
            #[cfg(test)]
            test_hooks: IndexTestHooks::default(),
            handles: HandleRegistry::new(128),
            handle_baselines: Mutex::new(HashMap::new()),
            warming_indexed_files: AtomicUsize::new(0),
            warming_total_files: AtomicUsize::new(0),
        }))
    }

    fn vcs_backend(&self) -> Option<Arc<dyn VcsBackend>> {
        detect_vcs_backend(&self.canonical_root, Arc::new(SystemCommandRunner))
    }

    /// Canonical workspace root path.
    #[must_use]
    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    /// Deterministic workspace identifier.
    #[must_use]
    pub const fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    /// Persisted database incarnation ID.
    #[must_use]
    pub fn db_incarnation(&self) -> &str {
        self.store.db_incarnation()
    }

    /// Waits for a serialized scan starting after this request was admitted.
    pub fn index_workspace(self: &Arc<Self>) -> Result<GenerationId, EngineError> {
        let waiter = Arc::new(IndexWaiter::default());
        let start_worker = {
            let mut state = self.coordinator.lock();
            if state.closed {
                return Err(EngineError::Indexing("index admission is closed".into()));
            }
            state.waiters.push(Arc::clone(&waiter));
            let run = !state.running;
            state.running = true;
            run
        };
        if start_worker {
            // The caller waits for its own covering pass, not for all future polling
            // work. A dedicated worker may keep draining after this waiter completes.
            self.spawn_index_worker();
        }
        let mut result = waiter.result.lock();
        while result.is_none() {
            waiter.ready.wait(&mut result);
        }
        match result.take() {
            Some(Ok(generation)) => Ok(generation),
            Some(Err(error)) => Err(EngineError::Indexing(error)),
            None => Err(EngineError::Indexing(
                "index worker did not complete".into(),
            )),
        }
    }

    fn drain_index_jobs(&self) {
        loop {
            let waiters = {
                let mut state = self.coordinator.lock();
                if !state.refresh_pending && state.waiters.is_empty() {
                    state.running = false;
                    self.index_idle.notify_all();
                    return;
                }
                state.refresh_pending = false;
                std::mem::take(&mut state.waiters)
            };
            // A parser/VCS panic must release every waiter and allow shutdown to drain.
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.run_index_pass()))
                    .map_err(|_| "index worker panicked; retry explicit indexing".to_string())
                    .and_then(|result| result.map_err(|error| error.to_string()));
            if let Err(error) = &result {
                tracing::warn!(%error, "workspace indexing failed");
            }
            self.coordinator.lock().last_error = result.as_ref().err().cloned();
            for waiter in waiters {
                *waiter.result.lock() = Some(result.clone());
                waiter.ready.notify_all();
            }
        }
    }

    /// Closes admission, drains scheduled and active indexing, then checkpoints SQLite.
    /// Call outside an async executor; repeated calls are safe.
    pub fn shutdown_indexing(&self) -> Result<(), EngineError> {
        let mut state = self.coordinator.lock();
        state.closed = true;
        while state.running {
            self.index_idle.wait(&mut state);
        }
        self.store.checkpoint()?;
        Ok(())
    }

    fn run_index_pass(&self) -> Result<GenerationId, EngineError> {
        #[cfg(test)]
        {
            self.test_hooks.passes.fetch_add(1, Ordering::Relaxed);
            assert!(
                !self.test_hooks.panic_next.swap(false, Ordering::AcqRel),
                "injected worker panic"
            );
        }
        let (allowed_files, _secret_excluded) = self.searcher.list_allowed_files();
        self.warming_total_files
            .store(allowed_files.len(), Ordering::Relaxed);
        self.warming_indexed_files.store(0, Ordering::Relaxed);

        let previous = self.published.load_full();
        let vcs_backend = self.vcs_backend();
        let head_fp = match &vcs_backend {
            Some(backend) => backend.head_fingerprint(&self.canonical_root)?,
            None => None,
        };
        let current_head_digest = head_fp.as_ref().map(|fp| fp.state_digest.clone());
        let current_head_badge = head_fp.as_ref().map(|fp| fp.display_badge.clone());
        let mut extracted_files = Vec::with_capacity(allowed_files.len());
        let mut manifests = Vec::new();
        let mut next_cache = HashMap::new();
        let cache = self.index_cache.lock();
        let mut allowed_files = allowed_files;
        allowed_files.sort_by(|a, b| a.1.cmp(&b.1));
        for (idx, (canonical_path, rel_path)) in allowed_files.iter().enumerate() {
            let buffer = match self.reader.read_file(canonical_path) {
                Ok(buffer) => buffer,
                Err(
                    SingleBufferError::DeniedByPolicy { .. }
                    | SingleBufferError::BinaryOrNonUtf8 { .. }
                    | SingleBufferError::FileTooLarge { .. },
                ) => continue,
                Err(SingleBufferError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    continue;
                }
                Err(error) => return Err(EngineError::Indexing(error.to_string())),
            };
            let cached = match cache.get(rel_path) {
                Some(cached) if &cached.extracted.file.content_hash == buffer.content_hash() => {
                    cached.clone()
                }
                _ => {
                    #[cfg(test)]
                    self.test_hooks.parses.fetch_add(1, Ordering::Relaxed);
                    CachedFile {
                        extracted: self.parsers.extract_file(&buffer),
                        manifest: detect_manifest(rel_path, buffer.as_str()),
                    }
                }
            };
            if let Some(manifest) = &cached.manifest {
                manifests.push(manifest.clone());
            }
            extracted_files.push(cached.extracted.clone());
            next_cache.insert(rel_path.clone(), cached);
            self.warming_indexed_files.store(idx + 1, Ordering::Relaxed);
        }
        drop(cache);
        *self.index_cache.lock() = next_cache;
        #[cfg(test)]
        {
            let pause = self.test_hooks.after_scan.lock().take();
            if let Some((arrived, resume)) = pause {
                arrived.wait();
                resume.wait();
            }
        }

        if let Some(previous) = &previous
            && previous.vcs.head_digest == current_head_digest
            && previous.graph.files.len() == extracted_files.len()
            && extracted_files.iter().all(|file| {
                previous
                    .graph
                    .file_by_path(&file.file.rel_path)
                    .is_some_and(|old| old.content_hash == file.file.content_hash)
            })
        {
            return Ok(previous.graph.gen_id);
        }
        let revisions = match &vcs_backend {
            Some(backend) => backend.recent_revisions(
                &self.canonical_root,
                &self.guard,
                Self::MAX_VCS_REVISIONS,
            )?,
            None => Vec::new(),
        };

        // Reverse newest-first VCS log into oldest-first before stable timestamp sorting
        // so same-second commits preserve parent-before-child rename order.
        let mut chronological_revs = revisions.clone();
        chronological_revs.reverse();
        let now_secs = i64::try_from(current_epoch_ms() / 1000).unwrap_or(0);
        let cochange =
            BipartiteCoChangeIndex::build(&chronological_revs, CoChangeConfig::default(), now_secs);

        let resolved = self.resolver.resolve(&extracted_files, &manifests);
        let mut symbols_by_file: HashMap<String, Vec<SymbolRecord>> = HashMap::new();
        for sym in &resolved.symbols {
            symbols_by_file
                .entry(sym.rel_path.clone())
                .or_default()
                .push(sym.clone());
        }

        let next_gen = self.store.current_generation()?.next();
        // Expensive graph construction precedes the short database/publication gate.
        let generation = Arc::new(GraphGeneration::build(next_gen, resolved.clone()));
        let snapshot = Arc::new(PublishedSnapshot {
            graph: generation,
            vcs: VcsSnapshot {
                head_badge: current_head_badge.clone(),
                head_digest: current_head_digest,
                cochange,
                symbols_by_file,
                manifests,
            },
        });
        let _publication = self.publication.write();
        self.store.publish_generation_with_vcs(
            next_gen,
            &resolved,
            current_head_badge.as_deref(),
            &revisions,
        )?;
        self.published.store(Some(snapshot));
        Ok(next_gen)
    }

    /// Schedules one coalesced refresh without blocking the caller. Admission is recorded
    /// before spawning, so shutdown also waits for jobs which have not started yet.
    pub fn trigger_background_index(self: &Arc<Self>) {
        {
            let mut state = self.coordinator.lock();
            if state.closed {
                return;
            }
            state.refresh_pending = true;
            if state.running {
                return;
            }
            state.running = true;
        }
        self.spawn_index_worker();
    }

    /// Called only after setting `running`, which registers ownership before spawn.
    fn spawn_index_worker(self: &Arc<Self>) {
        // An OS thread avoids a queued Tokio blocking job deadlocking runtime shutdown.
        let engine = Arc::clone(self);
        if let Err(error) = std::thread::Builder::new()
            .name("ledger-bg-index".to_string())
            .spawn(move || engine.drain_index_jobs())
        {
            let mut state = self.coordinator.lock();
            state.running = false;
            state.refresh_pending = false;
            state.last_error = Some(error.to_string());
            for waiter in state.waiters.drain(..) {
                *waiter.result.lock() = Some(Err(error.to_string()));
                waiter.ready.notify_all();
            }
            self.index_idle.notify_all();
        }
    }

    /// Compiles a fresh, snapshot-consistent [`ContextPack`] for `ledger_context` or `enhance`.
    pub fn query_context(
        self: &Arc<Self>,
        req: &ContextRequest,
    ) -> Result<ContextPack, EngineError> {
        let started = Instant::now();
        let budget = ApproxTokens(req.clamped_budget());
        let anchors: Vec<Anchor> = req
            .anchors
            .iter()
            .cloned()
            .map(|a| a.into_anchor())
            .collect();

        let (mut pack, mut evidence) = if let Some(snapshot) = self.published.load_full() {
            self.query_warm_snapshot(&snapshot, req, &anchors, budget)?
        } else {
            // Progressive Cold-Start Serving: kick off background index and serve immediately via live search + on-demand AST!
            self.trigger_background_index();
            self.query_cold_start_live(req, &anchors, budget)?
        };

        if let Some(error) = self.coordinator.lock().last_error.clone() {
            pack.unknowns.insert(0, format!("Indexed refresh failed: {error}; this response uses the last published snapshot plus available live evidence. Retry explicit indexing."));
            pack.route.push_str("+refresh_failed");
        }
        bound_handle_evidence(&mut pack, &mut evidence);
        let (rendered, delivered) = PackFormatter::render_context_pack_with_delivery(&pack);
        self.handles.insert_with_symbols(
            pack.handle.to_string(),
            pack.handle.generation,
            delivered,
            evidence.symbols,
        );
        {
            let mut baselines = self.handle_baselines.lock();
            baselines.retain(|handle, _| self.handles.get(handle).is_some());
            baselines.insert(
                pack.handle.to_string(),
                HandleSourceBaseline {
                    ambiguous_files: evidence.ambiguous_files,
                    file_hashes: evidence.file_hashes,
                },
            );
        }
        let approx_tokens = ApproxTokens::estimate(&rendered).0;
        let latency_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);

        let mut edit_target_files = Vec::new();
        for t in &pack.edit_targets {
            if !edit_target_files.contains(&t.rel_path) {
                edit_target_files.push(t.rel_path.clone());
            }
        }
        let mut reference_files = Vec::new();
        for r in &pack.reference_context {
            if !reference_files.contains(&r.rel_path) {
                reference_files.push(r.rel_path.clone());
            }
        }

        let _ = self.store.record_telemetry(&TelemetryRecord {
            timestamp_ms: current_epoch_ms(),
            tool_name: "ledger_context".to_string(),
            mode: format!("{:?}", req.mode).to_ascii_lowercase(),
            query_summary: req.task.chars().take(160).collect(),
            route: pack.route.clone(),
            generation: pack.handle.generation.0,
            edit_target_files,
            reference_files,
            latency_us,
            approx_tokens,
        });

        Ok(pack)
    }

    fn query_warm_snapshot(
        self: &Arc<Self>,
        initial_snapshot: &Arc<PublishedSnapshot>,
        req: &ContextRequest,
        anchors: &[Anchor],
        budget: ApproxTokens,
    ) -> Result<(ContextPack, HandleEvidence), EngineError> {
        let terms = extract_query_terms(&req.task);
        let preferred_layer = detect_requested_layer(&req.task);

        let (snapshot, fts_hits, pinned_history_hits) = {
            let _publication = self.publication.read();
            let snapshot = self
                .published
                .load_full()
                .unwrap_or_else(|| Arc::clone(initial_snapshot));
            let hits = self
                .store
                .search_fts_pinned(snapshot.graph.gen_id, &terms, 24)?;
            let history = self
                .store
                .search_history_pinned(snapshot.graph.gen_id, &terms, 6)?;
            (snapshot, hits, history)
        };
        let graph = &snapshot.graph;
        let vcs_snap = Some(&snapshot.vcs);

        let mut seed_scores: HashMap<LogicalSymbolKey, f32> = HashMap::new();
        let mut match_reasons: HashMap<LogicalSymbolKey, String> = HashMap::new();
        let mut literal_file_lines: Vec<(String, u32, String)> = Vec::new();
        let mut secret_excluded_count = 0usize;
        let mut used_literal_fast_path = false;

        // 1. Evaluate explicit anchors (`Literal`, `Path`, `Symbol`, `Route`).
        for anchor in anchors {
            let kind = anchor.resolved_kind();
            let val = anchor.value.trim();
            if val.is_empty() {
                continue;
            }
            match kind {
                AnchorKind::Literal | AnchorKind::Route => {
                    let report = self
                        .searcher
                        .search_literal(val, &SearchBudget::default())?;
                    secret_excluded_count = secret_excluded_count.max(report.secret_files_excluded);
                    used_literal_fast_path = true;
                    for m in report.matches.iter().take(8) {
                        if let Some(sym) = graph.symbol_at_line(&m.rel_path, m.line_number) {
                            *seed_scores.entry(sym.logical_key.clone()).or_insert(0.0) += 12.0;
                            match_reasons.insert(
                                sym.logical_key.clone(),
                                format!("EXACT_LITERAL \"{val}\""),
                            );
                        } else {
                            literal_file_lines.push((
                                m.rel_path.clone(),
                                m.line_number,
                                format!("EXACT_LITERAL \"{val}\""),
                            ));
                        }
                    }
                }
                AnchorKind::Path => {
                    let clean_p = val.trim_start_matches("./");
                    for sym in graph.symbols_in_file(clean_p) {
                        if sym.kind != SymbolKind::Test {
                            *seed_scores.entry(sym.logical_key.clone()).or_insert(0.0) += 8.0;
                            match_reasons
                                .entry(sym.logical_key.clone())
                                .or_insert_with(|| format!("PATH_ANCHOR \"{clean_p}\""));
                        }
                    }
                }
                AnchorKind::Symbol | AnchorKind::Auto => {
                    let sym_matches = graph.symbols_by_name_ci(val);
                    if !sym_matches.is_empty() {
                        for sym in sym_matches {
                            *seed_scores.entry(sym.logical_key.clone()).or_insert(0.0) += 10.0;
                            match_reasons.insert(
                                sym.logical_key.clone(),
                                format!("EXACT_SYMBOL \"{}\"", sym.name),
                            );
                        }
                    } else {
                        // Fall back to literal live search if symbol name isn't an AST declaration (e.g. error constant string).
                        let report = self
                            .searcher
                            .search_literal(val, &SearchBudget::default())?;
                        secret_excluded_count =
                            secret_excluded_count.max(report.secret_files_excluded);
                        if !report.matches.is_empty() {
                            used_literal_fast_path = true;
                            for m in report.matches.iter().take(6) {
                                if let Some(sym) = graph.symbol_at_line(&m.rel_path, m.line_number)
                                {
                                    *seed_scores.entry(sym.logical_key.clone()).or_insert(0.0) +=
                                        10.0;
                                    match_reasons.insert(
                                        sym.logical_key.clone(),
                                        format!("EXACT_LITERAL \"{val}\""),
                                    );
                                } else {
                                    literal_file_lines.push((
                                        m.rel_path.clone(),
                                        m.line_number,
                                        format!("EXACT_LITERAL \"{val}\""),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        // 2. Fold separated FTS5 channels (`lex_code` weight 1.0, `lex_docs` weight 0.6).
        for hit in fts_hits {
            *seed_scores.entry(hit.symbol_key.clone()).or_insert(0.0) += hit.score;
            match_reasons
                .entry(hit.symbol_key)
                .or_insert_with(|| format!("FTS_{}", hit.channel.to_ascii_uppercase()));
        }

        // 2b. Fold provenance-only `fts_lex_history` (`0.35` weight ceiling) when querying
        // `QueryMode::History` or when code/doc symbol channels matched nothing (e.g. pure
        // commit/incident vocabulary), preventing broad historical commits from polluting
        // primary `EDIT_TARGETS` on anchored code queries.
        let history_hits = if req.mode == QueryMode::History
            || (seed_scores.is_empty() && literal_file_lines.is_empty())
        {
            pinned_history_hits
        } else {
            Vec::new()
        };
        let history_weight_mult = if req.mode == QueryMode::History {
            2.0
        } else {
            1.0
        };
        for h_hit in &history_hits {
            for rel_path in &h_hit.touched_files {
                for sym in graph.symbols_in_file(rel_path) {
                    if sym.kind != SymbolKind::Test && sym.kind != SymbolKind::Module {
                        *seed_scores.entry(sym.logical_key.clone()).or_insert(0.0) +=
                            h_hit.score * history_weight_mult;
                        match_reasons
                            .entry(sym.logical_key.clone())
                            .or_insert_with(|| {
                                format!("FTS_LEX_HISTORY {} \"{}\"", h_hit.short_id, h_hit.summary)
                            });
                    }
                }
            }
        }

        // 3. If FTS + anchors found nothing (e.g. brand-new dirty file added 50ms ago or literal inside function body),
        // run live `RipgrepSearcher::search_terms`!
        if seed_scores.is_empty() && literal_file_lines.is_empty() && !terms.is_empty() {
            let report = self
                .searcher
                .search_terms(&terms, &SearchBudget::default())?;
            secret_excluded_count = secret_excluded_count.max(report.secret_files_excluded);
            for m in report.matches.iter().take(10) {
                if let Some(sym) = graph.symbol_at_line(&m.rel_path, m.line_number) {
                    *seed_scores.entry(sym.logical_key.clone()).or_insert(0.0) += 3.0;
                    match_reasons
                        .entry(sym.logical_key.clone())
                        .or_insert_with(|| "LIVE_LEXICAL_BODY_MATCH".to_string());
                } else {
                    literal_file_lines.push((
                        m.rel_path.clone(),
                        m.line_number,
                        "LIVE_LEXICAL_MATCH".to_string(),
                    ));
                }
            }
        }

        // 4. Spread activation across `petgraph` structural + contract topology.
        let mut ranked = graph.spread_symbol_scores(&seed_scores, preferred_layer);

        // Apply `focus_paths` (`Boost` vs `Restrict`).
        if !req.focus_paths.is_empty() {
            match req.focus_mode {
                FocusMode::Restrict => {
                    ranked.retain(|(sym, _)| {
                        req.focus_paths
                            .iter()
                            .any(|fp| sym.rel_path.starts_with(fp.trim_start_matches("./")))
                    });
                }
                FocusMode::Boost => {
                    for (sym, score) in &mut ranked {
                        if req
                            .focus_paths
                            .iter()
                            .any(|fp| sym.rel_path.starts_with(fp.trim_start_matches("./")))
                        {
                            *score *= 1.6;
                        }
                    }
                    ranked
                        .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                }
            }
        }

        // 5. Read winning files ONCE via `SingleBufferReader`, detect live dirty edits & interface changes!
        let max_targets = if used_literal_fast_path && ranked.len() == 1 {
            1
        } else {
            4
        };

        let mut buffer_cache: HashMap<String, ledger_core::FileBuffer> = HashMap::new();
        let mut dirty_files: Vec<String> = Vec::new();
        let mut changed_interfaces: HashMap<String, InterfaceHash> = HashMap::new();
        let mut edit_targets: Vec<TargetSlice> = Vec::new();
        let mut selected_files: Vec<String> = Vec::new();
        let mut selected_symbols: Vec<LogicalSymbolKey> = Vec::new();
        let mut symbol_descriptors = HashMap::new();
        let mut ambiguous_files = HashMap::new();
        let mut file_hashes = HashMap::new();
        let mut coverage_notes = Vec::new();

        for (sym, _score) in ranked.iter().take(max_targets) {
            if !buffer_cache.contains_key(&sym.rel_path) {
                let Ok(buf) = self
                    .reader
                    .read_file(&self.canonical_root.join(&sym.rel_path))
                else {
                    if !dirty_files.contains(&sym.rel_path) {
                        dirty_files.push(sym.rel_path.clone());
                    }
                    coverage_notes.push(format!(
                        "Indexed symbol {} is unavailable under current scope; stale span omitted.",
                        sym.name
                    ));
                    continue;
                };
                buffer_cache.insert(sym.rel_path.clone(), buf);
            }
            let Some(buf) = buffer_cache.get(&sym.rel_path) else {
                continue;
            };

            let indexed_file = graph.file_by_path(&sym.rel_path);
            let is_dirty = indexed_file.is_some_and(|f| &f.content_hash != buf.content_hash());

            let active_sym: SymbolRecord = if is_dirty {
                if !dirty_files.contains(&sym.rel_path) {
                    dirty_files.push(sym.rel_path.clone());
                }
                let live_extracted = self.parsers.extract_file(buf);
                if let Some(old_f) = indexed_file
                    && old_f.interface_hash != live_extracted.file.interface_hash
                {
                    changed_interfaces.insert(
                        sym.rel_path.clone(),
                        live_extracted.file.interface_hash.clone(),
                    );
                }
                let old_matches = graph
                    .symbols_in_file(&sym.rel_path)
                    .into_iter()
                    .filter(|old| {
                        old.kind == sym.kind && old.scope == sym.scope && old.name == sym.name
                    })
                    .count();
                let Some(live_symbol) = (old_matches == 1)
                    .then(|| resolve_live_symbol(sym, &live_extracted.symbols))
                    .flatten()
                else {
                    coverage_notes.push(format!(
                        "Deleted or ambiguous symbol {} in {}; stale span omitted.",
                        sym.name, sym.rel_path
                    ));
                    continue;
                };
                live_symbol.clone()
            } else {
                (*sym).clone()
            };

            let (render_span, omitted_expand_id) =
                if active_sym.line_span.line_count() > Self::MAX_INLINE_SYMBOL_LINES {
                    let end = active_sym
                        .line_span
                        .start_line
                        .saturating_add(Self::MAX_INLINE_SYMBOL_LINES - 1);
                    (
                        LineSpan::new(active_sym.line_span.start_line, end),
                        Some(format!("body:{}", active_sym.logical_key)),
                    )
                } else {
                    (active_sym.line_span, None)
                };

            let reason = match_reasons
                .get(&sym.logical_key)
                .cloned()
                .unwrap_or_else(|| "GRAPH_NEIGHBOR_PROPAGATION".to_string());

            let qualified_name = match &active_sym.scope {
                Some(sc) => format!("{sc}::{}", active_sym.name),
                None => active_sym.name.clone(),
            };

            symbol_descriptors.insert(
                active_sym.logical_key.clone(),
                bounded_descriptor(&active_sym),
            );
            file_hashes.insert(active_sym.rel_path.clone(), buf.content_hash().clone());
            if graph
                .symbols_in_file(&sym.rel_path)
                .into_iter()
                .filter(|old| {
                    old.kind == sym.kind && old.scope == sym.scope && old.name == sym.name
                })
                .count()
                > 1
            {
                ambiguous_files.insert(active_sym.logical_key.clone(), buf.content_hash().clone());
            }
            if !selected_files.contains(&active_sym.rel_path) {
                selected_files.push(active_sym.rel_path.clone());
            }
            selected_symbols.push(active_sym.logical_key.clone());

            edit_targets.push(TargetSlice {
                rel_path: active_sym.rel_path.clone(),
                symbol_key: Some(active_sym.logical_key),
                symbol_kind: Some(active_sym.kind),
                symbol_name: qualified_name,
                signature: active_sym.signature,
                line_span: render_span,
                file_hash: buf.content_hash().clone(),
                match_reason: reason,
                numbered_source: buf.render_numbered_slice(render_span),
                omitted_expand_id,
            });
        }

        // Include non-symbol literal matches (e.g., SQL migration or config file lines).
        for (rel_path, line_no, reason) in literal_file_lines {
            if edit_targets.len() >= max_targets || selected_files.contains(&rel_path) {
                continue;
            }
            let Ok(buf) = self.reader.read_file(&self.canonical_root.join(&rel_path)) else {
                continue;
            };
            if graph
                .file_by_path(&rel_path)
                .is_none_or(|file| &file.content_hash != buf.content_hash())
                && !dirty_files.contains(&rel_path)
            {
                dirty_files.push(rel_path.clone());
                coverage_notes.push(format!("Live lexical evidence in {rel_path} is newer than the indexed snapshot; graph relationships may be incomplete."));
            }
            file_hashes.insert(rel_path.clone(), buf.content_hash().clone());
            let span = LineSpan::new(line_no.saturating_sub(4).max(1), line_no.saturating_add(12));
            selected_files.push(rel_path.clone());
            edit_targets.push(TargetSlice {
                rel_path: rel_path.clone(),
                symbol_key: None,
                symbol_kind: None,
                symbol_name: rel_path,
                signature: format!("lines {span}"),
                line_span: span,
                file_hash: buf.content_hash().clone(),
                match_reason: reason,
                numbered_source: buf.render_numbered_slice(span),
                omitted_expand_id: None,
            });
        }

        // Check O(1) VCS HEAD / op_store fingerprint drift without blocking query latency.
        let live_head_fp = match self.vcs_backend() {
            Some(backend) => match backend.head_fingerprint(&self.canonical_root) {
                Ok(fingerprint) => fingerprint,
                Err(error) => {
                    coverage_notes.push(format!("VCS freshness could not be verified: {error}"));
                    self.trigger_background_index();
                    None
                }
            },
            None => None,
        };
        let vcs_head_moved = match (&vcs_snap, &live_head_fp) {
            (Some(v), Some(live)) => v.head_digest.as_deref() != Some(live.state_digest.as_str()),
            (Some(v), None) => v.head_digest.is_some(),
            (None, Some(_)) => true,
            (None, None) => false,
        };

        if !dirty_files.is_empty() || vcs_head_moved {
            // Schedule async advancement to gen_id + 1 without blocking this response!
            self.trigger_background_index();
        }

        // 6. Collect 1-hop `REFERENCE_CONTEXT` (Callers + Cross-Boundary Contracts),
        // respecting `changed_interfaces` (`StaleAfterEdit` downgrade).
        let raw_ref_edges =
            graph.collect_reference_edges(&selected_files, &selected_symbols, &changed_interfaces);
        let mut reference_context = Vec::new();
        let mut seen_refs: HashSet<(String, u32, String)> = HashSet::new();
        let mut ambiguous_count = 0usize;

        for mut edge in raw_ref_edges {
            if let EdgeConfidence::Ambiguous { .. } = edge.confidence {
                ambiguous_count += 1;
            }
            // Pick the side of the edge that provides external reference context.
            let (peer_file, peer_sym_key) = if selected_files.contains(&edge.dst_file)
                && !selected_files.contains(&edge.src_file)
            {
                (&edge.src_file, edge.src_symbol.as_ref())
            } else if selected_files.contains(&edge.src_file)
                && !selected_files.contains(&edge.dst_file)
            {
                (&edge.dst_file, edge.dst_symbol.as_ref())
            } else if let Some(src_k) = &edge.src_symbol
                && !selected_symbols.contains(src_k)
            {
                (&edge.src_file, Some(src_k))
            } else {
                continue;
            };

            let dedup = (
                peer_file.clone(),
                edge.evidence_line,
                edge.relation.to_string(),
            );
            if !seen_refs.insert(dedup) {
                continue;
            }

            let mut evidence_changed = false;
            let mut unavailable = false;
            // Validate the evidence file too, even when it is not a selected target.
            for path in [&edge.src_file, &edge.dst_file] {
                if !buffer_cache.contains_key(path) {
                    if let Ok(buffer) = self.reader.read_file(&self.canonical_root.join(path)) {
                        buffer_cache.insert(path.clone(), buffer);
                    } else {
                        unavailable = true;
                    }
                }
                let changed = buffer_cache.get(path).is_none_or(|buffer| {
                    graph
                        .file_by_path(path)
                        .is_none_or(|file| &file.content_hash != buffer.content_hash())
                });
                evidence_changed |= changed;
                if changed && !dirty_files.contains(path) {
                    dirty_files.push(path.clone());
                }
            }
            if unavailable {
                coverage_notes.push(format!("Indexed reference {} -> {} is unavailable under current scope; evidence omitted.", edge.src_file, edge.dst_file));
                continue;
            }
            let indexed_peer = peer_sym_key.and_then(|key| graph.symbol_by_key(key));
            let live_peer;
            let peer_sym = if let Some(old) = indexed_peer {
                let Some(buffer) = buffer_cache.get(peer_file) else {
                    continue;
                };
                if graph
                    .file_by_path(peer_file)
                    .is_some_and(|file| &file.content_hash != buffer.content_hash())
                {
                    let extracted = self.parsers.extract_file(buffer);
                    let old_matches = graph
                        .symbols_in_file(peer_file)
                        .into_iter()
                        .filter(|symbol| {
                            symbol.kind == old.kind
                                && symbol.scope == old.scope
                                && symbol.name == old.name
                        })
                        .count();
                    let Some(symbol) = (old_matches == 1)
                        .then(|| resolve_live_symbol(old, &extracted.symbols))
                        .flatten()
                    else {
                        coverage_notes.push(format!(
                            "Deleted or ambiguous reference {} in {}; indexed evidence omitted.",
                            old.name, old.rel_path
                        ));
                        continue;
                    };
                    live_peer = symbol.clone();
                    Some(&live_peer)
                } else {
                    Some(old)
                }
            } else {
                None
            };
            if let Some(symbol) = peer_sym {
                symbol_descriptors.insert(symbol.logical_key.clone(), bounded_descriptor(symbol));
                if let Some(buffer) = buffer_cache.get(peer_file) {
                    file_hashes.insert(peer_file.clone(), buffer.content_hash().clone());
                }
                if graph
                    .symbols_in_file(peer_file)
                    .into_iter()
                    .filter(|old| {
                        old.kind == symbol.kind
                            && old.scope == symbol.scope
                            && old.name == symbol.name
                    })
                    .count()
                    > 1
                    && let Some(buffer) = buffer_cache.get(peer_file)
                {
                    ambiguous_files
                        .insert(symbol.logical_key.clone(), buffer.content_hash().clone());
                }
            }
            if evidence_changed {
                edge.confidence = EdgeConfidence::StaleAfterEdit;
                edge.evidence_snippet = "Indexed relationship; source changed since snapshot, expand live source or re-index to verify.".into();
            }
            let peer_total = buffer_cache
                .get(peer_file)
                .map_or(1, ledger_core::FileBuffer::line_count);
            let peer_line = peer_sym.map_or_else(
                || {
                    if peer_file == &edge.src_file {
                        edge.evidence_line.min(peer_total).max(1)
                    } else {
                        1
                    }
                },
                |symbol| symbol.line_span.start_line,
            );
            let expand_id = match peer_sym {
                Some(symbol) => format!("body:{}", symbol.logical_key),
                None if peer_file == &edge.src_file => format!(
                    "slice:{peer_file}:{}-{}",
                    peer_line.saturating_sub(3).max(1),
                    peer_line.saturating_add(15).min(peer_total),
                ),
                // Evidence lines belong to the source of an edge. A symbol-less
                // destination has no known span; expose its complete live file,
                // paginated by expansion rather than borrowing a caller's lines.
                None => format!("slice:{peer_file}:1-{peer_total}"),
            };
            let peer_label = peer_sym.map(|s| s.name.clone());
            let summary = match &peer_label {
                Some(name) => format!("`{name}` -> {}", edge.evidence_snippet),
                None => edge.evidence_snippet.clone(),
            };

            reference_context.push(ReferenceItem {
                relation: edge.relation,
                confidence: edge.confidence,
                rel_path: peer_file.clone(),
                line: peer_line,
                peer_symbol_name: peer_label,
                summary,
                expand_id,
            });
            if reference_context.len() >= 10 {
                break;
            }
        }

        // 6b. Collect Non-Transitive 1-Hop VCS Co-Change Peers (`BLAST_RADIUS_AND_COCHANGE`).
        let cochange_max = match req.mode {
            QueryMode::BlastRadius | QueryMode::History => 8,
            _ => 5,
        };
        let cochange_peers = vcs_snap.as_ref().map_or_else(Vec::new, |v| {
            v.cochange
                .query_peers(&selected_files, &v.symbols_by_file, cochange_max)
        });

        // 7. Collect Manifest-Derived Test Candidates + Historical Co-Change Test Candidates.
        let mut test_candidates: Vec<TestCandidate> = Vec::new();
        let mut seen_cmds: HashSet<String> = HashSet::new();
        let mut seen_test_files: HashSet<String> = HashSet::new();
        for f in &selected_files {
            if let Some(list) = graph.tests_by_file.get(f) {
                for tc in list {
                    if seen_cmds.insert(tc.command.clone()) {
                        seen_test_files.insert(tc.test_file.clone());
                        test_candidates.push(tc.clone());
                    }
                }
            }
        }
        if let Some(v) = &vcs_snap {
            for co_test in derive_cochange_test_files(&cochange_peers) {
                if !seen_test_files.contains(&co_test.rel_path)
                    && let Some(tc) =
                        build_historical_test_candidate(&co_test.rel_path, &v.manifests)
                    && seen_cmds.insert(tc.command.clone())
                {
                    seen_test_files.insert(tc.test_file.clone());
                    test_candidates.push(tc);
                }
            }
        }

        if test_candidates.len() > 32 {
            coverage_notes.push(format!(
                "{} additional test candidate(s) omitted; run the broader project suite as needed.",
                test_candidates.len() - 32
            ));
            test_candidates.truncate(32);
        }
        // Test body IDs also need original descriptors for old-handle expansion.
        test_candidates.retain(|candidate| {
            if let Some(key) = &candidate.test_symbol {
                let Some(old) = graph.symbol_by_key(key) else { return false; };
                if !buffer_cache.contains_key(&old.rel_path) {
                    let Ok(buffer) = self.reader.read_file(&self.canonical_root.join(&old.rel_path)) else {
                        if !dirty_files.contains(&old.rel_path) { dirty_files.push(old.rel_path.clone()); }
                        coverage_notes.push(format!("Test candidate {} is unavailable under current scope; omitted.", old.rel_path));
                        return false;
                    };
                    buffer_cache.insert(old.rel_path.clone(), buffer);
                }
                let Some(buffer) = buffer_cache.get(&old.rel_path) else { return false; };
                let dirty = graph.file_by_path(&old.rel_path).is_none_or(|file| &file.content_hash != buffer.content_hash());
                let symbol = if dirty {
                    if !dirty_files.contains(&old.rel_path) { dirty_files.push(old.rel_path.clone()); }
                    coverage_notes.push(format!("Test candidate {} changed since indexing; its indexed relationship needs verification.", old.rel_path));
                    let extracted = self.parsers.extract_file(buffer);
                    let old_matches = graph.symbols_in_file(&old.rel_path).into_iter().filter(|symbol| {
                        symbol.kind == old.kind && symbol.scope == old.scope && symbol.name == old.name
                    }).count();
                    let Some(symbol) = (old_matches == 1).then(|| resolve_live_symbol(old, &extracted.symbols)).flatten() else { return false; };
                    symbol.clone()
                } else { old.clone() };
                symbol_descriptors.insert(key.clone(), bounded_descriptor(&symbol));
                file_hashes.insert(symbol.rel_path.clone(), buffer.content_hash().clone());
            }
            true
        });
        if !dirty_files.is_empty() {
            self.trigger_background_index();
        }
        dirty_files.sort();
        // 8. Assemble Honest Coverage & Unknowns.
        let mut unknowns = coverage_notes;
        if req.mode == QueryMode::History && !history_hits.is_empty() {
            for h in history_hits.iter().take(4) {
                unknowns.push(format!(
                    "history_hit [{} | score={:.2}]: \"{}\" (files: {:?})",
                    h.short_id, h.score, h.summary, h.touched_files
                ));
            }
        }
        let omitted_ranked = ranked.len().saturating_sub(edit_targets.len());
        if omitted_ranked > 0 {
            unknowns.push(format!(
                "{omitted_ranked} lower-ranked symbol candidate(s) omitted to keep context compact."
            ));
        }
        if ambiguous_count > 0 {
            unknowns.push(format!(
                "{ambiguous_count} call edge(s) marked AMBIGUOUS (multiple exported symbols share callee name; no LSP/SCIP typechecker loaded)."
            ));
        }
        if !changed_interfaces.is_empty() {
            unknowns.push(format!(
                "Exported interface changed on disk in {:?}; cross-file edges touching those files downgraded to STALE_AFTER_EDIT while background re-index runs.",
                changed_interfaces.keys().collect::<Vec<_>>()
            ));
        }
        if vcs_head_moved {
            unknowns.push(
                "VCS HEAD / operation head advanced on disk since snapshot; background re-index triggered."
                    .to_string(),
            );
        }
        if secret_excluded_count > 0 {
            unknowns.push(format!(
                "{secret_excluded_count} secret/credential file(s) excluded by ScopeGuard policy."
            ));
        }
        if let Some(v) = &vcs_snap
            && v.cochange.sweeps_filtered() > 0
        {
            unknowns.push(format!(
                "{} mass-formatting sweep revision(s) (>50 files) suppressed from co-change graph.",
                v.cochange.sweeps_filtered()
            ));
        }
        if unknowns.is_empty() {
            unknowns.push(format!(
                "Scanned {} indexed workspace files via import-scoped AST + cross-boundary contracts.",
                graph.files.len()
            ));
        }

        let index_state = if dirty_files.is_empty() {
            IndexState::Ready {
                generation: graph.gen_id,
                indexed_files: graph.files.len(),
            }
        } else {
            IndexState::ReadyWithLiveOverlay {
                base_generation: graph.gen_id,
                dirty_files,
            }
        };

        let seed = format!(
            "{}:{}:{}:{}",
            self.handle_namespace,
            req.task,
            graph.gen_id,
            self.query_serial.fetch_add(1, Ordering::Relaxed)
        );
        let handle = SnapshotHandle::new(&self.workspace_id, graph.gen_id, &seed);

        let route = if used_literal_fast_path {
            "literal_fast_path+1hop_graph+vcs_cochange".to_string()
        } else {
            "hybrid_fts5_ast_contracts_vcs".to_string()
        };
        let vcs_revision = snapshot.vcs.head_badge.clone();

        Ok((
            ContextPack {
                handle,
                workspace_root: self.canonical_root.display().to_string(),
                index_state,
                vcs_revision,
                route,
                mode: req.mode,
                edit_targets,
                reference_context,
                cochange_peers,
                test_candidates,
                language_coverage: graph.summarize_language_coverage(),
                unknowns,
                budget_ceiling: budget,
            },
            HandleEvidence {
                symbols: symbol_descriptors,
                ambiguous_files,
                file_hashes,
            },
        ))
    }

    fn query_cold_start_live(
        &self,
        req: &ContextRequest,
        anchors: &[Anchor],
        budget: ApproxTokens,
    ) -> Result<(ContextPack, HandleEvidence), EngineError> {
        let terms = extract_query_terms(&req.task);
        let mut report = None;

        for a in anchors {
            if a.resolved_kind() == AnchorKind::Path {
                let prefix = a
                    .value
                    .trim()
                    .trim_start_matches("./")
                    .trim_end_matches('/');
                let matches = self
                    .searcher
                    .list_allowed_files()
                    .0
                    .into_iter()
                    .filter(|(_, path)| path == prefix || path.starts_with(&format!("{prefix}/")))
                    .take(3)
                    .map(|(canonical_path, rel_path)| ledger_search::LexicalMatch {
                        canonical_path,
                        rel_path,
                        line_number: 1,
                        line_text: String::new(),
                    })
                    .collect::<Vec<_>>();
                if !matches.is_empty() {
                    report = Some((
                        ledger_search::LexicalSearchReport {
                            matches,
                            files_scanned: 0,
                            secret_files_excluded: 0,
                            search_incomplete: false,
                        },
                        format!("COLD_LIVE_PATH \"{prefix}\""),
                    ));
                    break;
                }
            }
            if !a.value.trim().is_empty() {
                let r = self
                    .searcher
                    .search_literal(a.value.trim(), &SearchBudget::default())?;
                if !r.matches.is_empty() {
                    report = Some((r, format!("COLD_LIVE_LITERAL \"{}\"", a.value.trim())));
                    break;
                }
            }
        }
        if report.is_none() && !terms.is_empty() {
            let r = self
                .searcher
                .search_terms(&terms, &SearchBudget::default())?;
            report = Some((r, "COLD_LIVE_TERMS".to_string()));
        }

        let mut edit_targets = Vec::new();
        let mut seen_files = HashSet::new();
        let mut symbol_descriptors = HashMap::new();
        let mut ambiguous_files = HashMap::new();
        let mut file_hashes = HashMap::new();
        let mut unknowns = vec![
            "Cross-file caller, contract & VCS co-change graph still warming in background; results come from live lexical + local AST parse.".to_string(),
        ];

        if let Some((rep, reason)) = report {
            if rep.search_incomplete {
                unknowns.push(
                    "search_incomplete: cold-start deadline reached before scanning all files."
                        .to_string(),
                );
            }
            if rep.secret_files_excluded > 0 {
                unknowns.push(format!(
                    "{} secret/credential file(s) excluded by ScopeGuard.",
                    rep.secret_files_excluded
                ));
            }
            for m in rep.matches {
                if edit_targets.len() >= 3 || !seen_files.insert(m.rel_path.clone()) {
                    continue;
                }
                let Ok(buf) = self.reader.read_file(&m.canonical_path) else {
                    continue;
                };
                let extracted = self.parsers.extract_file(&buf);
                if let Some(sym) = extracted
                    .symbols
                    .iter()
                    .filter(|s| {
                        m.line_number >= s.line_span.start_line
                            && m.line_number <= s.line_span.end_line
                    })
                    .min_by_key(|s| s.line_span.line_count())
                    .or_else(|| {
                        reason
                            .starts_with("COLD_LIVE_PATH")
                            .then(|| {
                                extracted
                                    .symbols
                                    .iter()
                                    .find(|symbol| symbol.kind != SymbolKind::Test)
                            })
                            .flatten()
                    })
                {
                    let span = LineSpan::new(
                        sym.line_span.start_line,
                        sym.line_span.end_line.min(
                            sym.line_span
                                .start_line
                                .saturating_add(Self::MAX_INLINE_SYMBOL_LINES - 1),
                        ),
                    );
                    symbol_descriptors.insert(sym.logical_key.clone(), bounded_descriptor(sym));
                    file_hashes.insert(sym.rel_path.clone(), buf.content_hash().clone());
                    if extracted
                        .symbols
                        .iter()
                        .filter(|other| {
                            other.kind == sym.kind
                                && other.scope == sym.scope
                                && other.name == sym.name
                        })
                        .count()
                        > 1
                    {
                        ambiguous_files.insert(sym.logical_key.clone(), buf.content_hash().clone());
                    }
                    edit_targets.push(TargetSlice {
                        rel_path: m.rel_path,
                        symbol_key: Some(sym.logical_key.clone()),
                        symbol_kind: Some(sym.kind),
                        symbol_name: sym.name.clone(),
                        signature: sym.signature.clone(),
                        line_span: span,
                        file_hash: buf.content_hash().clone(),
                        match_reason: reason.clone(),
                        numbered_source: buf.render_numbered_slice(span),
                        omitted_expand_id: (span != sym.line_span)
                            .then(|| format!("body:{}", sym.logical_key)),
                    });
                } else {
                    let span = LineSpan::new(
                        m.line_number.saturating_sub(4).max(1),
                        m.line_number.saturating_add(14),
                    );
                    edit_targets.push(TargetSlice {
                        rel_path: m.rel_path.clone(),
                        symbol_key: None,
                        symbol_kind: None,
                        symbol_name: m.rel_path,
                        signature: format!("lines {span}"),
                        line_span: span,
                        file_hash: buf.content_hash().clone(),
                        match_reason: reason.clone(),
                        numbered_source: buf.render_numbered_slice(span),
                        omitted_expand_id: None,
                    });
                }
            }
        }

        let handle = SnapshotHandle::new(
            &self.workspace_id,
            GenerationId::COLD_START,
            &format!(
                "cold:{}:{}:{}",
                self.handle_namespace,
                req.task,
                self.query_serial.fetch_add(1, Ordering::Relaxed)
            ),
        );
        let indexed = self.warming_indexed_files.load(Ordering::Relaxed);
        let total = self.warming_total_files.load(Ordering::Relaxed);
        let vcs_revision = self
            .vcs_backend()
            .and_then(|b| b.head_fingerprint(&self.canonical_root).ok().flatten())
            .map(|fp| fp.display_badge);

        Ok((
            ContextPack {
                handle,
                workspace_root: self.canonical_root.display().to_string(),
                index_state: IndexState::Warming {
                    indexed_files: indexed,
                    total_files: total,
                },
                vcs_revision,
                route: "cold_start_live_fast_path".to_string(),
                mode: req.mode,
                edit_targets,
                reference_context: Vec::new(),
                cochange_peers: Vec::new(),
                test_candidates: Vec::new(),
                language_coverage: "live_on_demand_ast".to_string(),
                unknowns,
                budget_ceiling: budget,
            },
            HandleEvidence {
                symbols: symbol_descriptors,
                ambiguous_files,
                file_hashes,
            },
        ))
    }

    /// Expands against the immutable initial delivery baseline. Continuations carry
    /// a content fingerprint and UTF-8 byte offset, so retries never mutate a handle.
    pub fn expand(&self, req: &ExpandRequest) -> Result<ExpandResponse, EngineError> {
        let started = Instant::now();
        let parsed_handle = SnapshotHandle::from_str(&req.handle)
            .map_err(|_| EngineError::InvalidOrExpiredHandle(req.handle.clone()))?;
        if parsed_handle.workspace_short != self.workspace_id.short_prefix() {
            return Err(EngineError::InvalidOrExpiredHandle(req.handle.clone()));
        }
        let Some(metadata) = self.handles.get(&req.handle) else {
            return Err(EngineError::InvalidOrExpiredHandle(req.handle.clone()));
        };
        if req.items.len() > 16 {
            return Err(EngineError::ExpansionBudget(
                "request at most 16 items per call".into(),
            ));
        }
        let baseline = self
            .handle_baselines
            .lock()
            .get(&req.handle)
            .cloned()
            .unwrap_or_default();
        let snapshot = self.published.load_full();
        let graph = snapshot.as_ref().map(|snapshot| snapshot.graph.as_ref());
        let mut chunks = Vec::new();
        let mut dirty_files = Vec::new();
        for item_id in &req.items {
            let cursor = parse_chunk_cursor(item_id);
            let base = cursor
                .as_ref()
                .map_or(item_id.as_str(), |cursor| cursor.base);
            let mut item = self.expand_single_item(
                base,
                &metadata,
                &baseline,
                if cursor.is_some() {
                    ExpandDelivery::Complete
                } else {
                    req.delivery
                },
                graph,
            );
            if (base.starts_with("body:") || base.starts_with("slice:"))
                && let (Some(graph), Some(path)) = (graph, &item.rel_path)
            {
                let changed = item.file_hash.as_ref().is_some_and(|hash| {
                    graph
                        .file_by_path(path)
                        .is_none_or(|file| &file.content_hash != hash)
                }) || matches!(
                    item.status,
                    ExpandStatus::SymbolDeleted | ExpandStatus::DeniedByPolicy
                );
                if changed && !dirty_files.contains(path) {
                    dirty_files.push(path.clone());
                }
            }
            let fingerprint = expansion_fingerprint(&item);
            let mut offset = 0;
            if let Some(cursor) = cursor {
                if cursor.fingerprint != fingerprint.as_str() {
                    item.status = ExpandStatus::UpdatedSinceHandle;
                    item.content =
                        format!("Source changed during continuation; restart with {base}");
                } else if cursor.offset >= item.content.len()
                    || !item.content.is_char_boundary(cursor.offset)
                {
                    item.status = ExpandStatus::InvalidItemId;
                    item.content = "Invalid continuation offset; restart the original item.".into();
                } else {
                    offset = cursor.offset;
                }
            }
            item.item_id = item_id.clone();
            let resumable = item_id == base || offset > 0;
            chunks.push(ExpansionChunk {
                item,
                base: base.to_string(),
                fingerprint,
                offset,
                resumable,
            });
        }
        let index_state = match graph {
            Some(graph) if !dirty_files.is_empty() => IndexState::ReadyWithLiveOverlay {
                base_generation: graph.gen_id,
                dirty_files,
            },
            Some(graph) => IndexState::Ready {
                generation: graph.gen_id,
                indexed_files: graph.files.len(),
            },
            None => IndexState::Warming {
                indexed_files: self.warming_indexed_files.load(Ordering::Relaxed),
                total_files: self.warming_total_files.load(Ordering::Relaxed),
            },
        };
        let mut response = ExpandResponse {
            handle: req.handle.clone(),
            index_state,
            results: Vec::new(),
        };
        let ceiling = req.max_approx_tokens.clamp(300, 12_000);
        // Reserve a progressing fragment (and its continuation metadata) for EVERY item
        // before allocating more content. Even the first item is subject to the budget.
        for chunk in &chunks {
            let remaining = &chunk.item.content[chunk.offset..];
            let min_bytes = remaining.chars().next().map_or(0, char::len_utf8);
            let partial = chunk.render(min_bytes);
            let full = chunk.render(remaining.len());
            let item_size = |item: &ExpandItemResult| {
                item.content.len() + item.continuation_id.as_ref().map_or(0, |id| id.len() + 60)
            };
            response
                .results
                .push(if item_size(&full) <= item_size(&partial) {
                    full
                } else {
                    partial
                });
        }
        if rendered_tokens(&response) > ceiling {
            return Err(EngineError::ExpansionBudget(
                "item headers and continuations exceed the budget; request fewer/shorter IDs or increase max_approx_tokens".into(),
            ));
        }
        for (index, chunk) in chunks.iter().enumerate() {
            let remaining = &chunk.item.content[chunk.offset..];
            response.results[index] = chunk.render(remaining.len());
            if rendered_tokens(&response) <= ceiling {
                continue;
            }
            let mut low = remaining.chars().next().map_or(0, char::len_utf8);
            let mut high = remaining.len();
            while low < high {
                let mut middle = low + (high - low).div_ceil(2);
                while !remaining.is_char_boundary(middle) {
                    middle -= 1;
                }
                if middle <= low {
                    break;
                }
                response.results[index] = chunk.render(middle);
                if rendered_tokens(&response) <= ceiling {
                    low = middle;
                } else {
                    high = middle - 1;
                }
            }
            response.results[index] = chunk.render(low);
        }
        let rendered = PackFormatter::render_expand_response(&response);
        if ApproxTokens::estimate(&rendered).0 > ceiling {
            return Err(EngineError::ExpansionBudget("rendered metadata exceeds the budget; request fewer items or increase max_approx_tokens".into()));
        }
        let _ = self.store.record_telemetry(&TelemetryRecord {
            timestamp_ms: current_epoch_ms(),
            tool_name: "ledger_expand".into(),
            mode: format!("{:?}", req.delivery).to_ascii_lowercase(),
            query_summary: req.items.join(", ").chars().take(160).collect(),
            route: "handle_expand".into(),
            generation: parsed_handle.generation.0,
            edit_target_files: response
                .results
                .iter()
                .filter_map(|item| item.rel_path.clone())
                .collect(),
            reference_files: Vec::new(),
            latency_us: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            approx_tokens: ApproxTokens::estimate(&rendered).0,
        });
        Ok(response)
    }

    fn expand_single_item(
        &self,
        item_id: &str,
        metadata: &ledger_store::HandleMetadata,
        baseline: &HandleSourceBaseline,
        delivery: ExpandDelivery,
        graph: Option<&GraphGeneration>,
    ) -> ExpandItemResult {
        let mut result = empty_expansion(item_id);
        if let Some(raw_key) = item_id.strip_prefix("body:") {
            let key = LogicalSymbolKey::from_raw(raw_key);
            // Prefer the original descriptor: a newer graph must not substitute a
            // different declaration for a deleted/ambiguous symbol in an old handle.
            let Some(descriptor) = metadata
                .symbols
                .get(&key)
                .or_else(|| graph.and_then(|graph| graph.symbol_by_key(&key)))
            else {
                result.status = ExpandStatus::SymbolDeleted;
                return result;
            };
            result.rel_path = Some(descriptor.rel_path.clone());
            let buffer = match self
                .reader
                .read_file(&self.canonical_root.join(&descriptor.rel_path))
            {
                Ok(buffer) => buffer,
                Err(error) => return expansion_read_error(result, &error),
            };
            result.file_hash = Some(buffer.content_hash().clone());
            let extracted = self.parsers.extract_file(&buffer);
            let Some(symbol) = resolve_handle_symbol(
                descriptor,
                &extracted.symbols,
                buffer.content_hash(),
                &baseline.ambiguous_files,
            ) else {
                result.status = ExpandStatus::SymbolDeleted;
                return result;
            };
            if delivery == ExpandDelivery::Delta
                && metadata.delivered_digests.get(item_id) == Some(buffer.content_hash())
            {
                result.status = ExpandStatus::UnchangedDeltaOmitted;
                result.content = "// [unchanged since initial context delivery]".into();
                return result;
            }
            result.status = if symbol.body_hash != descriptor.body_hash
                || symbol.signature_hash != descriptor.signature_hash
                || symbol.line_span != descriptor.line_span
                || baseline
                    .file_hashes
                    .get(&descriptor.rel_path)
                    .is_some_and(|hash| hash != buffer.content_hash())
            {
                ExpandStatus::UpdatedSinceHandle
            } else {
                ExpandStatus::Ok
            };
            result.content = buffer.render_numbered_slice(symbol.line_span);
            return result;
        }
        if let Some(raw_key) = item_id.strip_prefix("callers:") {
            let key = LogicalSymbolKey::from_raw(raw_key);
            let Some(graph) = graph else {
                result.content = "Caller graph is warming; request a body or slice, then retry callers after indexing.".into();
                return result;
            };
            let Some(descriptor) = metadata
                .symbols
                .get(&key)
                .or_else(|| graph.symbol_by_key(&key))
            else {
                result.status = ExpandStatus::SymbolDeleted;
                return result;
            };
            let buffer = match self
                .reader
                .read_file(&self.canonical_root.join(&descriptor.rel_path))
            {
                Ok(buffer) => buffer,
                Err(error) => return expansion_read_error(result, &error),
            };
            if resolve_handle_symbol(
                descriptor,
                &self.parsers.extract_file(&buffer).symbols,
                buffer.content_hash(),
                &baseline.ambiguous_files,
            )
            .is_none()
            {
                result.status = ExpandStatus::SymbolDeleted;
                return result;
            }
            let edges = graph.collect_reference_edges(&[], &[key], &HashMap::new());
            let mut lines = Vec::new();
            let mut versions = String::new();
            let mut changed = false;
            let mut live_files = HashMap::from([(descriptor.rel_path.clone(), Some(buffer))]);
            for edge in edges {
                let mut current = true;
                for path in [&edge.src_file, &edge.dst_file] {
                    let buffer = live_files.entry(path.clone()).or_insert_with(|| {
                        self.reader.read_file(&self.canonical_root.join(path)).ok()
                    });
                    versions.push_str(path);
                    if let Some(buffer) = buffer {
                        versions.push_str(buffer.content_hash().as_str());
                        current &= graph
                            .file_by_path(path)
                            .is_some_and(|file| &file.content_hash == buffer.content_hash());
                    } else {
                        versions.push_str("unavailable");
                        current = false;
                    }
                }
                changed |= !current;
                if current {
                    lines.push(format!(
                        "  - [{} | {}] {}:{} -> {} ({})",
                        edge.relation,
                        edge.confidence,
                        edge.src_file,
                        edge.evidence_line,
                        edge.dst_file,
                        edge.evidence_snippet
                    ));
                } else {
                    lines.push(format!("  - [{} | STALE_AFTER_EDIT] {} -> {} (indexed relationship; changed or unavailable source, re-index to verify)",
                        edge.relation, edge.src_file, edge.dst_file));
                }
            }
            result.status = if changed || graph.gen_id != metadata.generation {
                ExpandStatus::UpdatedSinceHandle
            } else {
                ExpandStatus::Ok
            };
            result.content = if lines.is_empty() {
                "  (no indexed callers found)".into()
            } else {
                lines.join("\n")
            };
            result.file_hash = Some(ContentHash::of_bytes(versions.as_bytes()));
            return result;
        }
        if let Some(rest) = item_id.strip_prefix("slice:")
            && let Some((path, range)) = rest.rsplit_once(':')
            && let Some((start, end)) = range.split_once('-')
            && let (Ok(start), Ok(end)) = (start.parse::<u32>(), end.parse::<u32>())
        {
            if start == 0 || end < start {
                result.content = "Use slice:<path>:<first>-<last> with 1 <= first <= last.".into();
                return result;
            }
            result.rel_path = Some(path.into());
            let buffer = match self.reader.read_file(&self.canonical_root.join(path)) {
                Ok(buffer) => buffer,
                Err(error) => return expansion_read_error(result, &error),
            };
            result.rel_path = Some(buffer.rel_path().into());
            result.file_hash = Some(buffer.content_hash().clone());
            if start > buffer.line_count() {
                result.content = format!(
                    "Start exceeds the current file's {} lines.",
                    buffer.line_count()
                );
                return result;
            }
            result.status = if baseline
                .file_hashes
                .get(buffer.rel_path())
                .is_some_and(|hash| hash != buffer.content_hash())
                || graph
                    .and_then(|graph| graph.file_by_path(buffer.rel_path()))
                    .is_some_and(|file| &file.content_hash != buffer.content_hash())
            {
                ExpandStatus::UpdatedSinceHandle
            } else {
                ExpandStatus::Ok
            };
            result.content = buffer.render_numbered_slice(LineSpan::new(start, end));
            return result;
        }
        if let Some(rest) = item_id.strip_prefix("diff:")
            && let Some((revision, path)) = rest.split_once(':')
            && !revision.is_empty()
            && !path.is_empty()
        {
            result.rel_path = Some(path.into());
            if !self.guard.is_history_path_allowed(Path::new(path)) {
                result.status = ExpandStatus::DeniedByPolicy;
                return result;
            }
            let Some(backend) = self.vcs_backend() else {
                result.content = "No VCS backend is available for this workspace.".into();
                return result;
            };
            return match backend.revision_file_diff(
                &self.canonical_root,
                &self.guard,
                revision,
                path,
                usize::MAX,
            ) {
                Ok(content) => {
                    result.file_hash = Some(ContentHash::of_bytes(content.as_bytes()));
                    result.status = ExpandStatus::Ok;
                    result.content = content;
                    result
                }
                Err(VcsError::DeniedByPolicy(_)) => {
                    result.status = ExpandStatus::DeniedByPolicy;
                    result
                }
                Err(error) => {
                    result.content = error.to_string();
                    result
                }
            };
        }
        result.content =
            "Use an advertised body:, callers:, slice:, diff:, or chunk: continuation ID.".into();
        result
    }

    /// Returns workspace index statistics and recent MCP/CLI observability telemetry.
    pub fn stats(&self) -> Result<StoreStats, EngineError> {
        Ok(self.store.stats()?)
    }
}

// Reserve metadata in semantic priority order before registering with the bounded
// store registry. Every advertised body ID must survive its descriptor limits.
fn bound_handle_evidence(pack: &mut ContextPack, evidence: &mut HandleEvidence) {
    let mut priority: Vec<_> = pack
        .edit_targets
        .iter()
        .filter_map(|target| target.symbol_key.clone())
        .collect();
    priority.extend(pack.reference_context.iter().filter_map(|reference| {
        reference
            .expand_id
            .strip_prefix("body:")
            .map(LogicalSymbolKey::from_raw)
    }));
    priority.extend(
        pack.test_candidates
            .iter()
            .filter_map(|test| test.test_symbol.clone()),
    );
    let mut retained = HashMap::new();
    let mut bytes = 0usize;
    for key in priority {
        if retained.contains_key(&key) {
            continue;
        }
        let Some(symbol) = evidence.symbols.get(&key) else {
            continue;
        };
        let size = key.as_str().len()
            + symbol.logical_key.as_str().len()
            + symbol.rel_path.len()
            + symbol.name.len()
            + symbol.scope.as_ref().map_or(0, String::len)
            + symbol.signature.len()
            + symbol.signature_hash.as_str().len()
            + symbol.body_hash.as_str().len();
        if retained.len() < HandleRegistry::MAX_SYMBOLS_PER_HANDLE
            && size <= HandleRegistry::MAX_SYMBOL_METADATA_BYTES.saturating_sub(bytes)
        {
            bytes += size;
            retained.insert(key, symbol.clone());
        }
    }
    let mut fallback = false;
    for target in &mut pack.edit_targets {
        if let Some(key) = &target.symbol_key
            && !retained.contains_key(key)
        {
            if target.omitted_expand_id.is_some() {
                let span = evidence
                    .symbols
                    .get(key)
                    .map_or(target.line_span, |symbol| symbol.line_span);
                target.omitted_expand_id = Some(format!("slice:{}:{span}", target.rel_path));
            }
            target.symbol_key = None;
            fallback = true;
        }
    }
    for reference in &mut pack.reference_context {
        if let Some(key) = reference
            .expand_id
            .strip_prefix("body:")
            .map(LogicalSymbolKey::from_raw)
            && !retained.contains_key(&key)
        {
            reference.expand_id = evidence.symbols.get(&key).map_or_else(
                || {
                    format!(
                        "slice:{}:{}-{}",
                        reference.rel_path,
                        reference.line,
                        reference.line.saturating_add(15)
                    )
                },
                |symbol| format!("slice:{}:{}", symbol.rel_path, symbol.line_span),
            );
            fallback = true;
        }
    }
    for test in &mut pack.test_candidates {
        if test
            .test_symbol
            .as_ref()
            .is_some_and(|key| !retained.contains_key(key))
        {
            test.test_symbol = None;
            fallback = true;
        }
    }
    evidence
        .ambiguous_files
        .retain(|key, _| retained.contains_key(key));
    evidence.symbols = retained;
    if fallback {
        pack.unknowns.push("Some symbol identities exceed handle metadata limits; use the advertised live slices or test file paths.".into());
    }
}

/// Identity resolution never falls back to a stale line span or a bare name.
fn resolve_live_symbol<'a>(
    old: &SymbolRecord,
    symbols: &'a [SymbolRecord],
) -> Option<&'a SymbolRecord> {
    let same_identity = |symbol: &&SymbolRecord| {
        symbol.rel_path == old.rel_path
            && symbol.kind == old.kind
            && symbol.scope == old.scope
            && symbol.name == old.name
    };
    let mut candidates = symbols.iter().filter(same_identity);
    let first = candidates.next()?;
    // Parser disambiguators are positional. Duplicate declarations can renumber after
    // deletion, so a matching logical key is insufficient in an ambiguous identity slot.
    if candidates.next().is_some() {
        return None;
    }
    Some(first)
}

fn resolve_handle_symbol<'a>(
    descriptor: &SymbolRecord,
    symbols: &'a [SymbolRecord],
    hash: &ContentHash,
    ambiguous_files: &HashMap<LogicalSymbolKey, ContentHash>,
) -> Option<&'a SymbolRecord> {
    if let Some(original_hash) = ambiguous_files.get(&descriptor.logical_key) {
        return (original_hash == hash)
            .then(|| {
                symbols
                    .iter()
                    .find(|symbol| symbol.logical_key == descriptor.logical_key)
            })
            .flatten();
    }
    resolve_live_symbol(descriptor, symbols)
}

fn bounded_descriptor(symbol: &SymbolRecord) -> SymbolRecord {
    let mut descriptor = symbol.clone();
    descriptor.docstring = None;
    descriptor.signature = descriptor.signature.chars().take(512).collect();
    descriptor
}

fn empty_expansion(item_id: &str) -> ExpandItemResult {
    ExpandItemResult {
        item_id: item_id.into(),
        status: ExpandStatus::InvalidItemId,
        rel_path: None,
        file_hash: None,
        content: String::new(),
        continuation_id: None,
    }
}

fn expansion_read_error(mut item: ExpandItemResult, error: &SingleBufferError) -> ExpandItemResult {
    item.status = match error {
        SingleBufferError::DeniedByPolicy { .. } => ExpandStatus::DeniedByPolicy,
        SingleBufferError::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            ExpandStatus::SymbolDeleted
        }
        _ => ExpandStatus::InvalidItemId,
    };
    if item.status == ExpandStatus::InvalidItemId {
        item.content = error.to_string();
    }
    item
}

#[derive(Debug)]
struct ChunkCursor<'a> {
    fingerprint: &'a str,
    offset: usize,
    base: &'a str,
}

fn parse_chunk_cursor(item: &str) -> Option<ChunkCursor<'_>> {
    let mut fields = item.strip_prefix("chunk:")?.splitn(3, ':');
    let fingerprint = fields.next()?;
    if fingerprint.len() != 64 || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let offset = fields.next()?.parse().ok()?;
    let base = fields.next()?;
    if base.starts_with("chunk:") {
        return None;
    }
    Some(ChunkCursor {
        fingerprint,
        offset,
        base,
    })
}

fn expansion_fingerprint(item: &ExpandItemResult) -> ContentHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(item.item_id.as_bytes());
    hasher.update(&[0]);
    hasher.update(
        item.file_hash
            .as_ref()
            .map_or("", ContentHash::as_str)
            .as_bytes(),
    );
    hasher.update(&[0]);
    hasher.update(item.content.as_bytes());
    ContentHash::from_hex(hasher.finalize().to_hex().to_string())
}

#[derive(Debug)]
struct ExpansionChunk {
    item: ExpandItemResult,
    base: String,
    fingerprint: ContentHash,
    offset: usize,
    resumable: bool,
}

impl ExpansionChunk {
    fn render(&self, bytes: usize) -> ExpandItemResult {
        let mut item = self.item.clone();
        if !self.resumable {
            return item;
        }
        let end = self.offset + bytes;
        item.content = self.item.content[self.offset..end].into();
        if end < self.item.content.len() {
            item.status = ExpandStatus::TruncatedWithContinuation;
            // Error messages can also be paged, using their deterministic rendered hash.
            item.continuation_id = self
                .resumable
                .then(|| format!("chunk:{}:{end}:{}", self.fingerprint.as_str(), self.base,));
        }
        item
    }
}

fn rendered_tokens(response: &ExpandResponse) -> u32 {
    ApproxTokens::estimate(&PackFormatter::render_expand_response(response)).0
}

fn build_historical_test_candidate(
    test_rel_path: &str,
    manifests: &[ManifestInfo],
) -> Option<TestCandidate> {
    ScopedResolver::format_test_command(
        test_rel_path,
        None,
        TestProvenance::HistoricalCochange,
        manifests,
    )
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

fn current_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::sync::{Barrier, mpsc};
    use std::time::Duration;

    type TestResult = Result<(), Box<dyn Error>>;

    fn query(symbol: &str) -> ContextRequest {
        ContextRequest {
            task: format!("Update {symbol}"),
            anchors: vec![ledger_core::AnchorInput::Typed(Anchor {
                kind: AnchorKind::Symbol,
                value: symbol.into(),
            })],
            mode: QueryMode::Auto,
            focus_paths: Vec::new(),
            focus_mode: FocusMode::Boost,
            max_approx_tokens: 12_000,
        }
    }

    fn until(mut predicate: impl FnMut() -> bool) -> TestResult {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !predicate() {
            if Instant::now() >= deadline {
                return Err("timed out waiting for test synchronization".into());
            }
            std::thread::yield_now();
        }
        Ok(())
    }

    #[test]
    fn cold_handles_cannot_alias_across_engine_lifetimes() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn target() {}\n")?;
        let old_handle = {
            let engine = WorkspaceEngine::open(directory.path())?;
            // Keep both queries cold without relying on a race with background parsing.
            engine.shutdown_indexing()?;
            engine.query_context(&query("target"))?.handle.to_string()
        };
        let replacement = WorkspaceEngine::open(directory.path())?;
        replacement.shutdown_indexing()?;
        let new_handle = replacement
            .query_context(&query("target"))?
            .handle
            .to_string();
        assert_ne!(old_handle, new_handle);
        assert!(matches!(
            replacement.expand(&ExpandRequest {
                handle: old_handle,
                items: vec!["slice:lib.rs:1-1".into()],
                delivery: ExpandDelivery::Complete,
                max_approx_tokens: 500,
            }),
            Err(EngineError::InvalidOrExpiredHandle(_))
        ));
        Ok(())
    }

    #[test]
    fn explicit_completed_scan_does_not_wait_for_later_refresh_work() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn original() {}\n")?;
        let engine = WorkspaceEngine::open(directory.path())?;
        let first_arrived = Arc::new(Barrier::new(2));
        let first_resume = Arc::new(Barrier::new(2));
        *engine.test_hooks.after_scan.lock() =
            Some((Arc::clone(&first_arrived), Arc::clone(&first_resume)));
        let requester = Arc::clone(&engine);
        let (completed_tx, completed_rx) = mpsc::channel();
        let explicit = std::thread::spawn(move || {
            let _ = completed_tx.send(requester.index_workspace());
        });
        first_arrived.wait();

        // A polling tick queues another scan after the explicit request's scan.
        // Hold that later scan open: the original caller must already be released.
        let later_arrived = Arc::new(Barrier::new(2));
        let later_resume = Arc::new(Barrier::new(2));
        *engine.test_hooks.after_scan.lock() =
            Some((Arc::clone(&later_arrived), Arc::clone(&later_resume)));
        engine.trigger_background_index();
        first_resume.wait();
        later_arrived.wait();
        let completed_before_refresh = completed_rx.recv_timeout(Duration::from_secs(2));
        // Always release workers before asserting, including on the buggy path.
        later_resume.wait();
        explicit.join().map_err(|_| "explicit requester panicked")?;
        engine.shutdown_indexing()?;
        assert!(
            completed_before_refresh.is_ok(),
            "completed explicit request waited for unrelated subsequent refresh work"
        );
        assert_eq!(completed_before_refresh??, GenerationId(1));
        Ok(())
    }

    #[test]
    fn explicit_mid_scan_request_gets_followup_and_shutdown_drains_it() -> TestResult {
        for preindexed in [false, true] {
            let directory = tempfile::tempdir()?;
            std::fs::write(directory.path().join("lib.rs"), "pub fn original() {}\n")?;
            let engine = WorkspaceEngine::open(directory.path())?;
            if preindexed {
                engine.index_workspace()?;
            }
            let arrived = Arc::new(Barrier::new(2));
            let resume = Arc::new(Barrier::new(2));
            *engine.test_hooks.after_scan.lock() =
                Some((Arc::clone(&arrived), Arc::clone(&resume)));
            engine.trigger_background_index();
            arrived.wait();
            std::fs::write(directory.path().join("lib.rs"), "pub fn replacement() {}\n")?;
            let requester = Arc::clone(&engine);
            let explicit = std::thread::spawn(move || requester.index_workspace());
            until(|| engine.coordinator.lock().waiters.len() == 1)?;
            let closer = Arc::clone(&engine);
            let (closed_tx, closed_rx) = mpsc::channel();
            let shutdown = std::thread::spawn(move || {
                let result = closer.shutdown_indexing();
                let _ = closed_tx.send(result);
            });
            until(|| engine.coordinator.lock().closed)?;
            assert!(
                closed_rx.try_recv().is_err(),
                "shutdown returned with a scheduled writer"
            );
            resume.wait();
            let generation = explicit.join().map_err(|_| "explicit worker panicked")??;
            closed_rx.recv_timeout(Duration::from_secs(5))??;
            shutdown.join().map_err(|_| "shutdown worker panicked")?;
            assert_eq!(generation, GenerationId(2));
            let snapshot = engine
                .published
                .load_full()
                .ok_or("no published snapshot")?;
            assert_eq!(snapshot.graph.symbols_by_name_ci("replacement").len(), 1);
            assert!(snapshot.graph.symbols_by_name_ci("original").is_empty());
            assert_eq!(
                engine.test_hooks.passes.load(Ordering::Relaxed),
                2 + usize::from(preindexed)
            );
            assert!(!engine.coordinator.lock().running);
            engine.shutdown_indexing()?;
            assert!(engine.index_workspace().is_err());
            engine.trigger_background_index();
            assert_eq!(
                engine.test_hooks.passes.load(Ordering::Relaxed),
                2 + usize::from(preindexed)
            );
        }
        Ok(())
    }

    #[test]
    fn concurrent_cold_requests_publish_once_and_reuse_parses() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn original() {}\n")?;
        let engine = WorkspaceEngine::open(directory.path())?;
        let start = Arc::new(Barrier::new(9));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let engine = Arc::clone(&engine);
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    engine.index_workspace()
                })
            })
            .collect();
        start.wait();
        for worker in workers {
            assert_eq!(
                worker.join().map_err(|_| "index thread panicked")??,
                GenerationId(1)
            );
        }
        assert_eq!(engine.test_hooks.parses.load(Ordering::Relaxed), 1);
        assert_eq!(engine.index_workspace()?, GenerationId(1));
        assert_eq!(engine.test_hooks.parses.load(Ordering::Relaxed), 1);
        engine.shutdown_indexing()?;
        Ok(())
    }

    #[test]
    fn cache_reconciles_rename_delete_ignore_and_nested_repository() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        let initialized = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(root)
            .status()?;
        assert!(initialized.success());
        std::fs::write(root.join("first.rs"), "pub fn original() {}\n")?;
        let engine = WorkspaceEngine::open(root)?;
        engine.index_workspace()?;
        let parses = engine.test_hooks.parses.load(Ordering::Relaxed);
        engine.index_workspace()?;
        assert_eq!(engine.test_hooks.parses.load(Ordering::Relaxed), parses);
        std::fs::rename(root.join("first.rs"), root.join("renamed.rs"))?;
        engine.index_workspace()?;
        assert_eq!(engine.test_hooks.parses.load(Ordering::Relaxed), parses + 1);
        let snapshot = engine.published.load_full().ok_or("no snapshot")?;
        assert!(snapshot.graph.file_by_path("first.rs").is_none());
        assert!(snapshot.graph.file_by_path("renamed.rs").is_some());
        drop(snapshot);
        std::fs::create_dir(root.join("nested"))?;
        std::fs::write(root.join("nested/.git"), "gitdir: elsewhere\n")?;
        std::fs::write(root.join("nested/hidden.rs"), "pub fn hidden() {}\n")?;
        std::fs::write(root.join("new.rs"), "pub fn newly_added() {}\n")?;
        std::fs::remove_file(root.join("renamed.rs"))?;
        engine.index_workspace()?;
        assert!(engine.index_cache.lock().contains_key("new.rs"));
        assert!(!engine.index_cache.lock().contains_key("renamed.rs"));
        assert!(!engine.index_cache.lock().contains_key("nested/hidden.rs"));
        std::fs::write(root.join(".gitignore"), "new.rs\n")?;
        engine.index_workspace()?;
        assert!(!engine.index_cache.lock().contains_key("new.rs"));
        engine.shutdown_indexing()?;
        Ok(())
    }

    #[test]
    fn panic_releases_waiters_and_allows_retry_and_shutdown() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn target() {}\n")?;
        let engine = WorkspaceEngine::open(directory.path())?;
        engine.test_hooks.panic_next.store(true, Ordering::Release);
        assert!(engine.index_workspace().is_err());
        assert!(!engine.coordinator.lock().running);
        assert_eq!(engine.index_workspace()?, GenerationId(1));
        engine.test_hooks.panic_next.store(true, Ordering::Release);
        engine.trigger_background_index();
        engine.shutdown_indexing()?;
        assert!(!engine.coordinator.lock().running);
        Ok(())
    }

    #[test]
    fn publication_and_pinned_queries_remain_coherent_under_writes() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join("lib.rs"),
            "pub fn target() -> u32 { 0 }\n",
        )?;
        let engine = WorkspaceEngine::open(directory.path())?;
        engine.index_workspace()?;
        let indexer = Arc::clone(&engine);
        let writer = std::thread::spawn(move || -> Result<(), String> {
            for version in 1..30 {
                let source = format!("pub fn target() -> u32 {{ {version} }}\n");
                std::fs::write(indexer.canonical_root.join("lib.rs"), source)
                    .map_err(|error| error.to_string())?;
                indexer
                    .index_workspace()
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        });
        for _ in 0..40 {
            let pack = engine.query_context(&query("target"))?;
            match pack.index_state {
                IndexState::Ready { generation, .. } => {
                    assert_eq!(generation, pack.handle.generation)
                }
                IndexState::ReadyWithLiveOverlay {
                    base_generation, ..
                } => assert_eq!(base_generation, pack.handle.generation),
                IndexState::Warming { .. } => {
                    return Err("warm query unexpectedly lost its snapshot".into());
                }
            }
            let _gate = engine.publication.read();
            let snapshot = engine.published.load_full().ok_or("no snapshot")?;
            assert_eq!(engine.store.current_generation()?, snapshot.graph.gen_id);
            engine
                .store
                .search_fts_pinned(snapshot.graph.gen_id, &["target".into()], 3)?;
            engine
                .store
                .search_history_pinned(snapshot.graph.gen_id, &["target".into()], 3)?;
        }
        writer.join().map_err(|_| "writer panicked")??;
        engine.shutdown_indexing()?;
        Ok(())
    }

    #[test]
    fn pinned_store_read_failures_are_explicit() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        std::fs::write(root.join("lib.rs"), "pub fn target() {}\n")?;
        let engine = WorkspaceEngine::open(root)?;
        engine.index_workspace()?;
        engine.store.checkpoint()?;
        let database = root.join(".ledger/ledger.db");
        let saved = root.join(".ledger/saved.db");
        std::fs::rename(&database, &saved)?;
        std::fs::create_dir(&database)?;
        assert!(
            matches!(
                engine.query_context(&query("target")),
                Err(EngineError::Store(_))
            ),
            "indexed evidence errors must not masquerade as empty matches"
        );
        assert_eq!(
            engine
                .published
                .load_full()
                .ok_or("missing snapshot")?
                .graph
                .gen_id,
            GenerationId(1)
        );
        std::fs::remove_dir(&database)?;
        std::fs::rename(&saved, &database)?;
        engine.shutdown_indexing()?;
        Ok(())
    }

    #[test]
    fn failed_vcs_pass_preserves_the_previous_publication_and_can_retry() -> TestResult {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(root)
                .status()?
                .success()
        );
        std::fs::write(root.join("lib.rs"), "pub fn target() {}\n")?;
        let engine = WorkspaceEngine::open(root)?;
        engine.index_workspace()?;
        let original_head = std::fs::read(root.join(".git/HEAD"))?;
        std::fs::write(root.join(".git/HEAD"), "broken ref value\n")?;
        std::fs::write(
            root.join("lib.rs"),
            "pub fn target() { let newer = true; }\n",
        )?;
        assert!(engine.index_workspace().is_err());
        assert!(!engine.coordinator.lock().running);
        assert_eq!(
            engine
                .published
                .load_full()
                .ok_or("missing snapshot")?
                .graph
                .gen_id,
            GenerationId(1)
        );
        std::fs::write(root.join(".git/HEAD"), original_head)?;
        assert_eq!(engine.index_workspace()?, GenerationId(2));
        engine.shutdown_indexing()?;
        Ok(())
    }

    #[test]
    fn failed_background_refresh_remains_visible_in_query_coverage() -> TestResult {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("lib.rs"), "pub fn target() {}\n")?;
        let engine = WorkspaceEngine::open(directory.path())?;
        engine.index_workspace()?;
        engine.test_hooks.panic_next.store(true, Ordering::Release);
        engine.trigger_background_index();
        engine.shutdown_indexing()?;
        let pack = engine.query_context(&query("target"))?;
        assert!(pack.route.contains("refresh_failed"));
        assert!(
            pack.unknowns
                .iter()
                .any(|note| note.contains("Indexed refresh failed"))
        );
        Ok(())
    }

    #[test]
    fn cold_long_body_expands_before_any_graph_exists() -> TestResult {
        let directory = tempfile::tempdir()?;
        let source = format!(
            "pub fn cold_target() {{\n{}\n}}\n",
            "    // cold_target continues\n".repeat(160)
        );
        std::fs::write(directory.path().join("lib.rs"), &source)?;
        let engine = WorkspaceEngine::open(directory.path())?;
        // The direct cold path is intentionally isolated from production ownership and
        // scheduling; no graph has ever been published in this engine.
        let request = query("cold_target");
        let anchors: Vec<_> = request
            .anchors
            .iter()
            .cloned()
            .map(|input| input.into_anchor())
            .collect();
        let (pack, evidence) =
            engine.query_cold_start_live(&request, &anchors, ApproxTokens(12_000))?;
        let (rendered, delivered) = PackFormatter::render_context_pack_with_delivery(&pack);
        assert!(rendered.contains("body:"));
        assert!(delivered.is_empty());
        engine.handles.insert_with_symbols(
            pack.handle.to_string(),
            pack.handle.generation,
            delivered,
            evidence.symbols,
        );
        let id = pack
            .edit_targets
            .first()
            .and_then(|target| target.omitted_expand_id.clone())
            .ok_or("cold body continuation missing")?;
        let response = engine.expand(&ExpandRequest {
            handle: pack.handle.to_string(),
            items: vec![id],
            delivery: ExpandDelivery::Delta,
            max_approx_tokens: 12_000,
        })?;
        assert_eq!(response.results[0].status, ExpandStatus::Ok);
        assert!(response.results[0].content.contains(" 163| }"));
        assert!(engine.published.load_full().is_none());
        engine.shutdown_indexing()?;
        Ok(())
    }
}
