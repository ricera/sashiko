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

use crate::project::ProjectId;

pub mod guard;
#[cfg(feature = "server")]
pub mod linux_bug;
pub mod linux_patch_review;
pub mod sashiko_patch_review;

/// Returns the short UI label for a review stage belonging to `project`.
pub fn stage_short_label(project: ProjectId, stage: &str) -> Option<&'static str> {
    match project {
        ProjectId::Linux => linux_patch_review::stage_short_label(stage),
        ProjectId::Sashiko => sashiko_patch_review::stage_short_label(stage),
    }
}

/// Rank of a review stage of `project` in pipeline order, or `None` for a
/// name the project's tables do not know (including the pre-screen and the
/// planner, which are not stage rows).
///
/// Ordering lives on the daemon side because only the daemon knows which
/// project it reviews; clients receive the rank rather than deriving one from
/// the name.
pub fn stage_order(project: ProjectId, stage: &str) -> Option<u32> {
    match project {
        ProjectId::Linux => linux_patch_review::stage_order(stage),
        ProjectId::Sashiko => sashiko_patch_review::stage_order(stage),
    }
}

/// Whether a stage of `project` is shown as its own row of live activity.
///
/// The single predicate shared by the daemon's decoding of worker progress and
/// its model-slot observer. The pre-screen and the planner tag their model calls
/// like every other stage, and the worker reports their turns too (they are
/// counted stages for the terminal progress bar), but their progress is the
/// patch's phase rather than a stage row. A row opened for one of them would
/// have nobody responsible for closing it, and would sit at its first reported
/// wait until the whole patchset ended.
pub fn reports_stage_row(project: ProjectId, stage: &str) -> bool {
    stage_short_label(project, stage).is_some()
}

/// Returns the default total stage count (all analysis + consolidation stages)
/// when dynamic planning has not yet narrowed the fan-out.
pub fn default_stage_count(project: ProjectId) -> usize {
    match project {
        ProjectId::Linux => {
            linux_patch_review::ANALYSIS_STAGES.len()
                + linux_patch_review::CONSOLIDATION_STAGES.len()
        }
        ProjectId::Sashiko => {
            sashiko_patch_review::ANALYSIS_STAGES.len()
                + sashiko_patch_review::CONSOLIDATION_STAGES.len()
        }
    }
}

/// Whether a stage counts towards the review progress display.
///
/// The analysis and consolidation stages are the ones `planned_stages_from()`
/// totals in advance. The pre-screen and the planner cannot be totalled that
/// way, because whether either runs depends on `--stages`, so the display counts
/// them as it sees them start instead. Either way a stage that finishes has to
/// say so, or the bar stops short of the work it did.
pub fn is_counted_stage(project: ProjectId, name: &str) -> bool {
    stage_short_label(project, name).is_some() || matches!(name, "pre-screen" | "planning")
}

/// Resolves the ordered list of stages a review will run (analysis fan-out
/// followed by consolidation stages).
pub fn planned_stages_from(project: ProjectId, stage_names: &[&'static str]) -> Vec<String> {
    match project {
        ProjectId::Linux => {
            let mut planned: Vec<String> = stage_names
                .iter()
                .filter(|n| linux_patch_review::analysis_stage_by_name(n).is_some())
                .map(|n| n.to_string())
                .collect();
            if !planned.is_empty() {
                planned.extend(
                    linux_patch_review::CONSOLIDATION_STAGES
                        .iter()
                        .map(|s| s.name.to_string()),
                );
            }
            planned
        }
        ProjectId::Sashiko => {
            let mut planned: Vec<String> = stage_names
                .iter()
                .filter(|n| sashiko_patch_review::analysis_stage_by_name(n).is_some())
                .map(|n| n.to_string())
                .collect();
            if !planned.is_empty() {
                planned.extend(
                    sashiko_patch_review::CONSOLIDATION_STAGES
                        .iter()
                        .map(|s| s.name.to_string()),
                );
            }
            planned
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Display order comes from the tables, in the order the pipeline runs
    /// them, for each project separately.
    #[test]
    fn test_stage_order_follows_each_projects_pipeline() {
        for project in [ProjectId::Linux, ProjectId::Sashiko] {
            let names: Vec<&str> = match project {
                ProjectId::Linux => linux_patch_review::ANALYSIS_STAGES
                    .iter()
                    .map(|s| s.name)
                    .chain(
                        linux_patch_review::CONSOLIDATION_STAGES
                            .iter()
                            .map(|s| s.name),
                    )
                    .collect(),
                ProjectId::Sashiko => sashiko_patch_review::ANALYSIS_STAGES
                    .iter()
                    .map(|s| s.name)
                    .chain(
                        sashiko_patch_review::CONSOLIDATION_STAGES
                            .iter()
                            .map(|s| s.name),
                    )
                    .collect(),
            };
            for (rank, name) in names.iter().enumerate() {
                assert_eq!(stage_order(project, name), Some(rank as u32), "{name}");
                assert!(reports_stage_row(project, name), "{name}");
            }
            // The planner and the pre-screen are the patch's phase, not rows,
            // and have no rank; nor does a name the table does not know.
            for name in ["planning", "pre-screen", "nonexistent"] {
                assert_eq!(stage_order(project, name), None, "{name}");
                assert!(!reports_stage_row(project, name), "{name}");
            }
        }
        // Names are matched the way the rest of the pipeline matches them.
        assert_eq!(stage_order(ProjectId::Linux, "Stage_Goal"), Some(0));
        assert_eq!(stage_order(ProjectId::Sashiko, "llm-pipeline"), Some(5));
    }
}
