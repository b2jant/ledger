//! Bounded bipartite File <-> Revision CSR co-change graph, mass-sweep filter (`k_r > 50`),
//! time-decayed inverse-log revision weights, regularized conditional probability, and Lift.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use ledger_core::{CoChangePeer, HunkConfidence, LineSpan, SymbolRecord, VcsRevision};

use crate::history::HunkSymbolAttributor;

const MAX_REVISIONS_PER_ANCHOR: usize = 250;

/// Tunable mathematical hyperparameters for [`BipartiteCoChangeIndex`].
#[derive(Debug, Clone, Copy)]
pub struct CoChangeConfig {
    /// Maximum allowed touched files in a revision before classifying it as a mass-format sweep (`50`).
    pub max_files_per_revision: usize,
    /// Exponential time-decay constant `lambda` in seconds^-1 (default: 90-day half-life).
    pub decay_lambda_per_sec: f64,
    /// Laplace/Dirichlet smoothing prior `alpha` in `P_reg(B | A) = W(A,B) / (W(A) + alpha)` (`3.0`).
    pub smoothing_alpha: f64,
    /// Minimum shared raw non-sweep revisions required to surface a peer (`2`).
    pub min_shared_commits: u32,
    /// Minimum Lift required to surface a peer (`1.15`).
    pub min_lift: f32,
}

impl Default for CoChangeConfig {
    fn default() -> Self {
        let half_life_secs = 90.0 * 86_400.0;
        Self {
            max_files_per_revision: 50,
            decay_lambda_per_sec: std::f64::consts::LN_2 / half_life_secs,
            smoothing_alpha: 3.0,
            min_shared_commits: 2,
            min_lift: 1.15,
        }
    }
}

/// Bounded bipartite File <-> Revision CSR co-change index (`O(sum k_r)` memory, zero `O(k_r^2)` blowup).
#[derive(Debug, Clone, Default)]
pub struct BipartiteCoChangeIndex {
    config: CoChangeConfig,
    sweeps_filtered: usize,
    file_ids: HashMap<String, u32>,
    files: Vec<IndexedFile>,
    revisions: Vec<IndexedRevision>,
    // CSR rows: revision -> unique file touches, and file -> newest-first revisions.
    revision_offsets: Vec<usize>,
    revision_touches: Vec<RevisionTouch>,
    file_offsets: Vec<usize>,
    file_revision_ids: Vec<u32>,
    total_weight: f64,
}

#[derive(Debug, Clone)]
struct IndexedFile {
    rel_path: String,
    weight: f64,
}

#[derive(Debug, Clone)]
struct IndexedRevision {
    rev_id: String,
    short_id: String,
    summary: String,
    timestamp_secs: i64,
    weight: f64,
}

#[derive(Debug, Clone)]
struct RevisionTouch {
    file_id: u32,
    hunks: Vec<LineSpan>,
}

#[derive(Debug)]
struct PeerEvidence {
    shared_weight: f64,
    shared_commits: u32,
    recent_revision: u32,
    symbol_verified: bool,
}

#[derive(Debug)]
struct RankedPeer {
    peer: CoChangePeer,
    score: f64,
}

impl BipartiteCoChangeIndex {
    /// Builds a bounded bipartite CSR co-change index from `revisions` using `reference_now_secs`.
    /// Revisions are processed chronologically; equal timestamps retain input order.
    /// Invalid negative/non-finite numerical hyperparameters fall back to their defaults.
    ///
    /// # Panics
    /// Panics if the number of file or revision nodes exceeds the `u32` ID space.
    #[must_use]
    pub fn build(
        revisions: &[VcsRevision],
        config: CoChangeConfig,
        reference_now_secs: i64,
    ) -> Self {
        let defaults = CoChangeConfig::default();
        let config = CoChangeConfig {
            decay_lambda_per_sec: valid_nonnegative(
                config.decay_lambda_per_sec,
                defaults.decay_lambda_per_sec,
            ),
            smoothing_alpha: valid_nonnegative(config.smoothing_alpha, defaults.smoothing_alpha),
            min_lift: valid_nonnegative(f64::from(config.min_lift), f64::from(defaults.min_lift))
                as f32,
            ..config
        };
        let mut index = Self {
            config,
            revision_offsets: vec![0],
            ..Self::default()
        };
        let mut chronological: Vec<_> = revisions.iter().collect();
        chronological.sort_by_key(|revision| revision.timestamp_secs);

        for revision in chronological {
            // Even a suppressed sweep may rename a file whose earlier history is retained.
            let touches = index.canonical_touches(revision);
            let file_count = touches.len();
            if file_count == 0 {
                continue;
            }
            if file_count > config.max_files_per_revision {
                index.sweeps_filtered += 1;
                continue;
            }
            let age = (i128::from(reference_now_secs) - i128::from(revision.timestamp_secs)).max(0);
            let weight =
                (-config.decay_lambda_per_sec * age as f64).exp() / (file_count as f64).ln_1p();
            let _revision_id = node_id(index.revisions.len());
            index.revisions.push(IndexedRevision {
                rev_id: revision.rev_id.clone(),
                short_id: revision.short_id.clone(),
                summary: revision.summary.clone(),
                timestamp_secs: revision.timestamp_secs,
                weight,
            });
            index.total_weight += weight;
            for touch in &touches {
                index.files[touch.file_id as usize].weight += weight;
            }
            index.revision_touches.extend(touches);
            index.revision_offsets.push(index.revision_touches.len());
        }
        index.build_file_rows();
        index
    }

