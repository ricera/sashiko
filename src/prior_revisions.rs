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

//! What the earlier revisions of a pull request were told, as the daemon
//! sends it to the review of a later revision.
//!
//! The daemon reads the findings of a pull request's earlier revisions once
//! per patchset and cuts each one down to what a summary of it shows: its
//! severity, its title, what it leads to and where it is. The same list goes
//! into the payload of every patch, so these types are the wire format
//! between the daemon and the worker, and nothing in them is a field no
//! summary reads.

use crate::baseline::CommitId;
use crate::db::Severity;
use crate::finding_view::{FindingLocation, clip_title, consequence_of, one_line};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Most earlier revisions a review is shown, counting only revisions that
/// have findings, newest first.
pub const MAX_PRIOR_REVISIONS: usize = 5;

/// Longest consequence a summary of an earlier finding carries, in code
/// points. The consequence is one line of a summary, not the argument.
pub const CONSEQUENCE_MAX_CHARS: usize = 300;

/// Most locations a summary of an earlier finding carries. The first two
/// name where the defect is and, usually, where it is reached from.
pub const MAX_PRIOR_LOCATIONS: usize = 2;

/// One earlier revision of a pull request and the findings its reviews
/// stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorRevision {
    /// How the revision is named to the model: "v3" from its subject, or
    /// "patchset 41" when its subject carries no pull request prefix.
    pub label: String,
    /// The revision's head commit, from its placeholder id. It is not shown
    /// to the model; a link to one of its findings carries it to the page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<CommitId>,
    /// Whether the revision's base is the base of the revision under
    /// review, both from their placeholder ids: what a note that a patch is
    /// the same change adds about the code around it. `None` when either
    /// base is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_base: Option<bool>,
    /// The revision's patches that have findings, in part order.
    pub patches: Vec<PriorPatch>,
}

/// One patch of an earlier revision, with the findings stored for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorPatch {
    /// The patch's part number within its revision.
    pub index: i64,
    /// The patch's subject, made one line.
    pub subject: String,
    /// The forge commit of this earlier patch, when its message id is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<CommitId>,
    /// Index of the patch under review with the same git patch-id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_change_as: Option<i64>,
    /// The patch as stored; only on the newest returned revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// The patch's findings, newest review first, without literal
    /// duplicates.
    pub findings: Vec<PriorFinding>,
}

/// A finding of an earlier revision, cut down to what its summary shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorFinding {
    pub severity: Severity,
    /// The finding's `problem`, made one line and clipped at
    /// `finding_view::TITLE_MAX_CHARS`: the title a later finding names it
    /// by.
    pub problem: String,
    /// What the finding leads to (`finding_view::consequence_of`), clipped
    /// at `CONSEQUENCE_MAX_CHARS`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consequence: Option<String>,
    /// At most `MAX_PRIOR_LOCATIONS` locations that name something.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<FindingLocation>,
}

impl PriorFinding {
    /// Cuts a stored finding down to its summary. `None` when its `problem`
    /// is blank: such a finding has no title, so no later finding could name
    /// it, and nothing in it says what the defect was.
    pub fn from_stored(
        severity: Severity,
        problem: &str,
        severity_explanation: Option<&str>,
        locations: Option<&Value>,
    ) -> Option<Self> {
        // The blank check, the title and `literal_duplicate_key` read the
        // same one-line text, so a problem made only of invisible or control
        // characters is blank rather than listed as "(no description)".
        let problem = one_line(problem);
        if problem.is_empty() {
            return None;
        }
        let locations = locations
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(FindingLocation::from_json)
                    .take(MAX_PRIOR_LOCATIONS)
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            severity,
            problem: clip_title(&problem),
            consequence: severity_explanation
                .and_then(|text| consequence_of(text, CONSEQUENCE_MAX_CHARS)),
            locations,
        })
    }
}

/// The key under which two stored findings of one patch count as the same
/// finding: its `problem` made one line by `finding_view::one_line`, as
/// `PriorFinding::from_stored` reads it for the title, and case-folded, so
/// two problems that read the same once made one line share one key. The
/// key covers the whole problem, not the clipped title. It catches one
/// finding stored by two runs, not a reworded repeat; recognising a repeat
/// is the model's job.
pub fn literal_duplicate_key(problem: &str) -> String {
    one_line(problem).to_lowercase()
}

