pub mod metrics;
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

#[cfg(feature = "server")]
pub mod access;
pub mod activity;
pub mod ai;
pub mod api;
pub mod auth;
#[cfg(feature = "server")]
pub mod backfill;
pub mod baseline;
#[cfg(feature = "server")]
pub mod compression;
pub mod cover_letter;
pub mod db;
#[cfg(feature = "server")]
pub mod email_policy;
#[cfg(feature = "server")]
pub mod email_router;
#[cfg(feature = "server")]
pub mod events;
#[cfg(feature = "server")]
pub mod fetcher;
pub mod finding_view;
#[cfg(feature = "server")]
pub mod forge;
pub mod git_cmd;
pub mod git_ops;
#[cfg(feature = "server")]
pub mod ingestor;
pub mod local_review;
pub mod maintainers;
#[cfg(feature = "server")]
pub mod mbox;
#[cfg(feature = "server")]
pub mod nntp;
pub mod patch;
#[cfg(feature = "server")]
pub mod patchwork;
#[cfg(feature = "server")]
pub mod prerequisites;
pub mod prior_revisions;
pub mod project;
pub mod prompt_bundle;
pub mod references;
#[cfg(feature = "server")]
pub mod reviewer;
#[cfg(feature = "server")]
pub mod server;
pub mod settings;
#[cfg(test)]
pub(crate) mod test_support;
pub mod toolbox;
pub mod utils;
pub mod worker;
pub mod workflow;
pub mod workflows;

use std::fmt;
use std::str::FromStr;

/// Represents the current status of a patchset review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewStatus {
    /// The patchset is not yet fully received (e.g., waiting for more parts).
    Incomplete,
    /// The patchset is complete and waiting for review.
    Pending,
    /// The patchset is currently under AI review.
    InReview,
    /// The review process was cancelled.
    Cancelled,
    /// The review was skipped (e.g., due to configuration or filtering).
    Skipped,
    /// The review process completed successfully.
    Reviewed,
    /// The review ran out of time. Its findings come from the stages that
    /// finished, so its coverage is incomplete. It is neither a failure nor
    /// `Reviewed`, so no gate that asks "was this reviewed?" accepts it.
    Partial,
    /// The review process failed due to an error.
    Failed,
    /// The patchset failed to apply to the baseline.
    FailedToApply,
}

impl fmt::Display for ReviewStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReviewStatus::Incomplete => write!(f, "Incomplete"),
            ReviewStatus::Pending => write!(f, "Pending"),
            ReviewStatus::InReview => write!(f, "In Review"),
            ReviewStatus::Cancelled => write!(f, "Cancelled"),
            ReviewStatus::Skipped => write!(f, "Skipped"),
            ReviewStatus::Reviewed => write!(f, "Reviewed"),
            ReviewStatus::Partial => write!(f, "Partial"),
            ReviewStatus::Failed => write!(f, "Failed"),
            ReviewStatus::FailedToApply => write!(f, "Failed To Apply"),
        }
    }
}

impl FromStr for ReviewStatus {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Incomplete" => Ok(ReviewStatus::Incomplete),
            "Pending" => Ok(ReviewStatus::Pending),
            "In Review" => Ok(ReviewStatus::InReview),
            "Cancelled" => Ok(ReviewStatus::Cancelled),
            "Skipped" => Ok(ReviewStatus::Skipped),
            "Reviewed" => Ok(ReviewStatus::Reviewed),
            "Partial" => Ok(ReviewStatus::Partial),
            "Failed" => Ok(ReviewStatus::Failed),
            "Failed To Apply" => Ok(ReviewStatus::FailedToApply),
            _ => Err(()),
        }
    }
}

impl ReviewStatus {
    /// Every status, in declaration order. Tests iterate it to check that a
    /// rule holds for each status, so a status added later is checked too.
    /// The `position` match in this file's tests is exhaustive and requires
    /// each variant to name its slot here, so a new variant fails to compile
    /// the tests until it is listed.
    #[cfg(test)]
    pub(crate) const ALL: [ReviewStatus; 9] = [
        ReviewStatus::Incomplete,
        ReviewStatus::Pending,
        ReviewStatus::InReview,
        ReviewStatus::Cancelled,
        ReviewStatus::Skipped,
        ReviewStatus::Reviewed,
        ReviewStatus::Partial,
        ReviewStatus::Failed,
        ReviewStatus::FailedToApply,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            ReviewStatus::Incomplete => "Incomplete",
            ReviewStatus::Pending => "Pending",
            ReviewStatus::InReview => "In Review",
            ReviewStatus::Cancelled => "Cancelled",
            ReviewStatus::Skipped => "Skipped",
            ReviewStatus::Reviewed => "Reviewed",
            ReviewStatus::Partial => "Partial",
            ReviewStatus::Failed => "Failed",
            ReviewStatus::FailedToApply => "Failed To Apply",
        }
    }
}

