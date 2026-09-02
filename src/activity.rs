// Copyright 2026 The Sashiko Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Live activity tracking for in-flight fetch and review work.
//!
//! Answers "what is this patchset doing right now, and how long has it been
//! doing it?" — the question that was previously unanswerable when a patchset
//! sat in `Fetching` or `In Review` with no external sign of progress.
//!
//! Live state is in-memory: nothing is actually running after a restart, so an
//! empty registry is the truthful answer rather than a gap. Every write funnels
//! through [`ActivityRegistry::update`], which is also where the optional
//! write-through to `patchset_activity` hooks in. That durable copy covers the
//! one case memory cannot: work interrupted by a restart, where nothing is live
//! but the patchset is still sitting in a non-terminal state and needs to
//! explain itself.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Identifies a unit of tracked work.
///
/// Fetches are keyed by commit rather than patchset id because `FetchRequest`
/// carries no patchset id — the fetch queue batches by repository, not by
/// patchset. Callers correlate a commit back to its patchset via the
/// `{sha}@sashiko.local` message id written by `create_fetching_patchset`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ActivityKey {
    /// Coarse lifecycle of a whole patchset (queued, reviewing its patches).
    ///
    /// Written once per transition by the review task itself, never by a patch's
    /// worker. Per-patch phases used to land here, and because a patchset
    /// reviews several patches at once their phases interleaved — every
    /// alternation between one patch planning and another reviewing counted as
    /// a new activity and restarted the clock, so the reported age was the time
    /// since whichever patch moved last rather than the age of the patchset.
    Patchset(i64),
    /// Coarse lifecycle of one patch's review, above its individual stages.
    PatchsetPatch {
        patchset_id: i64,
        patch_id: i64,
    },
    /// One review stage of one patch.
    ///
    /// Keyed by patch as well as stage because both dimensions run
    /// concurrently: a patchset reviews several patches at once, and each patch
    /// runs its component stages in parallel. Any two units sharing a slot would
    /// make the reported phase flap between whichever spoke last, hiding a
    /// wedged one behind its still-active siblings.
    PatchsetStage {
        patchset_id: i64,
        patch_id: i64,
        stage: u8,
    },
    Commit(String),
}

impl ActivityKey {
    /// Whether this key belongs to the given patchset, at any granularity.
    pub fn belongs_to_patchset(&self, patchset_id: i64) -> bool {
        self.patchset_id() == Some(patchset_id)
    }

    /// The patchset this key belongs to, if any. Commit-keyed fetch work has
    /// none, because `FetchRequest` carries no patchset id.
    pub fn patchset_id(&self) -> Option<i64> {
        match self {
            ActivityKey::Patchset(id)
            | ActivityKey::PatchsetPatch {
                patchset_id: id, ..
            }
            | ActivityKey::PatchsetStage {
                patchset_id: id, ..
            } => Some(*id),
            ActivityKey::Commit(_) => None,
        }
    }

    /// The patch this key belongs to, if it is about one patch.
    ///
    /// `None` for the coarse patchset entry, which describes the whole review
    /// rather than any one patch and so has no card to sit under.
    pub fn patch_id(&self) -> Option<i64> {
        match self {
            ActivityKey::PatchsetPatch { patch_id, .. }
            | ActivityKey::PatchsetStage { patch_id, .. } => Some(*patch_id),
            _ => None,
        }
    }

    /// The review stage this key belongs to, if it is stage-level.
    pub fn stage(&self) -> Option<u8> {
        match self {
            ActivityKey::PatchsetStage { stage, .. } => Some(*stage),
            _ => None,
        }
    }

    /// The keys this one is nested inside, nearest first.
    ///
    /// A patch's entry changes phase twice and a patchset's once, so their own
    /// `updated` stops advancing almost immediately while the work beneath them
    /// is busy. Left alone, `idle_seconds` at those levels equals `age_seconds`
    /// forever and a healthy review reports itself as making no progress.
    /// Liveness there is a property of the work underneath, so an update to a
    /// child counts as a sign of life for its ancestors.
    fn ancestors(&self) -> Vec<ActivityKey> {
        match self {
            ActivityKey::PatchsetStage {
                patchset_id,
                patch_id,
                ..
            } => vec![
                ActivityKey::PatchsetPatch {
                    patchset_id: *patchset_id,
                    patch_id: *patch_id,
                },
                ActivityKey::Patchset(*patchset_id),
            ],
            ActivityKey::PatchsetPatch { patchset_id, .. } => {
                vec![ActivityKey::Patchset(*patchset_id)]
            }
            ActivityKey::Patchset(_) | ActivityKey::Commit(_) => Vec::new(),
        }
    }

    /// Parses the [`Display`](std::fmt::Display) form back into a key.
    ///
    /// Live entries keep their key as a value, but persisted rows store only the
    /// rendered string, so recovering the patch a stage belongs to means reading
    /// it back out. Kept as the exact inverse of `Display` — the round-trip is
    /// asserted in the tests.
    pub fn parse(s: &str) -> Option<ActivityKey> {
        if let Some(sha) = s.strip_prefix("commit:") {
            return (!sha.is_empty()).then(|| ActivityKey::Commit(sha.to_string()));
        }

        let rest = s.strip_prefix("patchset:")?;
        let Some((patchset, tail)) = rest.split_once('/') else {
            return rest.parse().ok().map(ActivityKey::Patchset);
        };
        let patchset_id = patchset.parse().ok()?;
        let patch = tail.strip_prefix("patch:")?;

        match patch.split_once("/stage:") {
            None => Some(ActivityKey::PatchsetPatch {
                patchset_id,
                patch_id: patch.parse().ok()?,
            }),
            Some((patch, stage)) => Some(ActivityKey::PatchsetStage {
                patchset_id,
                patch_id: patch.parse().ok()?,
                stage: stage.parse().ok()?,
            }),
        }
    }
}

impl std::fmt::Display for ActivityKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActivityKey::Patchset(id) => write!(f, "patchset:{}", id),
            ActivityKey::PatchsetPatch {
                patchset_id,
                patch_id,
            } => write!(f, "patchset:{}/patch:{}", patchset_id, patch_id),
            ActivityKey::PatchsetStage {
                patchset_id,
                patch_id,
                stage,
            } => write!(
                f,
                "patchset:{}/patch:{}/stage:{}",
                patchset_id, patch_id, stage
            ),
            ActivityKey::Commit(sha) => write!(f, "commit:{}", sha),
        }
    }
}

/// What a unit of work is currently doing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Phase {
    /// Accepted but not yet picked up.
    Queued,
    /// Running a git subprocess. `op` is a human-readable command summary.
    GitOp { op: String },
    /// Fetching objects from a remote.
    Fetching { remote: String, commits: usize },
    /// Deciding which review stages are relevant.
    ///
    /// `attempt` is 1-based. A review is retried up to `review.max_retries`
    /// times, and each retry restarts the whole pipeline, so the attempt number
    /// is the difference between "this is slow" and "this is the third go".
    Planning { attempt: u32, max_attempts: u32 },
    /// Review stages are running; per-stage detail lives under `PatchsetStage` keys.
    Reviewing { attempt: u32, max_attempts: u32 },
    /// The patchset is working through its patches.
    ///
    /// Deliberately says nothing about attempts. Attempts are counted per patch,
    /// and with several reviewed at once there is no single attempt number that
    /// is true of the patchset — reporting one patch's here is what made the
    /// patchset's clock track whichever patch moved most recently.
    ReviewingPatches { patches: usize },
    /// Executing a review stage. `turn` is the current LLM round trip.
    Stage {
        stage: u8,
        turn: usize,
        max_turns: usize,
        /// What the turn is currently blocked on.
        waiting: StageWait,
    },
    /// A stage finished.
    ///
    /// Kept rather than dropped. A stage that vanished the moment it succeeded
    /// left the patch's card showing less and less as the review progressed,
    /// with the completed work only reappearing once the whole review ended and
    /// the recorded breakdown replaced the live view. Carrying the same numbers
    /// the breakdown will show means each row settles in place instead.
    StageDone { stage: u8, seconds: u64, turns: u64 },
    /// A stage stopped before finishing.
    ///
    /// Recorded rather than cleared. Clearing would make the stage silently
    /// vanish from a review that is still running, and the alternative —
    /// leaving the last [`Phase::Stage`] in place — is worse still: it claims
    /// the stage is mid-turn when it is not. The turn cap is the sharpest case,
    /// because it fires between round trips, so the last thing reported is
    /// "turn N/N, awaiting model", which reads exactly like a hang.
    StageFailed {
        stage: u8,
        reason: String,
        cancelled: bool,
    },
}