/// The index of the patch under review whose git patch-id equals
/// `earlier`, given the patch-id of each patch under review. Both ids must
/// be present: a patch stored before patch-ids were recorded has none, and
/// two missing ids say nothing about whether the changes are the same.
pub fn same_change_as(earlier: Option<&str>, current: &[(i64, Option<String>)]) -> Option<i64> {
    let earlier = earlier?;
    current
        .iter()
        .find(|(_, patch_id)| patch_id.as_deref() == Some(earlier))
        .map(|(index, _)| *index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn location(file: &str, function: &str, line: u64) -> FindingLocation {
        FindingLocation {
            file: Some(file.to_string()),
            function_or_symbol: Some(function.to_string()),
            line: Some(line),
        }
    }

    /// A summary keeps the title, the consequence and the first two
    /// locations that name something, and nothing of the argument.
    #[test]
    fn a_stored_finding_is_cut_down_to_its_summary() {
        let explanation = "Consequence: every failed refill leaks a page.\n\
                           Triggering path: the allocator fails under memory pressure.";
        let locations = json!([
            {"file": "a.c", "function_or_symbol": "fill", "line": 12},
            {"line": 3},
            {"file": "b.c", "function_or_symbol": "drain", "line": 40},
            {"file": "c.c", "function_or_symbol": "unused", "line": 1}
        ]);
        let finding = PriorFinding::from_stored(
            Severity::High,
            "  leak of rx\nbuffers  ",
            Some(explanation),
            Some(&locations),
        )
        .unwrap();
        assert_eq!(
            finding,
            PriorFinding {
                severity: Severity::High,
                problem: "leak of rx buffers".to_string(),
                consequence: Some("every failed refill leaks a page.".to_string()),
                locations: vec![location("a.c", "fill", 12), location("b.c", "drain", 40)],
            }
        );
    }

    /// A long title and a long consequence are clipped, and a finding
    /// without an explanation or locations has none.
    #[test]
    fn a_summary_is_clipped_and_may_be_bare() {
        let long = "word ".repeat(200);
        let finding =
            PriorFinding::from_stored(Severity::Low, &long, Some(&long), Some(&json!({}))).unwrap();
        assert!(
            finding.problem.ends_with('…')
                && finding.problem.chars().count() <= crate::finding_view::TITLE_MAX_CHARS + 1,
            "{finding:?}"
        );
        assert!(
            finding
                .consequence
                .as_ref()
                .is_some_and(|c| c.chars().count() <= CONSEQUENCE_MAX_CHARS + 1 && c.ends_with('…')),
            "{finding:?}"
        );
        assert!(finding.locations.is_empty(), "a non-array has no locations");

        let bare = PriorFinding::from_stored(Severity::Medium, "race", None, None).unwrap();
        assert_eq!(bare.consequence, None);
        assert!(bare.locations.is_empty());
    }

    /// A problem with nothing a summary would show, whitespace or only
    /// invisible and control characters, gives no title.
    #[test]
    fn a_finding_without_a_title_is_no_summary() {
        for problem in [" \n", "\u{200b}", "\u{202e}\x1b", "\x07 \u{2066}"] {
            assert_eq!(
                PriorFinding::from_stored(Severity::High, problem, Some("Consequence: x."), None),
                None,
                "{problem:?}"
            );
        }
    }

    /// Two problems that show the same title are one finding, whatever
    /// case, whitespace or invisible characters tell them apart.
    #[test]
    fn literal_duplicates_differ_only_in_what_a_title_does_not_show() {
        for problem in [
            "  Leak in Fill \n",
            "leak\tin fill",
            "leak  in\u{200b}fill",
            "\u{202e}leak in fill",
        ] {
            assert_eq!(
                literal_duplicate_key(problem),
                literal_duplicate_key("leak in fill"),
                "{problem:?}"
            );
        }
        assert_ne!(
            literal_duplicate_key("leak in fill"),
            literal_duplicate_key("leak in drain")
        );
    }

    /// Equal patch-ids name the same change; a missing id on either side
    /// never does, so two missing ids are not a match.
    #[test]
    fn the_same_change_needs_two_patch_ids() {
        let current = [(1, None), (2, Some("abc".to_string())), (3, None)];
        assert_eq!(same_change_as(Some("abc"), &current), Some(2));
        assert_eq!(same_change_as(Some("def"), &current), None);
        assert_eq!(same_change_as(None, &current), None);
    }

    /// The summaries cross from the daemon to the worker as JSON, and read
    /// back as they were sent; absent parts are left out of the JSON.
    #[test]
    fn a_prior_revision_reads_back_from_its_json() {
        let revision = PriorRevision {
            label: "v2".to_string(),
            head: CommitId::parse(&"c".repeat(40)),
            same_base: Some(false),
            patches: vec![PriorPatch {
                index: 1,
                subject: "net: frob the widget".to_string(),
                commit: CommitId::parse(&"d".repeat(40)),
                same_change_as: Some(1),
                diff: Some("+frob();".to_string()),
                findings: vec![PriorFinding {
                    severity: Severity::Critical,
                    problem: "use after free".to_string(),
                    consequence: None,
                    locations: vec![location("w.c", "frob", 7)],
                }],
            }],
        };
        let value = serde_json::to_value(&revision).unwrap();
        assert_eq!(value["patches"][0]["findings"][0]["severity"], "Critical");
        assert!(
            value["patches"][0]["findings"][0]
                .get("consequence")
                .is_none()
        );
        assert_eq!(
            serde_json::from_value::<PriorRevision>(value).unwrap(),
            revision
        );

        let bare = json!({"label": "patchset 4", "patches": []});
        let bare = serde_json::from_value::<PriorRevision>(bare).unwrap();
        assert_eq!((bare.head, bare.same_base), (None, None));
    }
}