    /// Number of non-sweep revisions retained in the CSR graph.
    #[must_use]
    pub const fn revisions_indexed(&self) -> usize {
        self.revisions.len()
    }

    /// Number of mass-sweep revisions (`k_r > max_files_per_revision`) excluded from co-change math.
    #[must_use]
    pub const fn sweeps_filtered(&self) -> usize {
        self.sweeps_filtered
    }

    /// Queries non-transitive 1-hop co-change peers for `anchor_files`, optionally upgrading
    /// confidence to `HunkVerifiedSymbol` using `symbols_by_file`.
    ///
    /// Shared evidence scans the 250 most recent retained revisions per anchor. Marginal
    /// weights and `anchor_total_commits` include the complete retained history, so the
    /// bounded numerator conservatively underestimates probability for longer histories.
    #[must_use]
    pub fn query_peers(
        &self,
        anchor_files: &[String],
        symbols_by_file: &HashMap<String, Vec<SymbolRecord>>,
        max_peers: usize,
    ) -> Vec<CoChangePeer> {
        if max_peers == 0 || self.total_weight <= 0.0 {
            return Vec::new();
        }
        let anchors: HashSet<_> = anchor_files
            .iter()
            .filter_map(|path| self.file_ids.get(path).copied())
            .collect();
        let mut best_by_path: HashMap<String, RankedPeer> = HashMap::new();
        for &anchor_id in &anchors {
            let anchor = &self.files[anchor_id as usize];
            let revision_ids = self.file_revisions(anchor_id);
            let denominator = anchor.weight + self.config.smoothing_alpha;
            if denominator <= 0.0 {
                continue;
            }
            let mut candidates: HashMap<u32, PeerEvidence> = HashMap::new();
            for &revision_id in revision_ids.iter().take(MAX_REVISIONS_PER_ANCHOR) {
                let revision = &self.revisions[revision_id as usize];
                let touches = self.revision_files(revision_id);
                let anchor_verified = touches.iter().any(|touch| {
                    touch.file_id == anchor_id && self.symbol_verified(touch, symbols_by_file)
                });
                for touch in touches {
                    if anchors.contains(&touch.file_id) {
                        continue;
                    }
                    let evidence = candidates.entry(touch.file_id).or_insert(PeerEvidence {
                        shared_weight: 0.0,
                        shared_commits: 0,
                        recent_revision: revision_id,
                        symbol_verified: false,
                    });
                    evidence.shared_weight += revision.weight;
                    evidence.shared_commits += 1;
                    evidence.symbol_verified |=
                        anchor_verified || self.symbol_verified(touch, symbols_by_file);
                    let recent = &self.revisions[evidence.recent_revision as usize];
                    if (revision.timestamp_secs, &revision.rev_id)
                        > (recent.timestamp_secs, &recent.rev_id)
                    {
                        evidence.recent_revision = revision_id;
                    }
                }
            }
            for (peer_id, evidence) in candidates {
                if evidence.shared_commits < self.config.min_shared_commits {
                    continue;
                }
                let file = &self.files[peer_id as usize];
                let probability = evidence.shared_weight / denominator;
                let marginal = file.weight / self.total_weight;
                let lift = probability / marginal.max(1e-9);
                if lift < f64::from(self.config.min_lift) {
                    continue;
                }
                let recent = &self.revisions[evidence.recent_revision as usize];
                let ranked = RankedPeer {
                    peer: CoChangePeer {
                        rel_path: file.rel_path.clone(),
                        anchor_file: anchor.rel_path.clone(),
                        cochange_commits: evidence.shared_commits,
                        anchor_total_commits: node_id(revision_ids.len()),
                        regularized_prob: probability as f32,
                        lift: lift as f32,
                        confidence: if evidence.symbol_verified {
                            HunkConfidence::HunkVerifiedSymbol
                        } else {
                            HunkConfidence::FileLevel
                        },
                        recent_rev_short: recent.short_id.clone(),
                        recent_summary: recent.summary.clone(),
                        expand_id: format!("diff:{}:{}", recent.rev_id, file.rel_path),
                    },
                    score: probability * (1.0 + lift.ln_1p()),
                };
                match best_by_path.get_mut(&file.rel_path) {
                    Some(best) if rank_order(&ranked, best).is_lt() => *best = ranked,
                    None => {
                        best_by_path.insert(file.rel_path.clone(), ranked);
                    }
                    Some(_) => {}
                }
            }
        }
        let mut ranked: Vec<_> = best_by_path.into_values().collect();
        ranked.sort_by(rank_order);
        ranked.truncate(max_peers);
        ranked.into_iter().map(|entry| entry.peer).collect()
    }

