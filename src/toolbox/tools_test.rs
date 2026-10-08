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

    /// Every review builds its own ToolBox, often in a fresh process, and the
    /// response cache only replays a request that is byte-for-byte the same.
    /// Two toolboxes must therefore declare their tools identically.
    #[test]
    fn two_toolboxes_declare_their_tools_identically() {
        let (repo, prompts) = get_test_paths();
        let declare = || {
            let tools = ToolBox::new(repo.clone(), Some(prompts.clone()));
            serde_json::to_string(&tools.get_declarations_generic())
                .expect("tool declarations serialize")
        };
        let first = declare();
        for _ in 0..8 {
            assert_eq!(declare(), first);
        }
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

    /// Builds a second reference repo, a stand-in for rdma-core, whose content
    /// cannot be confused with either the review repo or the kernel fixture.
    ///
    /// Its pin, `v59`, is not the tip either: a commit past it adds
    /// `rdma_tip_only`. It also carries a `v6.12` tag, deliberately on a
    /// different commit than the kernel fixture's, so a cache keyed on the tag
    /// name alone would serve one tree's answer for the other.
    fn setup_rdma_reference_repo() -> (tempfile::TempDir, PathBuf) {
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
        std::fs::create_dir_all(repo_path.join("providers/ionic")).unwrap();
        let abi = repo_path.join("providers/ionic/ionic-abi.h");
        std::fs::write(&abi, "struct ionic_ctx_resp {\n\t__u32 version;\n};\n").unwrap();
        run_git(&["add", "."]);
        run_git(&["commit", "-m", "Reference rdma-core tree"]);
        run_git(&["tag", "v6.12"]);
        std::fs::write(
            &abi,
            "struct ionic_ctx_resp {\n\t__u32 version;\n\t__u32 udma_count;\n};\n",
        )
        .unwrap();
        run_git(&["commit", "-am", "rdma-core release"]);
        run_git(&["tag", "v59"]);
        std::fs::write(
            &abi,
            "struct ionic_ctx_resp {\n\t__u32 version;\n\t__u32 udma_count;\n\
             \t__u32 rdma_tip_only;\n};\n",
        )
        .unwrap();
        run_git(&["commit", "-am", "rdma past the pin"]);

        (temp_dir, repo_path)
    }

    /// A reference named `name` pinned at `tag`, with the commit it resolves to
    /// filled in as startup validation would.
    fn pinned_named(
        name: &str,
        kind: crate::settings::ReferenceKind,
        path: PathBuf,
        tag: &str,
    ) -> crate::toolbox::ReferenceRepo {
        let commit = rev_parse(&path, &format!("{tag}^{{commit}}"));
        crate::toolbox::ReferenceRepo {
            name: name.to_string(),
            kind,
            path,
            revision: tag.to_string(),
            commit,
            description: (kind == crate::settings::ReferenceKind::Source)
                .then(|| "A userspace tree.".to_string()),
            applies_to: None,
        }
    }

    /// The kernel fixture as the "kernel" reference, pinned at `v6.12`.
    fn pinned_reference(path: PathBuf) -> crate::toolbox::ReferenceRepo {
        pinned_named(
            "kernel",
            crate::settings::ReferenceKind::Kernel,
            path,
            "v6.12",
        )
    }

    /// The rdma-core fixture as the "rdma-core" reference, pinned at `v59`.
    fn pinned_rdma(path: PathBuf) -> crate::toolbox::ReferenceRepo {
        pinned_named(
            "rdma-core",
            crate::settings::ReferenceKind::Source,
            path,
            "v59",
        )
    }

    /// A reference that is never read, for tests that only inspect schemas.
    fn unread_named(
        name: &str,
        kind: crate::settings::ReferenceKind,
        revision: &str,
    ) -> crate::toolbox::ReferenceRepo {
        crate::toolbox::ReferenceRepo {
            name: name.to_string(),
            kind,
            path: PathBuf::from("/nonexistent"),
            revision: revision.to_string(),
            commit: "0".repeat(40),
            description: None,
            applies_to: None,
        }
    }

    /// A kernel reference that is never read.
    fn unread_reference(revision: &str) -> crate::toolbox::ReferenceRepo {
        unread_named("kernel", crate::settings::ReferenceKind::Kernel, revision)
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
            msg.contains("git.references"),
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
    /// so what HEAD means belongs here, and the pins belong in the repo
    /// argument beside it.
    ///
    /// Told the revision only elsewhere in the prompt, it sent "HEAD" on about
    /// half its kernel reads. Those were served the pin regardless, but the log
    /// then recorded "HEAD" against a tracking clone whose tip was several
    /// hundred commits off the pinned tag -- indistinguishable from a review
    /// that really had read the wrong tree. git_show, git_log and git_diff name
    /// their revision in `object`, `range` and a base/target pair, so those
    /// arguments carry the note as well. With several references, each pin is
    /// named once, in the repo argument, rather than in every revision
    /// argument of every tool.
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
                    desc.contains("reads that repo's pin"),
                    "{}.{key} must say what HEAD means where the model chooses one: {desc}",
                    tool.name
                );
            }
            if let Some(repo) = tool.parameters["properties"].get("repo") {
                let desc = repo["description"].as_str().unwrap_or_default();
                assert!(
                    desc.contains("kernel (kernel tree, v6.12)"),
                    "{}.repo must name the pin: {desc}",
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
                    .contains("reads that repo's pin"),
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
                .contains("pin"),
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
        assert_eq!(
            res[crate::toolbox::REPO_READ_KEY],
            "kernel",
            "a reference read must say which reference it read: {res}"
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
        assert!(
            res.get(crate::toolbox::REPO_READ_KEY).is_none(),
            "a review read is not a reference read: {res}"
        );
    }

    /// The pin is inserted literally. A `$` in a revision would otherwise be
    /// read as a capture-group reference and quietly change what is served.
    #[test]
    fn head_substitution_inserts_the_replacement_literally() {
        let (_review_dir, review_path) = setup_test_repo();
        let toolbox =
            ToolBox::new(review_path, None).with_reference(crate::toolbox::ReferenceRepo {
                commit: "a$1b${2}c".to_string(),
                ..unread_reference("odd")
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
        assert!(
            res.get(crate::toolbox::REPO_READ_KEY).is_none(),
            "read_prompt did not read a reference repository: {res}"
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

    /// The same refusal holds in every reference repository: pointing the tool
    /// at a reference tree must not reopen the host-file hole.
    #[test]
    fn git_diff_reference_revisions_cannot_name_host_files() {
        let (_review_dir, review_path) = setup_test_repo();
        let (_ref_dir, ref_path) = setup_reference_repo();
        let (_rdma_dir, rdma_path) = setup_rdma_reference_repo();
        let outside = tempfile::tempdir().unwrap();
        let a = outside.path().join("a.txt");
        let b = outside.path().join("b.txt");
        std::fs::write(&a, "host secret\n").unwrap();
        std::fs::write(&b, "host secret\n").unwrap();

        let toolbox = ToolBox::new(review_path, None)
            .with_reference(pinned_reference(ref_path))
            .with_reference(pinned_rdma(rdma_path));
        let rt = Runtime::new().unwrap();
        for repo in ["kernel", "rdma-core"] {
            let args = json!({
                "repo": repo,
                "base_revision": a.to_str().unwrap(),
                "target_revision": b.to_str().unwrap(),
            });
            match rt.block_on(toolbox.call("git_diff", args)) {
                Err(e) => assert!(
                    !e.to_string().contains("host secret"),
                    "the refusal must not echo the file: {e}"
                ),
                Ok(res) => panic!("a host path must not be diffed in {repo}: {res}"),
            }
        }
    }

    /// Each reference is read in its own root: a search in one never sees the
    /// other's files, and the review repo sees neither.
    #[test]
    fn two_references_route_to_their_own_roots() {
        let (_review_dir, review_path) = setup_test_repo();
        let (_ref_dir, ref_path) = setup_reference_repo();
        let (_rdma_dir, rdma_path) = setup_rdma_reference_repo();
        let toolbox = ToolBox::new(review_path.clone(), None)
            .with_reference(pinned_reference(ref_path.clone()))
            .with_reference(pinned_rdma(rdma_path.clone()));
        let rt = Runtime::new().unwrap();

        let find = |repo: Option<&str>| {
            let mut args = json!({ "revision": "HEAD", "pattern": "struct", "is_literal": true });
            if let Some(repo) = repo {
                args["repo"] = json!(repo);
            }
            rt.block_on(toolbox.call("git_grep", args.clone()))
                .unwrap_or_else(|e| panic!("{args}: {e}"))["content"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        };
        let kernel = find(Some("kernel"));
        assert!(kernel.contains("netdevice.h"), "{kernel}");
        assert!(!kernel.contains("ionic-abi.h"), "{kernel}");
        let rdma = find(Some("rdma-core"));
        assert!(rdma.contains("ionic-abi.h"), "{rdma}");
        assert!(!rdma.contains("netdevice.h"), "{rdma}");
        let review = find(None);
        assert!(!review.contains("netdevice.h"), "{review}");
        assert!(!review.contains("ionic-abi.h"), "{review}");

        let ctx = &toolbox.context;
        assert_eq!(
            ctx.repo_root(&json!({ "repo": "kernel" })).unwrap(),
            ref_path
        );
        assert_eq!(
            ctx.repo_root(&json!({ "repo": "rdma-core" })).unwrap(),
            rdma_path
        );
        assert_eq!(ctx.repo_root(&json!({})).unwrap(), review_path);
    }

    /// HEAD, `@` and an omitted revision mean each reference's own pin, never
    /// another reference's and never the review's virtual head.
    #[test]
    fn head_maps_to_each_references_own_pin() {
        let (_review_dir, review_path) = setup_test_repo();
        let (_ref_dir, ref_path) = setup_reference_repo();
        let (_rdma_dir, rdma_path) = setup_rdma_reference_repo();
        let kernel_pin = rev_parse(&ref_path, "v6.12^{commit}");
        let rdma_pin = rev_parse(&rdma_path, "v59^{commit}");
        let review_head = rev_parse(&review_path, "HEAD~1");

        let mut toolbox = ToolBox::new(review_path, None)
            .with_reference(pinned_reference(ref_path))
            .with_reference(pinned_rdma(rdma_path));
        toolbox.set_virtual_head(review_head.clone());

        for (repo, pin) in [("kernel", &kernel_pin), ("rdma-core", &rdma_pin)] {
            let args = json!({ "repo": repo });
            let resolve = |raw: &str| toolbox.context.resolve_ref(&args, raw).unwrap();
            assert_eq!(&resolve("HEAD"), pin, "{repo}");
            assert_eq!(&resolve(""), pin, "{repo}");
            assert_eq!(resolve("@:x"), format!("{pin}:x"), "{repo}");
            assert_eq!(resolve("HEAD~1..HEAD"), format!("{pin}~1..{pin}"), "{repo}");
            assert!(!resolve("HEAD").contains(&review_head), "{repo}");
        }

        let rt = Runtime::new().unwrap();
        let res = rt
            .block_on(toolbox.call(
                "git_show",
                json!({ "repo": "rdma-core", "object": "HEAD:providers/ionic/ionic-abi.h" }),
            ))
            .expect("the reference read must succeed");
        let content = res["content"].as_str().unwrap();
        assert!(content.contains("udma_count"), "{content}");
        assert!(
            !content.contains("rdma_tip_only"),
            "HEAD in rdma-core must read its pin, not the tip: {content}"
        );
        assert_eq!(res[crate::toolbox::REVISION_READ_KEY], rdma_pin.as_str());
        assert_eq!(res[crate::toolbox::REPO_READ_KEY], "rdma-core");
    }

    /// The repo enum is "review" and then the references in configured order,
    /// identical in every tool that takes it, and its description names each
    /// reference with its kind and pin.
    #[test]
    fn repo_enum_lists_names_in_config_order() {
        use crate::settings::ReferenceKind;
        let (_review_dir, review_path) = setup_test_repo();
        let toolbox = ToolBox::new(review_path, None).with_references([
            unread_named("rdma-core", ReferenceKind::Source, "ionic-rdma-v59"),
            unread_reference("v6.18"),
        ]);

        let mut seen = 0;
        for tool in toolbox.get_declarations_generic() {
            let Some(repo) = tool.parameters["properties"].get("repo") else {
                continue;
            };
            seen += 1;
            assert_eq!(
                repo["enum"],
                json!(["review", "rdma-core", "kernel"]),
                "{}",
                tool.name
            );
            let desc = repo["description"].as_str().unwrap();
            for needle in [
                "rdma-core (source, ionic-rdma-v59)",
                "kernel (kernel tree, v6.18)",
            ] {
                assert!(
                    desc.contains(needle),
                    "{}: missing {needle}: {desc}",
                    tool.name
                );
            }
        }
        assert_eq!(seen, 8, "every git tool takes repo");
    }

    /// A configured reference left out of this patch by its `applies_to`.
    fn dormant(name: &str, globs: &[&str]) -> crate::toolbox::DormantReference {
        crate::toolbox::DormantReference {
            name: name.to_string(),
            applies_to: globs.iter().map(|g| g.to_string()).collect(),
        }
    }

    /// A dormant reference is offered nowhere: not in the repo enum, not in
    /// any description, and with nothing attached, not as a repo argument or
    /// a pin note at all.
    #[test]
    fn dormant_references_are_not_offered() {
        let (_review_dir, review_path) = setup_test_repo();
        let toolbox = ToolBox::new(review_path.clone(), None)
            .with_references([unread_reference("v6.18")])
            .with_dormant_references([dormant("rdma-core", &["rdma/drv/**"])]);
        let mut seen = 0;
        for tool in toolbox.get_declarations_generic() {
            let props = tool.parameters["properties"].as_object().unwrap();
            if let Some(repo) = props.get("repo") {
                seen += 1;
                assert_eq!(repo["enum"], json!(["review", "kernel"]), "{}", tool.name);
            }
            for (key, arg) in props {
                let desc = arg["description"].as_str().unwrap_or_default();
                assert!(
                    !desc.contains("rdma-core"),
                    "{}.{key} offers a dormant reference: {desc}",
                    tool.name
                );
            }
        }
        assert_eq!(seen, 8, "every git tool takes repo");

        let toolbox = ToolBox::new(review_path, None)
            .with_dormant_references([dormant("rdma-core", &["rdma/drv/**"])]);
        for tool in toolbox.get_declarations_generic() {
            let props = tool.parameters["properties"].as_object().unwrap();
            assert!(
                !props.contains_key("repo"),
                "{} offers repo with every reference dormant",
                tool.name
            );
            for (key, arg) in props {
                let desc = arg["description"].as_str().unwrap_or_default();
                assert!(
                    !desc.contains("reads that repo's pin") && !desc.contains("rdma-core"),
                    "{}.{key} describes a reference that is not attached: {desc}",
                    tool.name
                );
            }
        }
    }

    /// Every call that names an attached reference counts against it --
    /// cache hits and calls git refuses included -- and nothing else does.
    #[test]
    fn reference_reads_are_counted_per_reference() {
        let (_review_dir, review_path) = setup_test_repo();
        let (_kernel_dir, kernel_path) = setup_reference_repo();
        let (_rdma_dir, rdma_path) = setup_rdma_reference_repo();
        let prompts_dir = tempfile::tempdir().unwrap();
        std::fs::write(prompts_dir.path().join("a.md"), "prompt").unwrap();
        let toolbox = ToolBox::new(review_path, Some(prompts_dir.path().to_path_buf()))
            .with_references([pinned_reference(kernel_path), pinned_rdma(rdma_path)])
            .with_dormant_references([dormant("iproute2", &["ip/**"])]);
        assert_eq!(toolbox.reference_reads(), [0, 0]);

        let rt = Runtime::new().unwrap();
        let call = |name: &str, args: serde_json::Value| rt.block_on(toolbox.call(name, args));
        let kernel_grep = json!({ "repo": "kernel", "revision": "HEAD", "pattern": "ndo_open" });

        call("git_grep", kernel_grep.clone()).unwrap();
        let first = toolbox.cache.read().unwrap().len();
        call("git_grep", kernel_grep).unwrap();
        assert_eq!(
            toolbox.cache.read().unwrap().len(),
            first,
            "the second grep must be a cache hit"
        );
        call(
            "git_grep",
            json!({ "repo": "rdma-core", "revision": "HEAD", "pattern": "ionic_ctx_resp" }),
        )
        .unwrap();
        call("git_grep", json!({ "revision": "HEAD", "pattern": "x" })).unwrap();
        call(
            "git_grep",
            json!({ "repo": "iproute2", "revision": "HEAD", "pattern": "x" }),
        )
        .unwrap_err();
        call(
            "git_grep",
            json!({ "repo": "nosuch", "revision": "HEAD", "pattern": "x" }),
        )
        .unwrap_err();
        // read_prompt does not read a repository, so a stray repo means
        // nothing to it.
        let _ = call("read_prompt", json!({ "name": "a.md", "repo": "kernel" }));
        let refused = call(
            "git_grep",
            json!({ "repo": "kernel", "revision": "no-such-tag", "pattern": "x" }),
        )
        .unwrap();
        assert!(refused.get("error").is_some(), "{refused}");

        assert_eq!(toolbox.reference_reads(), [3, 1]);
    }

    /// Whether a toolbox error is the refusal of a repo name, as opposed to
    /// an unknown tool or a git failure. Matched on the three messages
    /// repo_target writes.
    fn is_repo_refusal(err: &anyhow::Error) -> bool {
        let text = err.to_string();
        [
            "Unknown repo",
            "configured but not attached",
            "No reference repositories are attached",
        ]
        .iter()
        .any(|needle| text.contains(needle))
    }

    /// call_target judges a recorded call without a toolbox, so the counts
    /// shown for a review are only right if it reaches the toolbox's own
    /// answer. Each call goes through both: the per-reference tallies must
    /// equal the toolbox's reads, and a call is Unattached exactly when the
    /// toolbox refused its repo.
    #[test]
    fn call_target_agrees_with_the_toolbox() {
        use crate::toolbox::{CallTarget, call_target};
        let (_review_dir, review_path) = setup_test_repo();
        let (_kernel_dir, kernel_path) = setup_reference_repo();
        let (_rdma_dir, rdma_path) = setup_rdma_reference_repo();
        let prompts_dir = tempfile::tempdir().unwrap();
        std::fs::write(prompts_dir.path().join("a.md"), "prompt").unwrap();
        let toolbox = ToolBox::new(review_path, Some(prompts_dir.path().to_path_buf()))
            .with_references([pinned_reference(kernel_path), pinned_rdma(rdma_path)])
            .with_dormant_references([dormant("iproute2", &["ip/**"])]);
        let attached: Vec<String> = toolbox
            .references()
            .iter()
            .map(|r| r.name.clone())
            .collect();
        let attached: Vec<&str> = attached.iter().map(String::as_str).collect();
        assert_eq!(attached, ["kernel", "rdma-core"]);

        let grep = |repo: Option<serde_json::Value>| {
            let mut args = json!({ "revision": "HEAD", "pattern": "x" });
            if let Some(repo) = repo {
                args["repo"] = repo;
            }
            ("git_grep", args)
        };
        let calls = [
            grep(None),
            grep(Some(json!("review"))),
            grep(Some(json!("kernel"))),
            // The same call again, served from the cache.
            grep(Some(json!("kernel"))),
            grep(Some(json!("rdma-core"))),
            grep(Some(json!("iproute2"))),
            grep(Some(json!("nope"))),
            grep(Some(json!(""))),
            grep(Some(json!(5))),
            ("read_prompt", json!({ "name": "a.md", "repo": "kernel" })),
            ("git_frobnicate", json!({ "repo": "kernel" })),
            (
                "GIT_GREP",
                json!({ "repo": "kernel", "revision": "HEAD", "pattern": "y" }),
            ),
        ];

        let rt = Runtime::new().unwrap();
        let mut tallies = vec![0u64; attached.len()];
        for (tool, args) in calls {
            let outcome = rt.block_on(toolbox.call(tool, args.clone()));
            let target = call_target(tool, args["repo"].as_str(), &attached);
            let refused = outcome.as_ref().is_err_and(is_repo_refusal);
            assert_eq!(
                target == CallTarget::Unattached,
                refused,
                "{tool} {args}: classified {target:?}, toolbox gave {outcome:?}"
            );
            if let CallTarget::Reference(index) = target {
                tallies[index] += 1;
            }
        }
        assert_eq!(tallies, [3, 1]);
        assert_eq!(toolbox.reference_reads(), tallies);
    }

    /// With nothing attached, every name is one the review does not have,
    /// and the toolbox refuses it; a tool that reads no repository still
    /// ignores the argument.
    #[test]
    fn call_target_with_nothing_attached() {
        use crate::toolbox::{CallTarget, call_target};
        for name in ["kernel", "rdma-core", "nope", ""] {
            assert_eq!(
                call_target("git_grep", Some(name), &[]),
                CallTarget::Unattached,
                "{name:?}"
            );
            assert_eq!(
                call_target("read_prompt", Some(name), &[]),
                CallTarget::NoRepository,
                "{name:?}"
            );
        }
        assert_eq!(call_target("git_grep", None, &[]), CallTarget::Review);
        assert_eq!(
            call_target("git_grep", Some("review"), &[]),
            CallTarget::Review
        );

        let toolbox = ToolBox::new(PathBuf::from("."), None);
        let rt = Runtime::new().unwrap();
        let err = rt
            .block_on(toolbox.call(
                "git_grep",
                json!({ "repo": "kernel", "revision": "HEAD", "pattern": "x" }),
            ))
            .expect_err("nothing is attached, so no name can be served");
        assert!(is_repo_refusal(&err), "{err}");
    }

    /// tool_reads_repo is derived from the registered schemas, so it must
    /// say yes exactly for the tools that declare repo, under any case or
    /// padding the model uses.
    #[test]
    fn tool_reads_repo_matches_every_declaration() {
        use crate::toolbox::tool_reads_repo;
        let declarations = crate::toolbox::review_registry(true).declarations();
        let mut reading = Vec::new();
        for decl in &declarations {
            let name = decl["name"].as_str().unwrap();
            let declares = decl["parameters"]["properties"].get("repo").is_some();
            assert_eq!(tool_reads_repo(name), declares, "{name}");
            if declares {
                reading.push(name);
            }
        }
        assert_eq!(
            reading,
            [
                "git_blame",
                "git_diff",
                "git_find_files",
                "git_grep",
                "git_log",
                "git_ls",
                "git_read_files",
                "git_show",
            ]
        );
        assert!(!tool_reads_repo("read_prompt"));
        assert!(tool_reads_repo("  GIT_GREP "));
        assert!(!tool_reads_repo("git_frobnicate"));
    }

    /// Naming a dormant reference says it exists, why it is not attached, and
    /// what is; a name that is not configured at all is still unknown.
    #[test]
    fn a_dormant_reference_names_why_it_is_not_attached() {
        let (_review_dir, review_path) = setup_test_repo();
        let rt = Runtime::new().unwrap();
        let grep = |toolbox: &ToolBox, repo: &str| {
            rt.block_on(toolbox.call(
                "git_grep",
                json!({ "repo": repo, "revision": "HEAD", "pattern": "x" }),
            ))
            .expect_err("a reference that is not attached must not be served")
            .to_string()
        };

        let toolbox = ToolBox::new(review_path.clone(), None)
            .with_references([unread_reference("v6.18")])
            .with_dormant_references([dormant(
                "rdma-core",
                &["rdma/drv/**", "include/uapi/rdma/**"],
            )]);
        let err = grep(&toolbox, "rdma-core");
        for needle in [
            "\"rdma-core\"",
            "configured but not attached",
            "rdma/drv/**, include/uapi/rdma/**",
            "\"review\"",
            "\"kernel\"",
        ] {
            assert!(err.contains(needle), "missing {needle}: {err}");
        }
        let valid = err.split("Valid values here:").nth(1).unwrap();
        assert!(!valid.contains("rdma-core"), "{err}");

        let err = grep(&toolbox, "iproute2");
        assert!(err.contains("Unknown repo 'iproute2'"), "{err}");
        assert!(!err.contains("configured but not attached"), "{err}");

        let only_dormant = ToolBox::new(review_path, None)
            .with_dormant_references([dormant("rdma-core", &["rdma/drv/**"])]);
        let err = grep(&only_dormant, "rdma-core");
        assert!(err.contains("configured but not attached"), "{err}");
        let valid = err.split("Valid values here:").nth(1).unwrap();
        assert_eq!(valid.trim(), "\"review\" (the code under review).", "{err}");
    }

    /// An unknown name is refused with every valid one, so the retry can be
    /// right.
    #[test]
    fn unknown_repo_lists_valid_names() {
        let (_review_dir, review_path) = setup_test_repo();
        let toolbox = ToolBox::new(review_path, None).with_references([
            unread_reference("v6.18"),
            unread_named(
                "rdma-core",
                crate::settings::ReferenceKind::Source,
                "ionic-rdma-v59",
            ),
        ]);
        let rt = Runtime::new().unwrap();
        let err = rt
            .block_on(toolbox.call(
                "git_grep",
                json!({ "repo": "iproute2", "revision": "HEAD", "pattern": "x" }),
            ))
            .expect_err("an unknown repo must not be served")
            .to_string();
        for needle in ["iproute2", "\"review\"", "\"kernel\"", "\"rdma-core\""] {
            assert!(err.contains(needle), "missing {needle}: {err}");
        }
    }

    /// The same tag in two references, and in the review repo, names three
    /// different commits. Neither the central cache nor git_show's raw cache
    /// may serve one for another, on the first call or on a repeat.
    #[test]
    fn same_tag_in_two_references_does_not_collide() {
        let (_review_dir, review_path) = setup_test_repo();
        let (_ref_dir, ref_path) = setup_reference_repo();
        let (_rdma_dir, rdma_path) = setup_rdma_reference_repo();
        let status = crate::git_cmd::in_dir(&review_path)
            .args(["tag", "v6.12"])
            .status()
            .unwrap();
        assert!(status.success());

        let toolbox = ToolBox::new(review_path, None)
            .with_reference(pinned_reference(ref_path))
            .with_reference(pinned_rdma(rdma_path));
        let rt = Runtime::new().unwrap();

        // The second round repeats the first call exactly, which is a central
        // cache hit, and then asks for a line range, which misses the central
        // cache but reads git_show's raw cache under the same object.
        for extra in [
            json!({}),
            json!({}),
            json!({ "start_line": 1, "end_line": 200 }),
        ] {
            let show = |repo: Option<&str>| {
                let mut args = json!({ "object": "v6.12" });
                if let Some(repo) = repo {
                    args["repo"] = json!(repo);
                }
                for (k, v) in extra.as_object().unwrap() {
                    args[k] = v.clone();
                }
                rt.block_on(toolbox.call("git_show", args)).unwrap()
            };
            let kernel = show(Some("kernel"));
            let rdma = show(Some("rdma-core"));
            let review = show(None);

            let text = |v: &serde_json::Value| v["content"].as_str().unwrap().to_string();
            assert!(text(&kernel).contains("Reference kernel tree"), "{kernel}");
            assert!(!text(&kernel).contains("rdma-core"), "{kernel}");
            assert!(text(&rdma).contains("Reference rdma-core tree"), "{rdma}");
            assert!(!text(&rdma).contains("kernel tree"), "{rdma}");
            assert!(text(&review).contains("Second commit"), "{review}");

            assert_eq!(kernel[crate::toolbox::REPO_READ_KEY], "kernel");
            assert_eq!(rdma[crate::toolbox::REPO_READ_KEY], "rdma-core");
            assert!(review.get(crate::toolbox::REPO_READ_KEY).is_none());
        }
    }

    /// One clone may back two references at different pins. Each is served its
    /// own pin, so two releases of one tree can be compared.
    #[test]
    fn same_path_two_pins_is_served_per_pin() {
        use crate::settings::ReferenceKind;
        let (_review_dir, review_path) = setup_test_repo();
        let (_ref_dir, ref_path) = setup_reference_repo();
        let tip = rev_parse(&ref_path, "HEAD");
        let toolbox = ToolBox::new(review_path, None)
            .with_reference(pinned_reference(ref_path.clone()))
            .with_reference(pinned_named(
                "net-next",
                ReferenceKind::Source,
                ref_path,
                &tip,
            ));
        let rt = Runtime::new().unwrap();

        let read = |repo: &str| {
            rt.block_on(toolbox.call(
                "git_show",
                json!({ "repo": repo, "object": "HEAD:include/linux/netdevice.h" }),
            ))
            .unwrap()
        };
        let pinned = read("kernel");
        let next = read("net-next");
        assert!(
            !pinned["content"].as_str().unwrap().contains("ndo_tip_only"),
            "{pinned}"
        );
        assert!(
            next["content"].as_str().unwrap().contains("ndo_tip_only"),
            "{next}"
        );
        assert_eq!(next[crate::toolbox::REVISION_READ_KEY], tip.as_str());
        assert_eq!(next[crate::toolbox::REPO_READ_KEY], "net-next");
    }

    /// The suggestion is computed on the name ToolBox normalized, so padding
    /// and case do not hide the near miss. A second call fails the same way,
    /// which shows the error was not cached as if it were a result.
    #[test]
    fn unknown_tool_suggests_the_near_miss_after_normalization() {
        let toolbox = ToolBox::new(PathBuf::from("."), None);
        let rt = Runtime::new().unwrap();

        for attempt in ["first", "second"] {
            let err = rt
                .block_on(toolbox.call("  GIT_GRAY ", json!({})))
                .expect_err("an unregistered name must not dispatch")
                .to_string();
            assert!(
                err.contains(r#"Tool not found: "git_gray"."#),
                "{attempt} call must quote the normalized name: {err}"
            );
            assert!(
                err.contains(r#"Did you mean "git_grep"?"#),
                "{attempt} call must suggest git_grep: {err}"
            );
        }
        assert!(
            toolbox.cache.read().unwrap().is_empty(),
            "an unknown tool must leave nothing in the cache"
        );
    }
}
