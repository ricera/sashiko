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

use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct Pagination {
    pub page: Option<usize>,
    pub per_page: Option<usize>,
    pub q: Option<String>,
    pub mailing_list: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct PatchsetsResponse {
    pub items: Vec<crate::db::PatchsetRow>,
    pub total: usize,
    pub page: usize,
    pub per_page: usize,
}

#[derive(Serialize, Deserialize)]
pub struct MessagesResponse {
    pub items: Vec<crate::db::MessageRow>,
    pub total: usize,
    pub page: usize,
    pub per_page: usize,
}

#[derive(Deserialize)]
pub struct PatchQuery {
    pub id: String,
    pub page: Option<u32>,
    pub per_page: Option<u32>,
}

#[derive(Deserialize)]
pub struct ReviewQuery {
    pub id: Option<i64>,
    pub patchset_id: Option<i64>,
    /// Only return live log entries after this sequence number, so a poll on
    /// a long review does not refetch the whole conversation.
    pub after_seq: Option<i64>,
}

/// One message of a review's conversation log, with the stage it belongs
/// to: the shape of every entry of a stored `reviews.logs`, whichever path
/// wrote it, and of the worker output's `history`.
///
/// A review's log has two writers. A worker that returns a review sends its
/// history, and the daemon keeps the conversation it streamed for an attempt
/// that ended without one. Both store this shape, so a reader of the column
/// meets one kind of entry. Only the message and its stage are kept: the
/// stream's own bookkeeping (a live entry's `seq` and `created_at`) says where
/// an entry sat in a poll, which means nothing once the log is final.
///
/// Rows written before every entry carried its stage are plain messages, and
/// read as entries with no stage, because both stage fields are optional and
/// left out of the JSON when absent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReviewLogEntry {
    /// The stage the message belongs to, by name; `None` for a message that
    /// belongs to no stage, such as the system prompt or the header of a log
    /// kept from a review that did not finish. Stages run concurrently, so a
    /// streamed conversation interleaves them, and the page marks each entry
    /// with its stage so a reader can tell the conversations apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    /// The project's short label for `stage`, set by the daemon as it is for
    /// activity entries; `None` for an unknown stage or no stage. The page
    /// shows it in place of the name when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_label: Option<String>,
    #[serde(flatten)]
    pub message: crate::ai::AiMessage,
}

impl ReviewLogEntry {
    /// An entry for `message`, belonging to `stage`, not yet labelled.
    pub fn new(stage: Option<&str>, message: crate::ai::AiMessage) -> Self {
        Self {
            stage: stage.map(str::to_string),
            stage_label: None,
            message,
        }
    }

    /// Sets `stage_label` to the project's short label for `stage`.
    ///
    /// Done by the daemon, as it is for activity entries, because the labels
    /// are the project's and the page keeps no table of them.
    pub fn label_stage(&mut self, project: crate::project::ProjectId) {
        self.stage_label = self
            .stage
            .as_deref()
            .and_then(|s| crate::workflows::stage_short_label(project, s))
            .map(str::to_string);
    }
}

/// One message of a running review's conversation, as `live_log` serves it.
///
/// A stored log entry plus where it sits in the stream. One renderer
/// therefore draws both logs, and fields it does not know about are ignored.
/// A review that did not finish keeps the `entry` part of each, under a
/// header saying so, as its final log, because its worker returned no history
/// of its own.
///
/// An entry of no stage leaves `stage` and `stage_label` out of the JSON, as
/// a stored entry does, where builds before the one-shape change sent them as
/// `null`; a client treats a missing key as no stage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LiveLogEntry {
    /// Position in the review's conversation. It only grows for one review
    /// row: a rerun of the row continues past the last run's seqs, so a page
    /// holding a cursor from one run cannot mistake another run's entries
    /// for what follows it.
    pub seq: i64,
    /// Unix time the daemon stored it.
    pub created_at: i64,
    #[serde(flatten)]
    pub entry: ReviewLogEntry,
}