/// Condenses a failure reason to something that fits on one line.
///
/// Reasons are `{:#}`-formatted anyhow chains: multi-line, and long enough to
/// push everything else off the row. The first line carries the identifying
/// part, which is what a reader scanning a list of stages needs.
fn summarize_reason(reason: &str) -> String {
    const MAX: usize = 140;
    let first = reason.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return "no reason given".to_string();
    }
    match first.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}…", &first[..cut]),
        None => first.to_string(),
    }
}

/// What a review turn is currently blocked on.
///
/// A turn's elapsed time is split between these, and they call for completely
/// different responses: a slow model is patience, a rate limit is capacity, a
/// full queue is concurrency settings, and a tree-wide `git grep` is a prompt
/// that needs narrowing. Reporting them all as "idle" made a stalled stage
/// undiagnosable without reading the daemon log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum StageWait {
    /// Request is with the model.
    Model,
    /// Waiting for a slot: the LLM semaphore is full.
    Queued,
    /// Backing off after the provider asked us to slow down.
    RateLimited { retry_in_seconds: u64 },
    /// Running git tools the model asked for.
    Tools { names: Vec<String> },
}

impl StageWait {
    fn describe(&self) -> String {
        match self {
            StageWait::Model => "awaiting model".to_string(),
            StageWait::Queued => "queued for a model slot".to_string(),
            StageWait::RateLimited { retry_in_seconds } => {
                format!("rate limited, retrying in {}s", retry_in_seconds)
            }
            StageWait::Tools { names } if !names.is_empty() => {
                format!("running {}", names.join(", "))
            }
            StageWait::Tools { .. } => "running tools".to_string(),
        }
    }
}

/// Renders the attempt as a suffix, but only once a retry has happened.
///
/// Nearly every review succeeds first time; printing "(attempt 1/4)" everywhere
/// would be noise that hides the case worth noticing.
fn attempt_suffix(attempt: u32, max_attempts: u32) -> String {
    if attempt > 1 {
        format!(" (attempt {}/{})", attempt, max_attempts)
    } else {
        String::new()
    }
}

impl Phase {
    /// Short human-readable description, used by the CLI and web UI.
    pub fn describe(&self) -> String {
        match self {
            Phase::Queued => "queued".to_string(),
            Phase::GitOp { op } => format!("running {}", op),
            Phase::Fetching { remote, commits } => {
                format!("fetching {} commit(s) from {}", commits, remote)
            }
            Phase::Planning {
                attempt,
                max_attempts,
            } => format!("planning stages{}", attempt_suffix(*attempt, *max_attempts)),
            Phase::Reviewing {
                attempt,
                max_attempts,
            } => format!(
                "running review stages{}",
                attempt_suffix(*attempt, *max_attempts)
            ),
            Phase::ReviewingPatches { patches } => {
                if *patches == 1 {
                    "reviewing 1 patch".to_string()
                } else {
                    format!("reviewing {} patches", patches)
                }
            }
            Phase::Stage {
                stage,
                turn,
                max_turns,
                waiting,
            } => {
                let where_ = if *max_turns == 0 {
                    format!("stage {}, starting", stage)
                } else {
                    format!("stage {}, turn {}/{}", stage, turn, max_turns)
                };
                format!("{} ({})", where_, waiting.describe())
            }
            Phase::StageDone {
                stage,
                seconds,
                turns,
            } => {
                let turns = match turns {
                    0 => String::new(),
                    1 => ", 1 turn".to_string(),
                    n => format!(", {} turns", n),
                };
                format!(
                    "stage {} done in {}{}",
                    stage,
                    format_duration(*seconds),
                    turns
                )
            }
            Phase::StageFailed {
                stage,
                reason,
                cancelled,
            } => format!(
                "stage {} {}: {}",
                stage,
                if *cancelled { "cancelled" } else { "failed" },
                summarize_reason(reason)
            ),
        }
    }

