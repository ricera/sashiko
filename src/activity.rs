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

use crate::api::ActivityEntry;
use crate::project::ProjectId;
use crate::workflow::StageStopCause;

/// Identifies a unit of tracked work.
///
/// Fetches are keyed by commit rather than patchset id because `FetchRequest`
/// carries no patchset id — the fetch queue batches by repository, not by
/// patchset. Callers correlate a commit back to its patchset through the
/// placeholder message id the patchset was created with
/// (`Database::get_patchset_fetch_commit`).
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
        stage: String,
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
    pub fn stage(&self) -> Option<&str> {
        match self {
            ActivityKey::PatchsetStage { stage, .. } => Some(stage.as_str()),
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
            // An empty stage name is rejected rather than round-tripped: the
            // numeric form this replaced could not produce one.
            Some((_, "")) => None,
            Some((patch, stage)) => Some(ActivityKey::PatchsetStage {
                patchset_id,
                patch_id: patch.parse().ok()?,
                stage: stage.to_string(),
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
    /// Screening the patch before planning, to pick the guides and decide
    /// whether a full review is warranted at all. Reported as the patch's phase
    /// rather than as a stage row, like planning.
    PreScreening { attempt: u32, max_attempts: u32 },
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
        stage: String,
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
    StageDone {
        stage: String,
        seconds: u64,
        turns: u64,
    },
    /// A stage stopped before finishing.
    ///
    /// Recorded rather than cleared. Clearing would make the stage silently
    /// vanish from a review that is still running, and the alternative —
    /// leaving the last [`Phase::Stage`] in place — is worse still: it claims
    /// the stage is mid-turn when it is not. The turn cap is the sharpest case,
    /// because it fires between round trips, so the last thing reported is
    /// "turn N/N, awaiting model", which reads exactly like a hang.
    StageFailed {
        stage: String,
        reason: String,
        cause: StageStopCause,
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

/// Text after this marker in a failure reason is an output-validation
/// violation, and the parse violation quotes the model's raw output
/// (`workflow::output`). That is review text.
const VIOLATION_MARKER: &str = "Last violation:";

/// Reduces a stage failure reason to what may be shown without credentials.
///
/// The activity routes are public and are served for embargoed patchsets too,
/// so the reason they carry must not be able to hold review text. A full
/// `{:#}` chain can: a stage that exhausts its validation attempts reports the
/// last violation, which quotes up to 200 bytes of what the model wrote. The
/// violation is replaced by a fixed note, because what the model said is what
/// the embargo hides, while the fact that its output failed validation is
/// progress metadata. The result is then cut to one line so the stored and
/// served phase is bounded, not just its rendered description.
fn public_reason(reason: &str) -> String {
    match reason.find(VIOLATION_MARKER) {
        Some(at) => summarize_reason(&format!(
            "{} (output failed validation)",
            reason[..at].trim_end().trim_end_matches('.')
        )),
        None => summarize_reason(reason),
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

/// Seconds to report for a backoff of `wait`, rounded up.
///
/// Both the worker (its session's transient retries) and the daemon (its
/// backoff provider) report [`StageWait::RateLimited`], so they share one
/// rounding. Rounded up because truncation turns any sub-second wait into
/// "retrying in 0s", which reads as a retry that is already overdue.
pub fn retry_in_seconds(wait: std::time::Duration) -> u64 {
    wait.as_secs_f64().ceil() as u64
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
            Phase::PreScreening {
                attempt,
                max_attempts,
            } => format!("pre-screening{}", attempt_suffix(*attempt, *max_attempts)),
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
                cause,
            } => format!(
                "stage {} {}: {}",
                stage,
                cause.label(),
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
            (Phase::PreScreening { attempt: a, .. }, Phase::PreScreening { attempt: b, .. }) => {
                a == b
            }
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
///
/// Defined whatever the build, so the registry has one shape; only the task
/// that drains these needs the database and is built with the server.
#[derive(Debug)]
#[cfg_attr(not(feature = "server"), allow(dead_code))]
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
    pub stage: Option<String>,
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
    /// The run that currently owns each patchset's entries, by run number.
    ///
    /// A rerun requested while a review is still going leaves the old review
    /// task running beside the new one, and both write under the same
    /// patchset. Only the newest may clear the patchset when it ends, or the
    /// old task finishing would wipe the new run's live state.
    runs: Mutex<HashMap<i64, u64>>,
    next_run: std::sync::atomic::AtomicU64,
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
    ///
    /// The queue to the writer is unbounded: a sustained database stall grows
    /// it rather than blocking the review, which is the right trade for state
    /// that is advisory. Repeated advances within one activity are throttled by
    /// [`PERSIST_INTERVAL`], so in practice it holds a handful of rows.
    #[cfg(feature = "server")]
    pub fn with_persistence(db: Arc<crate::db::Database>) -> Arc<Self> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PersistOp>();

        tokio::spawn(async move {
            // Fetch rows from a previous run are cleared before anything new
            // is written. They are keyed by commit with no patchset, so no
            // patchset view can serve them, and only the fetch batch that
            // wrote them deletes them; one stranded by a restart would
            // otherwise stay forever. Done by the writer, ahead of its first
            // op, so it cannot race a fetch this run has already reported.
            if let Err(e) = db.delete_fetch_activity().await {
                tracing::debug!("Failed to clear stale fetch activity: {}", e);
            }
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
            persist_tx: Some(tx),
            ..Self::default()
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

    /// Advances a stage's turn numbers without erasing a wait only the daemon
    /// can see.
    ///
    /// The mirror of [`update_stage_wait`](Self::update_stage_wait), and needed
    /// for the same reason. That one keeps the worker's turn numbers because the
    /// daemon does not know them; this one keeps the daemon's queue wait because
    /// the worker does not know about it. The worker reports a turn as "awaiting
    /// model" -- true from where it stands, since it has handed the request over
    /// -- and that report races the daemon's "queued for a slot" on the way to
    /// the registry. Whichever arrived last used to win, and it was usually the
    /// worker, so the queue wait was written and erased a millisecond later.
    pub fn update_stage_turn(
        &self,
        key: ActivityKey,
        stage: String,
        turn: usize,
        max_turns: usize,
    ) {
        let now = Instant::now();
        let (phase, should_persist) = {
            let mut entries = self.entries.lock().unwrap();
            // Only `Queued` is preserved. Every other wait is one the worker
            // itself reported, so a new turn supersedes it.
            let waiting = match entries.get(&key).map(|a| &a.phase) {
                Some(Phase::Stage {
                    waiting: StageWait::Queued,
                    ..
                }) => StageWait::Queued,
                _ => StageWait::Model,
            };
            let phase = Phase::Stage {
                stage,
                turn,
                max_turns,
                waiting,
            };
            let persist = Self::record(&mut entries, &key, phase.clone(), now);
            (phase, persist)
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
    pub fn update_stage_wait(&self, key: ActivityKey, stage: String, wait: StageWait) {
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

    /// All live entries for one patchset, in key order: the patchset's own
    /// entry, then each patch's entry ahead of that patch's stages.
    ///
    /// Stages are ordered by name here, not by pipeline position, because the
    /// registry does not know which project's pipeline it is tracking. Display
    /// order comes from [`sort_for_display`], which does, and which treats live
    /// and persisted entries the same way.
    pub fn patchset_snapshot(&self, patchset_id: i64) -> Vec<ActivitySnapshot> {
        let now = Instant::now();
        let entries = self.entries.lock().unwrap();
        let mut out: Vec<(ActivityKey, ActivitySnapshot)> = entries
            .iter()
            .filter(|(k, _)| k.belongs_to_patchset(patchset_id))
            .map(|(k, a)| (k.clone(), snapshot(k, a, now)))
            .collect();
        out.sort_by(|(a, _), (b, _)| key_order(a).cmp(&key_order(b)));
        out.into_iter().map(|(_, s)| s).collect()
    }

    /// Marks `key` as alive without changing what it is doing.
    ///
    /// For progress that belongs to an entry but is not shown as one of its
    /// own: the pre-screen and the planner report turns like any stage, but
    /// they are the patch's phase rather than stage rows, so a turn of theirs
    /// is a sign of life for the patch. Like the ancestor heartbeat, it is not
    /// persisted -- it changes nothing a restarted daemon would need to say.
    pub fn touch(&self, key: &ActivityKey) {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.get_mut(key) {
            entry.updated = now;
        }
        for ancestor in key.ancestors() {
            if let Some(entry) = entries.get_mut(&ancestor) {
                entry.updated = now;
            }
        }
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

/// Identifies one review run of one patchset, and says whether it is still
/// the newest.
///
/// Cloneable so that work outliving a step of the review, such as its clock,
/// can stop writing once a newer run has taken the patchset over.
#[derive(Clone)]
pub struct PatchsetRun {
    registry: Arc<ActivityRegistry>,
    patchset_id: i64,
    run: u64,
}

impl PatchsetRun {
    pub fn patchset_id(&self) -> i64 {
        self.patchset_id
    }

    /// Whether no newer run of this patchset has started since this one.
    pub fn is_current(&self) -> bool {
        self.registry.runs.lock().unwrap().get(&self.patchset_id) == Some(&self.run)
    }
}

/// Clears its entry when dropped.
///
/// Review tasks have many early-return paths; without a guard, any one of them
/// would leave a stale entry claiming work is still in progress — exactly the
/// misleading state this module exists to prevent.
pub struct PatchsetActivityGuard {
    run: PatchsetRun,
}

impl PatchsetActivityGuard {
    pub fn new(registry: Arc<ActivityRegistry>, patchset_id: i64, phase: Phase) -> Self {
        let run = registry
            .next_run
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        registry.runs.lock().unwrap().insert(patchset_id, run);
        registry.update(ActivityKey::Patchset(patchset_id), phase);
        Self {
            run: PatchsetRun {
                registry,
                patchset_id,
                run,
            },
        }
    }

    pub fn update(&self, phase: Phase) {
        self.run
            .registry
            .update(ActivityKey::Patchset(self.run.patchset_id), phase);
    }

    /// This run's identity, for work that must stop once it is superseded.
    pub fn run(&self) -> PatchsetRun {
        self.run.clone()
    }
}

impl Drop for PatchsetActivityGuard {
    fn drop(&mut self) {
        let PatchsetRun {
            registry,
            patchset_id,
            run,
        } = &self.run;
        // A superseded run leaves the entries alone: they are the newer run's
        // now, and it clears them when it ends.
        {
            let mut runs = registry.runs.lock().unwrap();
            if runs.get(patchset_id) != Some(run) {
                return;
            }
            runs.remove(patchset_id);
        }
        // Clears the per-stage entries too: a stage that died without emitting
        // StageFinished would otherwise linger forever, claiming to be running.
        registry.clear_patchset(*patchset_id);
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

/// Sort key for the registry's own ordering: the patchset, then each patch's
/// entry ahead of its stages, stages by name, and fetch work last.
fn key_order(key: &ActivityKey) -> (u8, i64, u8, &str) {
    match key {
        ActivityKey::Patchset(_) => (0, 0, 0, ""),
        ActivityKey::PatchsetPatch { patch_id, .. } => (1, *patch_id, 0, ""),
        ActivityKey::PatchsetStage {
            patch_id, stage, ..
        } => (1, *patch_id, 1, stage.as_str()),
        ActivityKey::Commit(sha) => (2, 0, 0, sha.as_str()),
    }
}

impl From<ActivitySnapshot> for ActivityEntry {
    fn from(s: ActivitySnapshot) -> Self {
        ActivityEntry {
            key: s.key,
            phase: Some(s.phase),
            description: s.description,
            patch_id: s.patch_id,
            stage: s.stage,
            stage_label: None,
            order: None,
            age_seconds: Some(s.age_seconds),
            idle_seconds: Some(s.idle_seconds),
            updated_at: None,
        }
    }
}

/// Fills in what only the daemon's project can say about each entry: the
/// stage's short label and its rank in the pipeline.
///
/// `order` is `None` for anything that is not a known stage, never a sentinel
/// number. A JSON reader in JavaScript holds numbers as doubles, so a value like
/// `usize::MAX` would arrive as something else; clients sort `None` last.
pub fn annotate(entries: &mut [ActivityEntry], project: ProjectId) {
    for e in entries.iter_mut() {
        let stage = e.stage.as_deref();
        e.stage_label = stage
            .and_then(|s| crate::workflows::stage_short_label(project, s))
            .map(str::to_string);
        e.order = stage.and_then(|s| crate::workflows::stage_order(project, s));
    }
}

/// Gives a review's recorded stage rows the same two fields [`annotate`] gives
/// live entries: `order`, the stage's rank in the pipeline (`null` for a name
/// the project does not know, never a sentinel), and `stage_label`.
///
/// Applied to `stage_durations` and `stage_failures` on their way out of the
/// API, so the page draws recorded and live rows from the same fields and
/// sorts both on the server's order rather than on a table of its own. Fields
/// that are absent or not arrays are left alone, as is any element that is not
/// an object with a string `stage`.
pub fn annotate_stage_records(review: &mut serde_json::Value, project: ProjectId) {
    for field in ["stage_durations", "stage_failures"] {
        let Some(rows) = review.get_mut(field).and_then(|v| v.as_array_mut()) else {
            continue;
        };
        for row in rows.iter_mut() {
            let Some(obj) = row.as_object_mut() else {
                continue;
            };
            let stage = obj
                .get("stage")
                .and_then(|s| s.as_str())
                .map(str::to_string);
            let order = stage
                .as_deref()
                .and_then(|s| crate::workflows::stage_order(project, s));
            let label = stage
                .as_deref()
                .and_then(|s| crate::workflows::stage_short_label(project, s));
            obj.insert("order".to_string(), serde_json::json!(order));
            obj.insert("stage_label".to_string(), serde_json::json!(label));
        }
    }
}

/// Puts entries in the order a reader expects, for live and persisted entries
/// alike: the patchset's own entry first, then each patch's entry ahead of its
/// stages, stages in pipeline order (unknown names after the known ones, by
/// name), and fetch work last.
///
/// Done here, on the daemon, because the order depends on which project's
/// pipeline is running and only the daemon knows that. Clients keep the order
/// they are given, or sort on `order`; neither derives one from the name.
pub fn sort_for_display(entries: &mut [ActivityEntry], project: ProjectId) {
    annotate(entries, project);
    entries.sort_by_cached_key(display_order);
}

fn display_order(e: &ActivityEntry) -> (u8, i64, u8, u32, String) {
    let stage_rank = e.order.unwrap_or(u32::MAX);
    match ActivityKey::parse(&e.key) {
        Some(ActivityKey::Patchset(_)) => (0, 0, 0, 0, String::new()),
        Some(ActivityKey::PatchsetPatch { patch_id, .. }) => (1, patch_id, 0, 0, String::new()),
        Some(ActivityKey::PatchsetStage {
            patch_id, stage, ..
        }) => (1, patch_id, 1, stage_rank, stage),
        Some(ActivityKey::Commit(sha)) => (2, 0, 0, 0, sha),
        // A key this build cannot read is still shown, after everything else.
        None => (3, 0, 0, 0, e.key.clone()),
    }
}

// ---------------------------------------------------------------------------
// Worker-to-daemon progress
// ---------------------------------------------------------------------------

/// The `type` of a progress line on the worker's stdout.
///
/// Both ends of the wire live in this module, with one test over the contract,
/// in the same way `references::handoff_line` and `parse_handoff` do: the worker
/// and the daemon are different processes and different files, and a renamed
/// field would otherwise compile on both sides and silently report nothing.
pub const PROGRESS_MESSAGE: &str = "progress";

/// One review progress event, as it crosses from a daemon-spawned worker to the
/// daemon.
///
/// A single serde enum on both ends, so a stage is a name (`String`) at every
/// step. A payload carrying a stage of any other type fails to decode and is
/// dropped, rather than being read as some other stage.
///
/// `PartialEq` only: a log entry carries an [`crate::ai::AiMessage`], whose
/// tool-call arguments are a JSON value with no `Eq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProgressWire {
    /// The patch's pre-screen began.
    PreScreen,
    /// The patch's planner began.
    Planning,
    /// The plan is settled and the review stages are about to run. Sent whether
    /// or not the planner ran, so a patch reviewed with fixed stages still
    /// moves on from planning.
    PlanReady {
        planned_stages: Vec<String>,
    },
    StageStarted {
        stage: String,
    },
    StageTurn {
        stage: String,
        turn: usize,
        max_turns: usize,
    },
    /// Tools started (non-empty `tools`) or finished (empty).
    StageTools {
        stage: String,
        tools: Vec<String>,
        turn: usize,
        max_turns: usize,
    },
    /// The worker is backing off; `None` once the wait is over.
    StageBackoff {
        stage: String,
        retry_in_seconds: Option<u64>,
        turn: usize,
        max_turns: usize,
    },
    StageFinished {
        stage: String,
    },
    StageFailed {
        stage: String,
        reason: String,
        cause: StageStopCause,
    },
    /// One message joining the review's conversation, for its live log.
    ///
    /// The message is the type the finished log stores, so the live log and
    /// the finished one cannot drift into two shapes. `stage` is a stage name,
    /// or `None` for the system message the log opens with. It is never
    /// activity: [`progress_update`] discards it.
    LogEntry {
        #[serde(default)]
        stage: Option<String>,
        message: crate::ai::AiMessage,
    },
}

/// The worker's side: one JSON line for the daemon.
pub fn progress_line(patch_index: i64, wire: &ProgressWire) -> Option<String> {
    serde_json::to_string(&serde_json::json!({
        "type": PROGRESS_MESSAGE,
        "patch_index": patch_index,
        "payload": wire,
    }))
    .ok()
}

/// The daemon's side: the patch index and the event, or `None` for anything
/// that is not a well-formed progress line. Progress is advisory, so a line
/// that does not decode is dropped rather than allowed to disturb the review.
pub fn parse_progress(msg: &serde_json::Value) -> Option<(i64, ProgressWire)> {
    if msg.get("type")?.as_str()? != PROGRESS_MESSAGE {
        return None;
    }
    let patch_index = msg.get("patch_index")?.as_i64()?;
    let wire = serde_json::from_value(msg.get("payload")?.clone()).ok()?;
    Some((patch_index, wire))
}

/// Recovers the stage name from a request's context tag.
///
/// The stage layer builds the tag as `[ps:N p:M] [s:NAME] `. The daemon sees
/// the request and nothing else, so this is the only thing tying a model call
/// back to the stage that made it.
pub fn stage_from_context_tag(tag: &str) -> Option<String> {
    let start = tag.find("[s:")? + 3;
    let rest = &tag[start..];
    let end = rest.find(']')?;
    let name = rest[..end].trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// What one worker progress event should do to the activity registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressUpdate {
    /// Replace the entry's phase.
    Set(ActivityKey, Phase),
    /// Advance a stage's turn numbers. Routed through
    /// [`ActivityRegistry::update_stage_turn`] so a queue wait only the daemon
    /// can see survives the worker's "awaiting model".
    Turn {
        key: ActivityKey,
        stage: String,
        turn: usize,
        max_turns: usize,
    },
    /// The stage finished. Recorded by the daemon, which alone holds the
    /// stage's start time and turn count.
    Finished { key: ActivityKey, stage: String },
    /// A sign of life for the entry, with no change to what it is doing.
    Touch(ActivityKey),
}

/// Translates a worker progress event into a registry update.
///
/// Stage events are routed to per-stage keys because the review stages run
/// concurrently; sharing one key would make the reported phase flap between
/// whichever stage reported last and hide a wedged stage behind its siblings.
///
/// Only stages shown as their own row get one ([`crate::workflows::reports_stage_row`]).
/// The worker also reports turns for the pre-screen and the planner, because
/// they are counted stages for its own progress bar, but here they are the
/// patch's phase: a row opened for one would have nothing to close it.
pub fn progress_update(
    project: ProjectId,
    patchset_id: i64,
    patch_id: i64,
    wire: &ProgressWire,
    attempt: u32,
    max_attempts: u32,
) -> Option<ProgressUpdate> {
    let patch = ActivityKey::PatchsetPatch {
        patchset_id,
        patch_id,
    };
    let stage_key = |stage: &str| -> Option<ActivityKey> {
        crate::workflows::reports_stage_row(project, stage).then(|| ActivityKey::PatchsetStage {
            patchset_id,
            patch_id,
            stage: stage.to_string(),
        })
    };

    let update = match wire {
        // Per patch, not per patchset: each patch's worker screens and plans
        // its own review, and `attempt` is that patch's retry count. The
        // patchset's own entry is left to describe the patchset.
        ProgressWire::PreScreen => ProgressUpdate::Set(
            patch,
            Phase::PreScreening {
                attempt,
                max_attempts,
            },
        ),
        ProgressWire::Planning => ProgressUpdate::Set(
            patch,
            Phase::Planning {
                attempt,
                max_attempts,
            },
        ),
        ProgressWire::PlanReady { .. } => ProgressUpdate::Set(
            patch,
            Phase::Reviewing {
                attempt,
                max_attempts,
            },
        ),
        ProgressWire::StageStarted { stage } => match stage_key(stage) {
            // No turn yet: `max_turns: 0` renders as "starting".
            Some(key) => ProgressUpdate::Turn {
                key,
                stage: stage.clone(),
                turn: 0,
                max_turns: 0,
            },
            None => ProgressUpdate::Touch(patch),
        },
        // A turn beginning means the request has gone to the model, as far as
        // the worker can tell.
        ProgressWire::StageTurn {
            stage,
            turn,
            max_turns,
        } => match stage_key(stage) {
            Some(key) => ProgressUpdate::Turn {
                key,
                stage: stage.clone(),
                turn: *turn,
                max_turns: *max_turns,
            },
            None => ProgressUpdate::Touch(patch),
        },
        // The model answered and the worker is running git. An empty list means
        // the tools finished and the turn is back with the model -- which may
        // already be queued for a slot, so that goes through the turn path.
        ProgressWire::StageTools {
            stage,
            tools,
            turn,
            max_turns,
        } => match stage_key(stage) {
            Some(key) if tools.is_empty() => ProgressUpdate::Turn {
                key,
                stage: stage.clone(),
                turn: *turn,
                max_turns: *max_turns,
            },
            Some(key) => ProgressUpdate::Set(
                key,
                Phase::Stage {
                    stage: stage.clone(),
                    turn: *turn,
                    max_turns: *max_turns,
                    waiting: StageWait::Tools {
                        names: tools.clone(),
                    },
                },
            ),
            None => ProgressUpdate::Touch(patch),
        },
        // A rate limit with no Retry-After header sleeps a flat 60s, which is
        // otherwise indistinguishable from a very slow model.
        ProgressWire::StageBackoff {
            stage,
            retry_in_seconds,
            turn,
            max_turns,
        } => match (stage_key(stage), retry_in_seconds) {
            (Some(key), Some(secs)) => ProgressUpdate::Set(
                key,
                Phase::Stage {
                    stage: stage.clone(),
                    turn: *turn,
                    max_turns: *max_turns,
                    waiting: StageWait::RateLimited {
                        retry_in_seconds: *secs,
                    },
                },
            ),
            (Some(key), None) => ProgressUpdate::Turn {
                key,
                stage: stage.clone(),
                turn: *turn,
                max_turns: *max_turns,
            },
            (None, _) => ProgressUpdate::Touch(patch),
        },
        ProgressWire::StageFinished { stage } => match stage_key(stage) {
            Some(key) => ProgressUpdate::Finished {
                key,
                stage: stage.clone(),
            },
            None => ProgressUpdate::Touch(patch),
        },
        // A failed stage is relabelled, not cleared: the review carries on
        // without it, and a stage that silently disappeared would be
        // indistinguishable from one that never ran. Replacing the phase is
        // what stops the entry claiming to be mid-turn.
        ProgressWire::StageFailed {
            stage,
            reason,
            cause,
        } => match stage_key(stage) {
            Some(key) => ProgressUpdate::Set(
                key,
                Phase::StageFailed {
                    stage: stage.clone(),
                    reason: public_reason(reason),
                    cause: *cause,
                },
            ),
            None => ProgressUpdate::Touch(patch),
        },
        // Conversation text never enters the registry. /api/activity and
        // /api/patchset/activity read from it, and both are public and served
        // for embargoed patchsets too, while the conversation is transcript:
        // it goes to the review's live log, which is served only where the
        // finished log already is.
        ProgressWire::LogEntry { .. } => return None,
    };
    Some(update)
}

/// How long one stage of one review ran, and how many turns it needed.
///
/// Stored as JSON in `reviews.stage_durations`. The stage is its name: there
/// is no numeric stage identity anywhere on this path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageTiming {
    pub stage: String,
    pub seconds: u64,
    pub turns: u64,
}

/// Applies decoded progress to the registry, and keeps the per-stage timing
/// that only the daemon can measure.
///
/// Derived from the progress events the worker already emits, rather than
/// timed inside the worker, so every stage is covered the same way, including
/// the consolidation stages that run outside the concurrent block.
pub struct StageTracker {
    activity: Arc<ActivityRegistry>,
    /// When each stage first reported, which is when its clock starts.
    starts: HashMap<String, Instant>,
    /// Highest turn seen per stage: how many round trips it needed.
    turns: HashMap<String, u64>,
    finished: Vec<StageTiming>,
}

impl StageTracker {
    pub fn new(activity: Arc<ActivityRegistry>) -> Self {
        Self {
            activity,
            starts: HashMap::new(),
            turns: HashMap::new(),
            finished: Vec::new(),
        }
    }

    pub fn apply(&mut self, update: ProgressUpdate) {
        match update {
            ProgressUpdate::Set(key, phase) => self.activity.update(key, phase),
            ProgressUpdate::Turn {
                key,
                stage,
                turn,
                max_turns,
            } => {
                self.starts
                    .entry(stage.clone())
                    .or_insert_with(Instant::now);
                let seen = self.turns.entry(stage.clone()).or_insert(0);
                *seen = (*seen).max(turn as u64);
                self.activity.update_stage_turn(key, stage, turn, max_turns);
            }
            ProgressUpdate::Finished { key, stage } => {
                // Marked done rather than removed. Dropping it made a patch's
                // card show less and less as the review went on. Carries the
                // numbers the recorded breakdown will show, so the row settles
                // in place.
                match self.starts.remove(&stage) {
                    Some(started) => {
                        let seconds = started.elapsed().as_secs();
                        let turns = self.turns.remove(&stage).unwrap_or(0);
                        self.finished.push(StageTiming {
                            stage: stage.clone(),
                            seconds,
                            turns,
                        });
                        self.activity.update(
                            key,
                            Phase::StageDone {
                                stage,
                                seconds,
                                turns,
                            },
                        );
                    }
                    // No start time means nothing true to say about it, and a
                    // row saying only "done" is worse than none.
                    None => self.activity.clear(&key),
                }
            }
            ProgressUpdate::Touch(key) => self.activity.touch(&key),
        }
    }

    /// Every stage that finished, in the order they finished.
    pub fn timings(&self) -> &[StageTiming] {
        &self.finished
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
        stage: key.stage().map(str::to_string),
        age_seconds: now.duration_since(activity.since).as_secs(),
        idle_seconds: now.duration_since(activity.updated).as_secs(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A patchset's live entries as the API serves them.
    fn displayed(
        reg: &ActivityRegistry,
        patchset_id: i64,
        project: ProjectId,
    ) -> Vec<ActivityEntry> {
        let mut entries: Vec<ActivityEntry> = reg
            .patchset_snapshot(patchset_id)
            .into_iter()
            .map(Into::into)
            .collect();
        sort_for_display(&mut entries, project);
        entries
    }

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
            stage: "execution-flow".to_string(),
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
                stage: "resources".to_string(),
                turn: 7,
                max_turns: 100,
                waiting: StageWait::Model,
            }
            .describe(),
            "stage resources, turn 7/100 (awaiting model)"
        );
        assert_eq!(
            Phase::Stage {
                stage: "resources".to_string(),
                turn: 7,
                max_turns: 100,
                waiting: StageWait::Tools {
                    names: vec!["git_grep".to_string(), "git_show".to_string()]
                },
            }
            .describe(),
            "stage resources, turn 7/100 (running git_grep, git_show)"
        );
        assert_eq!(
            Phase::GitOp {
                op: "git fetch origin".to_string()
            }
            .describe(),
            "running git fetch origin"
        );
    }

    /// A rerun requested mid-review starts a second task for the same
    /// patchset while the first is still unwinding. The first one ending must
    /// not wipe what the second is reporting.
    #[test]
    fn a_superseded_run_leaves_the_newer_runs_entries_alone() {
        let reg = ActivityRegistry::new();
        let stage_key = ActivityKey::PatchsetStage {
            patchset_id: 9,
            patch_id: 1,
            stage: "resources".to_string(),
        };

        let old = PatchsetActivityGuard::new(reg.clone(), 9, Phase::Queued);
        let old_run = old.run();
        assert!(old_run.is_current());

        let new = PatchsetActivityGuard::new(reg.clone(), 9, Phase::Queued);
        assert!(!old_run.is_current(), "the rerun supersedes the old run");
        assert!(new.run().is_current());
        reg.update(
            stage_key.clone(),
            Phase::Stage {
                stage: "resources".to_string(),
                turn: 1,
                max_turns: 10,
                waiting: StageWait::Model,
            },
        );

        drop(old);
        assert!(reg.get(&ActivityKey::Patchset(9)).is_some());
        assert!(reg.get(&stage_key).is_some());

        drop(new);
        assert!(reg.get(&ActivityKey::Patchset(9)).is_none());
        assert!(reg.get(&stage_key).is_none());
        assert!(!old_run.is_current());
    }

    #[test]
    fn test_guard_clears_on_drop() {
        let reg = ActivityRegistry::new();
        let key = ActivityKey::Patchset(9);

        let stage_key = ActivityKey::PatchsetStage {
            patchset_id: 9,
            patch_id: 1,
            stage: "resources".to_string(),
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
                    stage: "resources".to_string(),
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
            stage: "resources".to_string(),
        };

        reg.update(
            key.clone(),
            Phase::Stage {
                stage: "resources".to_string(),
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
                stage: "resources".to_string(),
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
            stage: "resources".to_string(),
        };

        reg.update(
            key.clone(),
            Phase::Stage {
                stage: "resources".to_string(),
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
                stage: "locking".to_string(),
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
        for stage in ["execution-flow", "locking", "security"] {
            reg.update(
                ActivityKey::PatchsetStage {
                    patchset_id: 7,
                    patch_id: 1,
                    stage: stage.to_string(),
                },
                Phase::Stage {
                    stage: stage.to_string(),
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
        assert_eq!(entries[1].key, "patchset:7/patch:1/stage:execution-flow");
        assert_eq!(entries[2].key, "patchset:7/patch:1/stage:locking");
        assert_eq!(entries[3].key, "patchset:7/patch:1/stage:security");

        // One stage finishing leaves the others untouched.
        reg.clear(&ActivityKey::PatchsetStage {
            patchset_id: 7,
            patch_id: 1,
            stage: "locking".to_string(),
        });
        let entries = reg.patchset_snapshot(7);
        assert_eq!(entries.len(), 3);
        assert!(
            entries
                .iter()
                .all(|e| e.key != "patchset:7/patch:1/stage:locking")
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
                    stage: "execution-flow".to_string(),
                },
                Phase::Stage {
                    stage: "execution-flow".to_string(),
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
                .any(|e| e.key == "patchset:7/patch:10/stage:execution-flow")
        );
        assert!(
            entries
                .iter()
                .any(|e| e.key == "patchset:7/patch:11/stage:execution-flow")
        );

        // Grouped by patch, so one patch's stages read together.
        reg.update(
            ActivityKey::PatchsetStage {
                patchset_id: 7,
                patch_id: 10,
                stage: "locking".to_string(),
            },
            Phase::Stage {
                stage: "locking".to_string(),
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
                "patchset:7/patch:10/stage:execution-flow",
                "patchset:7/patch:10/stage:locking",
                "patchset:7/patch:11/stage:execution-flow",
            ]
        );

        // Finishing one patch's stage leaves the other patch untouched.
        reg.clear(&ActivityKey::PatchsetStage {
            patchset_id: 7,
            patch_id: 10,
            stage: "execution-flow".to_string(),
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
                stage: "implementation".to_string(),
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
            stage: "execution-flow".to_string(),
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
                stage: "execution-flow".to_string(),
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
                stage: "execution-flow".to_string(),
            },
            Phase::Stage {
                stage: "execution-flow".to_string(),
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

        for stage in ["execution-flow", "goal"] {
            reg.update(
                ActivityKey::PatchsetStage {
                    patchset_id: 7,
                    patch_id: 10,
                    stage: stage.to_string(),
                },
                Phase::Stage {
                    stage: stage.to_string(),
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

        let keys: Vec<String> = displayed(&reg, 7, ProjectId::Linux)
            .into_iter()
            .map(|e| e.key)
            .collect();
        // Stages in pipeline order, which only the project's table knows:
        // goal runs before execution-flow, though it sorts after it by name.
        assert_eq!(
            keys,
            [
                "patchset:7",
                "patchset:7/patch:9",
                "patchset:7/patch:10",
                "patchset:7/patch:10/stage:goal",
                "patchset:7/patch:10/stage:execution-flow",
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

    /// The other half of the same split: a turn report must not erase a queue
    /// wait.
    ///
    /// The worker reports every turn as "awaiting model", which is true from
    /// where it stands -- it has handed the request over and cannot see that the
    /// daemon is holding it behind a full semaphore. That report reaches the
    /// registry a millisecond after the daemon says "queued", so writing a whole
    /// phase meant the queue wait was recorded and immediately overwritten. It
    /// was never once visible in the UI.
    #[test]
    fn test_a_turn_report_does_not_erase_a_queue_wait() {
        let reg = ActivityRegistry::new();
        let key = ActivityKey::PatchsetStage {
            patchset_id: 1,
            patch_id: 2,
            stage: "locking".to_string(),
        };

        // The daemon sees the request queue before the worker's turn report
        // arrives, which is the order these actually turn up in.
        reg.update_stage_wait(key.clone(), "locking".to_string(), StageWait::Queued);
        reg.update_stage_turn(key.clone(), "locking".to_string(), 1, 100);

        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage locking, turn 1/100 (queued for a model slot)",
            "the turn numbers must land without the wait being lost"
        );

        // Only the daemon can clear it, because only the daemon knows the slot
        // was granted.
        reg.update_stage_wait(key.clone(), "locking".to_string(), StageWait::Model);
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage locking, turn 1/100 (awaiting model)"
        );

        // A later turn with no wait outstanding reads as the model's, not as a
        // queue wait that never got cleared.
        reg.update_stage_turn(key.clone(), "locking".to_string(), 2, 100);
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage locking, turn 2/100 (awaiting model)"
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
            stage: "resources".to_string(),
        };

        reg.update(
            key.clone(),
            Phase::Stage {
                stage: "resources".to_string(),
                turn: 7,
                max_turns: 50,
                waiting: StageWait::Model,
            },
        );

        reg.update_stage_wait(key.clone(), "resources".to_string(), StageWait::Queued);
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage resources, turn 7/50 (queued for a model slot)"
        );

        reg.update_stage_wait(key.clone(), "resources".to_string(), StageWait::Model);
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage resources, turn 7/50 (awaiting model)"
        );

        // The worker still owns the numbers, and a later turn advances them.
        reg.update(
            key.clone(),
            Phase::Stage {
                stage: "resources".to_string(),
                turn: 8,
                max_turns: 50,
                waiting: StageWait::Model,
            },
        );
        reg.update_stage_wait(
            key.clone(),
            "resources".to_string(),
            StageWait::RateLimited {
                retry_in_seconds: 30,
            },
        );
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage resources, turn 8/50 (rate limited, retrying in 30s)"
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
            stage: "resources".to_string(),
        };

        reg.update_stage_wait(key.clone(), "resources".to_string(), StageWait::Queued);
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage resources, starting (queued for a model slot)"
        );

        // And it corrects itself as soon as the worker reports.
        reg.update(
            key.clone(),
            Phase::Stage {
                stage: "resources".to_string(),
                turn: 1,
                max_turns: 50,
                waiting: StageWait::Model,
            },
        );
        reg.update_stage_wait(key.clone(), "resources".to_string(), StageWait::Queued);
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage resources, turn 1/50 (queued for a model slot)"
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
            stage: "resources".to_string(),
        };

        reg.update(
            key.clone(),
            Phase::StageFailed {
                stage: "resources".to_string(),
                reason: "Session exceeded max turns limit (50)".to_string(),
                cause: StageStopCause::Error,
            },
        );
        reg.update_stage_wait(key.clone(), "resources".to_string(), StageWait::Model);

        // It does become a Stage again -- the proxy is authoritative about there
        // being a live request -- but with no invented turn numbers behind it.
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage resources, starting (awaiting model)"
        );
    }

    /// A stage used to vanish the moment it succeeded, so a patch's card showed
    /// less and less as its review progressed and the completed work only
    /// reappeared once the whole review ended and the recorded breakdown
    /// replaced the live view.
    #[test]
    fn test_a_finished_stage_stays_beside_the_running_ones() {
        let reg = ActivityRegistry::new();
        let key = |stage: &str| ActivityKey::PatchsetStage {
            patchset_id: 7,
            patch_id: 10,
            stage: stage.to_string(),
        };

        for stage in ["goal", "implementation"] {
            reg.update(
                key(stage),
                Phase::Stage {
                    stage: stage.to_string(),
                    turn: 1,
                    max_turns: 50,
                    waiting: StageWait::Model,
                },
            );
        }

        reg.update(
            key("goal"),
            Phase::StageDone {
                stage: "goal".to_string(),
                seconds: 185,
                turns: 12,
            },
        );

        let entries = reg.patchset_snapshot(7);
        assert_eq!(entries.len(), 2, "the finished stage must keep its slot");
        assert_eq!(entries[0].description, "stage goal done in 3m 5s, 12 turns");
        assert!(
            !entries[0].description.contains("turn 1/50"),
            "a finished stage must not still claim a turn: {}",
            entries[0].description
        );
        // Its sibling is untouched and still running.
        assert_eq!(
            entries[1].description,
            "stage implementation, turn 1/50 (awaiting model)"
        );
    }

    #[test]
    fn test_a_finished_stage_reports_the_numbers_the_breakdown_will_show() {
        let describe = |seconds, turns| {
            Phase::StageDone {
                stage: "resources".to_string(),
                seconds,
                turns,
            }
            .describe()
        };

        assert_eq!(describe(20, 1), "stage resources done in 20s, 1 turn");
        assert_eq!(describe(185, 12), "stage resources done in 3m 5s, 12 turns");
        // A stage that reported no turns says nothing rather than "0 turns".
        assert_eq!(describe(3, 0), "stage resources done in 3s");
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
            stage: "implementation".to_string(),
        };

        reg.update(
            key.clone(),
            Phase::Stage {
                stage: "implementation".to_string(),
                turn: 50,
                max_turns: 50,
                waiting: StageWait::Model,
            },
        );
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage implementation, turn 50/50 (awaiting model)"
        );

        reg.update(
            key.clone(),
            Phase::StageFailed {
                stage: "implementation".to_string(),
                reason: "Session exceeded max turns limit (50)".to_string(),
                cause: StageStopCause::Error,
            },
        );

        let snap = reg.get(&key).expect("a failed stage keeps its entry");
        assert_eq!(
            snap.description,
            "stage implementation failed: Session exceeded max turns limit (50)"
        );
        assert!(
            !snap.description.contains("awaiting model"),
            "the entry must stop claiming the stage is mid-request"
        );
        // Kept, not cleared: a stage that vanished from a still-running review
        // would be indistinguishable from one that was never planned.
        assert_eq!(snap.patch_id, Some(2));
        assert_eq!(snap.stage, Some("implementation".to_string()));
    }

    #[test]
    fn test_failure_is_a_new_activity_so_the_clock_restarts() {
        let reg = ActivityRegistry::new();
        let key = ActivityKey::PatchsetStage {
            patchset_id: 1,
            patch_id: 2,
            stage: "implementation".to_string(),
        };

        reg.update(
            key.clone(),
            Phase::Stage {
                stage: "implementation".to_string(),
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
                stage: "implementation".to_string(),
                reason: "boom".to_string(),
                cause: StageStopCause::Error,
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
                stage: "resources".to_string(),
                reason: "Session cancelled by supervisor".to_string(),
                cause: StageStopCause::Cancelled,
            }
            .describe(),
            "stage resources cancelled: Session cancelled by supervisor"
        );
    }

    /// The description is what the CLI and the public activity routes show,
    /// so each cause reads with the label every other view uses for it.
    #[test]
    fn test_a_stopped_stage_is_described_by_its_cause() {
        for (cause, expected) in [
            (StageStopCause::Error, "stage goal failed: why"),
            (StageStopCause::Cancelled, "stage goal cancelled: why"),
            (
                StageStopCause::OutOfTime,
                "stage goal stopped: out of time: why",
            ),
            (StageStopCause::CutShort, "stage goal cut short: why"),
            (
                StageStopCause::Interrupted,
                "stage goal still running when the review stopped: why",
            ),
        ] {
            let phase = Phase::StageFailed {
                stage: "goal".to_string(),
                reason: "why".to_string(),
                cause,
            };
            assert_eq!(phase.describe(), expected);
        }
    }

    /// A failed stage's phase is stored and read back after a restart, so the
    /// cause is written in the spelling the page reads.
    #[test]
    fn test_a_stored_stage_failure_names_its_cause() {
        let phase = Phase::StageFailed {
            stage: "goal".to_string(),
            reason: "why".to_string(),
            cause: StageStopCause::OutOfTime,
        };
        let json = serde_json::to_value(&phase).unwrap();
        assert_eq!(json["cause"], "out_of_time", "{json}");
        assert!(json.get("cancelled").is_none(), "{json}");
        assert_eq!(serde_json::from_value::<Phase>(json).unwrap(), phase);
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
                stage: "execution-flow".to_string(),
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
            "patchset:1/patch:x/stage:execution-flow",
            // Any non-empty stage is a name now, so only an absent one is bad.
            "patchset:1/patch:2/stage:",
            "patchset:1/stage:execution-flow",
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
                stage: "execution-flow".to_string(),
            },
            Phase::Stage {
                stage: "execution-flow".to_string(),
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
        assert_eq!(entries[1].stage, Some("execution-flow".to_string()));

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
                stage: "implementation".to_string(),
                turn: 3,
                max_turns: 50,
                waiting: StageWait::Model,
            },
        );

        let json = serde_json::to_value(reg.snapshot()).unwrap();
        assert_eq!(json[0]["phase"]["kind"], "stage");
        assert_eq!(json[0]["phase"]["stage"], "implementation");
        assert_eq!(json[0]["phase"]["turn"], 3);
    }

    /// Commit B's page and the CLI switch on these strings, and persisted rows
    /// keep them across builds. Every kind and every wait is pinned, so a
    /// rename shows up here rather than as a blank cell in a browser.
    #[test]
    fn test_every_phase_kind_and_wait_is_pinned() {
        let kind = |p: Phase| serde_json::to_value(p).unwrap()["kind"].clone();
        let stage = |waiting| Phase::Stage {
            stage: "goal".into(),
            turn: 1,
            max_turns: 2,
            waiting,
        };
        let pairs = [
            (Phase::Queued, "queued"),
            (Phase::GitOp { op: "x".into() }, "git_op"),
            (
                Phase::Fetching {
                    remote: "r".into(),
                    commits: 1,
                },
                "fetching",
            ),
            (
                Phase::PreScreening {
                    attempt: 1,
                    max_attempts: 1,
                },
                "pre_screening",
            ),
            (
                Phase::Planning {
                    attempt: 1,
                    max_attempts: 1,
                },
                "planning",
            ),
            (
                Phase::Reviewing {
                    attempt: 1,
                    max_attempts: 1,
                },
                "reviewing",
            ),
            (Phase::ReviewingPatches { patches: 1 }, "reviewing_patches"),
            (stage(StageWait::Model), "stage"),
            (
                Phase::StageDone {
                    stage: "goal".into(),
                    seconds: 1,
                    turns: 1,
                },
                "stage_done",
            ),
            (
                Phase::StageFailed {
                    stage: "goal".into(),
                    reason: "r".into(),
                    cause: StageStopCause::Error,
                },
                "stage_failed",
            ),
        ];
        for (phase, expected) in pairs {
            let back: Phase =
                serde_json::from_value(serde_json::to_value(&phase).unwrap()).unwrap();
            assert_eq!(back, phase, "{expected} must round-trip");
            assert_eq!(kind(phase), expected);
        }

        // The page's describeStageWait switches on exactly these four strings,
        // and the UI verifier feeds it exactly these; a rename here must fail.
        let on = |w: StageWait| serde_json::to_value(stage(w)).unwrap()["waiting"]["on"].clone();
        assert_eq!(on(StageWait::Model), "model");
        assert_eq!(on(StageWait::Queued), "queued");
        assert_eq!(
            on(StageWait::RateLimited {
                retry_in_seconds: 3
            }),
            "rate_limited"
        );
        assert_eq!(on(StageWait::Tools { names: vec![] }), "tools");
    }

    #[test]
    fn test_pre_screening_is_its_own_activity_per_attempt() {
        let first = Phase::PreScreening {
            attempt: 1,
            max_attempts: 4,
        };
        assert_eq!(first.describe(), "pre-screening");
        assert_eq!(
            Phase::PreScreening {
                attempt: 2,
                max_attempts: 4
            }
            .describe(),
            "pre-screening (attempt 2/4)"
        );
        assert!(first.same_activity(&first.clone()));
        assert!(!first.same_activity(&Phase::PreScreening {
            attempt: 2,
            max_attempts: 4
        }));
        assert!(!first.same_activity(&Phase::Planning {
            attempt: 1,
            max_attempts: 4
        }));
    }

    /// Stage keys are split on `/stage:`, so a stage name with a slash in it
    /// would not survive a restart. Holds for every stage of both projects.
    #[test]
    fn test_no_stage_name_can_break_its_key() {
        use crate::workflows::{linux_patch_review as linux, sashiko_patch_review as sashiko};
        let names = linux::ANALYSIS_STAGES
            .iter()
            .map(|s| s.name)
            .chain(linux::CONSOLIDATION_STAGES.iter().map(|s| s.name))
            .chain(sashiko::ANALYSIS_STAGES.iter().map(|s| s.name))
            .chain(sashiko::CONSOLIDATION_STAGES.iter().map(|s| s.name));
        for name in names {
            assert!(!name.contains('/'), "{name}");
            let key = ActivityKey::PatchsetStage {
                patchset_id: 1,
                patch_id: 2,
                stage: name.to_string(),
            };
            assert_eq!(ActivityKey::parse(&key.to_string()), Some(key));
        }
    }

    fn all_wire_events() -> Vec<ProgressWire> {
        vec![
            ProgressWire::PreScreen,
            ProgressWire::Planning,
            ProgressWire::PlanReady {
                planned_stages: vec!["goal".into(), "report".into()],
            },
            ProgressWire::StageStarted {
                stage: "goal".into(),
            },
            ProgressWire::StageTurn {
                stage: "goal".into(),
                turn: 3,
                max_turns: 20,
            },
            ProgressWire::StageTools {
                stage: "goal".into(),
                tools: vec!["git_grep".into()],
                turn: 3,
                max_turns: 20,
            },
            ProgressWire::StageBackoff {
                stage: "goal".into(),
                retry_in_seconds: Some(60),
                turn: 3,
                max_turns: 20,
            },
            ProgressWire::StageFinished {
                stage: "goal".into(),
            },
            ProgressWire::StageFailed {
                stage: "goal".into(),
                reason: "Session exceeded max turns limit (20)".into(),
                cause: StageStopCause::Error,
            },
        ]
    }

    /// Both ends of the wire, through the same text a worker writes.
    #[test]
    fn test_every_progress_event_survives_the_wire() {
        for wire in all_wire_events() {
            let line = progress_line(4, &wire).expect("encodes");
            let msg: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(msg["type"], PROGRESS_MESSAGE);
            assert_eq!(parse_progress(&msg), Some((4, wire.clone())), "{line}");
            assert!(
                progress_update(ProjectId::Linux, 7, 70, &wire, 1, 4).is_some(),
                "{line}"
            );
        }

        // The kind strings are the contract, not an accident of the enum.
        let kinds: Vec<String> = all_wire_events()
            .iter()
            .map(|w| {
                serde_json::to_value(w).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "pre_screen",
                "planning",
                "plan_ready",
                "stage_started",
                "stage_turn",
                "stage_tools",
                "stage_backoff",
                "stage_finished",
                "stage_failed",
            ]
        );
    }

    fn log_message() -> crate::ai::AiMessage {
        crate::ai::AiMessage {
            role: crate::ai::AiRole::Assistant,
            content: Some("I will look at the locking".to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: Some(vec![crate::ai::ToolCall {
                id: "c0".to_string(),
                function_name: "git_grep".to_string(),
                arguments: serde_json::json!({"pattern": "spin_lock"}),
                thought_signature: None,
            }]),
            tool_call_id: None,
        }
    }

    /// A log entry crosses the same wire as progress, as kind `log_entry`, and
    /// comes back whole. It is never activity: the activity routes are public,
    /// and a conversation is transcript.
    #[test]
    fn test_a_log_entry_survives_the_wire_and_never_becomes_activity() {
        for stage in [Some("locking".to_string()), None] {
            let wire = ProgressWire::LogEntry {
                stage: stage.clone(),
                message: log_message(),
            };
            let line = progress_line(2, &wire).expect("encodes");
            let msg: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(msg["payload"]["kind"], "log_entry");
            assert_eq!(msg["payload"]["message"]["role"], "assistant");
            assert_eq!(
                msg["payload"]["message"]["tool_calls"][0]["function_name"],
                "git_grep"
            );
            assert_eq!(parse_progress(&msg), Some((2, wire.clone())), "{line}");
            assert_eq!(
                progress_update(ProjectId::Linux, 7, 70, &wire, 1, 4),
                None,
                "conversation text must not reach the registry"
            );
        }

        // A system message has no stage, and may say so by leaving it out.
        let no_stage = serde_json::json!({
            "type": "progress",
            "patch_index": 1,
            "payload": {"kind": "log_entry", "message": {"role": "system", "content": "s"}},
        });
        assert!(matches!(
            parse_progress(&no_stage),
            Some((1, ProgressWire::LogEntry { stage: None, .. }))
        ));
    }

    /// A stage is a name on this wire too, and a message is an `AiMessage`;
    /// anything else is dropped rather than stored as something it is not.
    #[test]
    fn test_a_malformed_log_entry_is_dropped() {
        let message = serde_json::json!({"role": "user", "content": "hi"});
        for payload in [
            serde_json::json!({"kind": "log_entry", "stage": 5, "message": message}),
            serde_json::json!({"kind": "log_entry", "stage": "goal",
                "message": {"role": "narrator", "content": "hi"}}),
            serde_json::json!({"kind": "log_entry", "stage": "goal"}),
            serde_json::json!({"kind": "log_entry", "stage": "goal", "message": "hi"}),
        ] {
            let msg = serde_json::json!({"type": "progress", "patch_index": 1, "payload": payload});
            assert_eq!(parse_progress(&msg), None, "{msg}");
        }
    }

    /// Recorded stage rows carry the same order and label live entries do, so
    /// the page sorts both on one field from one source.
    #[test]
    fn test_recorded_stage_rows_are_annotated_like_live_ones() {
        let mut review = serde_json::json!({
            "stage_durations": [
                {"stage": "locking", "seconds": 10, "turns": 2},
                {"stage": "no-such-stage", "seconds": 1, "turns": 1},
            ],
            "stage_failures": [
                {"stage": "goal", "reason": "x", "cause": "error"},
            ],
        });
        annotate_stage_records(&mut review, ProjectId::Linux);

        let locking = &review["stage_durations"][0];
        let expected = crate::workflows::stage_order(ProjectId::Linux, "locking");
        assert!(expected.is_some());
        assert_eq!(locking["order"], serde_json::json!(expected));
        assert_eq!(
            locking["stage_label"],
            serde_json::json!(crate::workflows::stage_short_label(
                ProjectId::Linux,
                "locking"
            ))
        );
        assert!(locking["stage_label"].is_string());
        // The rest of the row is untouched.
        assert_eq!(locking["seconds"], 10);

        let unknown = &review["stage_durations"][1];
        assert!(unknown["order"].is_null(), "never a sentinel: {unknown}");
        assert!(unknown["stage_label"].is_null());
        assert!(review["stage_failures"][0]["order"].is_number());

        // The same order annotate() gives a live entry for the same name.
        let mut live = vec![ActivityEntry {
            key: "patchset:1/patch:2/stage:locking".to_string(),
            phase: None,
            description: String::new(),
            patch_id: Some(2),
            stage: Some("locking".to_string()),
            stage_label: None,
            order: None,
            age_seconds: None,
            idle_seconds: None,
            updated_at: None,
        }];
        annotate(&mut live, ProjectId::Linux);
        assert_eq!(locking["order"], serde_json::json!(live[0].order));
        assert_eq!(
            locking["stage_label"],
            serde_json::json!(live[0].stage_label)
        );

        // Absent, null or non-array fields are left as they are.
        for mut other in [
            serde_json::json!({}),
            serde_json::json!({"stage_durations": null, "stage_failures": "[]"}),
            serde_json::json!({"stage_durations": [1, "x", {"seconds": 3}]}),
        ] {
            let before = other.clone();
            annotate_stage_records(&mut other, ProjectId::Linux);
            if before
                .get("stage_durations")
                .and_then(|v| v.as_array())
                .is_some()
            {
                // An object without a stage gets null fields; anything else
                // is untouched.
                assert_eq!(other["stage_durations"][0], 1);
                assert_eq!(other["stage_durations"][1], "x");
                assert!(other["stage_durations"][2]["order"].is_null());
            } else {
                assert_eq!(other, before);
            }
        }
    }

    /// A stage is a name on the wire. A number where the name belongs is not
    /// read as some stage; the line is dropped.
    #[test]
    fn test_malformed_progress_is_dropped_not_guessed() {
        let numeric = serde_json::json!({
            "type": "progress",
            "patch_index": 1,
            "payload": {"kind": "stage_turn", "stage": 5, "turn": 1, "max_turns": 2},
        });
        assert_eq!(parse_progress(&numeric), None);

        for bad in [
            serde_json::json!({}),
            serde_json::json!({"type": "progress", "payload": {"kind": "planning"}}),
            serde_json::json!({"type": "progress", "patch_index": 1, "payload": {"kind": "nonsense"}}),
            serde_json::json!({"type": "progress", "patch_index": 1, "payload": {"kind": "stage_turn"}}),
            serde_json::json!({"type": "reference_repos", "patch_index": 1, "payload": {"kind": "planning"}}),
            serde_json::Value::Null,
        ] {
            assert_eq!(parse_progress(&bad), None, "{bad}");
        }
    }

    /// The worker reports turns for the pre-screen and the planner too, because
    /// its own progress bar counts them. Here they are the patch's phase, so
    /// they must not open stage rows nothing would ever close.
    #[test]
    fn test_planning_and_pre_screen_open_no_stage_rows() {
        let patch = ActivityKey::PatchsetPatch {
            patchset_id: 7,
            patch_id: 70,
        };
        for project in [ProjectId::Linux, ProjectId::Sashiko] {
            for stage in ["planning", "pre-screen"] {
                for wire in [
                    ProgressWire::StageStarted {
                        stage: stage.into(),
                    },
                    ProgressWire::StageTurn {
                        stage: stage.into(),
                        turn: 1,
                        max_turns: 5,
                    },
                    ProgressWire::StageTools {
                        stage: stage.into(),
                        tools: vec!["git_grep".into()],
                        turn: 1,
                        max_turns: 5,
                    },
                    ProgressWire::StageBackoff {
                        stage: stage.into(),
                        retry_in_seconds: Some(3),
                        turn: 1,
                        max_turns: 5,
                    },
                    ProgressWire::StageFinished {
                        stage: stage.into(),
                    },
                    ProgressWire::StageFailed {
                        stage: stage.into(),
                        reason: "boom".into(),
                        cause: StageStopCause::Error,
                    },
                ] {
                    assert_eq!(
                        progress_update(project, 7, 70, &wire, 1, 4),
                        Some(ProgressUpdate::Touch(patch.clone())),
                        "{project:?} {wire:?}"
                    );
                }
            }
        }

        // And applying them leaves nothing behind but the patch's own entry.
        let reg = ActivityRegistry::new();
        let mut tracker = StageTracker::new(reg.clone());
        for wire in [
            ProgressWire::PreScreen,
            ProgressWire::StageTurn {
                stage: "pre-screen".into(),
                turn: 1,
                max_turns: 5,
            },
            ProgressWire::StageFinished {
                stage: "pre-screen".into(),
            },
            ProgressWire::Planning,
            ProgressWire::StageFinished {
                stage: "planning".into(),
            },
        ] {
            tracker.apply(progress_update(ProjectId::Linux, 7, 70, &wire, 1, 4).unwrap());
        }
        let entries = reg.patchset_snapshot(7);
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].description, "planning stages");
        assert!(tracker.timings().is_empty());
    }

    #[test]
    fn test_the_patch_moves_from_screening_to_planning_to_reviewing() {
        let phase = |wire| match progress_update(ProjectId::Linux, 7, 70, &wire, 2, 4) {
            Some(ProgressUpdate::Set(ActivityKey::PatchsetPatch { .. }, phase)) => phase,
            other => panic!("expected a patch phase, got {other:?}"),
        };
        assert_eq!(
            phase(ProgressWire::PreScreen),
            Phase::PreScreening {
                attempt: 2,
                max_attempts: 4
            }
        );
        assert_eq!(
            phase(ProgressWire::Planning),
            Phase::Planning {
                attempt: 2,
                max_attempts: 4
            }
        );
        // Sent whether or not the planner ran, so fixed stages still move on.
        assert_eq!(
            phase(ProgressWire::PlanReady {
                planned_stages: vec![]
            }),
            Phase::Reviewing {
                attempt: 2,
                max_attempts: 4
            }
        );
    }

    /// Ported from the daemon's own decoding tests: what each worker event does
    /// to the registry.
    #[test]
    fn progress_events_round_trip_from_worker_to_activity_update() {
        let key = |stage: &str| ActivityKey::PatchsetStage {
            patchset_id: 77,
            patch_id: 3,
            stage: stage.to_string(),
        };
        let cases = vec![
            (
                ProgressWire::StageStarted {
                    stage: "resources".into(),
                },
                ProgressUpdate::Turn {
                    key: key("resources"),
                    stage: "resources".into(),
                    turn: 0,
                    max_turns: 0,
                },
            ),
            (
                ProgressWire::StageTurn {
                    stage: "security".into(),
                    turn: 12,
                    max_turns: 100,
                },
                ProgressUpdate::Turn {
                    key: key("security"),
                    stage: "security".into(),
                    turn: 12,
                    max_turns: 100,
                },
            ),
            (
                ProgressWire::StageFinished {
                    stage: "execution-flow".into(),
                },
                ProgressUpdate::Finished {
                    key: key("execution-flow"),
                    stage: "execution-flow".into(),
                },
            ),
            // A failed stage is relabelled, not removed: the review carries on
            // without it, and a stage that vanished would be indistinguishable
            // from one that was never planned.
            (
                ProgressWire::StageFailed {
                    stage: "implementation".into(),
                    reason: "Session exceeded max turns limit (50)".into(),
                    cause: StageStopCause::Error,
                },
                ProgressUpdate::Set(
                    key("implementation"),
                    Phase::StageFailed {
                        stage: "implementation".into(),
                        reason: "Session exceeded max turns limit (50)".into(),
                        cause: StageStopCause::Error,
                    },
                ),
            ),
            // The cause crosses the pipe as it was sent, so the row says why
            // the stage stopped, not only that it did.
            (
                ProgressWire::StageFailed {
                    stage: "security".into(),
                    reason: "stopped".into(),
                    cause: StageStopCause::OutOfTime,
                },
                ProgressUpdate::Set(
                    key("security"),
                    Phase::StageFailed {
                        stage: "security".into(),
                        reason: "stopped".into(),
                        cause: StageStopCause::OutOfTime,
                    },
                ),
            ),
        ];
        for (wire, expected) in cases {
            let line = progress_line(0, &wire).unwrap();
            let (_, decoded) = parse_progress(&serde_json::from_str(&line).unwrap()).unwrap();
            assert_eq!(
                progress_update(ProjectId::Linux, 77, 3, &decoded, 1, 4),
                Some(expected)
            );
        }
    }

    /// The failure reason is served by public routes and persisted, so what
    /// reaches the phase must be bounded and free of the model's output, not
    /// merely shortened when rendered.
    #[test]
    fn a_failure_reason_never_carries_model_output_into_the_phase() {
        let model_output = "SECRET-FINDING: use-after-free in foo_release()";
        let raw = format!(
            "Failed to generate valid response after 3 validation attempts. \
             Last violation: Failed to parse JSON from output (expected value \
             at line 1 column 1): {model_output}\n\nCaused by:\n    more"
        );
        let long = "x".repeat(1000);
        for reason in [raw.as_str(), long.as_str()] {
            let wire = ProgressWire::StageFailed {
                stage: "security".into(),
                reason: reason.to_string(),
                cause: StageStopCause::Error,
            };
            let Some(ProgressUpdate::Set(_, phase)) =
                progress_update(ProjectId::Linux, 77, 3, &wire, 1, 4)
            else {
                panic!("a failed stage must set its row");
            };
            let json = serde_json::to_string(&phase).unwrap();
            assert!(!json.contains("SECRET-FINDING"), "{json}");
            assert!(!json.contains("Caused by"), "{json}");
            let Phase::StageFailed { reason, .. } = phase else {
                panic!("expected a failed stage, got {phase:?}");
            };
            assert!(reason.chars().count() <= 141, "{reason}");
        }
        assert_eq!(
            public_reason(&raw),
            "Failed to generate valid response after 3 validation attempts \
             (output failed validation)"
        );
    }

    /// Concurrent stages must not share a slot, or a wedged stage would be
    /// masked by whichever sibling reported most recently.
    #[test]
    fn concurrent_stages_are_tracked_independently() {
        let reg = ActivityRegistry::new();
        let mut tracker = StageTracker::new(reg.clone());
        let apply = |tracker: &mut StageTracker, wire| {
            tracker.apply(progress_update(ProjectId::Linux, 5, 3, &wire, 1, 4).unwrap())
        };
        for stage in ["goal", "implementation", "execution-flow"] {
            apply(
                &mut tracker,
                ProgressWire::StageTurn {
                    stage: stage.into(),
                    turn: 4,
                    max_turns: 100,
                },
            );
        }
        assert_eq!(reg.patchset_snapshot(5).len(), 3);

        apply(
            &mut tracker,
            ProgressWire::StageFinished {
                stage: "implementation".into(),
            },
        );
        let remaining = reg.patchset_snapshot(5);
        assert_eq!(remaining.len(), 3, "a finished stage keeps its slot");
        let finished = remaining
            .iter()
            .find(|e| e.key == "patchset:5/patch:3/stage:implementation")
            .expect("the finished stage must still be listed");
        assert_eq!(
            finished.description,
            "stage implementation done in 0s, 4 turns"
        );
        assert_eq!(
            remaining
                .iter()
                .filter(|e| e.description.contains("turn 4/100"))
                .count(),
            2,
            "its siblings are untouched and still running"
        );
    }

    /// A stage reports the highest turn it reached, not the count of events,
    /// and once it finishes it is no longer open.
    #[test]
    fn test_stage_timings_are_derived_from_worker_progress() {
        let reg = ActivityRegistry::new();
        let mut tracker = StageTracker::new(reg.clone());
        for wire in [
            ProgressWire::StageStarted {
                stage: "execution-flow".into(),
            },
            ProgressWire::StageTurn {
                stage: "execution-flow".into(),
                turn: 1,
                max_turns: 100,
            },
            ProgressWire::StageTools {
                stage: "execution-flow".into(),
                tools: vec!["git_show".into()],
                turn: 1,
                max_turns: 100,
            },
            ProgressWire::StageTurn {
                stage: "execution-flow".into(),
                turn: 5,
                max_turns: 100,
            },
            ProgressWire::StageFinished {
                stage: "execution-flow".into(),
            },
        ] {
            tracker.apply(progress_update(ProjectId::Linux, 1, 2, &wire, 1, 1).unwrap());
        }
        assert_eq!(
            tracker.timings(),
            [StageTiming {
                stage: "execution-flow".into(),
                seconds: 0,
                turns: 5,
            }]
        );
        assert!(
            tracker.starts.is_empty(),
            "a finished stage must not stay open"
        );

        // A finish with no start has nothing true to say, so the row goes.
        tracker.apply(
            progress_update(
                ProjectId::Linux,
                1,
                2,
                &ProgressWire::StageFinished {
                    stage: "locking".into(),
                },
                1,
                1,
            )
            .unwrap(),
        );
        assert_eq!(tracker.timings().len(), 1);
        assert!(
            reg.get(&ActivityKey::PatchsetStage {
                patchset_id: 1,
                patch_id: 2,
                stage: "locking".into()
            })
            .is_none()
        );
    }

    /// A stalled stage is either waiting on the model or grinding through git,
    /// and those need different responses.
    #[test]
    fn tool_execution_is_distinguishable_from_waiting_on_the_model() {
        let reg = ActivityRegistry::new();
        let mut tracker = StageTracker::new(reg.clone());
        let key = ActivityKey::PatchsetStage {
            patchset_id: 7,
            patch_id: 2,
            stage: "resources".into(),
        };
        let tools = |names: Vec<String>| ProgressWire::StageTools {
            stage: "resources".into(),
            tools: names,
            turn: 9,
            max_turns: 100,
        };

        tracker.apply(
            progress_update(
                ProjectId::Linux,
                7,
                2,
                &tools(vec!["git_grep".into()]),
                1,
                4,
            )
            .unwrap(),
        );
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage resources, turn 9/100 (running git_grep)"
        );

        // An empty tool list means they finished: back with the model, turn
        // number kept.
        tracker.apply(progress_update(ProjectId::Linux, 7, 2, &tools(vec![]), 1, 4).unwrap());
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage resources, turn 9/100 (awaiting model)"
        );
    }

    /// The two sub-phases are the same stage, so the stage clock must not reset
    /// when a turn switches between them.
    #[test]
    fn switching_between_model_and_tools_keeps_the_stage_clock() {
        let reg = ActivityRegistry::new();
        let key = ActivityKey::PatchsetStage {
            patchset_id: 1,
            patch_id: 1,
            stage: "resources".into(),
        };
        let stage = |waiting| Phase::Stage {
            stage: "resources".into(),
            turn: 1,
            max_turns: 100,
            waiting,
        };
        reg.update(key.clone(), stage(StageWait::Model));
        let since = reg.entries.lock().unwrap().get(&key).unwrap().since;
        reg.update(
            key.clone(),
            stage(StageWait::Tools {
                names: vec!["git_grep".into()],
            }),
        );
        assert_eq!(reg.entries.lock().unwrap().get(&key).unwrap().since, since);
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage resources, turn 1/100 (running git_grep)"
        );
    }

    /// Every wait a stage can be blocked on must read differently, since each
    /// calls for a different response.
    #[test]
    fn every_stage_wait_reads_differently() {
        let describe = |waiting| {
            Phase::Stage {
                stage: "resources".into(),
                turn: 9,
                max_turns: 100,
                waiting,
            }
            .describe()
        };
        assert_eq!(
            describe(StageWait::Model),
            "stage resources, turn 9/100 (awaiting model)"
        );
        assert_eq!(
            describe(StageWait::Queued),
            "stage resources, turn 9/100 (queued for a model slot)"
        );
        assert_eq!(
            describe(StageWait::RateLimited {
                retry_in_seconds: 60
            }),
            "stage resources, turn 9/100 (rate limited, retrying in 60s)"
        );
        assert_eq!(
            describe(StageWait::Tools {
                names: vec!["git_grep".into()]
            }),
            "stage resources, turn 9/100 (running git_grep)"
        );
    }

    /// A backoff ending must put the turn back with the model rather than
    /// leaving it reporting a rate limit that has expired.
    #[test]
    fn backoff_events_round_trip_and_clear() {
        let reg = ActivityRegistry::new();
        let mut tracker = StageTracker::new(reg.clone());
        let key = ActivityKey::PatchsetStage {
            patchset_id: 1,
            patch_id: 2,
            stage: "resources".into(),
        };
        let backoff = |retry| ProgressWire::StageBackoff {
            stage: "resources".into(),
            retry_in_seconds: retry,
            turn: 9,
            max_turns: 100,
        };
        tracker.apply(progress_update(ProjectId::Linux, 1, 2, &backoff(Some(60)), 1, 4).unwrap());
        assert_eq!(
            reg.get(&key).unwrap().phase,
            Phase::Stage {
                stage: "resources".into(),
                turn: 9,
                max_turns: 100,
                waiting: StageWait::RateLimited {
                    retry_in_seconds: 60
                },
            }
        );
        tracker.apply(progress_update(ProjectId::Linux, 1, 2, &backoff(None), 1, 4).unwrap());
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage resources, turn 9/100 (awaiting model)"
        );
    }

    /// The daemon's queue report usually lands before the worker's turn report
    /// for the same request; the turn must not erase it.
    #[test]
    fn test_a_turn_from_the_wire_keeps_a_queue_wait() {
        let reg = ActivityRegistry::new();
        let mut tracker = StageTracker::new(reg.clone());
        let key = ActivityKey::PatchsetStage {
            patchset_id: 1,
            patch_id: 2,
            stage: "locking".into(),
        };
        reg.update_stage_wait(key.clone(), "locking".into(), StageWait::Queued);
        tracker.apply(
            progress_update(
                ProjectId::Linux,
                1,
                2,
                &ProgressWire::StageTurn {
                    stage: "locking".into(),
                    turn: 1,
                    max_turns: 100,
                },
                1,
                1,
            )
            .unwrap(),
        );
        assert_eq!(
            reg.get(&key).unwrap().description,
            "stage locking, turn 1/100 (queued for a model slot)"
        );
    }

    /// The tag the stage layer writes is the tag the daemon reads. Built here
    /// the same way the stage layer builds it.
    #[test]
    fn test_the_stage_survives_the_round_trip_through_the_context_tag() {
        let tag = format!("{}[s:{}] ", "[ps:12 p:34] ", "locking");
        assert_eq!(stage_from_context_tag(&tag), Some("locking".to_string()));

        // Names carry hyphens, which a numeric parser would have rejected.
        for name in ["conflict-resolution", "llm-pipeline", "interfaces-compat"] {
            let tag = format!("{}[s:{}] ", "[ps:1 p:2] ", name);
            assert_eq!(stage_from_context_tag(&tag), Some(name.to_string()));
        }

        // Anything else must not guess: attributing a queue wait to the wrong
        // stage is worse than not reporting it.
        assert_eq!(stage_from_context_tag("[ps:12 p:34] "), None);
        assert_eq!(stage_from_context_tag(""), None);
        assert_eq!(stage_from_context_tag("[s:]"), None);
        assert_eq!(stage_from_context_tag("[s:locking"), None);
    }

    /// Both projects' stages get rows and ranks from their own tables.
    #[test]
    fn test_stage_rows_and_order_follow_the_project() {
        assert!(crate::workflows::reports_stage_row(
            ProjectId::Sashiko,
            "llm-pipeline"
        ));
        assert!(!crate::workflows::reports_stage_row(
            ProjectId::Linux,
            "llm-pipeline"
        ));
        assert!(crate::workflows::reports_stage_row(
            ProjectId::Sashiko,
            "summary"
        ));
        assert!(!crate::workflows::reports_stage_row(
            ProjectId::Linux,
            "summary"
        ));

        let reg = ActivityRegistry::new();
        for stage in ["summary", "llm-pipeline", "goal"] {
            reg.update_stage_turn(
                ActivityKey::PatchsetStage {
                    patchset_id: 3,
                    patch_id: 1,
                    stage: stage.into(),
                },
                stage.into(),
                1,
                10,
            );
        }
        let entries = displayed(&reg, 3, ProjectId::Sashiko);
        let stages: Vec<_> = entries.iter().map(|e| e.stage.clone().unwrap()).collect();
        assert_eq!(stages, ["goal", "llm-pipeline", "summary"]);
        assert!(
            entries
                .iter()
                .all(|e| e.order.is_some() && e.stage_label.is_some())
        );
        assert!(entries.windows(2).all(|w| w[0].order < w[1].order));

        // Under the other project the unknown name has no rank, is listed after
        // the known ones, and serialises as null rather than a sentinel.
        let entries = displayed(&reg, 3, ProjectId::Linux);
        let last = entries.last().unwrap();
        assert_eq!(last.stage.as_deref(), Some("summary"));
        assert_eq!(last.order, None);
        assert!(serde_json::to_value(last).unwrap()["order"].is_null());
        assert!(
            entries.iter().filter_map(|e| e.order).all(|o| o < 1000),
            "ranks are small table positions, nothing a JSON reader could lose"
        );
    }

    /// Persisted rows carry no clocks and come back from the database in no
    /// particular order; displayed, they read in the same order as live ones.
    #[test]
    fn test_persisted_and_live_entries_display_in_the_same_order() {
        let reg = ActivityRegistry::new();
        reg.update(
            Ky::patch(9),
            Phase::Reviewing {
                attempt: 1,
                max_attempts: 1,
            },
        );
        for stage in ["report", "goal", "locking"] {
            reg.update_stage_turn(Ky::stage(9, stage), stage.into(), 1, 10);
        }
        reg.update(
            ActivityKey::Patchset(4),
            Phase::ReviewingPatches { patches: 1 },
        );
        let live: Vec<String> = displayed(&reg, 4, ProjectId::Linux)
            .into_iter()
            .map(|e| e.key)
            .collect();

        let mut persisted: Vec<ActivityEntry> = live
            .iter()
            .rev()
            .map(|key| {
                let parsed = ActivityKey::parse(key).unwrap();
                ActivityEntry {
                    key: key.clone(),
                    phase: None,
                    description: String::new(),
                    patch_id: parsed.patch_id(),
                    stage: parsed.stage().map(str::to_string),
                    stage_label: None,
                    order: None,
                    age_seconds: None,
                    idle_seconds: None,
                    updated_at: Some(1),
                }
            })
            .collect();
        sort_for_display(&mut persisted, ProjectId::Linux);
        let persisted: Vec<String> = persisted.into_iter().map(|e| e.key).collect();
        assert_eq!(persisted, live);
        assert_eq!(
            live,
            [
                "patchset:4",
                "patchset:4/patch:9",
                "patchset:4/patch:9/stage:goal",
                "patchset:4/patch:9/stage:locking",
                "patchset:4/patch:9/stage:report",
            ]
        );
    }

    struct Ky;
    impl Ky {
        fn patch(patch_id: i64) -> ActivityKey {
            ActivityKey::PatchsetPatch {
                patchset_id: 4,
                patch_id,
            }
        }
        fn stage(patch_id: i64, stage: &str) -> ActivityKey {
            ActivityKey::PatchsetStage {
                patchset_id: 4,
                patch_id,
                stage: stage.into(),
            }
        }
    }

    /// Progress from a stage that is not a row of its own is still a sign of
    /// life for the patch and the patchset.
    #[test]
    fn test_touch_keeps_the_patch_alive_without_changing_it() {
        let reg = ActivityRegistry::new();
        let patch = ActivityKey::PatchsetPatch {
            patchset_id: 1,
            patch_id: 2,
        };
        reg.update(
            ActivityKey::Patchset(1),
            Phase::ReviewingPatches { patches: 1 },
        );
        reg.update(
            patch.clone(),
            Phase::Planning {
                attempt: 1,
                max_attempts: 1,
            },
        );
        let before = reg.entries.lock().unwrap().get(&patch).unwrap().clone();
        std::thread::sleep(std::time::Duration::from_millis(20));
        reg.touch(&patch);
        let after = reg.entries.lock().unwrap().get(&patch).unwrap().clone();
        assert_eq!(after.since, before.since);
        assert!(after.updated > before.updated);
        assert_eq!(after.phase, before.phase);
        let ps = reg
            .entries
            .lock()
            .unwrap()
            .get(&ActivityKey::Patchset(1))
            .unwrap()
            .clone();
        assert!(ps.updated > ps.since);

        // Touching something that is not tracked creates nothing.
        reg.touch(&ActivityKey::Commit("x".into()));
        assert_eq!(reg.len(), 2);
    }
}