impl LiveLogEntry {
    /// Sets the entry's `stage_label`; see [`ReviewLogEntry::label_stage`].
    pub fn label_stage(&mut self, project: crate::project::ProjectId) {
        self.entry.label_stage(project);
    }
}

#[derive(Deserialize)]
pub struct BugQuery {
    pub id: Option<i64>,
    pub bugid: Option<String>,
    pub slug: Option<String>,
}

impl BugQuery {
    /// Returns the trimmed `bugid` or `slug` parameter when the lookup is not
    /// keyed on a sequential numeric `id`.
    pub fn effective_bugid(&self) -> Option<&str> {
        if self.id.is_some() {
            return None;
        }
        self.bugid
            .as_deref()
            .or(self.slug.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// Whether this query addresses a bug by its `bugid` / `slug` rather than
    /// its sequential integer `id`.
    pub fn by_bugid(&self) -> bool {
        self.effective_bugid().is_some()
    }
}

#[derive(Deserialize)]
pub struct BugListQuery {
    pub page: Option<usize>,
    pub per_page: Option<usize>,
    pub q: Option<String>,
    pub subsystem: Option<String>,
    pub subsystems: Option<String>,
    pub min_severity: Option<String>,
    pub severity: Option<String>,
    /// Triage state. Also accepts the historical `status` spelling.
    #[serde(alias = "status")]
    pub lifecycle_status: Option<String>,
    /// Analysis execution state.
    pub pipeline_state: Option<String>,
    /// Filters by assignee. The literal `none` selects unassigned bugs.
    pub assignee: Option<String>,
    pub sort_by: Option<String>,
    pub sort_order: Option<String>,
}

#[derive(Deserialize)]
pub struct BugSubsystemsQuery {
    #[serde(alias = "status")]
    pub lifecycle_status: Option<String>,
}

#[derive(Deserialize)]
pub struct RerunPatchQuery {
    pub patchset_id: i64,
    pub patch_id: i64,
}

#[derive(Deserialize)]
pub struct SubsystemQuery {
    pub subsystem_id: Option<i64>,
}

#[derive(Deserialize)]
pub struct CancelQuery {
    pub id: i64,
    #[serde(default)]
    pub force: bool,
}

#[derive(Deserialize)]
pub struct InjectRequest {
    pub raw: String,
    pub group: Option<String>,
    pub baseline: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SubmitRequest {
    Inject {
        raw: String,
        base_commit: Option<String>,
        skip_subjects: Option<Vec<String>>,
        only_subjects: Option<Vec<String>>,
    },
    Remote {
        sha: String,
        repo: Option<String>,
        skip_subjects: Option<Vec<String>>,
        only_subjects: Option<Vec<String>>,
    },
    #[serde(rename = "remote-range")]
    RemoteRange {
        sha: String,
        repo: Option<String>,
        skip_subjects: Option<Vec<String>>,
        only_subjects: Option<Vec<String>>,
    },
    Thread {
        msgid: String,
    },
}

#[derive(Serialize, Deserialize)]
pub struct SubmitResponse {
    pub status: String,
    pub id: String,
}

/// Input payload representing a candidate Linux kernel defect.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BugInput {
    pub problem: String,
    pub reasoning: String,
    pub locations: Option<serde_json::Value>,
    #[serde(default)]
    pub subsystems: Vec<crate::db::AttributedSubsystem>,
    pub source_files: Vec<String>,
    pub commit_sha: Option<String>,
    pub patchset_id: Option<i64>,
    pub patch_id: Option<i64>,
    pub baseline_sha: Option<String>,
    #[serde(default)]
    pub review_id: Option<i64>,
}

/// The result of processing a candidate Linux kernel bug through the pipeline.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BugOutcome {
    /// The candidate bug was discarded (invalid, false positive, or Low/Medium severity).
    Discarded {
        reason: String,
        logs: Option<String>,
    },
    /// The bug was confirmed as an identical duplicate of a known Linux kernel bug.
    Duplicate {
        existing_bug: crate::db::Bug,
        reasoning: String,
        logs: Option<String>,
    },
    /// The bug was confirmed as a newly discovered Linux kernel bug.
    NewlyDiscovered { bug: crate::db::Bug },
}

impl std::fmt::Display for BugOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BugOutcome::NewlyDiscovered { bug } => {
                write!(
                    f,
                    "newly discovered bug {} ({}) [severity: {}]",
                    bug.id,
                    bug.bugid,
                    bug.severity()
                )
            }
            BugOutcome::Duplicate {
                existing_bug,
                reasoning,
                ..
            } => {
                write!(
                    f,
                    "duplicate of bug {} ({}) - {}",
                    existing_bug.id, existing_bug.bugid, reasoning
                )
            }
            BugOutcome::Discarded { reason, .. } => {
                write!(f, "discarded - {}", reason)
            }
        }
    }
}