    /// Whether two phases represent the same ongoing activity.
    ///
    /// Turn number is deliberately excluded: turns 5 and 6 of stage 4 are one
    /// activity making progress, not two activities. That is precisely what lets
    /// `age_seconds` (how long this stage has been running) diverge from
    /// `idle_seconds` (how long since it last moved) — without this, every event
    /// would reset both clocks and the two numbers could never differ.
    fn same_activity(&self, other: &Phase) -> bool {
        match (self, other) {
            (Phase::Queued, Phase::Queued) => true,
            // A retry restarts the pipeline, so it is a new activity and the
            // clock should reset rather than carry over from the failed attempt.
            (Phase::Planning { attempt: a, .. }, Phase::Planning { attempt: b, .. }) => a == b,
            (Phase::Reviewing { attempt: a, .. }, Phase::Reviewing { attempt: b, .. }) => a == b,
            // The patchset stays one activity for its whole review, even as the
            // number of patches still in flight changes. This is the clock the
            // top-level display reads, and it must span the patchset.
            (Phase::ReviewingPatches { .. }, Phase::ReviewingPatches { .. }) => true,
            (Phase::GitOp { op: a }, Phase::GitOp { op: b }) => a == b,
            (Phase::Fetching { remote: a, .. }, Phase::Fetching { remote: b, .. }) => a == b,
            (Phase::Stage { stage: a, .. }, Phase::Stage { stage: b, .. }) => a == b,
            // A finished stage is a settled fact, not something whose age is
            // worth watching, but it still gets its own identity per stage.
            (Phase::StageDone { stage: a, .. }, Phase::StageDone { stage: b, .. }) => a == b,
            // Deliberately not the same activity as the `Stage` it replaces:
            // failing is a change, so the clock restarts and `age_seconds`
            // reads as how long ago the stage stopped rather than how long it
            // ran. How long it ran is already in the stage timings.
            (Phase::StageFailed { stage: a, .. }, Phase::StageFailed { stage: b, .. }) => a == b,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
struct Activity {
    phase: Phase,
    /// When the current *activity* began — reset only when the work genuinely
    /// changes, not when it merely advances (see [`Phase::same_activity`]).
    since: Instant,
    /// When the last update of any kind arrived, including an advance within the
    /// same activity.
    updated: Instant,
    /// When this entry was last written through to the database, used to keep
    /// per-turn updates from hammering SQLite.
    persisted: Option<Instant>,
}

/// A write-through instruction for the persistence task.
#[derive(Debug)]
enum PersistOp {
    Upsert {
        key: String,
        patchset_id: Option<i64>,
        phase: String,
        description: String,
        updated_at: i64,
    },
    Delete {
        key: String,
    },
    DeletePatchset {
        patchset_id: i64,
    },
}

/// How stale a persisted row may get before it is refreshed.
///
/// Activity changes always persist immediately; this only governs repeated
/// advances within one activity (turn 5 → turn 6 → …), which would otherwise
/// mean a database write per LLM round trip.
const PERSIST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// A serialisable view of one entry, with durations resolved against now.
#[derive(Debug, Clone, Serialize)]
pub struct ActivitySnapshot {
    pub key: String,
    pub phase: Phase,
    pub description: String,
    /// Which patch this belongs to, for surfaces that place activity beside the
    /// patch it describes. `None` for the coarse patchset entry and for
    /// commit-keyed fetch work, neither of which is about one patch.
    ///
    /// Derived here rather than left to the client to scrape out of `key`: the
    /// key format is an internal detail and two consumers already read these
    /// snapshots.
    pub patch_id: Option<i64>,
    /// Which review stage this is, for the same reason.
    pub stage: Option<u8>,
    /// Seconds since this activity began — how long the stage has been running.
    pub age_seconds: u64,
    /// Seconds since it last advanced. A large value beside a growing
    /// `age_seconds` is the signature of wedged work.
    pub idle_seconds: u64,
}

/// Tracks what every in-flight unit of work is currently doing.
#[derive(Debug, Default)]
pub struct ActivityRegistry {
    entries: Mutex<HashMap<ActivityKey, Activity>>,
    /// Set only when persistence is enabled. Writes are handed to a background
    /// task rather than performed inline: `update` is called from synchronous
    /// callbacks on the review hot path and must never block on I/O.
    persist_tx: Option<tokio::sync::mpsc::UnboundedSender<PersistOp>>,
}

impl ActivityRegistry {
    /// In-memory only. Used by tests and by any caller without a database.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// In-memory, with write-through to `patchset_activity`.
    ///
    /// The durable copy exists for one case: the daemon stops while work is in
    /// flight, so nothing is live to report, but the patchset is still sitting in
    /// `Fetching` or `In Review` and needs to explain itself.
    pub fn with_persistence(db: Arc<crate::db::Database>) -> Arc<Self> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PersistOp>();

        tokio::spawn(async move {
            while let Some(op) = rx.recv().await {
                // Best-effort: activity tracking must never be able to fail work.
                let result = match op {
                    PersistOp::Upsert {
                        key,
                        patchset_id,
                        phase,
                        description,
                        updated_at,
                    } => {
                        db.upsert_activity(&key, patchset_id, &phase, &description, updated_at)
                            .await
                    }
                    PersistOp::Delete { key } => db.delete_activity(&key).await,
                    PersistOp::DeletePatchset { patchset_id } => {
                        db.delete_patchset_activity(patchset_id).await
                    }
                };
                if let Err(e) = result {
                    tracing::debug!("Failed to persist activity: {}", e);
                }
            }
        });

        Arc::new(Self {
            entries: Mutex::new(HashMap::new()),
            persist_tx: Some(tx),
        })
    }

    /// Records the current phase for `key`.
    ///
    /// The single write path for all activity state. Advancing within the same
    /// activity (a new turn of the same stage) refreshes liveness without
    /// restarting the clock on how long that activity has been running.
    pub fn update(&self, key: ActivityKey, phase: Phase) {
        let now = Instant::now();
        let should_persist = {
            let mut entries = self.entries.lock().unwrap();
            Self::record(&mut entries, &key, phase.clone(), now)
        };

        if should_persist {
            self.send_persist(Self::upsert_op(&key, &phase));
        }
    }

    /// Updates only what a stage is waiting on, keeping its turn numbers.
    ///
    /// Two sides report on a stage and neither knows the whole story: the worker
    /// knows which turn it is on and what the cap is, while the daemon's request
    /// proxy knows whether a request is queued, rate limited or genuinely with
    /// the model. Writing a whole `Phase::Stage` from the proxy meant filling in
    /// the half it does not know with placeholders, which overwrote the worker's
    /// numbers on every single request — so a stage read "starting" for almost
    /// its entire life instead of "turn 7/50".
    ///
    /// Read and write happen under one lock, so a concurrent turn report cannot
    /// be lost between them.
    pub fn update_stage_wait(&self, key: ActivityKey, stage: u8, wait: StageWait) {
        let now = Instant::now();
        let (phase, should_persist) = {
            let mut entries = self.entries.lock().unwrap();
            // No entry yet means no turn has been reported: say so rather than
            // inventing a number. `max_turns: 0` renders as "starting".
            let (turn, max_turns) = match entries.get(&key).map(|a| &a.phase) {
                Some(Phase::Stage {
                    turn, max_turns, ..
                }) => (*turn, *max_turns),
                _ => (0, 0),
            };
            let phase = Phase::Stage {
                stage,
                turn,
                max_turns,
                waiting: wait,
            };
            let persist = Self::record(&mut entries, &key, phase.clone(), now);
            (phase, persist)
        };

        if should_persist {
            self.send_persist(Self::upsert_op(&key, &phase));
        }
    }

    fn upsert_op(key: &ActivityKey, phase: &Phase) -> PersistOp {
        PersistOp::Upsert {
            key: key.to_string(),
            patchset_id: key.patchset_id(),
            phase: serde_json::to_string(phase).unwrap_or_else(|_| "null".to_string()),
            description: phase.describe(),
            updated_at: unix_now(),
        }
    }

    /// Applies one phase change to the map. Returns whether it should persist.
    fn record(
        entries: &mut HashMap<ActivityKey, Activity>,
        key: &ActivityKey,
        phase: Phase,
        now: Instant,
    ) -> bool {
        let mut should_persist = false;

        {
            let entry = entries
                .entry(key.clone())
                .and_modify(|existing| {
                    // `since` tracks the activity, `updated` tracks the event. A
                    // new turn of the same stage advances `updated` only, so the
                    // gap between them is real elapsed silence.
                    let changed = !existing.phase.same_activity(&phase);
                    if changed {
                        existing.since = now;
                    }
                    existing.phase = phase.clone();
                    existing.updated = now;

                    // Persist on a genuine change, or when the stored row has
                    // gone stale. Persisting every turn would mean a write per
                    // LLM round trip for no extra information.
                    should_persist = changed
                        || existing
                            .persisted
                            .is_none_or(|p| now.duration_since(p) >= PERSIST_INTERVAL);
                    if should_persist {
                        existing.persisted = Some(now);
                    }
                })
                .or_insert_with(|| {
                    should_persist = true;
                    Activity {
                        phase: phase.clone(),
                        since: now,
                        updated: now,
                        persisted: Some(now),
                    }
                });
            let _ = entry;
        }

        // A child reporting is a sign of life for everything it sits inside.
        // `updated` only, never `since`: the patchset has not started a new
        // activity just because one of its stages took a turn, and resetting
        // `since` here would undo the clock that spans the whole review.
        //
        // Deliberately does not mark those entries for persistence. The durable
        // copy exists to say where interrupted work got to, which a heartbeat
        // does not change, and persisting one would mean a write per turn for
        // every level.
        for ancestor in key.ancestors() {
            if let Some(entry) = entries.get_mut(&ancestor) {
                entry.updated = now;
            }
        }

        should_persist
    }

    /// Drops tracking for `key`. Call when the work finishes, however it ends.
    pub fn clear(&self, key: &ActivityKey) {
        self.entries.lock().unwrap().remove(key);
        self.send_persist(PersistOp::Delete {
            key: key.to_string(),
        });
    }

    /// Drops every entry belonging to a patchset, including its per-stage entries.
    pub fn clear_patchset(&self, patchset_id: i64) {
        self.entries
            .lock()
            .unwrap()
            .retain(|key, _| !key.belongs_to_patchset(patchset_id));
        self.send_persist(PersistOp::DeletePatchset { patchset_id });
    }

    fn send_persist(&self, op: PersistOp) {
        if let Some(tx) = &self.persist_tx {
            let _ = tx.send(op);
        }
    }

    /// All live entries for one patchset, coarse entry first then stages in order.
    pub fn patchset_snapshot(&self, patchset_id: i64) -> Vec<ActivitySnapshot> {
        let now = Instant::now();
        let entries = self.entries.lock().unwrap();
        let mut out: Vec<(ActivityKey, ActivitySnapshot)> = entries
            .iter()
            .filter(|(k, _)| k.belongs_to_patchset(patchset_id))
            .map(|(k, a)| (k.clone(), snapshot(k, a, now)))
            .collect();

        // Patchset first, then each patch's coarse entry ahead of its own
        // stages. Patch ids are positive, so the patchset's (0, 0) cannot
        // collide with a patch's.
        out.sort_by_key(|(k, _)| match k {
            ActivityKey::Patchset(_) => (0i64, 0u16),
            ActivityKey::PatchsetPatch { patch_id, .. } => (*patch_id, 0),
            ActivityKey::PatchsetStage {
                patch_id, stage, ..
            } => (*patch_id, 1 + *stage as u16),
            ActivityKey::Commit(_) => (i64::MAX, u16::MAX),
        });
        out.into_iter().map(|(_, s)| s).collect()
    }

    /// Current activity for one key, if it is still live.
    pub fn get(&self, key: &ActivityKey) -> Option<ActivitySnapshot> {
        let now = Instant::now();
        let entries = self.entries.lock().unwrap();
        entries.get(key).map(|a| snapshot(key, a, now))
    }

    /// All live activity, most recently updated first.
    pub fn snapshot(&self) -> Vec<ActivitySnapshot> {
        let now = Instant::now();
        let entries = self.entries.lock().unwrap();
        let mut out: Vec<ActivitySnapshot> =
            entries.iter().map(|(k, a)| snapshot(k, a, now)).collect();
        out.sort_by(|a, b| a.idle_seconds.cmp(&b.idle_seconds));
        out
    }

    /// Number of tracked units. Primarily for tests and diagnostics.
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Clears its entry when dropped.
///
/// Review tasks have many early-return paths; without a guard, any one of them
/// would leave a stale entry claiming work is still in progress — exactly the
/// misleading state this module exists to prevent.
pub struct PatchsetActivityGuard {
    registry: Arc<ActivityRegistry>,
    patchset_id: i64,
}

impl PatchsetActivityGuard {
    pub fn new(registry: Arc<ActivityRegistry>, patchset_id: i64, phase: Phase) -> Self {
        registry.update(ActivityKey::Patchset(patchset_id), phase);
        Self {
            registry,
            patchset_id,
        }
    }

    pub fn update(&self, phase: Phase) {
        self.registry
            .update(ActivityKey::Patchset(self.patchset_id), phase);
    }
}

impl Drop for PatchsetActivityGuard {
    fn drop(&mut self) {
        // Clears the per-stage entries too: a stage that died without emitting
        // StageFinished would otherwise linger forever, claiming to be running.
        self.registry.clear_patchset(self.patchset_id);
    }
}

/// Formats an elapsed-seconds count for display.
///
/// Presentation only — the API payload keeps raw seconds so callers can compare
/// and threshold on them. Past roughly two minutes a bare second count stops
/// being readable at a glance: "137s" parses as a number, "2m 17s" parses as a
/// duration. Hours are broken out too, because the wedged work this module
/// exists to surface can sit for a very long time.
pub fn format_duration(seconds: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;

    if seconds < 2 * MINUTE {
        format!("{}s", seconds)
    } else if seconds < HOUR {
        format!("{}m {}s", seconds / MINUTE, seconds % MINUTE)
    } else {
        format!(
            "{}h {}m {}s",
            seconds / HOUR,
            (seconds % HOUR) / MINUTE,
            seconds % MINUTE
        )
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn snapshot(key: &ActivityKey, activity: &Activity, now: Instant) -> ActivitySnapshot {
    ActivitySnapshot {
        key: key.to_string(),
        phase: activity.phase.clone(),
        description: activity.phase.describe(),
        patch_id: key.patch_id(),
        stage: key.stage(),
        age_seconds: now.duration_since(activity.since).as_secs(),
        idle_seconds: now.duration_since(activity.updated).as_secs(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_update_and_get() {
        let reg = ActivityRegistry::new();
        let key = ActivityKey::Patchset(42);

        assert!(reg.get(&key).is_none());

        reg.update(
            key.clone(),
            Phase::Planning {
                attempt: 1,
                max_attempts: 1,
            },
        );
        let snap = reg.get(&key).expect("entry should exist");
        assert_eq!(
            snap.phase,
            Phase::Planning {
                attempt: 1,
                max_attempts: 1
            }
        );
        assert_eq!(snap.key, "patchset:42");
    }

    #[test]
    fn test_clear_removes_entry() {
        let reg = ActivityRegistry::new();
        let key = ActivityKey::Commit("abc123".to_string());

        reg.update(key.clone(), Phase::Queued);
        assert_eq!(reg.len(), 1);

        reg.clear(&key);
        assert!(reg.get(&key).is_none());
        assert!(reg.is_empty());
    }

    #[test]
    fn test_repeated_same_phase_preserves_age() {
        let reg = ActivityRegistry::new();
        let key = ActivityKey::Patchset(1);
        let phase = Phase::Stage {
            stage: 3,
            turn: 5,
            max_turns: 100,
            waiting: StageWait::Model,
        };

        reg.update(key.clone(), phase.clone());
        let since_first = {
            let entries = reg.entries.lock().unwrap();
            entries.get(&key).unwrap().since
        };

        // Re-reporting the same phase must not reset the clock, otherwise a
        // wedged stage would look perpetually fresh.
        reg.update(key.clone(), phase);
        let since_second = {
            let entries = reg.entries.lock().unwrap();
            entries.get(&key).unwrap().since
        };

        assert_eq!(since_first, since_second);
    }

    #[test]
    fn test_moving_to_a_new_activity_resets_age() {
        let reg = ActivityRegistry::new();
        let key = ActivityKey::Patchset(1);

        reg.update(
            key.clone(),
            Phase::Planning {
                attempt: 1,
                max_attempts: 1,
            },
        );
        let since_first = {
            let entries = reg.entries.lock().unwrap();
            entries.get(&key).unwrap().since
        };

        std::thread::sleep(std::time::Duration::from_millis(20));

        reg.update(
            key.clone(),
            Phase::Reviewing {
                attempt: 1,
                max_attempts: 1,
            },
        );
        let since_second = {
            let entries = reg.entries.lock().unwrap();
            entries.get(&key).unwrap().since
        };

        assert!(
            since_second > since_first,
            "a genuinely different activity must restart the clock"
        );
        assert_eq!(
            reg.get(&key).unwrap().phase,
            Phase::Reviewing {
                attempt: 1,
                max_attempts: 1
            }
        );
    }

    #[test]
    fn test_snapshot_lists_all_entries() {
        let reg = ActivityRegistry::new();
        reg.update(
            ActivityKey::Patchset(1),
            Phase::Planning {
                attempt: 1,
                max_attempts: 1,
            },
        );
        reg.update(
            ActivityKey::Commit("deadbeef".to_string()),
            Phase::Fetching {
                remote: "origin".to_string(),
                commits: 3,
            },
        );

        let all = reg.snapshot();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|s| s.key == "patchset:1"));
        assert!(
            all.iter()
                .any(|s| s.description.contains("fetching 3 commit(s) from origin"))
        );
    }

    #[test]
    fn test_attempt_is_shown_only_after_a_retry() {
        // The common case succeeds first time; annotating every review with
        // "(attempt 1/4)" would bury the case actually worth noticing.
        assert_eq!(
            Phase::Reviewing {
                attempt: 1,
                max_attempts: 4
            }
            .describe(),
            "running review stages"
        );
        assert_eq!(
            Phase::Reviewing {
                attempt: 3,
                max_attempts: 4
            }
            .describe(),
            "running review stages (attempt 3/4)"
        );
        assert_eq!(
            Phase::Planning {
                attempt: 2,
                max_attempts: 4
            }
            .describe(),
            "planning stages (attempt 2/4)"
        );
    }

    #[test]
    fn test_retry_restarts_the_activity_clock() {
        let reg = ActivityRegistry::new();
        let key = ActivityKey::Patchset(3);

        reg.update(
            key.clone(),
            Phase::Reviewing {
                attempt: 1,
                max_attempts: 4,
            },
        );
        let first = {
            let entries = reg.entries.lock().unwrap();
            entries.get(&key).unwrap().since
        };

        std::thread::sleep(std::time::Duration::from_millis(20));

        // A retry restarts the whole pipeline, so its clock should start fresh
        // rather than carry the failed attempt's elapsed time.
        reg.update(
            key.clone(),
            Phase::Reviewing {
                attempt: 2,
                max_attempts: 4,
            },
        );
        let second = {
            let entries = reg.entries.lock().unwrap();
            entries.get(&key).unwrap().since
        };

        assert!(second > first);
    }

    #[test]
    fn test_format_duration() {
        // Short durations stay in seconds; a bare count is fine at this scale.
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(119), "119s");

        // Two minutes is the switchover.
        assert_eq!(format_duration(120), "2m 0s");
        assert_eq!(format_duration(137), "2m 17s");
        assert_eq!(format_duration(3599), "59m 59s");

        // Wedged work can sit for a very long time; "1442m 17s" would not help.
        assert_eq!(format_duration(3600), "1h 0m 0s");
        assert_eq!(format_duration(86_537), "24h 2m 17s");
    }

    #[test]
    fn test_phase_descriptions() {
        assert_eq!(
            Phase::Stage {
                stage: 4,
                turn: 7,
                max_turns: 100,
                waiting: StageWait::Model,
            }
            .describe(),
            "stage 4, turn 7/100 (awaiting model)"
        );
        assert_eq!(
            Phase::Stage {
                stage: 4,
                turn: 7,
                max_turns: 100,
                waiting: StageWait::Tools {
                    names: vec!["git_grep".to_string(), "git_show".to_string()]
                },
            }
            .describe(),
            "stage 4, turn 7/100 (running git_grep, git_show)"
        );
        assert_eq!(
            Phase::GitOp {
                op: "git fetch origin".to_string()
            }
            .describe(),
            "running git fetch origin"
        );
    }

    #[test]
    fn test_guard_clears_on_drop() {
        let reg = ActivityRegistry::new();
        let key = ActivityKey::Patchset(9);

        let stage_key = ActivityKey::PatchsetStage {
            patchset_id: 9,
            patch_id: 1,
            stage: 4,
        };

        {
            let guard = PatchsetActivityGuard::new(reg.clone(), 9, Phase::Queued);
            assert_eq!(reg.get(&key).unwrap().phase, Phase::Queued);

            guard.update(Phase::Planning {
                attempt: 1,
                max_attempts: 1,
            });
            assert_eq!(
                reg.get(&key).unwrap().phase,
                Phase::Planning {
                    attempt: 1,
                    max_attempts: 1
                }
            );

            // A stage that never reports finishing must still be cleaned up.
            reg.update(
                stage_key.clone(),
                Phase::Stage {
                    stage: 4,
                    turn: 1,
                    max_turns: 10,
                    waiting: StageWait::Model,
                },
            );
        }

        assert!(
            reg.get(&key).is_none(),
            "guard must clear its entry when dropped, including on early return"
        );
        assert!(
            reg.get(&stage_key).is_none(),
            "guard must clear per-stage entries too, not just the patchset entry"
        );
    }

    #[test]
    fn test_age_and_idle_diverge_across_turns() {
        let reg = ActivityRegistry::new();
        let key = ActivityKey::PatchsetStage {
            patchset_id: 1,
            patch_id: 1,
            stage: 4,
        };

        reg.update(
            key.clone(),
            Phase::Stage {
                stage: 4,
                turn: 1,
                max_turns: 100,
                waiting: StageWait::Model,
            },
        );
        let started = {
            let entries = reg.entries.lock().unwrap();
            entries.get(&key).unwrap().since
        };

        std::thread::sleep(std::time::Duration::from_millis(20));

        // A new turn of the same stage is progress, not a new activity: the
        // stage clock keeps running while the silence clock resets.
        reg.update(
            key.clone(),
            Phase::Stage {
                stage: 4,
                turn: 2,
                max_turns: 100,
                waiting: StageWait::Model,
            },
        );

        let (since, updated) = {
            let entries = reg.entries.lock().unwrap();
            let a = entries.get(&key).unwrap();
            (a.since, a.updated)
        };

        assert_eq!(since, started, "stage start time must survive a new turn");
        assert!(
            updated > since,
            "a new turn must advance `updated` past `since`, otherwise idle_seconds \
             can never differ from age_seconds"
        );
    }

    #[test]
    fn test_different_stage_is_a_new_activity() {
        let reg = ActivityRegistry::new();
        let key = ActivityKey::PatchsetStage {
            patchset_id: 1,
            patch_id: 1,
            stage: 4,
        };

        reg.update(
            key.clone(),
            Phase::Stage {
                stage: 4,
                turn: 1,
                max_turns: 100,
                waiting: StageWait::Model,
            },
        );
        let first = {
            let entries = reg.entries.lock().unwrap();
            entries.get(&key).unwrap().since
        };

        std::thread::sleep(std::time::Duration::from_millis(20));

        reg.update(
            key.clone(),
            Phase::Stage {
                stage: 5,
                turn: 1,
                max_turns: 100,
                waiting: StageWait::Model,
            },
        );
        let second = {
            let entries = reg.entries.lock().unwrap();
            entries.get(&key).unwrap().since
        };

        assert!(
            second > first,
            "a different stage restarts the activity clock"
        );
    }

    #[test]
    fn test_concurrent_stages_get_independent_entries() {
        let reg = ActivityRegistry::new();

        // Component review stages run concurrently. Each must have its own slot,
        // or a wedged stage would be masked by its still-reporting siblings.
        for stage in [3u8, 5, 6] {
            reg.update(
                ActivityKey::PatchsetStage {
                    patchset_id: 7,
                    patch_id: 1,
                    stage,
                },
                Phase::Stage {
                    stage,
                    turn: 1,
                    max_turns: 100,
                    waiting: StageWait::Model,
                },
            );
        }
        reg.update(
            ActivityKey::Patchset(7),
            Phase::Reviewing {
                attempt: 1,
                max_attempts: 1,
            },
        );

        let entries = reg.patchset_snapshot(7);
        assert_eq!(entries.len(), 4);

        // Coarse entry first, then stages in ascending order.
        assert_eq!(entries[0].key, "patchset:7");
        assert_eq!(entries[1].key, "patchset:7/patch:1/stage:3");
        assert_eq!(entries[2].key, "patchset:7/patch:1/stage:5");
        assert_eq!(entries[3].key, "patchset:7/patch:1/stage:6");

        // One stage finishing leaves the others untouched.
        reg.clear(&ActivityKey::PatchsetStage {
            patchset_id: 7,
            patch_id: 1,
            stage: 5,
        });
        let entries = reg.patchset_snapshot(7);
        assert_eq!(entries.len(), 3);
        assert!(
            entries
                .iter()
                .all(|e| e.key != "patchset:7/patch:1/stage:5")
        );
    }

    /// A patchset reviews its patches concurrently, so the same stage number can
    /// be running for two patches at once. Without the patch in the key they
    /// would overwrite each other, and a wedged patch would be masked by a
    /// healthy one sitting at the same stage.
    #[test]
    fn test_concurrent_patches_do_not_share_a_slot() {
        let reg = ActivityRegistry::new();

        for patch_id in [10i64, 11] {
            reg.update(
                ActivityKey::PatchsetStage {
                    patchset_id: 7,
                    patch_id,
                    stage: 3,
                },
                Phase::Stage {
                    stage: 3,
                    turn: 1,
                    max_turns: 100,
                    waiting: StageWait::Model,
                },
            );
        }

        let entries = reg.patchset_snapshot(7);
        assert_eq!(
            entries.len(),
            2,
            "same stage on two patches must occupy two slots, not one"
        );
        assert!(
            entries
                .iter()
                .any(|e| e.key == "patchset:7/patch:10/stage:3")
        );
        assert!(
            entries
                .iter()
                .any(|e| e.key == "patchset:7/patch:11/stage:3")
        );

        // Grouped by patch, so one patch's stages read together.
        reg.update(
            ActivityKey::PatchsetStage {
                patchset_id: 7,
                patch_id: 10,
                stage: 5,
            },
            Phase::Stage {
                stage: 5,
                turn: 1,
                max_turns: 100,
                waiting: StageWait::Model,
            },
        );
        let keys: Vec<String> = reg
            .patchset_snapshot(7)
            .into_iter()
            .map(|e| e.key)
            .collect();
        assert_eq!(
            keys,
            [
                "patchset:7/patch:10/stage:3",
                "patchset:7/patch:10/stage:5",
                "patchset:7/patch:11/stage:3",
            ]
        );

        // Finishing one patch's stage leaves the other patch untouched.
        reg.clear(&ActivityKey::PatchsetStage {
            patchset_id: 7,
            patch_id: 10,
            stage: 3,
        });
        assert_eq!(reg.patchset_snapshot(7).len(), 2);
    }

    #[test]
    fn test_patchset_snapshot_ignores_other_patchsets() {
        let reg = ActivityRegistry::new();
        reg.update(
            ActivityKey::Patchset(1),
            Phase::Reviewing {
                attempt: 1,
                max_attempts: 1,
            },
        );
        reg.update(
            ActivityKey::PatchsetStage {
                patchset_id: 1,
                patch_id: 1,
                stage: 2,
            },
            Phase::Planning {
                attempt: 1,
                max_attempts: 1,
            },
        );
        reg.update(
            ActivityKey::Patchset(2),
            Phase::Reviewing {
                attempt: 1,
                max_attempts: 1,
            },
        );
        reg.update(ActivityKey::Commit("abc".into()), Phase::Queued);

        assert_eq!(reg.patchset_snapshot(1).len(), 2);
        assert_eq!(reg.patchset_snapshot(2).len(), 1);
        assert_eq!(reg.patchset_snapshot(99).len(), 0);

        reg.clear_patchset(1);
        assert_eq!(reg.patchset_snapshot(1).len(), 0);
        assert_eq!(reg.len(), 2, "other patchsets and commits must survive");
    }

    /// The top-level display reads the patchset entry's clock, so it has to span
    /// the patchset. Per-patch phases used to land on this key from every
    /// patch's worker, and because patches plan and review at overlapping times
    /// their phases interleaved — each alternation counted as a new activity and
    /// restarted the clock, so the number shown was the age of whichever patch
    /// moved most recently.
    #[test]
    fn test_the_patchset_clock_survives_its_patches_moving() {
        let reg = ActivityRegistry::new();
        let patchset = ActivityKey::Patchset(7);

        reg.update(patchset.clone(), Phase::ReviewingPatches { patches: 3 });
        let started = {
            let entries = reg.entries.lock().unwrap();
            entries.get(&patchset).unwrap().since
        };

        std::thread::sleep(std::time::Duration::from_millis(20));

        // Three patches, interleaving planning and reviewing, each on its own
        // attempt count. None of this is the patchset changing activity.
        for (patch_id, phase) in [
            (
                10,
                Phase::Planning {
                    attempt: 1,
                    max_attempts: 4,
                },
            ),
            (
                11,
                Phase::Reviewing {
                    attempt: 2,
                    max_attempts: 4,
                },
            ),
            (
                12,
                Phase::Planning {
                    attempt: 1,
                    max_attempts: 4,
                },
            ),
        ] {
            reg.update(
                ActivityKey::PatchsetPatch {
                    patchset_id: 7,
                    patch_id,
                },
                phase,
            );
        }
        // Even the patch count changing as patches finish is the same activity.
        reg.update(patchset.clone(), Phase::ReviewingPatches { patches: 1 });

        let now = {
            let entries = reg.entries.lock().unwrap();
            entries.get(&patchset).unwrap().since
        };
        assert_eq!(
            now, started,
            "the patchset's clock must not restart when a patch moves"
        );

        // Each patch keeps its own entry rather than overwriting a shared one.
        let entries = reg.patchset_snapshot(7);
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].key, "patchset:7");
        assert_eq!(entries[0].description, "reviewing 1 patch");
        assert_eq!(entries[0].patch_id, None);
        assert_eq!(
            entries
                .iter()
                .filter_map(|e| e.patch_id)
                .collect::<Vec<_>>(),
            [10, 11, 12]
        );
    }

    /// The patchset and patch entries change phase a handful of times and then
    /// never again, so their own `updated` stops advancing while the stages
    /// beneath them are busy. Without a sign of life from below, `idle_seconds`
    /// equals `age_seconds` forever and the display reports a healthy review as
    /// making no progress — in red, past five minutes.
    #[test]
    fn test_a_busy_stage_keeps_its_patch_and_patchset_looking_alive() {
        let reg = ActivityRegistry::new();
        let patchset = ActivityKey::Patchset(7);
        let patch = ActivityKey::PatchsetPatch {
            patchset_id: 7,
            patch_id: 10,
        };
        let stage = ActivityKey::PatchsetStage {
            patchset_id: 7,
            patch_id: 10,
            stage: 3,
        };

        reg.update(patchset.clone(), Phase::ReviewingPatches { patches: 1 });
        reg.update(
            patch.clone(),
            Phase::Reviewing {
                attempt: 1,
                max_attempts: 4,
            },
        );

        let read = |key: &ActivityKey| {
            let entries = reg.entries.lock().unwrap();
            let a = entries.get(key).unwrap();
            (a.since, a.updated)
        };
        let (patchset_since, patchset_updated) = read(&patchset);
        let (patch_since, patch_updated) = read(&patch);

        std::thread::sleep(std::time::Duration::from_millis(20));

        // One stage takes a turn. Nothing about the patch or the patchset has
        // changed activity, but both are demonstrably still working.
        reg.update(
            stage,
            Phase::Stage {
                stage: 3,
                turn: 2,
                max_turns: 50,
                waiting: StageWait::Model,
            },
        );

        let (patchset_since_now, patchset_updated_now) = read(&patchset);
        let (patch_since_now, patch_updated_now) = read(&patch);

        assert!(
            patchset_updated_now > patchset_updated,
            "a stage taking a turn must count as progress for the patchset"
        );
        assert!(
            patch_updated_now > patch_updated,
            "and for the patch it belongs to"
        );
        assert_eq!(
            patchset_since_now, patchset_since,
            "but must not restart the clock that spans the review"
        );
        assert_eq!(patch_since_now, patch_since);

        let entries = reg.patchset_snapshot(7);
        assert_eq!(entries[0].key, "patchset:7");
        assert_eq!(
            entries[0].idle_seconds, 0,
            "the patchset is not idle while a stage is turning"
        );
    }

    /// A patchset with nothing running underneath really is idle, and must still
    /// be able to say so — the heartbeat is a sign of life, not a mute button.
    #[test]
    fn test_nothing_underneath_leaves_the_patchset_free_to_look_idle() {
        let reg = ActivityRegistry::new();
        let patchset = ActivityKey::Patchset(7);

        reg.update(patchset.clone(), Phase::ReviewingPatches { patches: 1 });

        // A different patchset's stage is not this one's business.
        reg.update(
            ActivityKey::PatchsetStage {
                patchset_id: 8,
                patch_id: 1,
                stage: 3,
            },
            Phase::Stage {
                stage: 3,
                turn: 1,
                max_turns: 50,
                waiting: StageWait::Model,
            },
        );

        let (since, updated) = {
            let entries = reg.entries.lock().unwrap();
            let a = entries.get(&patchset).unwrap();
            (a.since, a.updated)
        };
        assert_eq!(since, updated, "no work underneath, so no sign of life");
    }

    /// A patch's coarse entry sorts above that patch's stages, so one patch
    /// reads as a block rather than being interleaved with its neighbours.
    #[test]
    fn test_a_patch_entry_sorts_above_its_own_stages() {
        let reg = ActivityRegistry::new();

        for stage in [3u8, 1] {
            reg.update(
                ActivityKey::PatchsetStage {
                    patchset_id: 7,
                    patch_id: 10,
                    stage,
                },
                Phase::Stage {
                    stage,
                    turn: 1,
                    max_turns: 50,
                    waiting: StageWait::Model,
                },
            );
        }
        reg.update(
            ActivityKey::PatchsetPatch {
                patchset_id: 7,
                patch_id: 10,
            },
            Phase::Reviewing {
                attempt: 1,
                max_attempts: 4,
            },
        );
        reg.update(
            ActivityKey::PatchsetPatch {
                patchset_id: 7,
                patch_id: 9,
            },
            Phase::Planning {
                attempt: 1,
                max_attempts: 4,
            },
        );
        reg.update(
            ActivityKey::Patchset(7),
            Phase::ReviewingPatches { patches: 2 },
        );

        let keys: Vec<String> = reg
            .patchset_snapshot(7)
            .into_iter()
            .map(|e| e.key)
            .collect();
        assert_eq!(
            keys,
            [
                "patchset:7",
                "patchset:7/patch:9",
                "patchset:7/patch:10",
                "patchset:7/patch:10/stage:1",
                "patchset:7/patch:10/stage:3",
            ]
        );
    }

    /// The patchset says nothing about attempts, because attempts are counted
    /// per patch and several run at once.
    #[test]
    fn test_reviewing_patches_describes_the_patchset() {
        assert_eq!(
            Phase::ReviewingPatches { patches: 1 }.describe(),
            "reviewing 1 patch"
        );
        assert_eq!(
            Phase::ReviewingPatches { patches: 4 }.describe(),
            "reviewing 4 patches"
        );
    }

    /// Two sides report on a stage and neither knows the whole story. The daemon
    /// proxy knows what a request is blocked on; only the worker knows the turn
    /// numbers. Writing a whole phase from the proxy filled the half it does not
    /// know with `max_turns: 0`, and since that happens twice per request
    /// (queued, then with the model) against one turn report, the placeholder
    /// won nearly every time — so a stage read "starting" for its whole life.
    #[test]
    fn test_a_wait_update_keeps_the_turn_numbers() {
        let reg = ActivityRegistry::new();
        let key = ActivityKey::PatchsetStage {
            patchset_id: 1,
            patch_id: 2,
            stage: 4,
        };

        reg.update(
            key.clone(),
            Phase::Stage {
                stage: 4,
                turn: 7,
                max_turns: 50,
                waiting: StageWait::Model,
            },
        );

        reg.update_stage_wait(key.clone(), 4, StageWait::Queued);
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage 4, turn 7/50 (queued for a model slot)"
        );

        reg.update_stage_wait(key.clone(), 4, StageWait::Model);
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage 4, turn 7/50 (awaiting model)"
        );

        // The worker still owns the numbers, and a later turn advances them.
        reg.update(
            key.clone(),
            Phase::Stage {
                stage: 4,
                turn: 8,
                max_turns: 50,
                waiting: StageWait::Model,
            },
        );
        reg.update_stage_wait(
            key.clone(),
            4,
            StageWait::RateLimited {
                retry_in_seconds: 30,
            },
        );
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage 4, turn 8/50 (rate limited, retrying in 30s)"
        );
    }

    #[test]
    fn test_a_wait_before_any_turn_report_admits_it_does_not_know() {
        // The proxy can reach a request before the worker's turn report crosses
        // the IPC boundary. Guessing a number would be worse than saying so.
        let reg = ActivityRegistry::new();
        let key = ActivityKey::PatchsetStage {
            patchset_id: 1,
            patch_id: 2,
            stage: 4,
        };

        reg.update_stage_wait(key.clone(), 4, StageWait::Queued);
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage 4, starting (queued for a model slot)"
        );

        // And it corrects itself as soon as the worker reports.
        reg.update(
            key.clone(),
            Phase::Stage {
                stage: 4,
                turn: 1,
                max_turns: 50,
                waiting: StageWait::Model,
            },
        );
        reg.update_stage_wait(key.clone(), 4, StageWait::Queued);
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage 4, turn 1/50 (queued for a model slot)"
        );
    }

    /// A failed stage is not a stage waiting on something, so a late wait update
    /// must not resurrect it as running.
    #[test]
    fn test_a_wait_update_does_not_revive_a_failed_stage() {
        let reg = ActivityRegistry::new();
        let key = ActivityKey::PatchsetStage {
            patchset_id: 1,
            patch_id: 2,
            stage: 4,
        };

        reg.update(
            key.clone(),
            Phase::StageFailed {
                stage: 4,
                reason: "Session exceeded max turns limit (50)".to_string(),
                cancelled: false,
            },
        );
        reg.update_stage_wait(key.clone(), 4, StageWait::Model);

        // It does become a Stage again -- the proxy is authoritative about there
        // being a live request -- but with no invented turn numbers behind it.
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage 4, starting (awaiting model)"
        );
    }

    /// A stage used to vanish the moment it succeeded, so a patch's card showed
    /// less and less as its review progressed and the completed work only
    /// reappeared once the whole review ended and the recorded breakdown
    /// replaced the live view.
    #[test]
    fn test_a_finished_stage_stays_beside_the_running_ones() {
        let reg = ActivityRegistry::new();
        let key = |stage| ActivityKey::PatchsetStage {
            patchset_id: 7,
            patch_id: 10,
            stage,
        };

        for stage in [1u8, 2] {
            reg.update(
                key(stage),
                Phase::Stage {
                    stage,
                    turn: 1,
                    max_turns: 50,
                    waiting: StageWait::Model,
                },
            );
        }

        reg.update(
            key(1),
            Phase::StageDone {
                stage: 1,
                seconds: 185,
                turns: 12,
            },
        );

        let entries = reg.patchset_snapshot(7);
        assert_eq!(entries.len(), 2, "the finished stage must keep its slot");
        assert_eq!(entries[0].description, "stage 1 done in 3m 5s, 12 turns");
        assert!(
            !entries[0].description.contains("turn 1/50"),
            "a finished stage must not still claim a turn: {}",
            entries[0].description
        );
        // Its sibling is untouched and still running.
        assert_eq!(
            entries[1].description,
            "stage 2, turn 1/50 (awaiting model)"
        );
    }

    #[test]
    fn test_a_finished_stage_reports_the_numbers_the_breakdown_will_show() {
        let describe = |seconds, turns| {
            Phase::StageDone {
                stage: 4,
                seconds,
                turns,
            }
            .describe()
        };

        assert_eq!(describe(20, 1), "stage 4 done in 20s, 1 turn");
        assert_eq!(describe(185, 12), "stage 4 done in 3m 5s, 12 turns");
        // A stage that reported no turns says nothing rather than "0 turns".
        assert_eq!(describe(3, 0), "stage 4 done in 3s");
    }

    /// A stage that failed used to report nothing, so its entry stayed frozen on
    /// the last turn it managed — claiming to be mid-request when the stage was
    /// already dead. The turn cap is the sharpest version: it fires between
    /// round trips, so the entry read "turn N/N (awaiting model)" with the idle
    /// clock climbing, which is indistinguishable from a hung connection.
    #[test]
    fn test_a_failed_stage_stops_claiming_to_be_running() {
        let reg = ActivityRegistry::new();
        let key = ActivityKey::PatchsetStage {
            patchset_id: 1,
            patch_id: 2,
            stage: 2,
        };

        reg.update(
            key.clone(),
            Phase::Stage {
                stage: 2,
                turn: 50,
                max_turns: 50,
                waiting: StageWait::Model,
            },
        );
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage 2, turn 50/50 (awaiting model)"
        );

        reg.update(
            key.clone(),
            Phase::StageFailed {
                stage: 2,
                reason: "Session exceeded max turns limit (50)".to_string(),
                cancelled: false,
            },
        );

        let snap = reg.get(&key).expect("a failed stage keeps its entry");
        assert_eq!(
            snap.description,
            "stage 2 failed: Session exceeded max turns limit (50)"
        );
        assert!(
            !snap.description.contains("awaiting model"),
            "the entry must stop claiming the stage is mid-request"
        );
        // Kept, not cleared: a stage that vanished from a still-running review
        // would be indistinguishable from one that was never planned.
        assert_eq!(snap.patch_id, Some(2));
        assert_eq!(snap.stage, Some(2));
    }

    #[test]
    fn test_failure_is_a_new_activity_so_the_clock_restarts() {
        let reg = ActivityRegistry::new();
        let key = ActivityKey::PatchsetStage {
            patchset_id: 1,
            patch_id: 2,
            stage: 2,
        };

        reg.update(
            key.clone(),
            Phase::Stage {
                stage: 2,
                turn: 1,
                max_turns: 50,
                waiting: StageWait::Model,
            },
        );
        let running_since = {
            let entries = reg.entries.lock().unwrap();
            entries.get(&key).unwrap().since
        };

        std::thread::sleep(std::time::Duration::from_millis(20));

        reg.update(
            key.clone(),
            Phase::StageFailed {
                stage: 2,
                reason: "boom".to_string(),
                cancelled: false,
            },
        );
        let failed_since = {
            let entries = reg.entries.lock().unwrap();
            entries.get(&key).unwrap().since
        };

        assert!(
            failed_since > running_since,
            "age_seconds must read as how long ago it failed, not how long it ran"
        );
    }

    #[test]
    fn test_cancellation_is_not_reported_as_a_failure() {
        // Cancelling is something the operator did; calling it a failure would
        // send them looking for a fault that is not there.
        assert_eq!(
            Phase::StageFailed {
                stage: 4,
                reason: "Session cancelled by supervisor".to_string(),
                cancelled: true,
            }
            .describe(),
            "stage 4 cancelled: Session cancelled by supervisor"
        );
    }

    #[test]
    fn test_failure_reasons_are_condensed_to_one_line() {
        // Reasons are `{:#}` anyhow chains: multi-line, and long enough to push
        // every other column off the row.
        assert_eq!(
            summarize_reason("Stage 4 failed\n\nCaused by:\n    connection reset"),
            "Stage 4 failed"
        );
        assert_eq!(summarize_reason("   "), "no reason given");
        assert_eq!(summarize_reason(""), "no reason given");

        let long = "x".repeat(400);
        let short = summarize_reason(&long);
        assert!(short.ends_with('…'));
        assert_eq!(short.chars().count(), 141);

        // Truncation is by character, not byte: slicing mid-codepoint panics.
        let wide = "é".repeat(400);
        assert_eq!(summarize_reason(&wide).chars().count(), 141);
    }

    /// Persisted rows keep only the rendered key, so parsing has to be the exact
    /// inverse of `Display` or a restarted daemon loses the patch a stage
    /// belonged to and its activity falls back to the patchset-wide list.
    #[test]
    fn test_key_parse_round_trips() {
        for key in [
            ActivityKey::Patchset(42),
            ActivityKey::PatchsetPatch {
                patchset_id: 7,
                patch_id: 10,
            },
            ActivityKey::PatchsetStage {
                patchset_id: 7,
                patch_id: 10,
                stage: 3,
            },
            ActivityKey::Commit("deadbeef".to_string()),
        ] {
            assert_eq!(ActivityKey::parse(&key.to_string()), Some(key.clone()));
        }

        // The two patch-scoped forms differ only by the stage suffix, so neither
        // may be read as the other.
        assert_eq!(
            ActivityKey::parse("patchset:7/patch:10"),
            Some(ActivityKey::PatchsetPatch {
                patchset_id: 7,
                patch_id: 10
            })
        );

        // Malformed input is not activity, and must not become a wrong key.
        for bad in [
            "",
            "patchset:",
            "patchset:x",
            "commit:",
            "patchset:1/patch:",
            "patchset:1/patch:x",
            "patchset:1/patch:x/stage:3",
            "patchset:1/patch:2/stage:x",
            "patchset:1/stage:3",
            "nonsense",
        ] {
            assert_eq!(ActivityKey::parse(bad), None, "{bad:?} should not parse");
        }
    }

    /// The web UI places stage activity under the patch it describes, so the
    /// patch has to travel with the snapshot rather than be scraped out of the
    /// key string by every consumer.
    #[test]
    fn test_snapshot_carries_patch_and_stage() {
        let reg = ActivityRegistry::new();
        reg.update(
            ActivityKey::PatchsetStage {
                patchset_id: 7,
                patch_id: 10,
                stage: 3,
            },
            Phase::Stage {
                stage: 3,
                turn: 1,
                max_turns: 100,
                waiting: StageWait::Model,
            },
        );
        reg.update(
            ActivityKey::Patchset(7),
            Phase::Reviewing {
                attempt: 1,
                max_attempts: 1,
            },
        );

        let entries = reg.patchset_snapshot(7);
        assert_eq!(entries[0].key, "patchset:7");
        assert_eq!(
            entries[0].patch_id, None,
            "the coarse entry is about the whole review, not one patch"
        );
        assert_eq!(entries[0].stage, None);
        assert_eq!(entries[1].patch_id, Some(10));
        assert_eq!(entries[1].stage, Some(3));

        // Fetch work predates any patch row, so it has neither.
        reg.update(ActivityKey::Commit("abc".into()), Phase::Queued);
        let fetch = reg.get(&ActivityKey::Commit("abc".into())).unwrap();
        assert_eq!(fetch.patch_id, None);
        assert_eq!(fetch.stage, None);
    }

    #[test]
    fn test_serializes_to_json() {
        let reg = ActivityRegistry::new();
        reg.update(
            ActivityKey::Patchset(7),
            Phase::Stage {
                stage: 2,
                turn: 3,
                max_turns: 50,
                waiting: StageWait::Model,
            },
        );

        let json = serde_json::to_value(reg.snapshot()).unwrap();
        assert_eq!(json[0]["phase"]["kind"], "stage");
        assert_eq!(json[0]["phase"]["stage"], 2);
        assert_eq!(json[0]["phase"]["turn"], 3);
    }
}