    fn file_id(&mut self, path: &str) -> u32 {
        if let Some(&id) = self.file_ids.get(path) {
            return id;
        }
        let id = node_id(self.files.len());
        self.files.push(IndexedFile {
            rel_path: path.to_owned(),
            weight: 0.0,
        });
        self.file_ids.insert(path.to_owned(), id);
        id
    }

    fn canonical_touches(&mut self, revision: &VcsRevision) -> Vec<RevisionTouch> {
        // Resolve every source before changing any destination: simultaneous name swaps
        // preserve two identities, and repeated renames never form path-alias cycles.
        let mut rename_sources = HashMap::new();
        for touch in &revision.touched_files {
            if let Some(old_path) = &touch.old_rel_path {
                let id = self.file_id(old_path);
                rename_sources.insert(old_path.as_str(), id);
            }
        }
        for old_path in rename_sources.keys() {
            self.file_ids.remove(*old_path);
        }
        for touch in &revision.touched_files {
            if let Some(old_path) = &touch.old_rel_path {
                let id = rename_sources[old_path.as_str()];
                self.file_ids.insert(touch.rel_path.clone(), id);
                self.files[id as usize].rel_path.clone_from(&touch.rel_path);
            }
        }
        let mut unique: HashMap<u32, Vec<LineSpan>> = HashMap::new();
        for touch in &revision.touched_files {
            let id = self
                .file_ids
                .get(&touch.rel_path)
                .copied()
                .or_else(|| rename_sources.get(touch.rel_path.as_str()).copied())
                .unwrap_or_else(|| self.file_id(&touch.rel_path));
            unique
                .entry(id)
                .or_default()
                .extend_from_slice(&touch.hunk_new_ranges);
        }
        let mut touches: Vec<_> = unique
            .into_iter()
            .map(|(file_id, mut hunks)| {
                hunks.sort_unstable_by_key(|span| (span.start_line, span.end_line));
                hunks.dedup();
                RevisionTouch { file_id, hunks }
            })
            .collect();
        touches.sort_unstable_by_key(|touch| touch.file_id);
        touches
    }

    fn build_file_rows(&mut self) {
        self.file_offsets = vec![0; self.files.len() + 1];
        for touch in &self.revision_touches {
            self.file_offsets[touch.file_id as usize + 1] += 1;
        }
        for file in 0..self.files.len() {
            self.file_offsets[file + 1] += self.file_offsets[file];
        }
        self.file_revision_ids = vec![0; self.revision_touches.len()];
        let mut cursors = self.file_offsets[..self.files.len()].to_vec();
        for revision in (0..self.revisions.len()).rev() {
            let revision_id = node_id(revision);
            let start = self.revision_offsets[revision];
            let end = self.revision_offsets[revision + 1];
            for touch in &self.revision_touches[start..end] {
                let cursor = &mut cursors[touch.file_id as usize];
                self.file_revision_ids[*cursor] = revision_id;
                *cursor += 1;
            }
        }
    }

    fn file_revisions(&self, file_id: u32) -> &[u32] {
        let file = file_id as usize;
        &self.file_revision_ids[self.file_offsets[file]..self.file_offsets[file + 1]]
    }

    fn revision_files(&self, revision_id: u32) -> &[RevisionTouch] {
        let revision = revision_id as usize;
        &self.revision_touches[self.revision_offsets[revision]..self.revision_offsets[revision + 1]]
    }

    fn symbol_verified(
        &self,
        touch: &RevisionTouch,
        symbols_by_file: &HashMap<String, Vec<SymbolRecord>>,
    ) -> bool {
        let path = &self.files[touch.file_id as usize].rel_path;
        symbols_by_file.get(path).is_some_and(|symbols| {
            HunkSymbolAttributor::classify_touch(&touch.hunks, symbols)
                == HunkConfidence::HunkVerifiedSymbol
        })
    }
}

fn valid_nonnegative(value: f64, default: f64) -> f64 {
    if value.is_finite() && value >= 0.0 {
        value
    } else {
        default
    }
}

fn node_id(index: usize) -> u32 {
    match u32::try_from(index) {
        Ok(id) => id,
        Err(_) => panic!("co-change graph exceeds the u32 node ID space"),
    }
}

fn rank_order(left: &RankedPeer, right: &RankedPeer) -> Ordering {
    right
        .score
        .total_cmp(&left.score)
        .then_with(|| right.peer.cochange_commits.cmp(&left.peer.cochange_commits))
        .then_with(|| left.peer.rel_path.cmp(&right.peer.rel_path))
        .then_with(|| left.peer.anchor_file.cmp(&right.peer.anchor_file))
}
