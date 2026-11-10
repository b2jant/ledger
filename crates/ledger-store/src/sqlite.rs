//! Authoritative SQLite WAL store with separated code, documentation, and history FTS5 channels,
//! atomic generation publishing, cross-store generation read verification, and telemetry logging.

use std::path::Path;

use ledger_core::tokens::split_identifier_tokens;
use ledger_core::{
    GenerationId, HistoryHit, LogicalSymbolKey, TelemetryRecord, VcsFileTouch, VcsRevision,
    WorkspaceId,
};
use ledger_parser::ResolvedWorkspaceGraph;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors returned by [`SqliteStore`].
#[derive(Debug, Error)]
pub enum StoreError {
    /// SQLite driver error.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// JSON serialization error for structured column payloads.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    /// Pinned `GraphGeneration` did not match SQLite `meta.current_gen_id`.
    #[error("snapshot generation mismatch (sqlite={db_gen}, graph={graph_gen})")]
    GenerationMismatch {
        /// Generation currently committed in SQLite.
        db_gen: GenerationId,
        /// Generation pinned in `ArcSwap<GraphGeneration>`.
        graph_gen: GenerationId,
    },
    /// Database belongs to a different canonical workspace root.
    #[error("workspace ID mismatch in ledger.db (expected {expected}, found {found})")]
    WorkspaceMismatch {
        /// Current canonical workspace ID.
        expected: String,
        /// Persisted workspace ID in `meta`.
        found: String,
    },
    /// A reader or another connection prevented a complete WAL checkpoint.
    #[error(
        "WAL checkpoint busy (log frames={log_frames}, checkpointed={checkpointed_frames}); close active readers and retry"
    )]
    CheckpointBusy {
        /// Frames SQLite reported in the WAL.
        log_frames: i64,
        /// Frames SQLite successfully checkpointed.
        checkpointed_frames: i64,
    },
}

/// Ranked FTS5 match from a separated lexical channel.
#[derive(Debug, Clone)]
pub struct FtsHit {
    /// Matched symbol key.
    pub symbol_key: LogicalSymbolKey,
    /// Containing file path.
    pub rel_path: String,
    /// Channel (`"lex_code"` or `"lex_docs"`).
    pub channel: &'static str,
    /// Positive normalized relevance score (`> 0.0`).
    pub score: f32,
}

/// High-level storage & observability counters returned by `ledger status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreStats {
    /// Schema version number.
    pub schema_version: u32,
    /// Persisted workspace identifier.
    pub workspace_id: String,
    /// Unique database incarnation identifier.
    pub db_incarnation: String,
    /// Latest committed generation ID.
    pub current_generation: u64,
    /// VCS head associated with the current generation, when available.
    #[serde(default)]
    pub vcs_head: Option<String>,
    /// Non-noise revisions indexed in the current generation.
    #[serde(default)]
    pub indexed_revisions: usize,
    /// Total indexed files in `current_generation`.
    pub file_count: usize,
    /// Total indexed AST symbols in `current_generation`.
    pub symbol_count: usize,
    /// Total resolved typed edges in `current_generation`.
    pub edge_count: usize,
    /// Total recorded MCP/CLI telemetry queries.
    pub total_queries_served: usize,
    /// Recent telemetry events (newest first, up to 10).
    pub recent_telemetry: Vec<TelemetryRecord>,
}

/// Thread-safe authoritative SQLite WAL store.
///
/// Uses a dedicated writer mutex and independent short-lived reader connections so
/// concurrent queries never contend on long transactions or block WAL checkpoints.
#[derive(Debug)]
pub struct SqliteStore {
    db_path: Option<std::path::PathBuf>,
    writer: Mutex<Connection>,
    workspace_id: WorkspaceId,
    db_incarnation: String,
}

impl SqliteStore {
    /// Current schema version; mismatched versions trigger clean rebuild of disposable index tables.
    pub const SCHEMA_VERSION: u32 = 2;

    /// Opens (or initializes) `.ledger/ledger.db` in WAL mode with an 8 MB bounded page cache.
    pub fn open(db_path: &Path, workspace_id: WorkspaceId) -> Result<Self, StoreError> {
        let mut conn = Connection::open(db_path)?;
        Self::configure_connection(&conn)?;
        let db_incarnation = Self::init_schema(&mut conn, &workspace_id)?;
        Ok(Self {
            db_path: Some(db_path.to_path_buf()),
            writer: Mutex::new(conn),
            workspace_id,
            db_incarnation,
        })
    }

