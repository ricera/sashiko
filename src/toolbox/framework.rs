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

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

/// A trait representing a single tool that can be exposed to an LLM.
///
/// The parameter `C` represents the execution context that is passed to the tool
/// during invocation (e.g., environment variables, file system paths, or project state).
#[async_trait]
pub trait LlmTool<C>: Send + Sync {
    /// The unique name of the tool (e.g., "git_grep").
    fn name(&self) -> &'static str;

    /// A detailed description of what the tool does, used by the LLM to understand when to call it.
    fn description(&self) -> &'static str;

    /// The JSON Schema defining the parameters this tool accepts.
    fn parameters(&self) -> Value;

    /// Normalizes the arguments passed by the LLM (e.g., filling in default values)
    /// to maximize cache hits. The default implementation returns the arguments unmodified.
    fn normalize_args(&self, args: &Value) -> Value {
        args.clone()
    }

    /// Executes the tool with the given arguments and context.
    async fn call(&self, args: Value, context: &C) -> Result<Value>;
}

/// A registry that manages a set of LLM tools and handles dynamic dispatching.
///
/// Tools are kept in name order so every process declares them in the same
/// order. A request lists the declarations, so a per-process hash order made
/// otherwise identical requests differ between runs, and the response cache
/// could never replay one.
pub struct ToolRegistry<C> {
    tools: BTreeMap<&'static str, Arc<dyn LlmTool<C>>>,
}

impl<C> ToolRegistry<C> {
    /// Creates a new empty `ToolRegistry`.
    pub fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
        }
    }

    /// Registers a tool in the registry.
    pub fn register(&mut self, tool: impl LlmTool<C> + 'static) {
        self.tools.insert(tool.name(), Arc::new(tool));
    }

    /// Registers a boxed tool in the registry.
    pub fn register_boxed(&mut self, tool: Box<dyn LlmTool<C>>) {
        self.tools.insert(tool.name(), Arc::from(tool));
    }

    /// Returns the list of tool declarations in a format suitable for the LLM API.
    ///
    /// The output is a vector of JSON objects, each containing:
    /// - `name`: The tool's name
    /// - `description`: Its description
    /// - `parameters`: Its parameter schema
    pub fn declarations(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(|t| {
                serde_json::json!({
                    "name": t.name(),
                    "description": t.description(),
                    "parameters": t.parameters(),
                })
            })
            .collect()
    }

    /// Returns the parameter schema of the tool registered under `name`.
    pub fn parameters(&self, name: &str) -> Option<Value> {
        self.tools.get(name).map(|t| t.parameters())
    }

    /// Normalizes the arguments for a specific tool by name.
    ///
    /// If the tool is registered, it delegates to the tool's `normalize_args` method.
    /// Otherwise, it returns the arguments unmodified.
    pub fn normalize_tool_args(&self, name: &str, args: &Value) -> Value {
        if let Some(tool) = self.tools.get(name) {
            tool.normalize_args(args)
        } else {
            args.clone()
        }
    }

    /// Dispatches a tool call to the registered tool with the given name.
    ///
    /// Returns an error if no tool is registered under the given name. The
    /// error names the closest registered tool, when one is near enough to be
    /// a typo, and lists every registered tool.
    pub async fn call(&self, name: &str, args: Value, context: &C) -> Result<Value> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("{}", self.unknown_tool_error(name)))?;

        tool.call(args, context).await
    }

    /// Explains an unknown tool name well enough for the caller to fix it.
    ///
    /// The model reaches this by emitting a name that does not exist — most of
    /// this registry's names share a `git_` prefix, so a near miss like
    /// `git_gray` for `git_grep` is the common shape. The error is fed back as
    /// the tool's result, so it is the model's only chance to correct itself,
    /// and each attempt costs a turn against `max_interactions`. Naming the
    /// alternatives turns that into one corrected call rather than a series of
    /// guesses against a tool list it has to recall unaided.
    fn unknown_tool_error(&self, name: &str) -> String {
        let mut available: Vec<&str> = self.tools.keys().copied().collect();
        available.sort_unstable();

        let suggestion = self
            .closest_tool(name)
            .map(|best| format!(" Did you mean \"{}\"?", best))
            .unwrap_or_default();

        format!(
            "Tool not found: \"{}\".{} Available tools: {}",
            name,
            suggestion,
            available.join(", ")
        )
    }

    /// The registered name closest to `name`, when one is close enough to be a
    /// plausible correction rather than a different request entirely.
    fn closest_tool(&self, name: &str) -> Option<&'static str> {
        // Scaled to the name's length: two edits is a typo in `git_grep` and a
        // coin flip in `git_ls`. Suggesting confidently and wrongly is worse
        // than not suggesting, because the model is inclined to take it.
        let budget = (name.chars().count() / 3).clamp(1, 3);

        self.tools
            .keys()
            .map(|candidate| (edit_distance(name, candidate), *candidate))
            .filter(|(distance, _)| *distance <= budget)
            // Ties broken by name so the message is stable across runs; the map
            // iterates in an arbitrary order.
            .min()
            .map(|(_, candidate)| candidate)
    }
}