impl std::fmt::Debug for BugOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BugOutcome::Discarded { reason, logs } => f
                .debug_struct("Discarded")
                .field("reason", reason)
                .field(
                    "logs",
                    &logs.as_ref().map(|l| format!("<{} bytes>", l.len())),
                )
                .finish(),
            BugOutcome::Duplicate {
                existing_bug,
                reasoning,
                logs,
            } => f
                .debug_struct("Duplicate")
                .field("existing_bug_id", &existing_bug.id)
                .field("existing_bug_bugid", &existing_bug.bugid)
                .field("reasoning", reasoning)
                .field(
                    "logs",
                    &logs.as_ref().map(|l| format!("<{} bytes>", l.len())),
                )
                .finish(),
            BugOutcome::NewlyDiscovered { bug } => f
                .debug_struct("NewlyDiscovered")
                .field("id", &bug.id)
                .field("bugid", &bug.bugid)
                .field("lifecycle_status", &bug.lifecycle_status)
                .field("pipeline_state", &bug.pipeline_state)
                .field("problem", &bug.problem())
                .field("severity", &bug.severity())
                .finish(),
        }
    }
}

#[derive(Deserialize)]
pub struct AnalyzeBugPayload {
    #[serde(flatten)]
    pub input: BugInput,
    pub tool: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BugActionPayload {
    /// Client-declared provenance. Author always comes from authentication.
    pub tool: Option<String>,
    pub model: Option<String>,
    #[serde(flatten)]
    pub action: BugAction,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BugAction {
    Comment {
        content: String,
    },
    Close {
        reason: Option<String>,
    },
    Dismiss {
        reason: Option<String>,
    },
    MarkDuplicate {
        duplicate_of_id: Option<i64>,
        duplicate_of_bugid: Option<String>,
        reasoning: Option<String>,
    },
    /// Hands a bug to someone, or drops the assignment when the assignee is
    /// absent, empty, or null.
    Assign {
        assignee: Option<String>,
        reason: Option<String>,
    },
}

#[derive(Deserialize)]
pub struct RequestLinkRequest {
    #[serde(default)]
    pub email: String,
}

#[derive(Deserialize)]
pub struct VerifyLinkQuery {
    pub token: String,
}

#[derive(Deserialize)]
pub struct CreateApiTokenRequest {
    pub email: Option<String>,
    pub max_bug_access: Option<String>,
    pub expires_in_days: Option<u64>,
}

/// One entry of live or last-known activity, as served by `/api/activity` and
/// `/api/patchset/activity` and read back by `sashiko-cli`.
///
/// One shape for both sources, so a reader never has to tell them apart by
/// which fields happen to be present: live entries carry `age_seconds` and
/// `idle_seconds`, persisted ones carry `updated_at` instead.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActivityEntry {
    /// The rendered `ActivityKey`. An identifier, not something to parse: the
    /// patch and stage it names are given below.
    pub key: String,
    /// What the work is doing. `None` for a phase this build cannot read --
    /// a persisted row from another build, or a newer daemon answering an
    /// older client -- in which case `description` still says it. Read
    /// leniently for the same reason: one unfamiliar entry must not make the
    /// whole response unreadable.
    #[serde(default, deserialize_with = "lenient_phase")]
    pub phase: Option<crate::activity::Phase>,
    pub description: String,
    pub patch_id: Option<i64>,
    /// The stage's name, for stage-level entries.
    pub stage: Option<String>,
    /// The stage's short label in the daemon's project.
    pub stage_label: Option<String>,
    /// The stage's rank in the daemon's pipeline; `None` for entries that are
    /// not stages and for names the pipeline does not know. Sort `None` last.
    pub order: Option<u32>,
    /// Seconds since this activity began. Live entries only.
    pub age_seconds: Option<u64>,
    /// Seconds since it last advanced. Live entries only.
    pub idle_seconds: Option<u64>,
    /// Unix time the row was last written. Persisted entries only.
    pub updated_at: Option<i64>,
}

fn lenient_phase<'de, D>(deserializer: D) -> Result<Option<crate::activity::Phase>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

/// Everything in flight across the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityResponse {
    pub count: usize,
    pub entries: Vec<ActivityEntry>,
}

