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
}

#[derive(Deserialize)]
pub struct BugQuery {
    pub id: Option<i64>,
    pub bugid: Option<String>,
    pub slug: Option<String>,
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
}

/// Query for `/api/patchset/activity`.
#[derive(Debug, Deserialize)]
pub struct PatchsetActivityQuery {
    pub id: i64,
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
    }
}