/// Levenshtein distance, counting insertions, deletions and substitutions.
///
/// Two rows rather than a full matrix: names are short, and this runs on a
/// path that is already an error.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();

    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];

    for (i, ac) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, bc) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ac != bc);
            let deletion = previous[j + 1] + 1;
            let insertion = current[j] + 1;
            current[j + 1] = substitution.min(deletion).min(insertion);
        }
        std::mem::swap(&mut previous, &mut current);
    }

    previous[b.len()]
}

impl<C> Default for ToolRegistry<C> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub(&'static str);

    #[async_trait]
    impl LlmTool<()> for Stub {
        fn name(&self) -> &'static str {
            self.0
        }
        fn description(&self) -> &'static str {
            "stub"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({})
        }
        async fn call(&self, _args: Value, _context: &()) -> Result<Value> {
            Ok(serde_json::json!({"ok": true}))
        }
    }

    /// The response cache keys on the whole request, declarations included,
    /// so their order has to be the same in every process. All nine real
    /// names, registered in reverse, so a hash order would pass this only by
    /// a one-in-362880 chance.
    #[test]
    fn declarations_come_in_name_order_whatever_the_registration_order() {
        let mut reg = ToolRegistry::new();
        for name in REAL_TOOL_NAMES.iter().rev() {
            reg.register(Stub(name));
        }
        let names: Vec<String> = reg
            .declarations()
            .iter()
            .filter_map(|d| d["name"].as_str().map(str::to_string))
            .collect();
        assert_eq!(names, REAL_TOOL_NAMES);
    }

    /// The registry's real tool names, in name order.
    const REAL_TOOL_NAMES: [&str; 9] = [
        "git_blame",
        "git_diff",
        "git_find_files",
        "git_grep",
        "git_log",
        "git_ls",
        "git_read_files",
        "git_show",
        "read_prompt",
    ];

    /// The real set, because the near misses this exists to catch come from the
    /// names sharing a `git_` prefix.
    fn registry() -> ToolRegistry<()> {
        let mut reg = ToolRegistry::new();
        for name in REAL_TOOL_NAMES {
            reg.register(Stub(name));
        }
        reg
    }

    /// Observed in the wild: the model emits a name with the right prefix and a
    /// wrong tail. The error is fed back as the tool result, so it is the only
    /// chance to correct, and every attempt costs a turn.
    #[tokio::test]
    async fn unknown_tool_names_the_alternatives_and_the_near_miss() {
        let reg = registry();

        for bad in ["git_gray", "git_grag"] {
            let err = reg
                .call(bad, serde_json::json!({}), &())
                .await
                .expect_err("an unregistered name must not dispatch")
                .to_string();

            assert!(
                err.contains(bad),
                "the error must quote what was tried: {err}"
            );
            assert!(
                err.contains(r#"Did you mean "git_grep"?"#),
                "{bad} should suggest git_grep, got: {err}"
            );
            // Recalling the list unaided is what the model just failed to do.
            assert!(
                err.contains(
                    "git_blame, git_diff, git_find_files, git_grep, git_log, \
                              git_ls, git_read_files, git_show, read_prompt"
                ),
                "the full list must be present and sorted, got: {err}"
            );
        }
    }

    #[tokio::test]
    async fn a_name_with_no_near_miss_suggests_nothing() {
        // Confidently wrong is worse than silent: the model tends to take the
        // suggestion, so an unrelated name must not be handed one.
        let err = registry()
            .call("run_tests", serde_json::json!({}), &())
            .await
            .expect_err("unregistered")
            .to_string();

        assert!(!err.contains("Did you mean"), "{err}");
        assert!(err.contains("Available tools: git_blame"), "{err}");
    }

    #[tokio::test]
    async fn short_names_get_a_tighter_budget() {
        let reg = registry();

        // Two edits from `git_ls` reaches `git_log`, but at six characters that
        // is a different tool, not a typo.
        let err = reg
            .call("git_l", serde_json::json!({}), &())
            .await
            .expect_err("unregistered")
            .to_string();
        assert!(
            err.contains(r#"Did you mean "git_log"?"#) || err.contains(r#"Did you mean "git_ls"?"#),
            "one edit from both, either is a fair suggestion: {err}"
        );

        // A long name tolerates more drift before it stops being a typo.
        let err = reg
            .call("git_read_file", serde_json::json!({}), &())
            .await
            .expect_err("unregistered")
            .to_string();
        assert!(err.contains(r#"Did you mean "git_read_files"?"#), "{err}");
    }

    /// Pins each length band of the budget at its edge, so dropping the
    /// scaling or the upper clamp fails here: every case past the budget would
    /// draw a suggestion under a flat budget of three or an unclamped one.
    #[test]
    fn the_budget_admits_its_own_distance_and_nothing_further() {
        let reg = registry();
        let cases = [
            // Four characters, budget one: `git_ls` is two edits away.
            ("git_", None),
            ("git_l", Some("git_ls")),
            // Seven characters, budget two: `git_ls` and `git_log` are three.
            ("git_xyz", None),
            ("git_lxx", Some("git_log")),
            // Eleven characters, budget three: three insertions still fit.
            ("git_read_fi", Some("git_read_files")),
            // Eighteen characters, clamped to three: four edits do not.
            ("git_find_filesxxxx", None),
            ("git_find_filesxxx", Some("git_find_files")),
        ];

        for (name, expected) in cases {
            assert_eq!(reg.closest_tool(name), expected, "for {name:?}");
        }
    }

    #[tokio::test]
    async fn a_registered_tool_still_dispatches() {
        let out = registry()
            .call("git_grep", serde_json::json!({}), &())
            .await
            .expect("registered tools must be unaffected");
        assert_eq!(out, serde_json::json!({"ok": true}));
    }

    #[test]
    fn edit_distance_counts_each_kind_of_edit() {
        assert_eq!(edit_distance("git_grep", "git_grep"), 0);
        assert_eq!(edit_distance("git_gray", "git_grep"), 2); // two substitutions
        assert_eq!(edit_distance("git_grep", "git_gre"), 1); // deletion
        assert_eq!(edit_distance("git_gre", "git_grep"), 1); // insertion
        assert_eq!(edit_distance("", "git_ls"), 6);
        assert_eq!(edit_distance("git_ls", ""), 6);
        // Compares by character, not byte, so a multi-byte name cannot panic or
        // score a single edit as several.
        assert_eq!(edit_distance("gít_grep", "git_grep"), 1);
    }
}