/// What one patchset is doing, or was doing when the daemon last stopped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchsetActivityResponse {
    /// False when the entries are the last known state persisted before a
    /// restart, rather than work running now.
    pub live: bool,
    pub entries: Vec<ActivityEntry>,
    /// True once the patchset has reached a state it will not leave by
    /// itself, so there is nothing left to poll for. Decided by the daemon,
    /// which knows the full set of statuses, so a client keeps no list of its
    /// own. Defaulted so a client can still read an older daemon's answer.
    #[serde(default)]
    pub finished: bool,
    /// Tool calls of each patch named by `entries`, over its current run (see
    /// [`review_runs`]). A patch that has no review row yet has no entry.
    /// Defaulted so a client can still read an older daemon's answer.
    #[serde(default)]
    pub tool_calls: Vec<PatchToolCalls>,
}

/// Query for `/api/patchset/activity`.
#[derive(Debug, Deserialize)]
pub struct PatchsetActivityQuery {
    pub id: i64,
}

/// Tool calls recorded for one review attempt, or summed over one run.
///
/// Served as `tool_calls` on every review of the review payloads. Counted
/// from `tool_usages`, so it covers the calls the model made as the daemon
/// recorded them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCallCounts {
    /// Every call the model made that the daemon recorded, of every tool, to
    /// every repository, including calls to unattached repositories and the
    /// few the session declined to run.
    pub total: u64,
    /// The calls split by reference, or `None` when it is not known which
    /// references the review had: no reference record (none configured, a
    /// row from before migration 014, a fork row, a lost hand-off).
    #[serde(default)]
    pub by_reference: Option<ReferenceSplit>,
}

/// How a review's calls fell across the references it had.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReferenceSplit {
    /// One entry per reference attached to the review, in configured order,
    /// zero counts included. Empty when every reference was dormant.
    pub references: Vec<ReferenceCalls>,
    /// Calls naming a repository the review did not have attached. The
    /// toolbox refused them; they read nothing.
    pub unattached: u64,
}

/// The calls one attached reference received.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceCalls {
    pub name: String,
    /// Calls aimed at this reference: cache hits and calls git rejected
    /// included.
    pub calls: u64,
}

impl ToolCallCounts {
    /// The plain-text clause that says where the calls went, such as
    /// "(72 to kernel, 0 to rdma-core)".
    ///
    /// Empty when there is nothing a reader could rely on: no calls (a zero
    /// for a reference only means "never looked there" once the review was
    /// looking somewhere), an unknown split, or a split with no attached
    /// reference and no call to an unattached repository.
    pub fn breakdown(&self) -> String {
        let Some(split) = self.by_reference.as_ref().filter(|_| self.total > 0) else {
            return String::new();
        };
        let references = split
            .references
            .iter()
            .map(|r| format!("{} to {}", r.calls, r.name))
            .collect::<Vec<_>>()
            .join(", ");
        let unattached = match split.unattached {
            0 => String::new(),
            1 => "1 to an unattached repository".to_string(),
            n => format!("{n} to unattached repositories"),
        };
        match (references.is_empty(), unattached.is_empty()) {
            (true, true) => String::new(),
            (false, true) => format!("({references})"),
            (true, false) => format!("({unattached})"),
            (false, false) => format!("({references}; {unattached})"),
        }
    }