    /// Opens an in-memory SQLite store for unit tests.
    pub fn open_in_memory(workspace_id: WorkspaceId) -> Result<Self, StoreError> {
        let mut conn = Connection::open_in_memory()?;
        Self::configure_connection(&conn)?;
        let db_incarnation = Self::init_schema(&mut conn, &workspace_id)?;
        Ok(Self {
            db_path: None,
            writer: Mutex::new(conn),
            workspace_id,
            db_incarnation,
        })
    }

    /// Serializes with writers and completely checkpoints/truncates the WAL.
    /// SQLite returns busy as a result row, not necessarily an error; propagate it.
    /// In-memory databases have no WAL and require no checkpoint.
    pub fn checkpoint(&self) -> Result<(), StoreError> {
        let writer = self.writer.lock();
        if self.db_path.is_none() {
            return Ok(());
        }
        let (busy, log_frames, checkpointed_frames): (i64, i64, i64) =
            writer.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
        if busy != 0 || (log_frames >= 0 && checkpointed_frames < log_frames) {
            return Err(StoreError::CheckpointBusy {
                log_frames,
                checkpointed_frames,
            });
        }
        Ok(())
    }

    fn configure_connection(conn: &Connection) -> Result<(), rusqlite::Error> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "cache_size", -8000i64)?; // Bounded 8 MB page cache
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        Ok(())
    }

    fn init_schema(
        conn: &mut Connection,
        workspace_id: &WorkspaceId,
    ) -> Result<String, StoreError> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS meta (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                schema_version INTEGER NOT NULL,
                workspace_id TEXT NOT NULL,
                db_incarnation TEXT NOT NULL,
                current_gen_id INTEGER NOT NULL,
                vcs_head TEXT
            );",
        )?;
        let existing: Option<(u32, String, String)> = tx
            .query_row(
                "SELECT schema_version, workspace_id, db_incarnation FROM meta WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((version, persisted_ws, _)) = &existing {
            if persisted_ws != workspace_id.as_str() {
                return Err(StoreError::WorkspaceMismatch {
                    expected: workspace_id.to_string(),
                    found: persisted_ws.clone(),
                });
            }
            if *version != Self::SCHEMA_VERSION {
                // Indexes are disposable; compatible observability data survives a rebuild.
                tx.execute_batch(
                    "DROP TABLE IF EXISTS fts_lex_code;
                     DROP TABLE IF EXISTS fts_lex_docs;
                     DROP TABLE IF EXISTS fts_lex_history;
                     DROP TABLE IF EXISTS vcs_revisions;
                     DROP TABLE IF EXISTS test_candidates;
                     DROP TABLE IF EXISTS edges;
                     DROP TABLE IF EXISTS symbols;
                     DROP TABLE IF EXISTS files;
                     DROP TABLE IF EXISTS generations;",
                )?;
            }
        }
        let has_vcs_head = {
            let mut columns = tx.prepare("PRAGMA table_info(meta)")?;
            let names = columns.query_map([], |row| row.get::<_, String>(1))?;
            let mut found = false;
            for name in names {
                found |= name? == "vcs_head";
            }
            found
        };
        if !has_vcs_head {
            tx.execute_batch("ALTER TABLE meta ADD COLUMN vcs_head TEXT;")?;
        }
        tx.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS generations (
                gen_id INTEGER PRIMARY KEY,
                indexed_files INTEGER NOT NULL,
                indexed_symbols INTEGER NOT NULL,
                indexed_edges INTEGER NOT NULL,
                published_at_ms INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS files (
                rel_path TEXT PRIMARY KEY,
                language TEXT NOT NULL,
                layer TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                interface_hash TEXT NOT NULL,
                coverage TEXT NOT NULL,
                byte_len INTEGER NOT NULL,
                line_count INTEGER NOT NULL,
                concepts_json TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS symbols (
                logical_key TEXT PRIMARY KEY,
                rel_path TEXT NOT NULL,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                scope TEXT,
                signature TEXT NOT NULL,
                docstring TEXT,
                exported INTEGER NOT NULL,
                start_byte INTEGER NOT NULL,
                end_byte INTEGER NOT NULL,
                start_line INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                signature_hash TEXT NOT NULL,
                body_hash TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(rel_path);
            CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);

            CREATE TABLE IF NOT EXISTS edges (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                src_file TEXT NOT NULL,
                src_symbol TEXT,
                dst_file TEXT NOT NULL,
                dst_symbol TEXT,
                relation TEXT NOT NULL,
                confidence_json TEXT NOT NULL,
                evidence_line INTEGER NOT NULL,
                evidence_snippet TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_edges_src_file ON edges(src_file);
            CREATE INDEX IF NOT EXISTS idx_edges_dst_file ON edges(dst_file);

            CREATE TABLE IF NOT EXISTS test_candidates (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                target_file TEXT NOT NULL,
                command TEXT NOT NULL,
                cwd TEXT NOT NULL,
                manifest_path TEXT NOT NULL,
                test_file TEXT NOT NULL,
                test_symbol TEXT,
                provenance TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_tests_target ON test_candidates(target_file);

            CREATE VIRTUAL TABLE IF NOT EXISTS fts_lex_code USING fts5(
                logical_key UNINDEXED,
                rel_path UNINDEXED,
                symbol_name,
                split_tokens,
                signature,
                tokenize = 'unicode61'
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS fts_lex_docs USING fts5(
                logical_key UNINDEXED,
                rel_path UNINDEXED,
                docstring,
                concepts,
                tokenize = 'unicode61'
            );

            CREATE TABLE IF NOT EXISTS vcs_revisions (
                gen_id INTEGER NOT NULL,
                rev_id TEXT NOT NULL,
                short_id TEXT NOT NULL,
                timestamp_secs INTEGER NOT NULL,
                author TEXT NOT NULL,
                summary TEXT NOT NULL,
                touched_files_json TEXT NOT NULL,
                is_noise INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (gen_id, rev_id)
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS fts_lex_history USING fts5(
                rev_id UNINDEXED,
                short_id UNINDEXED,
                summary_tokens,
                path_tokens,
                tokenize = 'unicode61'
            );

            CREATE TABLE IF NOT EXISTS query_telemetry (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp_ms INTEGER NOT NULL,
                tool_name TEXT NOT NULL,
                mode TEXT NOT NULL,
                query_summary TEXT NOT NULL,
                route TEXT NOT NULL,
                generation INTEGER NOT NULL,
                edit_targets_json TEXT NOT NULL,
                reference_files_json TEXT NOT NULL,
                latency_us INTEGER NOT NULL,
                approx_tokens INTEGER NOT NULL
            );
            ",
        )?;

        let incarnation = match existing {
            Some((ver, _, incarnation)) => {
                if ver != Self::SCHEMA_VERSION {
                    tx.execute(
                        "UPDATE meta SET schema_version = ?1, current_gen_id = 0, vcs_head = NULL WHERE id = 1",
                        params![Self::SCHEMA_VERSION],
                    )?;
                }
                incarnation
            }
            None => {
                let now_nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos());
                let inc_hash =
                    blake3::hash(format!("{}:{now_nanos}", workspace_id.as_str()).as_bytes());
                let incarnation = inc_hash.to_hex()[..12].to_string();
                tx.execute(
                    "INSERT INTO meta (id, schema_version, workspace_id, db_incarnation, current_gen_id)
                     VALUES (1, ?1, ?2, ?3, 0)",
                    params![Self::SCHEMA_VERSION, workspace_id.as_str(), incarnation],
                )?;
                incarnation
            }
        };
        tx.commit()?;
        Ok(incarnation)
    }

    /// Database incarnation string for daemon handshake validation.
    #[must_use]
    pub fn db_incarnation(&self) -> &str {
        &self.db_incarnation
    }

    /// Workspace identifier bound to this store.
    #[must_use]
    pub const fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    /// Returns the current committed generation ID.
    pub fn current_generation(&self) -> Result<GenerationId, StoreError> {
        let conn = self.writer.lock();
        read_generation(&conn)
    }

    /// Atomically publishes the next generation without VCS history.
    ///
    /// Retains the original auto-incrementing API used by the daemon. Both publishing
    /// APIs share the same transaction implementation, including history replacement.
    pub fn publish_generation(
        &self,
        resolved: &ResolvedWorkspaceGraph,
    ) -> Result<GenerationId, StoreError> {
        self.publish_generation_inner(None, resolved, None, &[])
    }

    /// Atomically publishes code, documentation, VCS history, and head for `gen_id`.
    /// Previous generations' index rows are pruned within the same write transaction.
    pub fn publish_generation_with_vcs(
        &self,
        gen_id: GenerationId,
        resolved: &ResolvedWorkspaceGraph,
        vcs_head: Option<&str>,
        revisions: &[VcsRevision],
    ) -> Result<(), StoreError> {
        self.publish_generation_inner(Some(gen_id), resolved, vcs_head, revisions)?;
        Ok(())
    }

    fn publish_generation_inner(
        &self,
        gen_id: Option<GenerationId>,
        resolved: &ResolvedWorkspaceGraph,
        vcs_head: Option<&str>,
        revisions: &[VcsRevision],
    ) -> Result<GenerationId, StoreError> {
        let mut conn = self.writer.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let next_gen = match gen_id {
            Some(generation) => generation,
            None => read_generation(&tx)?.next(),
        };
        let next_gen_i64 = generation_sql_value(next_gen)?;

        tx.execute_batch(
            "
            DELETE FROM files;
            DELETE FROM symbols;
            DELETE FROM edges;
            DELETE FROM test_candidates;
            DELETE FROM fts_lex_code;
            DELETE FROM fts_lex_docs;
            DELETE FROM generations;
            ",
        )?;

        let mut concepts_by_file = std::collections::HashMap::with_capacity(resolved.files.len());
        {
            let mut file_stmt = tx.prepare_cached(
                "INSERT INTO files (rel_path, language, layer, content_hash, interface_hash, coverage, byte_len, line_count, concepts_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for f in &resolved.files {
                let concepts_json = serde_json::to_string(&f.concepts)?;
                concepts_by_file.insert(f.rel_path.as_str(), f.concepts.join(" "));
                file_stmt.execute(params![
                    f.rel_path,
                    f.language.as_str(),
                    f.layer.as_str(),
                    f.content_hash.as_str(),
                    f.interface_hash.as_str(),
                    f.coverage.as_str(),
                    i64::try_from(f.byte_len).unwrap_or(i64::MAX),
                    f.line_count,
                    concepts_json,
                ])?;
            }
        }

        {
            let mut sym_stmt = tx.prepare_cached(
                "INSERT INTO symbols (logical_key, rel_path, kind, name, scope, signature, docstring, exported, start_byte, end_byte, start_line, end_line, signature_hash, body_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            )?;
            let mut code_fts_stmt = tx.prepare_cached(
                "INSERT INTO fts_lex_code (logical_key, rel_path, symbol_name, split_tokens, signature)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            let mut docs_fts_stmt = tx.prepare_cached(
                "INSERT INTO fts_lex_docs (logical_key, rel_path, docstring, concepts)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;

            for s in &resolved.symbols {
                sym_stmt.execute(params![
                    s.logical_key.as_str(),
                    s.rel_path,
                    s.kind.as_str(),
                    s.name,
                    s.scope,
                    s.signature,
                    s.docstring,
                    i32::from(s.exported),
                    i64::try_from(s.byte_span.start).unwrap_or(0),
                    i64::try_from(s.byte_span.end).unwrap_or(0),
                    s.line_span.start_line,
                    s.line_span.end_line,
                    s.signature_hash.as_str(),
                    s.body_hash.as_str(),
                ])?;

                let mut split_toks = split_identifier_tokens(&s.name);
                if let Some(sc) = &s.scope {
                    split_toks.extend(split_identifier_tokens(sc));
                }
                code_fts_stmt.execute(params![
                    s.logical_key.as_str(),
                    s.rel_path,
                    s.name,
                    split_toks.join(" "),
                    s.signature,
                ])?;

                let file_concepts = concepts_by_file
                    .get(s.rel_path.as_str())
                    .map_or("", String::as_str);
                let doc = s.docstring.as_deref().unwrap_or("");
                if !doc.is_empty() || !file_concepts.is_empty() {
                    docs_fts_stmt.execute(params![
                        s.logical_key.as_str(),
                        s.rel_path,
                        doc,
                        file_concepts,
                    ])?;
                }
            }
        }

        {
            let mut edge_stmt = tx.prepare_cached(
                "INSERT INTO edges (src_file, src_symbol, dst_file, dst_symbol, relation, confidence_json, evidence_line, evidence_snippet)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for e in &resolved.edges {
                let conf_json = serde_json::to_string(&e.confidence)?;
                edge_stmt.execute(params![
                    e.src_file,
                    e.src_symbol.as_ref().map(|k| k.as_str()),
                    e.dst_file,
                    e.dst_symbol.as_ref().map(|k| k.as_str()),
                    e.relation.to_string(),
                    conf_json,
                    e.evidence_line,
                    e.evidence_snippet,
                ])?;
            }
        }

        {
            let mut test_stmt = tx.prepare_cached(
                "INSERT INTO test_candidates (target_file, command, cwd, manifest_path, test_file, test_symbol, provenance)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for (target_file, list) in &resolved.tests_by_target_file {
                for tc in list {
                    test_stmt.execute(params![
                        target_file,
                        tc.command,
                        tc.cwd,
                        tc.manifest_path,
                        tc.test_file,
                        tc.test_symbol.as_ref().map(|k| k.as_str()),
                        tc.provenance.to_string(),
                    ])?;
                }
            }
        }

        Self::replace_vcs_history(&tx, next_gen, vcs_head, revisions)?;
        let now_ms = current_epoch_ms();
        tx.execute(
            "INSERT INTO generations (gen_id, indexed_files, indexed_symbols, indexed_edges, published_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                next_gen_i64,
                i64::try_from(resolved.files.len()).unwrap_or(0),
                i64::try_from(resolved.symbols.len()).unwrap_or(0),
                i64::try_from(resolved.edges.len()).unwrap_or(0),
                now_ms,
            ],
        )?;
        tx.execute(
            "UPDATE meta SET current_gen_id = ?1 WHERE id = 1",
            params![next_gen_i64],
        )?;

        tx.commit()?;
        Ok(next_gen)
    }

    /// Replaces history and the VCS head only if `gen_id` is still current.
    /// Returns the number of non-noise revisions inserted into the history FTS index.
    pub fn publish_vcs_history(
        &self,
        gen_id: GenerationId,
        vcs_head: Option<&str>,
        revisions: &[VcsRevision],
    ) -> Result<usize, StoreError> {
        let mut conn = self.writer.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_generation(&tx, gen_id)?;
        let indexed = Self::replace_vcs_history(&tx, gen_id, vcs_head, revisions)?;
        tx.commit()?;
        Ok(indexed)
    }

    fn replace_vcs_history(
        tx: &Transaction<'_>,
        gen_id: GenerationId,
        vcs_head: Option<&str>,
        revisions: &[VcsRevision],
    ) -> Result<usize, StoreError> {
        let gen_id_sql = generation_sql_value(gen_id)?;
        tx.execute_batch("DELETE FROM fts_lex_history; DELETE FROM vcs_revisions;")?;
        let mut revision_stmt = tx.prepare_cached(
            "INSERT INTO vcs_revisions
             (gen_id, rev_id, short_id, timestamp_secs, author, summary, touched_files_json, is_noise)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        let mut fts_stmt = tx.prepare_cached(
            "INSERT INTO fts_lex_history (rev_id, short_id, summary_tokens, path_tokens)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        let mut indexed = 0;
        for revision in revisions {
            let is_noise = is_noise_commit(revision);
            revision_stmt.execute(params![
                gen_id_sql,
                revision.rev_id,
                revision.short_id,
                revision.timestamp_secs,
                revision.author,
                revision.summary,
                serde_json::to_string(&revision.touched_files)?,
                i32::from(is_noise),
            ])?;
            if !is_noise {
                let summary_tokens = split_identifier_tokens(&revision.summary).join(" ");
                let path_tokens = revision
                    .touched_files
                    .iter()
                    .flat_map(|touch| {
                        std::iter::once(touch.rel_path.as_str())
                            .chain(touch.old_rel_path.as_deref())
                    })
                    .flat_map(split_identifier_tokens)
                    .collect::<Vec<_>>()
                    .join(" ");
                fts_stmt.execute(params![
                    revision.rev_id,
                    revision.short_id,
                    summary_tokens,
                    path_tokens,
                ])?;
                indexed += 1;
            }
        }
        tx.execute(
            "UPDATE meta SET vcs_head = ?1 WHERE id = 1",
            params![vcs_head],
        )?;
        Ok(indexed)
    }

    /// Searches provenance-only commit history inside a generation-verified read snapshot.
    /// BM25 scores are normalized into `(0.0, 0.35]`.
    pub fn search_history_pinned(
        &self,
        expected_gen: GenerationId,
        terms: &[String],
        limit: usize,
    ) -> Result<Vec<HistoryHit>, StoreError> {
        let query = history_fts_query(terms);
        self.with_pinned_read(expected_gen, |tx| {
            // Verify the generation even for empty queries and zero limits.
            if query.is_empty() || limit == 0 {
                return Ok(Vec::new());
            }
            let mut stmt = tx.prepare_cached(
                "SELECT revisions.rev_id, revisions.short_id, revisions.timestamp_secs,
                        revisions.summary, revisions.touched_files_json,
                        bm25(fts_lex_history, 0.0, 0.0, 2.0, 1.0) AS rank
                 FROM fts_lex_history
                 JOIN vcs_revisions AS revisions ON revisions.rev_id = fts_lex_history.rev_id
                 WHERE fts_lex_history MATCH ?1 AND revisions.gen_id = ?2 AND revisions.is_noise = 0
                 ORDER BY rank ASC, revisions.timestamp_secs DESC, revisions.rev_id ASC
                 LIMIT ?3",
            )?;
            let mut rows = stmt.query(params![
                query,
                generation_sql_value(expected_gen)?,
                i64::try_from(limit).unwrap_or(i64::MAX),
            ])?;
            let mut hits = Vec::new();
            while let Some(row) = rows.next()? {
                let touches_json: String = row.get(4)?;
                let touches: Vec<VcsFileTouch> = serde_json::from_str(&touches_json)?;
                let rank: f64 = row.get(5)?;
                let raw = (-rank).max(f64::from(f32::MIN_POSITIVE));
                let score = (0.35 * raw / (1.0 + raw)) as f32;
                hits.push(HistoryHit {
                    rev_id: row.get(0)?,
                    short_id: row.get(1)?,
                    timestamp_secs: row.get(2)?,
                    summary: row.get(3)?,
                    touched_files: touches.into_iter().map(|touch| touch.rel_path).collect(),
                    score,
                });
            }
            Ok(hits)
        })
    }

    /// Loads all persisted revisions, including noise, from the pinned generation.
    pub fn load_vcs_revisions_pinned(
        &self,
        expected_gen: GenerationId,
    ) -> Result<Vec<VcsRevision>, StoreError> {
        self.with_pinned_read(expected_gen, |tx| {
            let mut stmt = tx.prepare_cached(
                "SELECT rev_id, short_id, timestamp_secs, author, summary, touched_files_json
                 FROM vcs_revisions WHERE gen_id = ?1 ORDER BY timestamp_secs DESC, rev_id ASC",
            )?;
            let mut rows = stmt.query(params![generation_sql_value(expected_gen)?])?;
            let mut revisions = Vec::new();
            while let Some(row) = rows.next()? {
                let touches_json: String = row.get(5)?;
                revisions.push(VcsRevision {
                    rev_id: row.get(0)?,
                    short_id: row.get(1)?,
                    timestamp_secs: row.get(2)?,
                    author: row.get(3)?,
                    summary: row.get(4)?,
                    touched_files: serde_json::from_str(&touches_json)?,
                });
            }
            Ok(revisions)
        })
    }

    fn with_pinned_read<T>(
        &self,
        expected_gen: GenerationId,
        read: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let run = |conn: &mut Connection| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
            verify_generation(&tx, expected_gen)?;
            let result = read(&tx)?;
            tx.commit()?;
            Ok(result)
        };
        if let Some(path) = &self.db_path {
            let mut reader = Connection::open(path)?;
            Self::configure_connection(&reader)?;
            run(&mut reader)
        } else {
            run(&mut self.writer.lock())
        }
    }

    /// Compatibility name for the generation-pinned code and documentation search.
    pub fn search_lexical_pinned(
        &self,
        expected_gen: GenerationId,
        terms: &[String],
        limit_per_channel: usize,
    ) -> Result<Vec<FtsHit>, StoreError> {
        self.search_fts_pinned(expected_gen, terms, limit_per_channel)
    }

    /// Executes BM25 retrieval across the separated `fts_lex_code` (weight `1.0`) and
    /// `fts_lex_docs` (weight `0.6`) channels inside a **short-lived read snapshot**
    /// that verifies `meta.current_gen_id == expected_gen`.
    ///
    /// If a writer committed `gen_id + 1` concurrently, returns
    /// [`StoreError::GenerationMismatch`] so the caller reloads `ArcSwap` once and retries.
    pub fn search_fts_pinned(
        &self,
        expected_gen: GenerationId,
        terms: &[String],
        limit_per_channel: usize,
    ) -> Result<Vec<FtsHit>, StoreError> {
        let sanitized: Vec<String> = terms
            .iter()
            .map(|t| {
                t.chars()
                    .filter(|c| c.is_alphanumeric() || *c == '_')
                    .collect::<String>()
            })
            .filter(|t| t.len() >= 2)
            .take(12)
            .map(|t| format!("\"{t}\"*"))
            .collect();

        if sanitized.is_empty() {
            return Ok(Vec::new());
        }
        let fts_query = sanitized.join(" OR ");

        // Use an independent reader connection on disk DBs for lock-free WAL concurrency;
        // fall back to the shared mutex connection on `:memory:` test DBs.
        if let Some(path) = &self.db_path {
            let mut reader = Connection::open(path)?;
            Self::configure_connection(&reader)?;
            Self::run_pinned_fts_tx(&mut reader, expected_gen, &fts_query, limit_per_channel)
        } else {
            let mut guard = self.writer.lock();
            Self::run_pinned_fts_tx(&mut guard, expected_gen, &fts_query, limit_per_channel)
        }
    }

    fn run_pinned_fts_tx(
        conn: &mut Connection,
        expected_gen: GenerationId,
        fts_query: &str,
        limit_per_channel: usize,
    ) -> Result<Vec<FtsHit>, StoreError> {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let db_gen_raw: i64 =
            tx.query_row("SELECT current_gen_id FROM meta WHERE id = 1", [], |r| {
                r.get(0)
            })?;
        let db_gen = GenerationId(u64::try_from(db_gen_raw).unwrap_or(0));
        if db_gen != expected_gen {
            return Err(StoreError::GenerationMismatch {
                db_gen,
                graph_gen: expected_gen,
            });
        }

        let mut hits = Vec::new();
        let lim = i64::try_from(limit_per_channel).unwrap_or(32);

        {
            let mut stmt = tx.prepare_cached(
                "SELECT logical_key, rel_path, bm25(fts_lex_code, 0.0, 0.0, 4.0, 2.5, 1.2) AS rank
                 FROM fts_lex_code
                 WHERE fts_lex_code MATCH ?1
                 ORDER BY rank ASC
                 LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![fts_query, lim], |row| {
                let key: String = row.get(0)?;
                let path: String = row.get(1)?;
                let bm25_neg: f64 = row.get(2)?;
                Ok((key, path, bm25_neg))
            })?;
            for r in rows {
                let (key, path, bm25_neg) = r?;
                let score = ((-bm25_neg).max(0.05) as f32) * 1.0;
                hits.push(FtsHit {
                    symbol_key: LogicalSymbolKey::from_raw(key),
                    rel_path: path,
                    channel: "lex_code",
                    score,
                });
            }
        }

        {
            let mut stmt = tx.prepare_cached(
                "SELECT logical_key, rel_path, bm25(fts_lex_docs, 0.0, 0.0, 2.0, 1.5) AS rank
                 FROM fts_lex_docs
                 WHERE fts_lex_docs MATCH ?1
                 ORDER BY rank ASC
                 LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![fts_query, lim], |row| {
                let key: String = row.get(0)?;
                let path: String = row.get(1)?;
                let bm25_neg: f64 = row.get(2)?;
                Ok((key, path, bm25_neg))
            })?;
            for r in rows {
                let (key, path, bm25_neg) = r?;
                let score = ((-bm25_neg).max(0.05) as f32) * 0.6;
                hits.push(FtsHit {
                    symbol_key: LogicalSymbolKey::from_raw(key),
                    rel_path: path,
                    channel: "lex_docs",
                    score,
                });
            }
        }

        // Explicitly finish read snapshot immediately so WAL checkpoints are never held open.
        drop(tx);
        Ok(hits)
    }

    /// Appends an observability [`TelemetryRecord`] to `query_telemetry`.
    pub fn record_telemetry(&self, rec: &TelemetryRecord) -> Result<(), StoreError> {
        let conn = self.writer.lock();
        let edit_json = serde_json::to_string(&rec.edit_target_files)?;
        let ref_json = serde_json::to_string(&rec.reference_files)?;
        conn.execute(
            "INSERT INTO query_telemetry
             (timestamp_ms, tool_name, mode, query_summary, route, generation, edit_targets_json, reference_files_json, latency_us, approx_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                i64::try_from(rec.timestamp_ms).unwrap_or(0),
                rec.tool_name,
                rec.mode,
                rec.query_summary,
                rec.route,
                i64::try_from(rec.generation).unwrap_or(0),
                edit_json,
                ref_json,
                i64::try_from(rec.latency_us).unwrap_or(0),
                rec.approx_tokens,
            ],
        )?;
        Ok(())
    }

    /// Reads current workspace store counters and the 10 most recent telemetry records.
    pub fn stats(&self) -> Result<StoreStats, StoreError> {
        let mut conn = self.writer.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let (schema_version, workspace_id, db_incarnation, current_gen_raw, vcs_head): (
            u32,
            String,
            String,
            i64,
            Option<String>,
        ) = tx.query_row(
            "SELECT schema_version, workspace_id, db_incarnation, current_gen_id, vcs_head FROM meta WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        let current_generation = u64::try_from(current_gen_raw).unwrap_or(0);

        let count_of = |table: &str| -> Result<usize, rusqlite::Error> {
            let n: i64 =
                tx.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
            Ok(usize::try_from(n).unwrap_or(0))
        };

        let file_count = count_of("files")?;
        let symbol_count = count_of("symbols")?;
        let edge_count = count_of("edges")?;
        let total_queries_served = count_of("query_telemetry")?;
        let indexed_revisions_raw: i64 = tx.query_row(
            "SELECT COUNT(*) FROM vcs_revisions WHERE gen_id = ?1 AND is_noise = 0",
            params![current_gen_raw],
            |row| row.get(0),
        )?;
        let indexed_revisions = usize::try_from(indexed_revisions_raw).unwrap_or(0);

        let mut stmt = tx.prepare(
            "SELECT timestamp_ms, tool_name, mode, query_summary, route, generation, edit_targets_json, reference_files_json, latency_us, approx_tokens
             FROM query_telemetry
             ORDER BY id DESC
             LIMIT 10",
        )?;
        let rows = stmt.query_map([], |row| {
            let ts: i64 = row.get(0)?;
            let gen_raw: i64 = row.get(5)?;
            let edit_s: String = row.get(6)?;
            let ref_s: String = row.get(7)?;
            let lat: i64 = row.get(8)?;
            Ok(TelemetryRecord {
                timestamp_ms: u64::try_from(ts).unwrap_or(0),
                tool_name: row.get(1)?,
                mode: row.get(2)?,
                query_summary: row.get(3)?,
                route: row.get(4)?,
                generation: u64::try_from(gen_raw).unwrap_or(0),
                edit_target_files: serde_json::from_str(&edit_s).unwrap_or_default(),
                reference_files: serde_json::from_str(&ref_s).unwrap_or_default(),
                latency_us: u64::try_from(lat).unwrap_or(0),
                approx_tokens: row.get(9)?,
            })
        })?;
        let mut recent_telemetry = Vec::new();
        for r in rows {
            recent_telemetry.push(r?);
        }

        let stats = StoreStats {
            schema_version,
            workspace_id,
            db_incarnation,
            current_generation,
            vcs_head,
            indexed_revisions,
            file_count,
            symbol_count,
            edge_count,
            total_queries_served,
            recent_telemetry,
        };
        drop(stmt);
        tx.commit()?;
        Ok(stats)
    }
}

/// Identifies low-information summaries and broad sweeps that should not be retrieved as evidence.
#[must_use]
pub fn is_noise_commit(rev: &VcsRevision) -> bool {
    if rev.touched_files.is_empty() || rev.touched_files.len() > 50 {
        return true;
    }
    let summary = rev.summary.trim();
    if summary.len() < 4 {
        return true;
    }
    let lower = summary.to_lowercase();
    if [
        "merge branch ",
        "merge pull request ",
        "merge remote-tracking ",
        "auto-merge",
        "chore(deps)",
        "build(deps)",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
    {
        return true;
    }
    let remainder = match lower.split_once(':') {
        Some((prefix, remainder))
            if matches!(
                prefix.split(['(', '!']).next().unwrap_or(""),
                "build"
                    | "chore"
                    | "ci"
                    | "docs"
                    | "feat"
                    | "fix"
                    | "perf"
                    | "refactor"
                    | "revert"
                    | "style"
                    | "test"
            ) =>
        {
            remainder.trim()
        }
        _ => lower.as_str(),
    };
    remainder
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .all(|token| {
            matches!(
                token,
                "wip"
                    | "fix"
                    | "fixup"
                    | "lint"
                    | "fmt"
                    | "rustfmt"
                    | "prettier"
                    | "format"
                    | "formatting"
                    | "chore"
                    | "typo"
                    | "cleanup"
                    | "temp"
                    | "tmp"
                    | "update"
                    | "test"
                    | "tests"
            )
        })
}

fn history_fts_query(terms: &[String]) -> String {
    // Quoting only alphanumeric tokens prevents repository query text from becoming FTS syntax.
    terms
        .iter()
        .flat_map(|term| split_identifier_tokens(term))
        .take(12)
        .map(|token| format!("\"{token}\"*"))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn read_generation(conn: &Connection) -> Result<GenerationId, StoreError> {
    let raw: i64 = conn.query_row("SELECT current_gen_id FROM meta WHERE id = 1", [], |row| {
        row.get(0)
    })?;
    Ok(GenerationId(u64::try_from(raw).unwrap_or(0)))
}

fn verify_generation(conn: &Connection, expected_gen: GenerationId) -> Result<(), StoreError> {
    let db_gen = read_generation(conn)?;
    if db_gen != expected_gen {
        return Err(StoreError::GenerationMismatch {
            db_gen,
            graph_gen: expected_gen,
        });
    }
    Ok(())
}

fn generation_sql_value(generation: GenerationId) -> Result<i64, StoreError> {
    i64::try_from(generation.0)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)).into())
}

fn current_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}