/// The verdict the reviewer writes to a patch's status column.
///
/// The type is narrower than `ReviewStatus` on purpose: it rules out
/// in-flight values such as `Pending` or `In Review`, which a finished
/// attempt must never leave on a patch. It also carries `TimedOut`, a patch
/// verdict that no review or patchset ever holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchOutcome {
    /// The patch has a full review.
    Reviewed,
    /// The patch's review ran out of time, and its findings were salvaged
    /// from the stages that finished, so its coverage is incomplete.
    Partial,
    /// The patch's review ran out of time with nothing salvaged. A patch
    /// verdict only: its review row is an ordinary `Failed`, and no patchset
    /// ever takes this status, so it is not a `ReviewStatus`.
    TimedOut,
    /// The patch has no review because its attempts failed.
    Failed,
    /// The patch was deliberately not reviewed.
    Skipped,
    /// The review of the patch was cancelled.
    Cancelled,
}

impl PatchOutcome {
    /// The stored spelling. Values shared with `ReviewStatus` take its
    /// spelling so the two vocabularies cannot drift apart.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reviewed => ReviewStatus::Reviewed.as_str(),
            Self::Partial => ReviewStatus::Partial.as_str(),
            Self::TimedOut => "TimedOut",
            Self::Failed => ReviewStatus::Failed.as_str(),
            Self::Skipped => ReviewStatus::Skipped.as_str(),
            Self::Cancelled => ReviewStatus::Cancelled.as_str(),
        }
    }
}

impl fmt::Display for PatchOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub mod logging;

#[cfg(test)]
mod tests {
    use super::*;

    /// The `ReviewStatus` each outcome shares its spelling with, or `None`
    /// for an outcome that is a patch verdict only. The match is exhaustive,
    /// so a new outcome must state its counterpart here.
    fn shared_status(outcome: PatchOutcome) -> Option<ReviewStatus> {
        match outcome {
            PatchOutcome::Reviewed => Some(ReviewStatus::Reviewed),
            PatchOutcome::Partial => Some(ReviewStatus::Partial),
            PatchOutcome::TimedOut => None,
            PatchOutcome::Failed => Some(ReviewStatus::Failed),
            PatchOutcome::Skipped => Some(ReviewStatus::Skipped),
            PatchOutcome::Cancelled => Some(ReviewStatus::Cancelled),
        }
    }

    const ALL_OUTCOMES: [PatchOutcome; 6] = [
        PatchOutcome::Reviewed,
        PatchOutcome::Partial,
        PatchOutcome::TimedOut,
        PatchOutcome::Failed,
        PatchOutcome::Skipped,
        PatchOutcome::Cancelled,
    ];

    /// The slot each status takes in `ReviewStatus::ALL`. The match is
    /// exhaustive, so a new variant must state its slot here, and the test
    /// below fails until `ALL` lists it in that slot.
    fn position(status: ReviewStatus) -> usize {
        match status {
            ReviewStatus::Incomplete => 0,
            ReviewStatus::Pending => 1,
            ReviewStatus::InReview => 2,
            ReviewStatus::Cancelled => 3,
            ReviewStatus::Skipped => 4,
            ReviewStatus::Reviewed => 5,
            ReviewStatus::Partial => 6,
            ReviewStatus::Failed => 7,
            ReviewStatus::FailedToApply => 8,
        }
    }

    #[test]
    fn every_review_status_reads_back_as_itself() {
        for (index, status) in ReviewStatus::ALL.into_iter().enumerate() {
            assert_eq!(position(status), index, "{status:?} is listed out of place");
            assert_eq!(status.to_string(), status.as_str(), "{status:?}");
            assert_eq!(ReviewStatus::from_str(status.as_str()), Ok(status));
        }
    }

    #[test]
    fn partial_is_spelled_partial() {
        assert_eq!(ReviewStatus::Partial.as_str(), "Partial");
        assert_eq!(ReviewStatus::Partial.to_string(), "Partial");
        assert_eq!(ReviewStatus::from_str("Partial"), Ok(ReviewStatus::Partial));
    }

    #[test]
    fn a_patch_outcome_is_spelled_like_its_review_status() {
        for outcome in ALL_OUTCOMES {
            let Some(status) = shared_status(outcome) else {
                // A patch-only verdict must not be mistaken for any status
                // a review or a patchset can hold.
                assert!(
                    ReviewStatus::from_str(outcome.as_str()).is_err(),
                    "{outcome:?} reads as a review status"
                );
                continue;
            };
            assert_eq!(outcome.as_str(), status.as_str(), "{outcome:?}");
            assert_eq!(outcome.to_string(), status.to_string(), "{outcome:?}");
            assert_eq!(
                ReviewStatus::from_str(outcome.as_str()),
                Ok(status),
                "{outcome:?} does not read back as the status it shares"
            );
        }
    }

    #[test]
    fn a_timed_out_patch_is_spelled_timed_out() {
        assert_eq!(PatchOutcome::TimedOut.as_str(), "TimedOut");
        assert_eq!(PatchOutcome::TimedOut.to_string(), "TimedOut");
        assert_eq!(PatchOutcome::Partial.as_str(), "Partial");
    }
}