    /// The counts of several review attempts taken together, such as the
    /// attempts of one run.
    ///
    /// Totals add up. The split is known only when every attempt that made a
    /// call had a known split, because a call of an attempt whose references
    /// are unknown may have gone to any of them, and a summed zero would
    /// claim it did not. References merge by name, in the order each name
    /// first appears, and calls to unattached repositories add up.
    ///
    /// Attempts that made no call add nothing to any count and do not decide
    /// whether the split is known. Only when no attempt made a call do they
    /// decide it, by the same rule, so that a run that has not made its first
    /// call yet still says which references it has, as its single attempt
    /// would. With nothing to sum the result is zero with an unknown split.
    pub fn sum<'a>(counts: impl IntoIterator<Item = &'a ToolCallCounts>) -> ToolCallCounts {
        let counts: Vec<&ToolCallCounts> = counts.into_iter().collect();
        let total = counts
            .iter()
            .fold(0u64, |sum, c| sum.saturating_add(c.total));
        let calling: Vec<&ToolCallCounts> =
            counts.iter().copied().filter(|c| c.total > 0).collect();
        let deciding = if calling.is_empty() {
            &counts
        } else {
            &calling
        };
        let by_reference = if deciding.is_empty() {
            None
        } else {
            deciding
                .iter()
                .map(|c| c.by_reference.as_ref())
                .collect::<Option<Vec<&ReferenceSplit>>>()
                .map(|splits| ReferenceSplit::merge(&splits))
        };
        ToolCallCounts {
            total,
            by_reference,
        }
    }
}

impl ReferenceSplit {
    /// Several splits taken together: references merged by name, in the
    /// order each name first appears, and unattached calls added up.
    fn merge(splits: &[&ReferenceSplit]) -> ReferenceSplit {
        let mut merged = ReferenceSplit::default();
        for split in splits {
            merged.unattached = merged.unattached.saturating_add(split.unattached);
            for reference in &split.references {
                match merged
                    .references
                    .iter_mut()
                    .find(|r| r.name == reference.name)
                {
                    Some(r) => r.calls = r.calls.saturating_add(reference.calls),
                    None => merged.references.push(reference.clone()),
                }
            }
        }
        merged
    }
}

/// One patch's tool calls on the activity poll, summed over its current run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PatchToolCalls {
    pub patch_id: i64,
    /// Review rows in the patch's current run; more than one when the run
    /// was retried.
    pub attempts: u32,
    #[serde(flatten)]
    pub counts: ToolCallCounts,
}

/// One review row of a patch, as far as telling its runs apart needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewAttempt {
    pub review_id: i64,
    /// The retry the row was for, or `None` for a row that is no attempt of
    /// the retry loop: a Skipped patch's, or one written before attempts
    /// were recorded.
    pub attempt: Option<u32>,
}

