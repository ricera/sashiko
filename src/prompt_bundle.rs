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

use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::project::ProjectId;

include!(concat!(env!("OUT_DIR"), "/prompts_generated.rs"));

const COMPLETE_MARKER: &str = ".sashiko-prompts-complete";

/// The installed prompt directory for a project.
///
/// A missing directory is an error rather than a path returned anyway. Every
/// `@include` resolves against this root and silently yields nothing when the
/// file is absent, which is deliberate for an optional guide but would turn a
/// project with no prompts at all into a review that runs with an empty system
/// prompt and reports nothing.
pub fn project_prompts_path(project: ProjectId) -> Result<PathBuf> {
    let root = install_prompt_bundle(false)?;
    let path = root.join(project.prompt_dir());
    if !path.is_dir() {
        anyhow::bail!(
            "no prompts for project {project}: expected a {} directory in the prompt bundle at {}",
            project.prompt_dir(),
            path.display()
        );
    }
    Ok(path)
}

/// Returns the compiled-in content of `kernel/severity.md`.
pub fn kernel_severity_guide() -> &'static str {
    PROMPT_BUNDLE_FILES
        .iter()
        .find(|(path, _)| *path == "kernel/severity.md")
        .and_then(|(_, bytes)| std::str::from_utf8(bytes).ok())
        .expect("kernel/severity.md must exist in prompt bundle")
}

/// Returns the compiled-in content of `sashiko/severity.md`.
pub fn sashiko_severity_guide() -> &'static str {
    PROMPT_BUNDLE_FILES
        .iter()
        .find(|(path, _)| *path == "sashiko/severity.md")
        .and_then(|(_, bytes)| std::str::from_utf8(bytes).ok())
        .expect("sashiko/severity.md must exist in prompt bundle")
}

/// Returns the compiled-in severity guide for `project`.
pub fn severity_guide(project: ProjectId) -> &'static str {
    match project {
        ProjectId::Linux => kernel_severity_guide(),
        ProjectId::Sashiko => sashiko_severity_guide(),
    }
}

pub fn install_prompt_bundle(force: bool) -> Result<PathBuf> {
    let root = prompt_bundle_root()?;
    let marker = root.join(COMPLETE_MARKER);

    if !force && marker.exists() {
        return Ok(root);
    }

    if force && root.exists() {
        std::fs::remove_dir_all(&root)
            .with_context(|| format!("failed to remove {}", root.display()))?;
    }

    for (relative, content) in PROMPT_BUNDLE_FILES {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(&path, content)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    std::fs::write(&marker, PROMPT_BUNDLE_REVISION)
        .with_context(|| format!("failed to write {}", marker.display()))?;

    Ok(root)
}

pub fn prompt_bundle_root() -> Result<PathBuf> {
    Ok(crate::utils::data_home()?
        .join("sashiko/prompts")
        .join(PROMPT_BUNDLE_REVISION))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prompt_bundle_contains_kernel_review_core() {
        assert!(
            PROMPT_BUNDLE_FILES
                .iter()
                .any(|(path, _)| *path == "kernel/review-core.md")
        );
    }

    #[test]
    fn test_kernel_severity_guide_not_empty() {
        let guide = kernel_severity_guide();
        assert!(guide.contains("# Severity Levels"));
        assert!(guide.contains("## Critical"));
        assert!(guide.contains("## High"));
    }

    #[test]
    fn test_sashiko_severity_guide_not_empty() {
        let guide = sashiko_severity_guide();
        assert!(guide.contains("# Severity Levels"));
        assert!(guide.contains("Sashiko is not an operating system kernel"));
        assert_eq!(severity_guide(ProjectId::Linux), kernel_severity_guide());
        assert_eq!(severity_guide(ProjectId::Sashiko), sashiko_severity_guide());
    }

    #[test]
    fn test_prompt_bundle_root_uses_xdg_data_home() {
        let temp = tempfile::tempdir().unwrap();
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        unsafe {
            std::env::set_var("XDG_DATA_HOME", temp.path());
        }

        assert_eq!(
            prompt_bundle_root().unwrap(),
            temp.path()
                .join("sashiko/prompts")
                .join(PROMPT_BUNDLE_REVISION)
        );

        unsafe {
            if let Some(value) = old_xdg {
                std::env::set_var("XDG_DATA_HOME", value);
            } else {
                std::env::remove_var("XDG_DATA_HOME");
            }
        }
    }

    #[test]
    fn test_sashiko_prompt_bundle_integrity() {
        let required_framing = [
            "sashiko/review-core.md",
            "sashiko/severity.md",
            "sashiko/false-positive-guide.md",
            "sashiko/prompt-injection.md",
            "sashiko/github-summary-template.md",
            "sashiko/subsystem/subsystem.md",
        ];
        for path in required_framing {
            assert!(
                PROMPT_BUNDLE_FILES.iter().any(|(p, _)| *p == path),
                "missing framing file in bundle: {path}"
            );
        }

        assert_indexed_guides_bundled("sashiko");
    }

    /// The kernel index is what the Linux pre-screen picks guides from, and a
    /// row naming a file the bundle lacks is skipped without a word when the
    /// guide is included. The RDMA row is checked by name because its guide is
    /// carried on top of the vendored tree and must survive each sync.
    #[test]
    fn test_kernel_prompt_bundle_integrity() {
        let guides = assert_indexed_guides_bundled("kernel");
        assert!(guides.contains(&"rdma.md"), "{guides:?}");
        let index = bundled_text("kernel/subsystem/subsystem.md");
        let row = index
            .lines()
            .find(|l| l.trim_end().ends_with("| rdma.md |"))
            .expect("the RDMA row");
        for trigger in ["drivers/infiniband/", "uverbs", "rdma/drv/"] {
            assert!(row.contains(trigger), "missing {trigger}: {row}");
        }
    }

    fn bundled_text(path: &str) -> &'static str {
        PROMPT_BUNDLE_FILES
            .iter()
            .find(|(p, _)| *p == path)
            .and_then(|(_, bytes)| std::str::from_utf8(bytes).ok())
            .unwrap_or_else(|| panic!("{path} must be bundled and valid UTF-8"))
    }

    /// Parses `{dir}/subsystem/subsystem.md` and checks every guide it names is
    /// in the bundle under `{dir}/subsystem/` or `{dir}/patterns/`, returning
    /// the names.
    fn assert_indexed_guides_bundled(dir: &str) -> Vec<&'static str> {
        let index_path = format!("{dir}/subsystem/subsystem.md");
        let subsystem_index = bundled_text(&index_path);

        let mut referenced_guides = Vec::new();
        for line in subsystem_index.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('|') && trimmed.ends_with('|') {
                let cols: Vec<&str> = trimmed.split('|').map(str::trim).collect();
                // Markdown table row: ["", "Component/Pattern", "Triggers", "File", ""]
                if cols.len() >= 4 {
                    let file_col = cols[cols.len() - 2];
                    if file_col.ends_with(".md") && !file_col.contains(' ') {
                        referenced_guides.push(file_col);
                    }
                }
            }
        }

        assert!(
            !referenced_guides.is_empty(),
            "should parse referenced guides from {index_path}"
        );

        for guide in &referenced_guides {
            let in_subsystem = format!("{dir}/subsystem/{guide}");
            let in_patterns = format!("{dir}/patterns/{guide}");
            let found = PROMPT_BUNDLE_FILES
                .iter()
                .any(|(p, _)| *p == in_subsystem || *p == in_patterns);
            assert!(
                found,
                "guide {guide} referenced in {index_path} not found in bundle under subsystem/ or patterns/"
            );
        }
        referenced_guides
    }
}
