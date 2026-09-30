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

#[cfg(test)]
mod tests {
    use crate::toolbox::ToolBox;
    use serde_json::json;
    use std::path::PathBuf;
    use tokio::runtime::Runtime;

    fn get_test_paths() -> (PathBuf, PathBuf) {
        let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        // Use current repo as the test repo
        let linux_path = root.clone();
        let prompts_path = root.join("third_party/prompts/kernel");
        (linux_path, prompts_path)
    }

    #[test]
    fn test_virtualize_ref() {
        let mut toolbox = ToolBox::new(PathBuf::from("."), None);

        // Without virtual HEAD, should return original
        assert_eq!(toolbox.virtualize_ref("HEAD"), "HEAD");
        assert_eq!(toolbox.virtualize_ref("HEAD~1"), "HEAD~1");
        assert_eq!(toolbox.virtualize_ref("origin/HEAD"), "origin/HEAD");

        // Set virtual HEAD
        toolbox.set_virtual_head("abc123e".to_string());

        // Replacements
        assert_eq!(toolbox.virtualize_ref("HEAD"), "abc123e");
        assert_eq!(toolbox.virtualize_ref("HEAD~1"), "abc123e~1");
        assert_eq!(toolbox.virtualize_ref("HEAD^"), "abc123e^");
        assert_eq!(
            toolbox.virtualize_ref("baseline..HEAD"),
            "baseline..abc123e"
        );
        assert_eq!(
            toolbox.virtualize_ref("HEAD..baseline"),
            "abc123e..baseline"
        );
        assert_eq!(toolbox.virtualize_ref("HEAD:file.c"), "abc123e:file.c");

        // Non-replacements
        assert_eq!(toolbox.virtualize_ref("origin/HEAD"), "origin/HEAD");
        assert_eq!(toolbox.virtualize_ref("origin/HEAD~1"), "origin/HEAD~1");
        assert_eq!(
            toolbox.virtualize_ref("refs/remotes/origin/HEAD"),
            "refs/remotes/origin/HEAD"
        );
        assert_eq!(toolbox.virtualize_ref("FOREHEAD"), "FOREHEAD");
        assert_eq!(toolbox.virtualize_ref("my-HEAD-branch"), "my-HEAD-branch");
        assert_eq!(toolbox.virtualize_ref("HEAD-fixes"), "HEAD-fixes");
    }

    #[test]
    fn test_git_ls_linux() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({ "revision": "HEAD", "path": "." });
        let result = rt.block_on(toolbox.call("git_ls", args)).unwrap();
        let entries = result["entries"].as_array().unwrap();