/// One patch's review rows split into runs, each run its review ids in id
/// order, the runs oldest first.
///
/// A run is one pass of the daemon's retry loop over the patch. A row for
/// attempt 1, or with no attempt, starts a new run, and a row for a later
/// attempt continues the run before it. Rows are taken in id order whatever
/// order they are given in, because ids are the order the rows were created
/// in and a retry's row is always created after the row it retries.
pub fn review_runs(rows: &[ReviewAttempt]) -> Vec<Vec<i64>> {
    let mut rows = rows.to_vec();
    rows.sort_by_key(|row| row.review_id);
    let mut runs: Vec<Vec<i64>> = Vec::new();
    for row in rows {
        let continues = row.attempt.is_some_and(|attempt| attempt > 1);
        match runs.last_mut() {
            Some(run) if continues => run.push(row.review_id),
            _ => runs.push(vec![row.review_id]),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A newer daemon may report a phase this client has never heard of; the
    /// entry still reads, by its description, and so does the rest.
    #[test]
    fn test_an_unknown_phase_does_not_spoil_the_response() {
        let body = serde_json::json!({
            "live": true,
            "entries": [
                {"key": "patchset:1", "phase": {"kind": "from_the_future"},
                 "description": "doing something new", "patch_id": null, "stage": null,
                 "stage_label": null, "order": null, "age_seconds": 3, "idle_seconds": 1,
                 "updated_at": null},
                {"key": "patchset:1/patch:2", "phase": {"kind": "queued"},
                 "description": "queued", "patch_id": 2, "stage": null,
                 "stage_label": null, "order": null, "age_seconds": 3, "idle_seconds": 1,
                 "updated_at": null},
            ],
        });
        let resp: PatchsetActivityResponse = serde_json::from_value(body).unwrap();
        assert_eq!(resp.entries[0].phase, None);
        assert_eq!(resp.entries[0].description, "doing something new");
        assert_eq!(resp.entries[1].phase, Some(crate::activity::Phase::Queued));
        // An older daemon sends no `finished`; it reads as still running.
        assert!(!resp.finished);
    }

    fn tool_message() -> crate::ai::AiMessage {
        crate::ai::AiMessage {
            role: crate::ai::AiRole::Tool,
            content: Some("out".to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: Some("c0".to_string()),
        }
    }

    /// The live log entry is the stored-log entry with its position added,
    /// so the page renders both with the same code.
    #[test]
    fn test_a_live_log_entry_is_a_message_with_its_position() {
        let mut stored = ReviewLogEntry::new(Some("goal"), tool_message());
        stored.stage_label = Some("Goal".to_string());
        let entry = LiveLogEntry {
            seq: 3,
            created_at: 100,
            entry: stored.clone(),
        };
        let value = serde_json::to_value(&entry).unwrap();
        assert_eq!(value["role"], "tool");
        assert_eq!(value["content"], "out");
        assert_eq!(value["tool_call_id"], "c0");
        assert_eq!(value["seq"], 3);
        assert_eq!(value["stage"], "goal");
        let back: LiveLogEntry = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(back, entry);

        // The stored part is the live entry without its place in the stream.
        let mut without_position = value;
        if let Some(fields) = without_position.as_object_mut() {
            fields.remove("seq");
            fields.remove("created_at");
        }
        assert_eq!(serde_json::to_value(&stored).unwrap(), without_position);
    }

    /// A stored log entry holds the message and its stage, and nothing of the
    /// stream it may have come from; one with no stage is the bare message.
    #[test]
    fn test_a_stored_log_entry_is_a_message_with_its_stage() {
        let mut staged = ReviewLogEntry::new(Some("goal"), tool_message());
        staged.label_stage(crate::project::ProjectId::Linux);
        let value = serde_json::to_value(&staged).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .map(|o| o.keys().map(String::as_str).collect())
            .unwrap_or_default();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["content", "role", "stage", "stage_label", "tool_call_id"]
        );
        assert_eq!(
            serde_json::to_value(ReviewLogEntry::new(None, tool_message())).unwrap(),
            serde_json::to_value(tool_message()).unwrap(),
            "a message of no stage is stored as the message alone"
        );
    }

    /// Rows written by older builds read as entries: a worker's history held
    /// bare messages, and a promoted live log held live entries, whose stream
    /// fields are ignored.
    #[test]
    fn test_a_stored_log_entry_reads_rows_of_older_builds() {
        let bare: ReviewLogEntry =
            serde_json::from_value(serde_json::to_value(tool_message()).unwrap()).unwrap();
        assert_eq!(bare, ReviewLogEntry::new(None, tool_message()));

        let promoted: ReviewLogEntry = serde_json::from_value(serde_json::json!({
            "seq": 4, "stage": "goal", "stage_label": "Goal", "created_at": 100,
            "role": "tool", "content": "out", "tool_call_id": "c0",
        }))
        .unwrap();
        let mut expected = ReviewLogEntry::new(Some("goal"), tool_message());
        expected.stage_label = Some("Goal".to_string());
        assert_eq!(promoted, expected);
    }

    fn counts(total: u64, references: &[(&str, u64)], unattached: u64) -> ToolCallCounts {
        ToolCallCounts {
            total,
            by_reference: Some(ReferenceSplit {
                references: references
                    .iter()
                    .map(|(name, calls)| ReferenceCalls {
                        name: (*name).to_string(),
                        calls: *calls,
                    })
                    .collect(),
                unattached,
            }),
        }
    }

    /// The clause names every attached reference, zeros included, and adds
    /// calls to unattached repositories only when there are some.
    #[test]
    fn tool_call_breakdown_names_every_attached_reference() {
        assert_eq!(
            counts(350, &[("kernel", 72), ("rdma-core", 0)], 0).breakdown(),
            "(72 to kernel, 0 to rdma-core)"
        );
        assert_eq!(
            counts(350, &[("kernel", 72)], 3).breakdown(),
            "(72 to kernel; 3 to unattached repositories)"
        );
        assert_eq!(
            counts(5, &[], 1).breakdown(),
            "(1 to an unattached repository)"
        );
        assert_eq!(
            counts(5, &[], 3).breakdown(),
            "(3 to unattached repositories)"
        );
    }

    /// No clause where it would mislead: a review that made no call, an
    /// unknown split, or a split with nothing in it.
    #[test]
    fn tool_call_breakdown_is_empty_without_a_known_split_or_calls() {
        assert_eq!(counts(0, &[("kernel", 0)], 0).breakdown(), "");
        let unknown = ToolCallCounts {
            total: 12,
            by_reference: None,
        };
        assert_eq!(unknown.breakdown(), "");
        assert_eq!(counts(12, &[], 0).breakdown(), "");
    }

    /// A review served by a daemon that knows no split, or written before
    /// one existed, reads as an unknown split rather than failing.
    #[test]
    fn tool_call_counts_without_a_split_read_as_unknown() {
        let read: ToolCallCounts = serde_json::from_value(serde_json::json!({"total": 4})).unwrap();
        assert_eq!(
            read,
            ToolCallCounts {
                total: 4,
                by_reference: None
            }
        );
        let wire = counts(9, &[("kernel", 2)], 1);
        let back: ToolCallCounts =
            serde_json::from_value(serde_json::to_value(&wire).unwrap()).unwrap();
        assert_eq!(back, wire);
    }

    /// An older daemon sends no tool calls on the activity poll; its answer
    /// still reads, with none. A newer one sends each patch's counts flat
    /// beside its patch id and attempts, and they read back as sent.
    #[test]
    fn activity_tool_calls_are_optional_and_flat() {
        let older: PatchsetActivityResponse =
            serde_json::from_value(serde_json::json!({"live": true, "entries": []})).unwrap();
        assert!(older.tool_calls.is_empty());

        let patch = PatchToolCalls {
            patch_id: 20,
            attempts: 2,
            counts: counts(47, &[("kernel", 3)], 0),
        };
        let wire = serde_json::to_value(&patch).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "patch_id": 20,
                "attempts": 2,
                "total": 47,
                "by_reference": {"references": [{"name": "kernel", "calls": 3}], "unattached": 0},
            })
        );
        let back: PatchToolCalls = serde_json::from_value(wire).unwrap();
        assert_eq!(back, patch);

        // A patch whose split is unknown may be sent without one.
        let unknown: PatchToolCalls =
            serde_json::from_value(serde_json::json!({"patch_id": 21, "attempts": 1, "total": 5}))
                .unwrap();
        assert_eq!(unknown.counts.by_reference, None);
    }

    /// Review rows of one patch, as (id, attempt) pairs.
    fn attempts(rows: &[(i64, Option<u32>)]) -> Vec<ReviewAttempt> {
        rows.iter()
            .map(|&(review_id, attempt)| ReviewAttempt { review_id, attempt })
            .collect()
    }

    #[test]
    fn review_runs_start_at_attempt_one_or_no_attempt() {
        // One run retried three times.
        assert_eq!(
            review_runs(&attempts(&[
                (1, Some(1)),
                (2, Some(2)),
                (3, Some(3)),
                (4, Some(4))
            ])),
            vec![vec![1, 2, 3, 4]]
        );
        // Two runs of one attempt each: a rerun.
        assert_eq!(
            review_runs(&attempts(&[(1, Some(1)), (2, Some(1))])),
            vec![vec![1], vec![2]]
        );
        // A row with no attempt starts a run that its retry continues.
        assert_eq!(
            review_runs(&attempts(&[(1, None), (2, Some(2))])),
            vec![vec![1, 2]]
        );
        // A rerun after a retried run: the latest run is the last row alone,
        // even before it has made a call.
        assert_eq!(
            review_runs(&attempts(&[(1, Some(1)), (2, Some(2)), (3, Some(1))])),
            vec![vec![1, 2], vec![3]]
        );
        // Two rows with no attempt are two runs.
        assert_eq!(
            review_runs(&attempts(&[(1, None), (2, None)])),
            vec![vec![1], vec![2]]
        );
    }

    #[test]
    fn review_runs_follow_id_order_and_survive_odd_rows() {
        // Given out of order, the rows are still read in the order they were
        // created in.
        assert_eq!(
            review_runs(&attempts(&[(3, Some(1)), (2, Some(2)), (1, Some(1))])),
            vec![vec![1, 2], vec![3]]
        );
        // A retry whose earlier rows are gone is a run of its own. Reruns
        // made before reruns renumbered the rows they keep could leave such
        // a row behind.
        assert_eq!(review_runs(&attempts(&[(5, Some(2))])), vec![vec![5]]);
        assert!(review_runs(&[]).is_empty());
    }

    fn unknown(total: u64) -> ToolCallCounts {
        ToolCallCounts {
            total,
            by_reference: None,
        }
    }

    #[test]
    fn summed_counts_add_up_and_merge_references_by_name() {
        // Three attempts with 40, 0 and 7 calls.
        let run = [
            counts(40, &[("kernel", 10)], 1),
            counts(0, &[("kernel", 0)], 0),
            counts(7, &[("kernel", 2)], 0),
        ];
        assert_eq!(ToolCallCounts::sum(&run), counts(47, &[("kernel", 12)], 1));

        // Attempts with different references merge by name, in the order
        // each name first appears.
        let run = [
            counts(5, &[("kernel", 3)], 0),
            counts(6, &[("rdma-core", 1), ("kernel", 2)], 2),
        ];
        assert_eq!(
            ToolCallCounts::sum(&run),
            counts(11, &[("kernel", 5), ("rdma-core", 1)], 2)
        );
    }

    #[test]
    fn a_summed_split_is_unknown_when_an_attempt_with_calls_had_none() {
        // An attempt that made calls without a known split may have read any
        // reference, so the run's split is unknown.
        let run = [counts(5, &[("kernel", 3)], 0), unknown(4)];
        assert_eq!(ToolCallCounts::sum(&run), unknown(9));

        // An attempt with no split that made no call changes nothing.
        let run = [counts(5, &[("kernel", 3)], 0), unknown(0)];
        assert_eq!(ToolCallCounts::sum(&run), counts(5, &[("kernel", 3)], 0));
    }

    #[test]
    fn a_summed_run_without_calls_keeps_its_references() {
        // A run that has not made its first call reads as its attempt does.
        let fresh = counts(0, &[("kernel", 0)], 0);
        assert_eq!(ToolCallCounts::sum([&fresh]), fresh);
        assert_eq!(
            ToolCallCounts::sum(&[fresh.clone(), unknown(0)]),
            unknown(0)
        );
        // Nothing to sum is nothing known.
        assert_eq!(ToolCallCounts::sum(&[]), unknown(0));
    }
}