        assert!(entries.iter().any(|e| e["name"] == "README.md"));
        assert!(entries.iter().any(|e| e["name"] == "Cargo.toml"));
    }

    #[test]
    fn test_read_files_linux_readme() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "revision": "HEAD",
            "files": [
                { "path": "README.md", "start_line": 1, "end_line": 5 }
            ]
        });
        let result = rt.block_on(toolbox.call("git_read_files", args)).unwrap();
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);

        let content = results[0]["content"].as_str().unwrap();

        assert!(!content.is_empty());
        assert!(content.contains("Sashiko"));
    }

    #[test]
    fn test_git_log() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({ "range": "HEAD", "limit": 1 });
        let result = rt.block_on(toolbox.call("git_log", args)).unwrap();
        let output = result["output"].as_str().unwrap();

        assert!(output.contains("commit"));
        assert!(output.contains("Author:"));
    }

    #[test]
    fn test_git_show_head() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({ "object": "HEAD" });
        let result = rt.block_on(toolbox.call("git_show", args)).unwrap();
        let content = result["content"].as_str().unwrap();

        assert!(content.contains("commit"));
        assert!(content.contains("Author:"));
    }

    fn setup_test_repo() -> (tempfile::TempDir, PathBuf) {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path().to_path_buf();

        let run_git = |args: &[&str]| {
            let status = crate::git_cmd::in_dir(&repo_path)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        };

        run_git(&["init"]);
        run_git(&["config", "user.name", "Test User"]);
        run_git(&["config", "user.email", "test@example.com"]);
        run_git(&["commit", "--allow-empty", "-m", "Initial commit"]);
        run_git(&["commit", "--allow-empty", "-m", "Second commit"]);

        (temp_dir, repo_path)
    }

    #[test]
    fn test_git_show_virtual_head() {
        let (_temp_dir, repo_path) = setup_test_repo();
        let mut toolbox = ToolBox::new(repo_path.clone(), None);
        let rt = Runtime::new().unwrap();

        // Resolve actual HEAD~1 SHA
        let output = crate::git_cmd::in_dir(&repo_path)
            .args(["rev-parse", "HEAD~1"])
            .output()
            .unwrap();
        let head_minus_1 = String::from_utf8(output.stdout).unwrap().trim().to_string();

        // Set virtual HEAD to HEAD~1
        toolbox.set_virtual_head(head_minus_1.clone());

        // Call git_show with "HEAD"
        let args = json!({ "object": "HEAD" });
        let result = rt.block_on(toolbox.call("git_show", args)).unwrap();
        let content = result["content"].as_str().unwrap();

        // The content should match the commit info of HEAD~1 (which is head_minus_1)
        assert!(content.contains(&head_minus_1));

        // It should NOT contain the current HEAD SHA
        let output_current = crate::git_cmd::in_dir(&repo_path)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let current_head = String::from_utf8(output_current.stdout)
            .unwrap()
            .trim()
            .to_string();
        assert!(!content.contains(&current_head));
    }
    #[test]
    fn test_git_show_file_full() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({ "object": "HEAD:README.md" });
        let result = rt.block_on(toolbox.call("git_show", args)).unwrap();
        let content = result["content"].as_str().unwrap();

        assert!(content.contains("Sashiko"));
    }

    #[test]
    fn test_git_show_file_range() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "object": "HEAD:README.md",
            "start_line": 1,
            "end_line": 5
        });
        let result = rt.block_on(toolbox.call("git_show", args)).unwrap();
        let content = result["content"].as_str().unwrap();
        let end_line = result["end_line"].as_u64().unwrap();
        let start_line = result["start_line"].as_u64().unwrap();

        assert_eq!(start_line, 1);
        assert_eq!(end_line, 5);
        let lines_count = content.lines().count();
        assert_eq!(lines_count, 5);
    }

    #[test]
    fn test_git_show_file_default_limit() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "object": "HEAD:README.md",
            "start_line": 10
        });
        let result = rt.block_on(toolbox.call("git_show", args)).unwrap();
        let content = result["content"].as_str().unwrap();
        let end_line = result["end_line"].as_u64().unwrap();
        let start_line = result["start_line"].as_u64().unwrap();

        assert_eq!(start_line, 10);
        assert_eq!(end_line, 110);
        let lines_count = content.lines().count();
        assert_eq!(lines_count, 101); // 10 to 110 inclusive is 101 lines
    }

    #[test]
    fn test_git_show_raw_caching() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        // Clear/initialize cache check
        assert!(toolbox.cache.read().unwrap().is_empty());

        // 1. Read subrange lines 1-5
        let args1 = json!({
            "object": "HEAD:README.md",
            "start_line": 1,
            "end_line": 5
        });
        let result1 = rt.block_on(toolbox.call("git_show", args1)).unwrap();
        assert_eq!(result1["start_line"].as_u64().unwrap(), 1);
        assert_eq!(result1["end_line"].as_u64().unwrap(), 5);

        // Verify raw cache was populated
        // The repository is part of the key; see the comment on `raw_key` in
        // git_show.rs for why.
        let raw_key = "git_show_raw:Review:HEAD:README.md:false:None";
        {
            let cache = toolbox.cache.read().unwrap();
            assert!(cache.contains_key(raw_key), "Raw key should be cached");
            assert!(
                cache
                    .get(raw_key)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .contains("Sashiko")
            );
        }

        // 2. Read different subrange lines 10-15 of the same file
        let args2 = json!({
            "object": "HEAD:README.md",
            "start_line": 10,
            "end_line": 15
        });
        let result2 = rt.block_on(toolbox.call("git_show", args2)).unwrap();
        assert_eq!(result2["start_line"].as_u64().unwrap(), 10);
        assert_eq!(result2["end_line"].as_u64().unwrap(), 15);

        // Verify that no extra git_show raw keys were created
        {
            let cache = toolbox.cache.read().unwrap();
            // There should be exactly 3 keys:
            // 1. git_show:{"end_line":5,"object":"HEAD:README.md","start_line":1}
            // 2. git_show_raw:Review:HEAD:README.md:false:None
            // 3. git_show:{"end_line":15,"object":"HEAD:README.md","start_line":10}
            assert_eq!(cache.len(), 3);
        }
    }

    #[test]
    fn test_git_blame_readme() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args =
            json!({ "revision": "HEAD", "path": "README.md", "start_line": 1, "end_line": 3 });
        let result = rt.block_on(toolbox.call("git_blame", args)).unwrap();
        let content = result["content"].as_str().unwrap();

        assert!(!content.is_empty());
        // Typical git blame output starts with hash or (
        // e.g. ^1da177e4c3f (Linus Torvalds 2005-04-16 15:20:36 -0700 1) Linux kernel release 2.6.xx
    }

    #[test]
    fn test_git_blame_truncation() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "revision": "HEAD",
            "path": "src/worker/prompts.rs",
            "start_line": 1,
            "end_line": 3000
        });
        let result = rt.block_on(toolbox.call("git_blame", args)).unwrap();
        assert_eq!(result["truncated"].as_bool(), Some(true));

        let content = result["content"].as_str().unwrap();
        let returned_items = result["metadata"]["returned_items"].as_u64().unwrap() as usize;
        let actual_lines = content.lines().count();

        println!("git_blame returned_items metadata: {}", returned_items);
        println!("git_blame actual returned content lines: {}", actual_lines);

        assert!(actual_lines < 2400, "Blame was not truncated!");
        // returned_items should match the actual lines returned excluding the warning line.
        assert_eq!(
            returned_items + 1,
            actual_lines,
            "returned_items metadata does not match actual lines returned (accounting for warning line)!"
        );

        // Verify end_index calculation is start_index + returned_items - 1
        let start_index = result["metadata"]["start_index"].as_u64().unwrap();
        let end_index = result["metadata"]["end_index"].as_u64().unwrap();
        assert_eq!(end_index, start_index + returned_items as u64 - 1);

        // Verify next_page_hint suggests end_index + 1
        let hint = result["next_page_hint"].as_str().unwrap();
        let expected_next_start = end_index + 1;
        assert!(hint.contains(&format!("start_line={}", expected_next_start)));
    }

    #[test]
    fn test_git_grep_relative_path() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        // Search for "Sashiko" which should be in README.md
        let args = json!({
            "revision": "HEAD",
            "pattern": "Sashiko",
            "path": "README.md"
        });

        let result = rt.block_on(toolbox.call("git_grep", args)).unwrap();
        let content = result["content"].as_str().unwrap();

        assert!(!content.is_empty());
        // Verify path is relative (does not start with /)
        // Check that no line starts with /
        for line in content.lines() {
            assert!(
                !line.starts_with("/"),
                "Line starts with absolute path: {}",
                line
            );
        }

        // Check if README.md matches are found (it might not be the first match)
        assert!(content.contains("README.md") || content.contains("./README.md"));
    }

    #[test]
    fn test_git_grep_hyphen_pattern() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        // Search for a pattern starting with a hyphen (e.g. "-ETIMEDOUT" or "--")
        let args = json!({
            "revision": "HEAD",
            "pattern": "-ETIMEDOUT",
            "path": "src/"
        });

        let result = rt.block_on(toolbox.call("git_grep", args)).unwrap();
        // Should succeed without error even if no matches found
        assert!(
            result.get("content").is_some()
                || result.get("matches").is_some()
                || result.get("message").is_some()
        );
    }

    #[test]
    fn test_git_grep_invalid_revision() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "revision": "--invalid-option",
            "pattern": "foo"
        });

        let result = rt.block_on(toolbox.call("git_grep", args));
        assert!(result.is_err());
    }

    #[test]
    fn test_read_prompt() {
        let (linux_path, prompts_path) = get_test_paths();
        // Enable prompt tool by passing path
        let toolbox = ToolBox::new(linux_path.clone(), Some(prompts_path.clone()));
        let rt = Runtime::new().unwrap();

        // Ensure we have a dummy prompt file to read
        // The real review-prompts might not be populated in test env, check first
        // Or assume technical-patterns.md exists as per repo structure.
        // But tests might run in clean env. Let's create a dummy one if we can or check existence.
        // Since we are running in the actual repo, review-prompts should exist.

        let args = json!({ "name": "technical-patterns.md" });
        if prompts_path.join("technical-patterns.md").exists() {
            let result = rt
                .block_on(toolbox.call("read_prompt", args.clone()))
                .expect("Failed to call read_prompt");
            assert!(result.get("content").is_some());
        } else {
            // If file doesn't exist (e.g. CI), skip assertion on content but check tool availability
            println!("Skipping read_prompt content check: technical-patterns.md not found");
        }

        // Test disabled tool
        let toolbox_disabled = ToolBox::new(linux_path, None);
        let result = rt.block_on(toolbox_disabled.call("read_prompt", args));
        assert!(result.is_err());
    }

    /// read_prompt resolves the requested name against its base, so the base
    /// has to be the prompt directory. Handed a file, every call fails while
    /// canonicalizing, which is how a caller passing `<prompts>/tool.md` went
    /// unnoticed: the tool is optional, so its errors look like the model
    /// simply choosing not to use it.
    #[test]
    fn test_read_prompt_base_is_the_prompt_directory() {
        let (linux_path, prompts_path) = get_test_paths();
        let rt = Runtime::new().unwrap();
        // A guide the workflow already loads by name, so it cannot go missing
        // without breaking the review itself. The nested path also covers the
        // subdirectory case, which is most of what the tool is asked for.
        let args = json!({ "name": "subsystem/locking.md" });

        let toolbox = ToolBox::new(linux_path.clone(), Some(prompts_path.clone()));
        let result = rt
            .block_on(toolbox.call("read_prompt", args.clone()))
            .expect("a name under the prompt directory must resolve");
        assert!(!result["content"].as_str().unwrap_or_default().is_empty());

        // A file as the base cannot resolve anything.
        let wrong = ToolBox::new(linux_path, Some(prompts_path.join("tool.md")));
        assert!(rt.block_on(wrong.call("read_prompt", args)).is_err());
    }

    #[test]
    fn test_git_read_files_truncation() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "revision": "HEAD",
            "files": [
                { "path": "Cargo.lock" }
            ]
        });

        let result = rt.block_on(toolbox.call("git_read_files", args)).unwrap();
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);

        let res = &results[0];
        assert_eq!(res["truncated"].as_bool(), Some(true));

        let content = res["content"].as_str().unwrap();
        let returned_items = res["metadata"]["returned_items"].as_u64().unwrap() as usize;
        let actual_lines = content.lines().count();

        println!("returned_items metadata: {}", returned_items);
        println!("actual returned content lines: {}", actual_lines);

        assert!(actual_lines < 3200, "Content was not truncated!");
        assert_eq!(
            returned_items + 1,
            actual_lines,
            "returned_items metadata does not match actual lines returned (accounting for warning line)!"
        );
    }

    #[test]
    fn test_git_show_truncation() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "object": "HEAD:Cargo.lock",
            "start_line": 1,
            "end_line": 4000
        });

        let result = rt.block_on(toolbox.call("git_show", args)).unwrap();
        assert_eq!(result["truncated"].as_bool(), Some(true));

        let content = result["content"].as_str().unwrap();
        let returned_items = result["metadata"]["returned_items"].as_u64().unwrap() as usize;
        let actual_lines = content.lines().count();

        println!("git_show returned_items metadata: {}", returned_items);
        println!("git_show actual returned content lines: {}", actual_lines);

        assert!(actual_lines < 3200, "Content was not truncated!");
        assert_eq!(
            returned_items + 1,
            actual_lines,
            "git_show returned_items metadata does not match actual lines returned (accounting for warning line)!"
        );
    }

    #[test]
    fn test_git_read_files_empty_file_with_range() {
        // Regression: reading a file that is empty at the revision with an
        // explicit line range must not panic. total_lines is 0 for such a
        // file, and the bounds clamp used to invert (lower 1 > upper 0).
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path().to_path_buf();

        let run_git = |args: &[&str]| {
            let status = crate::git_cmd::in_dir(&repo_path)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        };

        run_git(&["init"]);
        run_git(&["config", "user.name", "Test User"]);
        run_git(&["config", "user.email", "test@example.com"]);
        std::fs::write(repo_path.join("empty.txt"), "").unwrap();
        run_git(&["add", "empty.txt"]);
        run_git(&["commit", "-m", "Add empty file"]);

        let toolbox = ToolBox::new(repo_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "revision": "HEAD",
            "files": [
                { "path": "empty.txt", "start_line": 1, "end_line": 5 }
            ]
        });
        let result = rt.block_on(toolbox.call("git_read_files", args)).unwrap();
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);

        let res = &results[0];
        assert!(res.get("error").is_none(), "unexpected error: {res:?}");
        assert_eq!(res["content"].as_str().unwrap(), "");
        assert_eq!(res["total_lines"].as_u64().unwrap(), 0);
    }

    #[test]
    fn test_git_read_files_with_float_line_ranges() {
        let (linux_path, _prompts_path) = get_test_paths();
        let toolbox = ToolBox::new(linux_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({
            "revision": "HEAD",
            "files": [
                {
                    "path": "README.md",
                    "start_line": 3.0,
                    "end_line": 6.0
                }
            ]
        });

        let result = rt.block_on(toolbox.call("git_read_files", args)).unwrap();
        let results = result["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["start_line"].as_u64().unwrap(), 3);
        assert_eq!(results[0]["end_line"].as_u64().unwrap(), 6);
    }

    /// Builds a reference repo whose content cannot be confused with the review
    /// repo's, so a test can tell which one actually answered.
    ///
    /// The pin, `v6.12`, is deliberately not the tip: one more commit lands on
    /// top of it, as on a tracking clone, so a read that ignores the pin and
    /// serves the tip is visible as `ndo_tip_only`.
    fn setup_reference_repo() -> (tempfile::TempDir, PathBuf) {
        let temp_dir = tempfile::tempdir().unwrap();
        let repo_path = temp_dir.path().to_path_buf();

        let run_git = |args: &[&str]| {
            let status = crate::git_cmd::in_dir(&repo_path)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        };

        run_git(&["init"]);
        run_git(&["config", "user.name", "Test User"]);
        run_git(&["config", "user.email", "test@example.com"]);
        std::fs::create_dir_all(repo_path.join("include/linux")).unwrap();
        std::fs::write(
            repo_path.join("include/linux/netdevice.h"),
            "struct net_device_ops {\n\tint (*ndo_open)(struct net_device *dev);\n};\n",
        )
        .unwrap();
        run_git(&["add", "."]);
        run_git(&["commit", "-m", "Reference kernel tree"]);
        run_git(&["tag", "v6.12"]);
        std::fs::write(
            repo_path.join("include/linux/netdevice.h"),
            "struct net_device_ops {\n\tint (*ndo_open)(struct net_device *dev);\n\
             \tint (*ndo_tip_only)(struct net_device *dev);\n};\n",
        )
        .unwrap();
        run_git(&["commit", "-am", "Past the pin"]);

        (temp_dir, repo_path)
    }

    /// The reference repository pinned at `v6.12`, with the commit it resolves
    /// to filled in as startup validation would.
    fn pinned_reference(path: PathBuf) -> crate::toolbox::ReferenceRepo {
        let commit = rev_parse(&path, "v6.12^{commit}");
        crate::toolbox::ReferenceRepo {
            path,
            revision: "v6.12".to_string(),
            commit,
        }
    }

    /// A reference that is never read, for tests that only inspect schemas.
    fn unread_reference(revision: &str) -> crate::toolbox::ReferenceRepo {
        crate::toolbox::ReferenceRepo {
            path: PathBuf::from("/nonexistent"),
            revision: revision.to_string(),
            commit: "0".repeat(40),
        }
    }

    fn rev_parse(repo: &std::path::Path, rev: &str) -> String {
        let output = crate::git_cmd::in_dir(repo)
            .args(["rev-parse", rev])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// The whole point: a `repo: "kernel"` read must reach the reference tree,
    /// not the repository under review.
    #[test]
    fn reference_reads_hit_the_reference_repo() {
        let (_review_dir, review_path) = setup_test_repo();
        let (_ref_dir, ref_path) = setup_reference_repo();
        let toolbox = ToolBox::new(review_path, None).with_reference(pinned_reference(ref_path));
        let rt = Runtime::new().unwrap();

        let args = json!({
            "repo": "kernel",
            "revision": "v6.12",
            "pattern": "net_device_ops",
            "is_literal": true,
        });
        let result = rt.block_on(toolbox.call("git_grep", args)).unwrap();
        assert!(
            result["content"].as_str().unwrap().contains("netdevice.h"),
            "expected a hit in the reference tree: {result:?}"
        );

        // The review repo has no such file, so the same search there must come
        // back empty rather than quietly reusing the reference answer.
        let args = json!({
            "revision": "HEAD",
            "pattern": "net_device_ops",
            "is_literal": true,
        });
        let result = rt.block_on(toolbox.call("git_grep", args)).unwrap();
        let content = result["content"].as_str().unwrap_or_default();
        assert!(
            !content.contains("netdevice.h"),
            "the review repo must not answer with reference content: {result:?}"
        );
    }

    /// Asking for a repository that was never configured has to say so. Falling
    /// back to the review repo would answer a question about the kernel with a
    /// search of the driver, and "no matches" reads as a fact.
    #[test]
    fn reference_read_without_a_reference_configured_is_an_error() {
        let (_review_dir, review_path) = setup_test_repo();
        let toolbox = ToolBox::new(review_path, None);
        let rt = Runtime::new().unwrap();

        let args = json!({ "repo": "kernel", "revision": "HEAD", "pattern": "anything" });
        let err = rt
            .block_on(toolbox.call("git_grep", args))
            .expect_err("must not silently search the review repo");
        let msg = err.to_string();
        assert!(
            msg.contains("reference_repository_path"),
            "the error must name the missing setting: {msg}"
        );
    }

    /// The virtual head is a commit in the review worktree. Rewriting `HEAD`
    /// into it before querying the kernel tree would ask for an object that is
    /// not there, and this is the failure most likely to survive review.
    #[test]
    fn virtual_head_does_not_leak_into_reference_reads() {
        let (_review_dir, review_path) = setup_test_repo();
        let (_ref_dir, ref_path) = setup_reference_repo();

        let review_head = rev_parse(&review_path, "HEAD~1");
        let pin = rev_parse(&ref_path, "v6.12^{commit}");

        let mut toolbox =
            ToolBox::new(review_path, None).with_reference(pinned_reference(ref_path));
        toolbox.set_virtual_head(review_head.clone());

        // Same input, two repositories, two correct answers.
        let args = json!({ "revision": "HEAD", "pattern": "x" });
        assert_eq!(
            toolbox.context.resolve_ref(&args, "HEAD").unwrap(),
            review_head,
            "the review repo still gets the virtual head"
        );

        let args = json!({ "repo": "kernel", "revision": "HEAD", "pattern": "x" });
        assert_eq!(
            toolbox.context.resolve_ref(&args, "HEAD").unwrap(),
            pin,
            "the reference repo gets its pinned commit, never the virtual head"
        );

        // HEAD inside a larger expression is the pin too. These are the forms
        // the model actually sends -- git_show's own schema example is
        // `HEAD:README.md` -- and each would otherwise read the clone's tip.
        assert_eq!(
            toolbox
                .context
                .resolve_ref(&args, "HEAD:include/linux/netdevice.h")
                .unwrap(),
            format!("{pin}:include/linux/netdevice.h")
        );
        assert_eq!(
            toolbox.context.resolve_ref(&args, "HEAD~1..HEAD").unwrap(),
            format!("{pin}~1..{pin}")
        );
        assert_eq!(toolbox.context.resolve_ref(&args, "").unwrap(), pin);

        // Git's `@` shorthand for HEAD is the pin as well, in every position
        // HEAD is; left alone it would read the clone's tip.
        for (raw, want) in [
            ("@", pin.clone()),
            ("@~1", format!("{pin}~1")),
            ("@^", format!("{pin}^")),
            (
                "@:include/linux/netdevice.h",
                format!("{pin}:include/linux/netdevice.h"),
            ),
            ("v6.11..@", format!("v6.11..{pin}")),
            ("@..@", format!("{pin}..{pin}")),
        ] {
            assert_eq!(
                toolbox.context.resolve_ref(&args, raw).unwrap(),
                want,
                "{raw}"
            );
        }
        // `@{...}` names a reflog entry or an upstream, not HEAD itself, and
        // an `@` inside a ref name is just a character.
        for raw in ["@{upstream}", "@{-1}", "user@host", "v6.12:dir/@"] {
            assert_eq!(toolbox.context.resolve_ref(&args, raw).unwrap(), raw);
        }
        // A ref that merely ends in HEAD names something else and is left be.
        assert_eq!(
            toolbox.context.resolve_ref(&args, "origin/HEAD").unwrap(),
            "origin/HEAD"
        );

        // An explicit revision is still honoured, so the model can ask whether
        // something changed in a later release.
        assert_eq!(
            toolbox.context.resolve_ref(&args, "v6.13").unwrap(),
            "v6.13"
        );
    }

    /// git_show keeps its own cache keyed on the object name. A tag names a
    /// different commit in each repository, so the key must separate them or one
    /// repo is served the other's content with no symptom.
    #[test]
    fn git_show_cache_does_not_collide_across_repos() {
        let (_review_dir, review_path) = setup_test_repo();
        let (_ref_dir, ref_path) = setup_reference_repo();

        // Same tag name in both repositories, pointing at different commits.
        let status = crate::git_cmd::in_dir(&review_path)
            .args(["tag", "v6.12"])
            .status()
            .unwrap();
        assert!(status.success());

        let toolbox = ToolBox::new(review_path, None).with_reference(pinned_reference(ref_path));
        let rt = Runtime::new().unwrap();

        let review = rt
            .block_on(toolbox.call("git_show", json!({ "object": "v6.12" })))
            .unwrap();
        let reference = rt
            .block_on(toolbox.call("git_show", json!({ "repo": "kernel", "object": "v6.12" })))
            .unwrap();

        let review = review["content"].as_str().unwrap();
        let reference = reference["content"].as_str().unwrap();
        assert!(review.contains("Second commit"), "{review}");
        assert!(reference.contains("Reference kernel tree"), "{reference}");
        assert_ne!(review, reference);
    }

    /// With nothing to point at, the model must not be offered the option: every
    /// attempt would cost a turn to learn what the schema could have said.
    #[test]
    fn repo_argument_is_hidden_when_no_reference_is_configured() {
        let (_review_dir, review_path) = setup_test_repo();

        let bare = ToolBox::new(review_path.clone(), None);
        for tool in bare.get_declarations_generic() {
            assert!(
                tool.parameters["properties"].get("repo").is_none(),
                "{} must not advertise repo without a reference repository",
                tool.name
            );
        }

        let with_ref = ToolBox::new(review_path, None).with_reference(unread_reference("HEAD"));
        let grep = with_ref
            .get_declarations_generic()
            .into_iter()
            .find(|t| t.name == "git_grep")
            .expect("git_grep is registered");
        assert!(
            grep.parameters["properties"]["repo"]["enum"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "kernel")
        );
    }

    /// The model is deciding what revision to pass while reading this schema,
    /// so the pin belongs here.
    ///
    /// Told the revision only elsewhere in the prompt, it sent "HEAD" on about
    /// half its kernel reads. Those were served the pin regardless, but the log
    /// then recorded "HEAD" against a tracking clone whose tip was several
    /// hundred commits off the pinned tag -- indistinguishable from a review
    /// that really had read the wrong tree. git_show, git_log and git_diff name
    /// their revision in `object`, `range` and a base/target pair, so those
    /// arguments carry the note as well.
    #[test]
    fn the_revision_schema_names_the_pin_when_one_is_configured() {
        let (_review_dir, review_path) = setup_test_repo();

        let pinned =
            ToolBox::new(review_path.clone(), None).with_reference(unread_reference("v6.12"));
        let declarations = pinned.get_declarations_generic();
        for tool in &declarations {
            for key in [
                "revision",
                "object",
                "range",
                "base_revision",
                "target_revision",
            ] {
                let Some(arg) = tool.parameters["properties"].get(key) else {
                    continue;
                };
                let desc = arg["description"].as_str().unwrap_or_default();
                assert!(
                    desc.contains("v6.12"),
                    "{}.{key} must name the pinned revision where the model chooses one: {desc}",
                    tool.name
                );
            }
        }
        for (tool, key) in [
            ("git_show", "object"),
            ("git_log", "range"),
            ("git_diff", "base_revision"),
            ("git_diff", "target_revision"),
        ] {
            let decl = declarations
                .iter()
                .find(|t| t.name == tool)
                .unwrap_or_else(|| panic!("{tool} is registered"));
            assert!(
                decl.parameters["properties"][key]["description"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("pinned to v6.12"),
                "{tool}.{key} must carry the pin note"
            );
        }

        // Without a reference repository there is no pin to name, and a
        // description inventing one would be worse than silence.
        let bare = ToolBox::new(review_path, None);
        let grep = bare
            .get_declarations_generic()
            .into_iter()
            .find(|t| t.name == "git_grep")
            .expect("git_grep is registered");
        assert!(
            !grep.parameters["properties"]["revision"]["description"]
                .as_str()
                .unwrap_or_default()
                .contains("pinned"),
            "a single-repository review has nothing to pin"
        );
    }

    /// A reference read reports the revision it was served, not the one asked
    /// for.
    ///
    /// The two differ whenever the caller leaves the revision unpinned, which
    /// is most of the time. Recording only the request makes a correct review
    /// and one reading a stale tracking clone produce identical logs.
    #[test]
    fn a_reference_read_reports_the_revision_it_was_served() {
        let (_review_dir, review_path) = setup_test_repo();
        let (_ref_dir, ref_path) = setup_reference_repo();
        let pin_sha = rev_parse(&ref_path, "v6.12^{commit}");
        let tip_sha = rev_parse(&ref_path, "HEAD");
        let toolbox = ToolBox::new(review_path, None).with_reference(pinned_reference(ref_path));
        let rt = Runtime::new().unwrap();

        // Asked for HEAD, served the pin: the case the log could not show.
        let res = rt
            .block_on(toolbox.call(
                "git_grep",
                json!({
                    "repo": "kernel", "revision": "HEAD",
                    "pattern": "net_device_ops", "is_literal": true
                }),
            ))
            .expect("the reference read must succeed");
        assert_eq!(
            res[crate::toolbox::REVISION_READ_KEY],
            pin_sha.as_str(),
            "an unpinned reference read must say which commit it was served: {res}"
        );

        // An explicitly named revision is honoured, so the annotation has to
        // follow the call rather than repeat the configuration.
        let res = rt
            .block_on(toolbox.call(
                "git_grep",
                json!({
                    "repo": "kernel", "revision": tip_sha,
                    "pattern": "net_device_ops", "is_literal": true
                }),
            ))
            .expect("the reference read must succeed");
        assert_eq!(
            res[crate::toolbox::REVISION_READ_KEY],
            tip_sha.as_str(),
            "a named revision must be reported as read, not replaced by the pin: {res}"
        );

        // git_show names its revision inside `object`. `HEAD:path` is the form
        // its own schema example teaches, and it must be served -- and
        // reported as -- the pin, not the clone's tip.
        let res = rt
            .block_on(toolbox.call(
                "git_show",
                json!({ "repo": "kernel", "object": "HEAD:include/linux/netdevice.h" }),
            ))
            .expect("the reference read must succeed");
        let content = res["content"].as_str().unwrap();
        assert!(content.contains("ndo_open"), "{content}");
        assert!(
            !content.contains("ndo_tip_only"),
            "HEAD in the kernel tree must read the pin, not the tip: {content}"
        );
        assert_eq!(
            res[crate::toolbox::REVISION_READ_KEY],
            pin_sha.as_str(),
            "{res}"
        );

        // An object naming another revision is read and reported as that
        // revision, never misreported as the pin.
        let object = format!("{tip_sha}:include/linux/netdevice.h");
        let res = rt
            .block_on(toolbox.call("git_show", json!({ "repo": "kernel", "object": object })))
            .expect("the reference read must succeed");
        assert!(
            res["content"].as_str().unwrap().contains("ndo_tip_only"),
            "{res}"
        );
        assert_eq!(res[crate::toolbox::REVISION_READ_KEY], tip_sha, "{res}");

        // A log range is reported with both ends resolved.
        let res = rt
            .block_on(toolbox.call(
                "git_log",
                json!({ "repo": "kernel", "range": format!("HEAD..{tip_sha}") }),
            ))
            .expect("the reference read must succeed");
        assert_eq!(
            res[crate::toolbox::REVISION_READ_KEY],
            format!("{pin_sha}..{tip_sha}"),
            "{res}"
        );
        assert!(
            res["content"].as_str().unwrap().contains("Past the pin"),
            "{res}"
        );

        // A review-repo read is unaffected: its revision means what it says.
        let res = rt
            .block_on(toolbox.call(
                "git_grep",
                json!({"revision": "HEAD", "pattern": "fn", "is_literal": true}),
            ))
            .expect("the review read must succeed");
        assert!(
            res.get(crate::toolbox::REVISION_READ_KEY).is_none(),
            "only reference reads are substituted, so only they need the note: {res}"
        );
    }

    /// The pin is inserted literally. A `$` in a revision would otherwise be
    /// read as a capture-group reference and quietly change what is served.
    #[test]
    fn head_substitution_inserts_the_replacement_literally() {
        let (_review_dir, review_path) = setup_test_repo();
        let toolbox =
            ToolBox::new(review_path, None).with_reference(crate::toolbox::ReferenceRepo {
                path: PathBuf::from("/nonexistent"),
                revision: "odd".to_string(),
                commit: "a$1b${2}c".to_string(),
            });
        let args = json!({ "repo": "kernel" });
        assert_eq!(
            toolbox.context.resolve_ref(&args, "HEAD~1").unwrap(),
            "a$1b${2}c~1"
        );
        assert_eq!(
            toolbox.context.resolve_ref(&args, "@:x").unwrap(),
            "a$1b${2}c:x"
        );
    }

    /// Only tools that take `repo` are checked and annotated for it. A stray
    /// `repo` on read_prompt must neither fail a call that ran nor earn a
    /// revision_read note for a read of the kernel tree that never happened.
    #[test]
    fn tools_without_a_repo_argument_ignore_it() {
        let (linux_path, prompts_path) = get_test_paths();
        let (_ref_dir, ref_path) = setup_reference_repo();
        let rt = Runtime::new().unwrap();
        let args = json!({ "name": "subsystem/locking.md", "repo": "kernel" });

        let grounded = ToolBox::new(linux_path.clone(), Some(prompts_path.clone()))
            .with_reference(pinned_reference(ref_path));
        let res = rt
            .block_on(grounded.call("read_prompt", args.clone()))
            .expect("read_prompt must ignore repo");
        assert!(
            res.get(crate::toolbox::REVISION_READ_KEY).is_none(),
            "read_prompt did not read the kernel tree: {res}"
        );

        // Nothing configured, and an unknown value: still ignored.
        let bare = ToolBox::new(linux_path, Some(prompts_path));
        rt.block_on(bare.call("read_prompt", args))
            .expect("repo is not validated for a tool that does not take it");
        rt.block_on(bare.call(
            "read_prompt",
            json!({ "name": "subsystem/locking.md", "repo": "x" }),
        ))
        .expect("repo is not validated for a tool that does not take it");
    }

    /// Two "revisions" that are really paths outside the work tree make git
    /// diff fall back to `--no-index` and compare them as plain files. The
    /// model supplies both, so the tool must refuse them. Identical files are
    /// the telling case: without the fix git exits 0 with an empty diff and
    /// the tool reports success, confirming what a host file contains (and a
    /// pair of differing files past the capture cap comes back verbatim).
    #[test]
    fn git_diff_revisions_cannot_name_host_files() {
        let (_dir, repo_path) = setup_test_repo();
        let outside = tempfile::tempdir().unwrap();
        let a = outside.path().join("a.txt");
        let b = outside.path().join("b.txt");
        std::fs::write(&a, "host secret\n").unwrap();
        std::fs::write(&b, "host secret\n").unwrap();

        let toolbox = ToolBox::new(repo_path, None);
        let rt = Runtime::new().unwrap();
        let args = json!({
            "base_revision": a.to_str().unwrap(),
            "target_revision": b.to_str().unwrap(),
        });
        match rt.block_on(toolbox.call("git_diff", args)) {
            Err(e) => assert!(
                !e.to_string().contains("host secret"),
                "the refusal must not echo the file: {e}"
            ),
            Ok(res) => panic!("a host path must not be diffed: {res}"),
        }
    }

    /// The same refusal holds in the reference repository: pointing the tool
    /// at the kernel tree must not reopen the host-file hole.
    #[test]
    fn git_diff_reference_revisions_cannot_name_host_files() {
        let (_review_dir, review_path) = setup_test_repo();
        let (_ref_dir, ref_path) = setup_reference_repo();
        let outside = tempfile::tempdir().unwrap();
        let a = outside.path().join("a.txt");
        let b = outside.path().join("b.txt");
        std::fs::write(&a, "host secret\n").unwrap();
        std::fs::write(&b, "host secret\n").unwrap();

        let toolbox = ToolBox::new(review_path, None).with_reference(pinned_reference(ref_path));
        let rt = Runtime::new().unwrap();
        let args = json!({
            "repo": "kernel",
            "base_revision": a.to_str().unwrap(),
            "target_revision": b.to_str().unwrap(),
        });
        match rt.block_on(toolbox.call("git_diff", args)) {
            Err(e) => assert!(
                !e.to_string().contains("host secret"),
                "the refusal must not echo the file: {e}"
            ),
            Ok(res) => panic!("a host path must not be diffed: {res}"),
        }
    }
}
