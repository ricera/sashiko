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
use chrono::{DateTime, Local, TimeZone, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use reqwest::Client;
use sashiko::activity::format_duration;
use sashiko::api::{PatchsetsResponse, SubmitRequest, SubmitResponse};
use sashiko::settings::Settings;
use sashiko::utils::utf8_prefix;
use serde_json::{Value, from_str};
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::OnceLock;
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

static COLOR_CHOICE: OnceLock<ColorChoice> = OnceLock::new();

#[derive(Parser)]
#[command(name = "sashiko-cli")]
#[command(about = "CLI tool for interacting with Sashiko", long_about = None)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Override server URL (default: from settings or http://127.0.0.1:8080)
    #[arg(long, global = true, env = "SASHIKO_SERVER")]
    server: Option<String>,

    /// Output format (text, json)
    #[arg(long, global = true, default_value = "text")]
    format: OutputFormat,

    /// When to use color: auto (default), always, never
    #[arg(long, global = true, default_value = "auto")]
    color: ColorMode,
}

#[derive(Clone, ValueEnum)]
enum OutputFormat {
    Text,
    Json,
}

#[derive(Clone, ValueEnum)]
enum ColorMode {
    Auto,
    Always,
    Never,
}

struct ShowOptions {
    patch: Option<i64>,
    summary: bool,
    issues: bool,
    since: Option<i64>,
    inline: bool,
    diff: Option<String>,
    timings: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Submit a patch or range for review
    Submit {
        /// Revision range, commit SHA, or path to mbox file.
        /// Defaults to "HEAD" if in a git repo, or reads from stdin if piped.
        #[arg(value_name = "INPUT")]
        input: Option<String>,

        /// Explicitly set type (overrides auto-detection)
        #[arg(long, value_enum)]
        r#type: Option<SubmitType>,

        /// Override repository path (defaults to settings)
        #[arg(long, short = 'r')]
        repo: Option<PathBuf>,

        /// Baseline commit (for mbox injection only)
        #[arg(long)]
        baseline: Option<String>,

        /// Skip review for patches matching subject pattern (with wildcards, e.g. mm:*)
        #[arg(long, value_name = "PATTERN")]
        skip_subject: Option<Vec<String>>,

        /// Only review patches matching subject pattern (with wildcards, e.g. *PRODKERNEL*)
        #[arg(long, value_name = "PATTERN")]
        only_subject: Option<Vec<String>>,

        /// Treat an empty first commit as the series cover letter (b4 prep style)
        #[arg(long)]
        b4_cover_letter: bool,
    },
    /// Show server status and statistics
    Status,
    /// List patchsets or reviews
    List {
        /// Filter query (e.g. "pending", "failed", "linux-mm")
        #[arg(value_name = "FILTER")]
        filter: Option<String>,

        /// Page number
        #[arg(long, default_value_t = 1)]
        page: usize,

        /// Items per page
        #[arg(long, default_value_t = 20)]
        per_page: usize,
    },
    /// Show details of a patchset or review
    Show {
        /// ID of the patchset or "latest"
        #[arg(default_value = "latest")]
        id: String,

        /// Stream status updates linearly
        #[arg(long, short = 'w')]
        watch: bool,

        /// Show only a single patch by part index (1-indexed)
        #[arg(long, conflicts_with = "issues")]
        patch: Option<i64>,

        /// Show compact progress summary
        #[arg(long, short = 's')]
        summary: bool,

        /// Show only patches with issues found
        #[arg(long, short = 'i', conflicts_with = "patch")]
        issues: bool,

        /// Show only reviews newer than this review ID
        #[arg(long)]
        since: Option<i64>,

        /// Include inline review content in text output
        #[arg(long)]
        inline: bool,

        /// Show per-stage timings and turn counts for each review
        #[arg(long)]
        timings: bool,

        /// Compare with another patchset ID
        #[arg(long, short = 'd')]
        diff: Option<String>,
    },
    /// Request a re-review of a completed patchset
    Rerun {
        /// ID of the patchset to re-review. Omit when using --pr.
        id: Option<i64>,

        /// Re-review a pull request by number, e.g. --pr 20. Resolves the PR's
        /// current head first, so a force-pushed PR is reviewed as it is now
        /// rather than as it was.
        #[arg(long, conflicts_with = "id")]
        pr: Option<i64>,
    },
    /// Cancel a pending review
    Cancel {
        /// ID of the patchset to cancel
        id: i64,

        /// Force cancel even if the review is already in progress
        #[arg(long, short)]
        force: bool,
    },
    /// Run a local review (or queue to running server)
    Local {
        /// Git revision, range (e.g. HEAD~3..HEAD), or commit SHA
        #[arg(default_value = "HEAD")]
        input: String,

        /// Baseline reference (default: parent of first commit in range)
        #[arg(long)]
        baseline: Option<String>,

        /// Path to git repository (default: current directory)
        #[arg(long, short = 'r')]
        repo: Option<PathBuf>,

        /// Skip AI review, only test patch application
        #[arg(long)]
        no_ai: bool,

        /// Custom prompt to append to the review task
        #[arg(long)]
        custom_prompt: Option<String>,

        /// Force local execution even if server is running
        #[arg(long)]
        force_local: bool,

        /// Pause on failure, wait for agent/user to fix code, and re-run automatically
        #[arg(long)]
        interactive: bool,
    },
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum SubmitType {
    /// Submit a raw mbox file (or - for stdin)
    Mbox,
    /// Submit a single remote commit
    Remote,
    /// Submit a range of remote commits
    Range,
    /// Fetch a thread from lore.kernel.org by message ID
    Thread,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    COLOR_CHOICE
        .set(match cli.color {
            ColorMode::Always => ColorChoice::Always,
            ColorMode::Never => ColorChoice::Never,
            ColorMode::Auto => {
                if std::io::stdout().is_terminal() {
                    ColorChoice::Auto
                } else {
                    ColorChoice::Never
                }
            }
        })
        .unwrap();

    // Load settings, falling back to defaults if file missing/invalid
    let base_url = cli.server.unwrap_or_else(|| match Settings::new() {
        Ok(s) => {
            if s.server.host.contains(':') {
                format!("http://[::1]:{}", s.server.port)
            } else {
                format!("http://{}:{}", s.server.host, s.server.port)
            }
        }
        Err(e) => {
            if e.to_string().contains("not found") {
                "http://127.0.0.1:8080".to_string()
            } else {
                panic!("Failed to parse Settings.toml: {}", e);
            }
        }
    });

    let client = Client::new();

    if let Err(e) = run_command(cli.command, &client, &base_url, cli.format).await {
        print_colored(Color::Red, "Error: ");
        println!("{}", e);

        // Provide helpful hints for common errors
        if let Some(req_err) = e.downcast_ref::<reqwest::Error>() {
            if req_err.is_connect() {
                println!("\nHint: Is the Sashiko server running at {}?", base_url);
                println!("      You can start it with `cargo run --bin sashiko`");
            } else if let Some(status) = req_err.status() {
                if status == reqwest::StatusCode::NOT_FOUND {
                    println!("\nHint: The requested resource was not found.");
                } else if status == reqwest::StatusCode::BAD_REQUEST {
                    println!("\nHint: The request was invalid. Check your arguments.");
                }
            }
        }
        std::process::exit(1);
    }

    Ok(())
}

async fn run_command(
    command: Commands,
    client: &Client,
    base_url: &str,
    format: OutputFormat,
) -> Result<()> {
    match command {
        Commands::Submit {
            input,
            r#type,
            repo,
            baseline,
            skip_subject,
            only_subject,
            b4_cover_letter,
        } => {
            handle_submit(
                client,
                base_url,
                input,
                r#type,
                repo,
                baseline,
                skip_subject,
                only_subject,
                b4_cover_letter,
                format,
            )
            .await
        }
        Commands::Status => handle_status(client, base_url, format).await,
        Commands::List {
            filter,
            page,
            per_page,
        } => handle_list(client, base_url, page, per_page, filter, format).await,
        Commands::Show {
            id,
            watch,
            patch,
            summary,
            issues,
            since,
            inline,
            timings,
            diff,
        } => {
            let opts = ShowOptions {
                patch,
                summary,
                issues,
                since,
                inline,
                timings,
                diff,
            };
            handle_show(client, base_url, id, watch, format, opts).await
        }
        Commands::Rerun { id, pr } => match (id, pr) {
            (_, Some(number)) => handle_pr_review(client, base_url, number, format).await,
            (Some(id), None) => handle_rerun(client, base_url, id, format).await,
            (None, None) => Err(anyhow::anyhow!(
                "Give a patchset id or --pr <number>: `sashiko-cli rerun 42` or `sashiko-cli rerun --pr 20`"
            )),
        },
        Commands::Cancel { id, force } => handle_cancel(client, base_url, id, force, format).await,
        Commands::Local {
            input,
            baseline,
            repo,
            no_ai,
            custom_prompt,
            force_local,
            interactive,
        } => {
            handle_local(
                client,
                base_url,
                input,
                baseline,
                repo,
                no_ai,
                custom_prompt,
                force_local,
                interactive,
                format,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_submit(
    client: &Client,
    base_url: &str,
    input: Option<String>,
    explicit_type: Option<SubmitType>,
    repo: Option<PathBuf>,
    baseline: Option<String>,
    skip_subjects: Option<Vec<String>>,
    only_subjects: Option<Vec<String>>,
    b4_cover_letter: bool,
    format: OutputFormat,
) -> Result<()> {
    // A pull request reference is not a git object, so it is recognised before
    // type detection rather than inside it: `#20` would otherwise fall through
    // to "assume commit ref" and fail against the repository.
    if explicit_type.is_none()
        && let Some(number) = input.as_deref().and_then(parse_pr_reference)
    {
        return handle_pr_review(client, base_url, number, format).await;
    }

    let url = format!("{}/api/submit", base_url);

    // DWIM Detection Logic
    let (submission_type, target) = if let Some(t) = explicit_type {
        (t, input.unwrap_or_else(|| "HEAD".to_string()))
    } else {
        // Auto-detect based on input
        if let Some(s) = input {
            if s == "-" {
                (SubmitType::Mbox, s)
            } else if s.contains("..") {
                (SubmitType::Range, s)
            } else if s.contains('@') && !s.contains('/') && !s.contains('\\') {
                // If it looks like an email address/msgid and doesn't look like a path, assume Thread
                (SubmitType::Thread, s)
            } else if PathBuf::from(&s).exists() {
                // If it's a file, assume mbox. If it's a dir, maybe repo?
                // For safety, if it looks like a commit (hex), prefer Remote unless file exists.
                // But filenames can look like anything.
                // Sashiko deals with mbox files primarily.
                let p = PathBuf::from(&s);
                if p.is_file() {
                    (SubmitType::Mbox, s)
                } else {
                    // Not a file, assume commit ref
                    (SubmitType::Remote, s)
                }
            } else {
                // Not a file on disk (or we can't see it). Assume commit ref.
                (SubmitType::Remote, s)
            }
        } else {
            // No input provided.
            // Check if stdin is piped
            if !std::io::stdin().is_terminal() {
                (SubmitType::Mbox, "-".to_string())
            } else {
                // Default to HEAD
                (SubmitType::Remote, "HEAD".to_string())
            }
        }
    };

    let payload = match submission_type {
        SubmitType::Mbox => {
            let content = if target == "-" {
                let mut buffer = String::new();
                std::io::stdin()
                    .read_to_string(&mut buffer)
                    .context("Failed to read from stdin")?;
                buffer
            } else {
                std::fs::read_to_string(&target)
                    .with_context(|| format!("Failed to read file {:?}", target))?
            };
            SubmitRequest::Inject {
                raw: content,
                base_commit: baseline,
                skip_subjects: skip_subjects.clone(),
                only_subjects: only_subjects.clone(),
            }
        }
        SubmitType::Remote => {
            let repo_path = repo.map(|p| p.to_string_lossy().to_string());

            SubmitRequest::Remote {
                sha: target,
                repo: repo_path,
                skip_subjects: skip_subjects.clone(),
                only_subjects: only_subjects.clone(),
                b4_cover_letter,
            }
        }
        SubmitType::Range => {
            let repo_path = repo.map(|p| p.to_string_lossy().to_string());

            SubmitRequest::RemoteRange {
                sha: target,
                repo: repo_path,
                skip_subjects: skip_subjects.clone(),
                only_subjects: only_subjects.clone(),
                b4_cover_letter,
            }
        }
        SubmitType::Thread => SubmitRequest::Thread { msgid: target },
    };

    let resp = client.post(&url).json(&payload).send().await?;

    if resp.status().is_success() {
        let result: SubmitResponse = resp.json().await?;
        match format {
            OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&result)?),
            OutputFormat::Text => {
                print_colored(Color::Green, "Success: ");
                println!("Submission accepted. ID: {}", result.id);
            }
        }
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Submission failed ({}): {}", status, text));
    }

    Ok(())
}

async fn handle_status(client: &Client, base_url: &str, format: OutputFormat) -> Result<()> {
    let url = format!("{}/api/stats", base_url);
    let resp = client.get(&url).send().await?;

    if resp.status().is_success() {
        let stats: Value = resp.json().await?;

        match format {
            OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&stats)?),
            OutputFormat::Text => {
                print_colored(Color::Cyan, "Server Status:\n");
                println!(
                    "  Version:   {}",
                    stats["version"].as_str().unwrap_or("unknown")
                );
                println!("  Messages:  {}", stats["messages"]);
                println!("  Patchsets: {}", stats["patchsets"]);

                if let Some(breakdown) = stats.get("breakdown") {
                    println!("\nQueue Breakdown:");
                    let items = [
                        ("Pending", "pending"),
                        ("In Review", "reviewing"),
                        ("Reviewed", "reviewed"),
                        ("Failed", "failed"),
                        ("Apply Failed", "failed_to_apply"),
                        ("Incomplete", "incomplete"),
                    ];

                    let zero = serde_json::json!(0);
                    for (label, key) in items {
                        let val = breakdown.get(key).unwrap_or(&zero);
                        println!("  {:<15} {}", label, val);
                    }
                }

                print_live_activity(client, base_url).await;
            }
        }
    } else {
        return Err(anyhow::anyhow!("Failed to get status: {}", resp.status()));
    }

    Ok(())
}

/// Prints everything the daemon is currently working on.
///
/// `idle` is the useful column: a large value means that unit of work has not
/// reported progress recently, which is the signature of a wedged fetch or review.
async fn print_live_activity(client: &Client, base_url: &str) {
    let url = format!("{}/api/activity", base_url);
    let Ok(resp) = client.get(&url).send().await else {
        return;
    };
    if !resp.status().is_success() {
        return;
    }
    let Ok(data) = resp.json::<Value>().await else {
        return;
    };
    let Some(entries) = data.get("entries").and_then(|e| e.as_array()) else {
        return;
    };

    println!();
    if entries.is_empty() {
        print_colored(Color::Cyan, "Live Activity:");
        println!(" (idle)");
        return;
    }

    print_colored(Color::Cyan, "Live Activity:\n");
    println!("  {:<28} {:<40} {:>10}", "WORK", "PHASE", "IDLE");
    for entry in entries {
        let key = entry["key"].as_str().unwrap_or("?");
        let desc = entry["description"].as_str().unwrap_or("?");
        let idle = entry["idle_seconds"].as_u64().unwrap_or(0);
        println!("  {:<28} {:<40} {:>10}", key, desc, format_duration(idle));
    }
}

async fn handle_list(
    client: &Client,
    base_url: &str,
    page: usize,
    per_page: usize,
    filter: Option<String>,
    format: OutputFormat,
) -> Result<()> {
    let mut url = format!(
        "{}/api/patchsets?page={}&per_page={}",
        base_url, page, per_page
    );
    if let Some(q) = filter {
        url.push_str(&format!("&q={}", q));
    }

    let resp = client.get(&url).send().await?;

    if resp.status().is_success() {
        let data: PatchsetsResponse = resp.json().await?;

        match format {
            OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&data)?),
            OutputFormat::Text => {
                if data.items.is_empty() {
                    println!("No items found.");
                    return Ok(());
                }

                println!(
                    "{:<10} {:<18} {:<50} {:<20}",
                    "ID", "Status", "Subject", "Date"
                );
                println!("{:-<10} {:-<18} {:-<50} {:-<20}", "", "", "", "");

                for item in data.items {
                    let status_str = item.status.as_deref().unwrap_or("Unknown");

                    let status_color = match status_str {
                        "Reviewed" => Color::Green,
                        "Embargoed" => Color::Magenta,
                        "Failed" | "Error" | "Failed To Apply" => Color::Red,
                        "Pending" | "In Review" => Color::Yellow,
                        "Cancelled" => Color::Red,
                        _ => Color::White,
                    };

                    print!("{:<10} ", item.id);
                    print_colored(status_color, &format!("{:<18}", status_str));

                    let subject = item.subject.unwrap_or_else(|| "(no subject)".to_string());
                    let subject_display = format_subject(&subject);

                    let date_display = if let Some(ts) = item.date {
                        format_timestamp(ts)
                    } else {
                        "-".to_string()
                    };

                    println!(" {:<50} {}", subject_display, date_display);
                }

                println!(
                    "\nPage {} of {} (Total: {})",
                    data.page,
                    data.total.div_ceil(data.per_page),
                    data.total
                );
            }
        }
    } else {
        return Err(anyhow::anyhow!(
            "Failed to list patchsets: {}",
            resp.status()
        ));
    }

    Ok(())
}

fn format_subject(subject: &str) -> String {
    if subject.len() > 48 {
        format!("{}...", utf8_prefix(subject, 45))
    } else {
        subject.to_string()
    }
}

fn review_has_issues(review: &Value) -> bool {
    review
        .get("inline_review")
        .and_then(|s| s.as_str())
        .is_some_and(|inline| !inline.is_empty() && inline != "No issues found.")
}

fn review_has_new_issues(review: &Value) -> bool {
    if let Some(output_str) = review.get("output").and_then(|o| o.as_str())
        && let Ok(output_json) = serde_json::from_str::<Value>(output_str)
        && let Some(findings) = output_json.get("findings").and_then(|f| f.as_array())
    {
        return findings.iter().any(|f| {
            let preexisting = f
                .get("preexisting")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            !preexisting
        });
    }
    review_has_issues(review)
}

fn find_best_review_for_patch(patch_id: i64, reviews: &[Value]) -> Option<&Value> {
    let refs: Vec<&Value> = reviews.iter().collect();
    find_best_review_for_patch_refs(patch_id, &refs)
}

fn find_best_review_for_patch_refs<'a>(patch_id: i64, reviews: &[&'a Value]) -> Option<&'a Value> {
    let mut best: Option<&Value> = None;
    for r in reviews {
        if r.get("patch_id").and_then(|id| id.as_i64()) != Some(patch_id) {
            continue;
        }
        let status = r.get("status").and_then(|s| s.as_str());
        let current_status = best.and_then(|pr| pr.get("status").and_then(|s| s.as_str()));
        if status == Some("Reviewed") || current_status != Some("Reviewed") {
            best = Some(r);
        }
    }
    best
}

fn review_result_label(review: &Value) -> &str {
    if review_has_new_issues(review) {
        "Issues Found"
    } else {
        review.get("status").and_then(|s| s.as_str()).unwrap_or("")
    }
}

async fn fetch_patchset(client: &Client, base_url: &str, id: &str) -> Result<Value> {
    let url = format!("{}/api/patch?id={}", base_url, id);
    let resp = client.get(&url).send().await?;
    if resp.status().is_success() {
        Ok(resp.json().await?)
    } else {
        Err(anyhow::anyhow!(
            "Failed to fetch patchset: {}",
            resp.status()
        ))
    }
}

fn print_patch_line(patch: &Value, review: Option<&Value>, show_inline: bool) {
    let idx = patch["part_index"].as_i64().unwrap_or(0);
    let status = patch["status"].as_str().unwrap_or("");
    let apply_err = patch["apply_error"].as_str();

    print!("  [{}] {}", idx, patch["subject"].as_str().unwrap_or(""));
    if !status.is_empty() && status != "Pending" {
        print!(" (");
        let color = match status {
            "Failed" | "Failed To Apply" | "Error" => Color::Red,
            "Embargoed" => Color::Magenta,
            _ => Color::Green,
        };
        print_colored(color, status);
        print!(")");
    }

    if let Some(rev) = review {
        let has_issues = review_has_new_issues(rev);
        let rev_status = rev.get("status").and_then(|s| s.as_str()).unwrap_or("");
        print!(" [");
        let color = if has_issues {
            Color::Yellow
        } else if rev_status == "Failed" {
            Color::Red
        } else {
            Color::Green
        };
        print_colored(color, review_result_label(rev));
        print!("]");
    }
    println!();

    if let Some(err) = apply_err {
        print_colored(Color::Red, "      Error: ");
        println!("{}", err.trim());
    }

    if show_inline
        && let Some(rev) = review
        && let Some(inline) = rev.get("inline_review").and_then(|s| s.as_str())
        && !inline.is_empty()
        && inline != "No issues found."
    {
        println!("{}", inline.trim());
        println!();
    }
}

/// Warns when a review did not run every stage it was supposed to.
///
/// The concurrent review stages are joined so one failure no longer aborts the
/// review; that is only acceptable if the missing coverage is stated. A review
/// that skipped the security stage but reads as clean is worse than one that
/// failed outright.
/// Notes how many attempts a review needed, when it needed more than one.
///
/// The live counter in the activity view vanishes the moment the review ends, so
/// without this there is no way to see after the fact that a result took three
/// goes to produce.
fn print_attempt_note(review: &Value) {
    let attempt = review.get("attempt").and_then(|a| a.as_u64());
    let duration = review.get("duration_seconds").and_then(|d| d.as_u64());

    let mut parts = Vec::new();
    if let Some(d) = duration {
        parts.push(format!("reviewed in {}", format_duration(d)));
    }
    // Only worth saying when it took more than one go; annotating every review
    // with "attempt 1" would bury the case worth noticing.
    if let Some(a) = attempt.filter(|a| *a > 1) {
        parts.push(format!("succeeded on attempt {}", a));
    }

    if !parts.is_empty() {
        println!("  ({})", parts.join(", "));
    }
}

/// Prints how long each review stage took and how many turns it needed.
///
/// The stage times deliberately do not sum to the patch total: the stages run
/// concurrently, so the total is the longest one, not the sum. Saying so avoids
/// the arithmetic looking broken.
/// Live stage activity for one patchset, grouped by the patch it describes.
struct PatchActivity {
    /// False once the daemon has stopped: the entries then say where the work
    /// got to, not what it is doing.
    live: bool,
    by_patch: std::collections::HashMap<i64, Vec<Value>>,
}

async fn fetch_patch_activity(
    client: &Client,
    base_url: &str,
    patchset_id: &str,
) -> Option<PatchActivity> {
    let url = format!("{}/api/patchset/activity?id={}", base_url, patchset_id);
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: Value = resp.json().await.ok()?;

    let mut by_patch: std::collections::HashMap<i64, Vec<Value>> = std::collections::HashMap::new();
    for entry in data.get("entries")?.as_array()? {
        // Entries with no patch describe the patchset as a whole and belong in
        // the activity summary, not under any one patch.
        if let Some(patch_id) = entry.get("patch_id").and_then(|p| p.as_i64()) {
            by_patch.entry(patch_id).or_default().push(entry.clone());
        }
    }

    Some(PatchActivity {
        live: data["live"].as_bool().unwrap_or(true),
        by_patch,
    })
}

/// One row of a patch's stage table.
///
/// Deliberately built from the deserialised `Phase` rather than the server's
/// rendered description, so the columns line up and the CLI does not carry a
/// second copy of the wording that can drift from the web's.
fn stage_row(entry: &Value, live: bool) -> String {
    use sashiko::activity::Phase;

    let phase: Option<Phase> = serde_json::from_value(entry["phase"].clone()).ok();
    let stage = entry
        .get("stage")
        .and_then(|s| s.as_i64())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "this patch".to_string());
    let age = entry["age_seconds"].as_u64().unwrap_or(0);

    let describe = |p: &Phase| p.describe();
    let dash = "—".to_string();

    // A persisted entry has no clocks: it records where the work got to, not
    // how long it has been there.
    if !live {
        let state = phase
            .as_ref()
            .map(describe)
            .unwrap_or_else(|| entry["description"].as_str().unwrap_or("?").to_string());
        return format!("    {:<12} {:>10}  {:>12}  {}", stage, dash, dash, state);
    }

    let (elapsed, turns, state) = match &phase {
        Some(Phase::StageDone { seconds, turns, .. }) => (
            format_duration(*seconds),
            match turns {
                0 => dash.clone(),
                1 => "1 turn".to_string(),
                n => format!("{} turns", n),
            },
            "done".to_string(),
        ),
        Some(Phase::StageFailed { cancelled, .. }) => (
            format!("{} ago", format_duration(age)),
            dash.clone(),
            phase
                .as_ref()
                .map(describe)
                .unwrap_or_else(|| if *cancelled { "cancelled" } else { "failed" }.to_string()),
        ),
        Some(Phase::Stage {
            turn,
            max_turns,
            waiting,
            ..
        }) => (
            format_duration(age),
            if *max_turns == 0 {
                "starting".to_string()
            } else {
                format!("turn {}/{}", turn, max_turns)
            },
            {
                // `describe` on a Stage repeats the stage and turn, which are
                // already their own columns; only the wait belongs here.
                let full = Phase::Stage {
                    stage: 0,
                    turn: *turn,
                    max_turns: *max_turns,
                    waiting: waiting.clone(),
                }
                .describe();
                full.rsplit_once('(')
                    .map(|(_, tail)| tail.trim_end_matches(')').to_string())
                    .unwrap_or(full)
            },
        ),
        // The patch's own entry: planning, or running its stages.
        other => (
            format_duration(age),
            dash.clone(),
            other
                .as_ref()
                .map(describe)
                .unwrap_or_else(|| entry["description"].as_str().unwrap_or("?").to_string()),
        ),
    };

    let idle = entry["idle_seconds"].as_u64().unwrap_or(0);
    let stalled = if idle > 300 && !matches!(phase, Some(Phase::StageDone { .. })) {
        format!("  — no progress for {}", format_duration(idle))
    } else {
        String::new()
    };

    format!(
        "    {:<12} {:>10}  {:>12}  {}{}",
        stage, elapsed, turns, state, stalled
    )
}

/// Footnote for a finished review's stage table.
///
/// The rows will not add up to the patch's review time, because the stages
/// overlap. Saying only "run concurrently" asserted that without showing it,
/// and was the same words under every table; the longest stage beside the sum
/// makes the gap visible, and both numbers are about this review.
///
/// With one stage there is nothing to overlap and nothing to sum.
fn stage_breakdown_note(stages: &[&Value]) -> String {
    let seconds: Vec<u64> = stages
        .iter()
        .map(|s| s["seconds"].as_u64().unwrap_or(0))
        .collect();

    if seconds.len() == 1 {
        return format!("1 stage, {}", format_duration(seconds[0]));
    }

    format!(
        "{} stages overlapping: longest {}, {} summed",
        seconds.len(),
        format_duration(seconds.iter().copied().max().unwrap_or(0)),
        format_duration(seconds.iter().sum())
    )
}

/// Per-patch stage tables, mirroring the web card.
///
/// Printed whatever the patchset's status. The recorded breakdown only exists
/// once a review has finished, and the most interesting moment to ask what the
/// stages are doing is while they are still doing it -- which is exactly when
/// this used to print nothing, because the whole per-patch section sat behind a
/// review-log fetch that only happens for terminal statuses.
fn print_patch_timings(patches: &[Value], reviews: &[&Value], activity: Option<&PatchActivity>) {
    if patches.is_empty() {
        return;
    }

    println!();
    print_colored(Color::Cyan, "Stage timings:\n");

    for patch in patches {
        let idx = patch["part_index"].as_i64().unwrap_or(0);
        let p_id = patch["id"].as_i64().unwrap_or(0);
        let subject = patch["subject"].as_str().unwrap_or("");
        println!("  Patch {}: {}", idx, subject);

        let live_entries = activity
            .and_then(|a| a.by_patch.get(&p_id))
            .filter(|e| !e.is_empty());
        let recorded = find_best_review_for_patch_refs(p_id, reviews)
            .and_then(|r| r.get("stage_durations"))
            .and_then(|d| d.as_array())
            .filter(|d| !d.is_empty());

        match (live_entries, recorded) {
            // Live wins: it is the current truth, and the recorded breakdown for
            // an earlier attempt would contradict it.
            (Some(entries), _) => {
                let live = activity.map(|a| a.live).unwrap_or(true);
                let mut entries: Vec<&Value> = entries.iter().collect();
                // The patch's own entry has no stage number and sorts first,
                // above the stages it covers.
                entries.sort_by_key(|e| e["stage"].as_i64().unwrap_or(-1));
                println!(
                    "    {:<12} {:>10}  {:>12}  STATE",
                    "STAGE", "ELAPSED", "TURNS"
                );
                for entry in entries {
                    println!("{}", stage_row(entry, live));
                }
            }
            (None, Some(stages)) => {
                let mut stages: Vec<&Value> = stages.iter().collect();
                stages.sort_by_key(|s| s["stage"].as_u64().unwrap_or(0));
                println!("    {:<12} {:>10}  {:>12}", "STAGE", "ELAPSED", "TURNS");
                for st in &stages {
                    let turns = match st["turns"].as_u64().unwrap_or(0) {
                        0 => "—".to_string(),
                        1 => "1 turn".to_string(),
                        n => format!("{} turns", n),
                    };
                    println!(
                        "    {:<12} {:>10}  {:>12}",
                        st["stage"].as_u64().unwrap_or(0),
                        format_duration(st["seconds"].as_u64().unwrap_or(0)),
                        turns
                    );
                }
                println!("    ({})", stage_breakdown_note(&stages));
            }
            // Said plainly rather than left blank, which is indistinguishable
            // from the flag not working.
            (None, None) => println!("    no stage timings recorded"),
        }
    }
}

fn print_coverage_warning(review: &Value) {
    let Some(failures) = review
        .get("stage_failures")
        .and_then(|f| f.as_array())
        .filter(|f| !f.is_empty())
    else {
        return;
    };

    println!();
    print_colored(
        Color::Red,
        &format!(
            "Incomplete coverage: {} stage(s) did not run\n",
            failures.len()
        ),
    );
    for f in failures {
        let stage = f["stage"].as_u64().unwrap_or(0);
        let what = if f["cancelled"].as_bool().unwrap_or(false) {
            "cancelled"
        } else {
            "failed"
        };
        let reason = f["reason"].as_str().unwrap_or("unknown");
        println!("  Stage {} {}: {}", stage, what, reason);
    }
}

/// Live activity description for one key, if the daemon reports any.
///
/// Best-effort by design: a daemon without `/api/activity` yields `None` rather
/// than breaking `show --watch`.
/// One-line summary of everything a patchset is currently doing.
///
/// Live entries report both clocks: `age` is how long that activity has been
/// running, `idle` is how long since it last moved. A large idle beside a growing
/// age is the signature of a wedged stage — the whole point of tracking two
/// numbers instead of one.
///
/// When nothing is live the daemon returns the last state recorded before it
/// stopped, prefixed "stopped while" — so it is neither mistaken for something
/// still running, nor read as the cause of a failure. It says where the work got
/// to; `failed_reason` says why it ended.
///
/// Best-effort by design: a daemon without the endpoint yields `None` rather
/// than breaking `show --watch`.
async fn fetch_activity_summary(
    client: &Client,
    base_url: &str,
    patchset_id: &str,
) -> Option<String> {
    let url = format!("{}/api/patchset/activity?id={}", base_url, patchset_id);
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let data: Value = resp.json().await.ok()?;
    let live = data["live"].as_bool().unwrap_or(true);
    let entries = data.get("entries")?.as_array()?;
    if entries.is_empty() {
        return None;
    }

    let parts: Vec<String> = entries
        .iter()
        .map(|e| {
            let desc = e["description"].as_str().unwrap_or("?");
            if live {
                let age = e["age_seconds"].as_u64().unwrap_or(0);
                let idle = e["idle_seconds"].as_u64().unwrap_or(0);
                format!(
                    "{} [{}, idle {}]",
                    desc,
                    format_duration(age),
                    format_duration(idle)
                )
            } else {
                desc.to_string()
            }
        })
        .collect();

    let joined = parts.join(" | ");
    if live {
        Some(joined)
    } else {
        // An observation, not a diagnosis: the work stopped during this
        // activity, which is not the same as failing because of it.
        Some(format!("stopped while: {}", joined))
    }
}

async fn handle_show(
    client: &Client,
    base_url: &str,
    mut id: String,
    watch: bool,
    format: OutputFormat,
    opts: ShowOptions,
) -> Result<()> {
    if id == "latest" {
        let list_url = format!("{}/api/patchsets?page=1&per_page=1", base_url);
        let resp = client.get(&list_url).send().await?;
        if resp.status().is_success() {
            let data: PatchsetsResponse = resp.json().await?;
            if let Some(latest) = data.items.first() {
                id = latest.id.to_string();
            } else {
                return Err(anyhow::anyhow!("No patchsets found"));
            }
        } else {
            return Err(anyhow::anyhow!(
                "Failed to find latest patchset: {}",
                resp.status()
            ));
        }
    }

    if let Some(ref diff_id) = opts.diff {
        return handle_show_diff(client, base_url, &id, diff_id, &format).await;
    }

    let mut last_status = String::new();
    let mut last_activity = String::new();
    loop {
        let url = format!("{}/api/patch?id={}", base_url, id);
        let resp = client.get(&url).send().await?;

        if resp.status().is_success() {
            let mut details: Value = resp.json().await?;
            let status = details["status"].as_str().unwrap_or("").to_string();

            let is_terminal = matches!(
                status.as_str(),
                "Reviewed" | "Failed" | "Error" | "Cancelled" | "Embargoed" | "Failed To Apply"
            );

            if watch && status != last_status {
                if matches!(format, OutputFormat::Text) {
                    println!(
                        "[{}] Status: {}",
                        chrono::Local::now().format("%H:%M:%S"),
                        status
                    );
                }
                last_status = status.clone();
            }

            if watch && !is_terminal {
                // Status alone sits on "In Review" or "Fetching" for the whole
                // run. The activity feed is what shows it is actually moving —
                // and, when it stops moving, where it got stuck.
                if matches!(format, OutputFormat::Text) {
                    let pid = details["id"].to_string();
                    if let Some(desc) = fetch_activity_summary(client, base_url, &pid).await
                        && desc != last_activity
                    {
                        println!("[{}]   {}", chrono::Local::now().format("%H:%M:%S"), desc);
                        last_activity = desc;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }

            let numeric_id = details["id"].to_string();

            let mut review_data = None;
            if status == "Reviewed" || status == "Failed" || status == "Failed To Apply" {
                let review_url = format!("{}/api/review_log?patchset_id={}", base_url, numeric_id);
                let review_resp = client.get(&review_url).send().await?;

                if review_resp.status().is_success() {
                    review_data = Some(review_resp.json::<Value>().await?);
                }
            }

            let patches = details
                .get("patches")
                .and_then(|p| p.as_array())
                .cloned()
                .unwrap_or_default();
            let reviews = details
                .get("reviews")
                .and_then(|r| r.as_array())
                .cloned()
                .unwrap_or_default();

            let reviews_filtered: Vec<&Value> = if let Some(since_id) = opts.since {
                reviews
                    .iter()
                    .filter(|r| r.get("id").and_then(|i| i.as_i64()).unwrap_or(0) > since_id)
                    .collect()
            } else {
                reviews.iter().collect()
            };

            if opts.summary {
                return show_summary(&details, &patches, &reviews_filtered, &format, &opts);
            }

            if let Some(patch_idx) = opts.patch {
                return show_single_patch(
                    patch_idx,
                    &patches,
                    &reviews_filtered,
                    &format,
                    opts.inline,
                );
            }

            if opts.issues {
                return show_issues(&patches, &reviews_filtered, &format, opts.inline);
            }

            match format {
                OutputFormat::Json => {
                    if let Some(r) = review_data {
                        details["review"] = r;
                    }
                    println!("{}", serde_json::to_string_pretty(&details)?);
                }
                OutputFormat::Text => {
                    print_colored(Color::Cyan, "Patchset Details:\n");
                    println!("  ID:        {}", details["id"]);
                    println!("  Subject:   {}", details["subject"].as_str().unwrap_or(""));
                    println!("  Author:    {}", details["author"].as_str().unwrap_or(""));
                    let status_str = details["status"].as_str().unwrap_or("");
                    if status_str == "Embargoed" {
                        if let Some(until_ts) =
                            details.get("embargo_until").and_then(|u| u.as_i64())
                        {
                            println!(
                                "  Status:    Embargoed until {}",
                                format_timestamp(until_ts)
                            );
                        } else {
                            println!("  Status:    Embargoed");
                        }
                    } else {
                        println!("  Status:    {}", status_str);
                    }

                    if let Some(ts) = details["date"].as_i64() {
                        println!("  Date:      {}", format_timestamp(ts));
                    }

                    if let Some(reason) = details.get("failed_reason").and_then(|r| r.as_str()) {
                        print_colored(Color::Red, "\nFailure Reason: ");
                        println!("{}", reason);
                    }

                    println!("\nPatches ({}):", patches.len());
                    for patch in &patches {
                        let p_id = patch["id"].as_i64().unwrap_or(0);
                        let review = find_best_review_for_patch(p_id, &reviews);
                        print_patch_line(patch, review, opts.inline);
                    }

                    // Outside the review-log block below, which is only fetched
                    // for terminal statuses. Timings asked for while a review is
                    // running are the ones worth having.
                    if opts.timings {
                        let activity = fetch_patch_activity(client, base_url, &numeric_id).await;
                        print_patch_timings(&patches, &reviews_filtered, activity.as_ref());
                    }

                    if let Some(review) = review_data {
                        println!("\nReview Summary:");
                        if let Some(verdict) = review.get("verdict").and_then(|v| v.as_str()) {
                            let color = match verdict {
                                "LGTM" => Color::Green,
                                "Request Changes" => Color::Red,
                                _ => Color::Yellow,
                            };
                            print!("  Verdict: ");
                            print_colored(color, verdict);
                            println!();
                        }

                        if let Some(model) = review.get("model").and_then(|m| m.as_str()) {
                            println!("  Model:   {}", model);
                        }

                        if let Some(summary) = review.get("summary").and_then(|s| s.as_str())
                            && summary != "No summary available."
                            && !summary.is_empty()
                        {
                            println!("\n{}", summary.trim());
                        }

                        println!();
                        for patch in &patches {
                            let idx = patch["part_index"].as_i64().unwrap_or(0);
                            let subject = patch["subject"].as_str().unwrap_or("");
                            let p_id = patch["id"].as_i64().unwrap_or(0);

                            if let Some(r) = find_best_review_for_patch(p_id, &reviews) {
                                print_attempt_note(r);
                                print_coverage_warning(r);

                                if let Some(output_str) = r.get("output").and_then(|o| o.as_str())
                                    && let Ok(output_json) = from_str::<Value>(output_str)
                                    && let Some(findings) =
                                        output_json.get("findings").and_then(|f| f.as_array())
                                {
                                    let inline = r.get("inline_review").and_then(|s| s.as_str());
                                    print_findings_summary(
                                        &format!("Patch {}: {}", idx, subject),
                                        findings,
                                        inline,
                                        SummaryMode::Patch,
                                    );
                                }
                            }
                        }
                    } else if let Some(logs) = details.get("baseline_logs").and_then(|l| l.as_str())
                        && status == "Failed To Apply"
                    {
                        println!("\nBaseline Logs:\n{}", logs);
                    }
                }
            }
            break;
        } else {
            return Err(anyhow::anyhow!(
                "Failed to show patchset: {}",
                resp.status()
            ));
        }
    }

    Ok(())
}

fn show_summary(
    details: &Value,
    patches: &[Value],
    reviews: &[&Value],
    format: &OutputFormat,
    opts: &ShowOptions,
) -> Result<()> {
    let ps_id = details["id"].as_i64().unwrap_or(0);
    let ps_status = details["status"].as_str().unwrap_or("");
    let total = patches.len();

    let mut reviewed_clean = 0usize;
    let mut reviewed_issues = 0usize;
    let mut in_review = 0usize;
    let mut not_started = 0usize;
    let mut failed = 0usize;
    let mut latest_review_id: i64 = 0;
    let mut issues_list: Vec<(i64, String)> = Vec::new();

    for patch in patches {
        let p_id = patch["id"].as_i64().unwrap_or(0);
        let matching: Vec<&&Value> = reviews
            .iter()
            .filter(|r| r.get("patch_id").and_then(|id| id.as_i64()) == Some(p_id))
            .collect();

        if matching.is_empty() {
            not_started += 1;
            continue;
        }

        let best = matching
            .iter()
            .filter(|r| r.get("status").and_then(|s| s.as_str()) == Some("Reviewed"))
            .max_by_key(|r| r.get("id").and_then(|i| i.as_i64()).unwrap_or(0))
            .or_else(|| matching.last());

        if let Some(rev) = best {
            let rev_id = rev.get("id").and_then(|i| i.as_i64()).unwrap_or(0);
            if rev_id > latest_review_id {
                latest_review_id = rev_id;
            }

            match rev.get("status").and_then(|s| s.as_str()).unwrap_or("") {
                "Reviewed" => {
                    if review_has_new_issues(rev) {
                        reviewed_issues += 1;
                        let idx = patch["part_index"].as_i64().unwrap_or(0);
                        let subject = patch["subject"].as_str().unwrap_or("").to_string();
                        issues_list.push((idx, subject));
                    } else {
                        reviewed_clean += 1;
                    }
                }
                "Failed" | "Error" => failed += 1,
                _ => in_review += 1,
            }
        }
    }

    match format {
        OutputFormat::Json => {
            let issues_json: Vec<Value> = issues_list
                .iter()
                .map(|(idx, subject)| {
                    serde_json::json!({
                        "part_index": idx,
                        "subject": subject,
                    })
                })
                .collect();
            let summary = serde_json::json!({
                "patchset_id": ps_id,
                "total_patches": total,
                "status": ps_status,
                "reviewed_clean": reviewed_clean,
                "reviewed_issues": reviewed_issues,
                "in_review": in_review,
                "not_started": not_started,
                "failed": failed,
                "latest_review_id": latest_review_id,
                "issues": issues_json,
            });
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        OutputFormat::Text => {
            println!("PS {} ({} patches) — Status: {}", ps_id, total, ps_status);
            println!(
                "  Reviewed:    {:>3}  (clean: {}, issues: {})",
                reviewed_clean + reviewed_issues,
                reviewed_clean,
                reviewed_issues
            );
            println!("  In Review:   {:>3}", in_review);
            println!("  Not Started: {:>3}", not_started);
            println!("  Failed:      {:>3}", failed);

            // Time spent reviewing, not time since submission: a patchset can
            // sit in the queue for a while before a worker picks it up.
            if let Some(secs) = details
                .get("review_duration_seconds")
                .and_then(|d| d.as_u64())
            {
                println!("  Review Time: {:>3}", format_duration(secs));
            }

            if !issues_list.is_empty() && !opts.issues {
                println!("\nIssues Found:");
                for (idx, subject) in &issues_list {
                    println!("  [{}] {}", idx, subject);
                }
            }

            if opts.issues {
                println!();
                let all_reviews: Vec<&Value> = reviews.to_vec();
                for patch in patches {
                    let p_id = patch["id"].as_i64().unwrap_or(0);
                    if let Some(rev) = find_best_review_for_patch_refs(p_id, &all_reviews)
                        && review_has_new_issues(rev)
                    {
                        print_patch_line(patch, Some(rev), opts.inline);
                    }
                }
            }
        }
    }
    Ok(())
}

fn show_single_patch(
    patch_idx: i64,
    patches: &[Value],
    reviews: &[&Value],
    format: &OutputFormat,
    show_inline: bool,
) -> Result<()> {
    let patch = patches
        .iter()
        .find(|p| p["part_index"].as_i64() == Some(patch_idx));

    let patch = match patch {
        Some(p) => p,
        None => return Err(anyhow::anyhow!("Patch {} not found", patch_idx)),
    };

    let p_id = patch["id"].as_i64().unwrap_or(0);
    let all_reviews: Vec<&Value> = reviews.to_vec();
    let review = find_best_review_for_patch_refs(p_id, &all_reviews);

    match format {
        OutputFormat::Json => {
            let result_label = review.map(review_result_label).unwrap_or("");
            let inline = review
                .and_then(|r| r.get("inline_review").and_then(|s| s.as_str()))
                .unwrap_or("");
            let review_id = review
                .and_then(|r| r.get("id").and_then(|i| i.as_i64()))
                .unwrap_or(0);
            let rev_status = review
                .and_then(|r| r.get("status").and_then(|s| s.as_str()))
                .unwrap_or("");

            let out = serde_json::json!({
                "part_index": patch_idx,
                "subject": patch["subject"].as_str().unwrap_or(""),
                "status": rev_status,
                "result": result_label,
                "inline_review": inline,
                "review_id": review_id,
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        OutputFormat::Text => {
            print_patch_line(patch, review, show_inline);

            if let Some(rev) = review
                && let Some(output_str) = rev.get("output").and_then(|o| o.as_str())
                && let Ok(output_json) = from_str::<Value>(output_str)
                && let Some(findings) = output_json.get("findings").and_then(|f| f.as_array())
            {
                let idx = patch["part_index"].as_i64().unwrap_or(0);
                let subject = patch["subject"].as_str().unwrap_or("");
                let inline = rev.get("inline_review").and_then(|s| s.as_str());
                print_findings_summary(
                    &format!("Patch {}: {}", idx, subject),
                    findings,
                    inline,
                    SummaryMode::Patch,
                );
            }
        }
    }
    Ok(())
}

fn show_issues(
    patches: &[Value],
    reviews: &[&Value],
    format: &OutputFormat,
    show_inline: bool,
) -> Result<()> {
    let all_reviews: Vec<&Value> = reviews.to_vec();
    let mut issue_patches: Vec<(&Value, &Value)> = Vec::new();

    for patch in patches {
        let p_id = patch["id"].as_i64().unwrap_or(0);
        if let Some(rev) = find_best_review_for_patch_refs(p_id, &all_reviews)
            && review_has_new_issues(rev)
        {
            issue_patches.push((patch, rev));
        }
    }

    match format {
        OutputFormat::Json => {
            let items: Vec<Value> = issue_patches
                .iter()
                .map(|(patch, rev)| {
                    serde_json::json!({
                        "part_index": patch["part_index"].as_i64().unwrap_or(0),
                        "subject": patch["subject"].as_str().unwrap_or(""),
                        "review_id": rev.get("id").and_then(|i| i.as_i64()).unwrap_or(0),
                        "inline_review": rev.get("inline_review").and_then(|s| s.as_str()).unwrap_or(""),
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&Value::Array(items))?);
        }
        OutputFormat::Text => {
            if issue_patches.is_empty() {
                println!("No issues found.");
                return Ok(());
            }
            for (patch, rev) in &issue_patches {
                print_patch_line(patch, Some(rev), show_inline);

                if !show_inline
                    && let Some(output_str) = rev.get("output").and_then(|o| o.as_str())
                    && let Ok(output_json) = from_str::<Value>(output_str)
                    && let Some(findings) = output_json.get("findings").and_then(|f| f.as_array())
                {
                    let idx = patch["part_index"].as_i64().unwrap_or(0);
                    let subject = patch["subject"].as_str().unwrap_or("");
                    let inline = rev.get("inline_review").and_then(|s| s.as_str());
                    print_findings_summary(
                        &format!("Patch {}: {}", idx, subject),
                        findings,
                        inline,
                        SummaryMode::Patch,
                    );
                }
            }
        }
    }
    Ok(())
}

async fn handle_show_diff(
    client: &Client,
    base_url: &str,
    current_id: &str,
    other_id: &str,
    format: &OutputFormat,
) -> Result<()> {
    let current = fetch_patchset(client, base_url, current_id).await?;
    let other = fetch_patchset(client, base_url, other_id).await?;

    let current_patches = current
        .get("patches")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let other_patches = other
        .get("patches")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let current_reviews = current
        .get("reviews")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    let other_reviews = other
        .get("reviews")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();

    let current_ps_id = current["id"].as_i64().unwrap_or(0);
    let other_ps_id = other["id"].as_i64().unwrap_or(0);

    struct PatchDiff {
        part_index: i64,
        subject: String,
        old_result: String,
        new_result: String,
        is_new: bool,
        is_removed: bool,
    }

    let mut diffs: Vec<PatchDiff> = Vec::new();

    for cp in &current_patches {
        let idx = cp["part_index"].as_i64().unwrap_or(0);
        let subject = cp["subject"].as_str().unwrap_or("").to_string();
        let cp_id = cp["id"].as_i64().unwrap_or(0);
        let c_rev = find_best_review_for_patch(cp_id, &current_reviews);
        let new_result = c_rev
            .map(|r| review_result_label(r).to_string())
            .unwrap_or_else(|| "Not Started".to_string());

        let matched = other_patches.iter().find(|op| {
            op["subject"].as_str().unwrap_or("") == subject
                || op["part_index"].as_i64() == Some(idx)
        });

        if let Some(op) = matched {
            let op_id = op["id"].as_i64().unwrap_or(0);
            let o_rev = find_best_review_for_patch(op_id, &other_reviews);
            let old_result = o_rev
                .map(|r| review_result_label(r).to_string())
                .unwrap_or_else(|| "Not Started".to_string());

            diffs.push(PatchDiff {
                part_index: idx,
                subject,
                old_result,
                new_result,
                is_new: false,
                is_removed: false,
            });
        } else {
            diffs.push(PatchDiff {
                part_index: idx,
                subject,
                old_result: String::new(),
                new_result,
                is_new: true,
                is_removed: false,
            });
        }
    }

    for op in &other_patches {
        let subject = op["subject"].as_str().unwrap_or("");
        let idx = op["part_index"].as_i64().unwrap_or(0);
        let already_matched = current_patches.iter().any(|cp| {
            cp["subject"].as_str().unwrap_or("") == subject
                || cp["part_index"].as_i64() == Some(idx)
        });
        if !already_matched {
            let op_id = op["id"].as_i64().unwrap_or(0);
            let o_rev = find_best_review_for_patch(op_id, &other_reviews);
            let old_result = o_rev
                .map(|r| review_result_label(r).to_string())
                .unwrap_or_else(|| "Not Started".to_string());
            diffs.push(PatchDiff {
                part_index: idx,
                subject: subject.to_string(),
                old_result,
                new_result: String::new(),
                is_new: false,
                is_removed: true,
            });
        }
    }

    match format {
        OutputFormat::Json => {
            let items: Vec<Value> = diffs
                .iter()
                .map(|d| {
                    let mut obj = serde_json::json!({
                        "part_index": d.part_index,
                        "subject": d.subject,
                    });
                    if d.is_new {
                        obj["change"] = "added".into();
                        obj["result"] = d.new_result.as_str().into();
                    } else if d.is_removed {
                        obj["change"] = "removed".into();
                        obj["result"] = d.old_result.as_str().into();
                    } else if d.old_result != d.new_result {
                        obj["change"] = "changed".into();
                        obj["old_result"] = d.old_result.as_str().into();
                        obj["new_result"] = d.new_result.as_str().into();
                    } else {
                        obj["change"] = "unchanged".into();
                        obj["result"] = d.new_result.as_str().into();
                    }
                    obj
                })
                .collect();
            let out = serde_json::json!({
                "current_patchset": current_ps_id,
                "compared_with": other_ps_id,
                "patches": items,
            });
            println!("{}", serde_json::to_string_pretty(&out)?);
        }
        OutputFormat::Text => {
            println!("Diff: PS {} vs PS {}\n", current_ps_id, other_ps_id);
            let changed: Vec<&PatchDiff> = diffs
                .iter()
                .filter(|d| d.is_new || d.is_removed || d.old_result != d.new_result)
                .collect();

            if changed.is_empty() {
                println!("No changes between patchsets.");
            } else {
                for d in &changed {
                    if d.is_new {
                        print_colored(Color::Green, "  + ");
                        println!("[{}] {} ({})", d.part_index, d.subject, d.new_result);
                    } else if d.is_removed {
                        print_colored(Color::Red, "  - ");
                        println!("[{}] {} (was: {})", d.part_index, d.subject, d.old_result);
                    } else {
                        print_colored(Color::Yellow, "  ~ ");
                        println!(
                            "[{}] {} ({} -> {})",
                            d.part_index, d.subject, d.old_result, d.new_result
                        );
                    }
                }
            }

            let unchanged_count = diffs.len() - changed.len();
            if unchanged_count > 0 {
                println!("\n  {} patches unchanged", unchanged_count);
            }
        }
    }
    Ok(())
}

async fn handle_rerun(
    client: &Client,
    base_url: &str,
    id: i64,
    format: OutputFormat,
) -> Result<()> {
    let url = format!("{}/api/patchset/rerun?id={}", base_url, id);
    let resp = client.post(&url).send().await?;

    if resp.status().is_success() {
        let result: serde_json::Value = resp.json().await?;
        match format {
            OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&result)?),
            OutputFormat::Text => {
                // The endpoint returns 200 either way; the body says whether
                // anything actually happened. Reporting success unconditionally
                // would hide a refused rerun.
                if result["status"] == "accepted" {
                    print_colored(Color::Green, "Rerun queued: ");
                    println!("Patchset {} has been re-added to the review queue.", id);
                } else {
                    print_colored(Color::Yellow, "Not rerun: ");
                    println!(
                        "{}",
                        result["reason"]
                            .as_str()
                            .unwrap_or("Patchset could not be requeued.")
                    );
                }
            }
        }
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Rerun failed ({}): {}", status, text));
    }

    Ok(())
}

/// Reviews or re-reviews a pull request by number.
///
/// The daemon does the resolving: it is the process that knows which repository
/// it watches and has credentials for the forge, and routing through it keeps a
/// hand-triggered review identical to the one the webhook would have run.
/// Recognises the ways someone refers to a pull request.
///
/// Accepts `#20`, `pr/20`, `PR20`, and a full GitHub or GitLab URL. A bare `20`
/// is deliberately not accepted: it is a valid git revision, and silently
/// treating it as a pull request would review the wrong thing without saying so.
fn parse_pr_reference(input: &str) -> Option<i64> {
    let input = input.trim();

    if let Some(rest) = input.strip_prefix('#') {
        return rest.parse().ok().filter(|n| *n > 0);
    }

    let lowered = input.to_ascii_lowercase();
    for prefix in ["pr/", "pr#", "pr-", "pr"] {
        if let Some(rest) = lowered.strip_prefix(prefix)
            && let Ok(number) = rest.parse::<i64>()
            && number > 0
        {
            return Some(number);
        }
    }

    // .../pull/20, .../pull/20/files, .../-/merge_requests/20
    if lowered.starts_with("http://") || lowered.starts_with("https://") {
        let parts: Vec<&str> = lowered.trim_end_matches('/').split('/').collect();
        for (i, part) in parts.iter().enumerate() {
            if matches!(*part, "pull" | "pulls" | "merge_requests")
                && let Some(number) = parts.get(i + 1).and_then(|n| n.parse::<i64>().ok())
                && number > 0
            {
                return Some(number);
            }
        }
    }

    None
}

async fn handle_pr_review(
    client: &Client,
    base_url: &str,
    number: i64,
    format: OutputFormat,
) -> Result<()> {
    let url = format!("{}/api/pr/review?number={}", base_url, number);
    let resp = client.post(&url).send().await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!(
            "Could not review PR #{} ({}): {}",
            number,
            status,
            text
        ));
    }

    let result: serde_json::Value = resp.json().await?;
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&result)?),
        OutputFormat::Text => {
            let message = result["message"].as_str();
            match result["status"].as_str() {
                Some("accepted") => {
                    print_colored(Color::Green, "Queued: ");
                    println!("{}", message.unwrap_or("Pull request queued for review."));
                }
                _ => {
                    print_colored(Color::Yellow, "Not queued: ");
                    println!(
                        "{}",
                        message
                            .or(result["reason"].as_str())
                            .unwrap_or("The pull request could not be queued.")
                    );
                }
            }
            // A stale re-review reports the PR number it was asked for, so say
            // plainly that it may not be the PR's current code.
            if result["stale"].as_bool() == Some(true) {
                print_colored(Color::Yellow, "Warning: ");
                println!("reviewing the last known range; new commits may be missing.");
            }
        }
    }

    Ok(())
}

async fn handle_cancel(
    client: &Client,
    base_url: &str,
    id: i64,
    force: bool,
    format: OutputFormat,
) -> Result<()> {
    let url = format!("{}/api/patchset/cancel?id={}&force={}", base_url, id, force);
    let resp = client.post(&url).send().await?;

    if resp.status().is_success() {
        let result: serde_json::Value = resp.json().await?;
        match format {
            OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&result)?),
            OutputFormat::Text => {
                let status = result["status"].as_str().unwrap_or("");
                if status == "cancelled" {
                    print_colored(Color::Green, "Cancelled: ");
                    println!("Patchset {} has been cancelled.", id);
                } else {
                    print_colored(Color::Yellow, "Not modified: ");
                    println!(
                        "{}",
                        result["reason"]
                            .as_str()
                            .unwrap_or("Patchset could not be cancelled.")
                    );
                }
            }
        }
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Cancel failed ({}): {}", status, text));
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_local(
    client: &Client,
    base_url: &str,
    input: String,
    baseline: Option<String>,
    repo: Option<PathBuf>,
    no_ai: bool,
    custom_prompt: Option<String>,
    force_local: bool,
    interactive: bool,
    format: OutputFormat,
) -> Result<()> {
    // Determine repository path
    let repo_path = if let Some(r) = repo {
        r
    } else {
        let cwd = std::env::current_dir().context("Failed to get current directory")?;
        // Verify CWD is a git repo
        if !cwd.join(".git").exists() {
            return Err(anyhow::anyhow!(
                "Current directory is not a git repository. Use --repo to specify one."
            ));
        }
        cwd
    };

    // Check if server is running (unless --force-local)
    if !force_local && let Ok(settings) = Settings::new() {
        let addr = format!("{}:{}", settings.server.host, settings.server.port);
        if tokio::net::TcpStream::connect(&addr).await.is_ok() {
            // Server is running — submit via API
            let submit_type = if input.contains("..") {
                SubmitType::Range
            } else {
                SubmitType::Remote
            };
            let repo_str = Some(repo_path.to_string_lossy().to_string());

            let url = format!("{}/api/submit", base_url);
            let payload = match submit_type {
                // `local` has no cover-letter flag of its own; use `submit`
                // for that.
                SubmitType::Range => SubmitRequest::RemoteRange {
                    sha: input.clone(),
                    repo: repo_str,
                    skip_subjects: None,
                    only_subjects: None,
                    b4_cover_letter: false,
                },
                _ => SubmitRequest::Remote {
                    sha: input.clone(),
                    repo: repo_str,
                    skip_subjects: None,
                    only_subjects: None,
                    b4_cover_letter: false,
                },
            };

            let resp = client.post(&url).json(&payload).send().await?;
            if resp.status().is_success() {
                let result: SubmitResponse = resp.json().await?;
                print_colored(Color::Green, "Queued: ");
                println!(
                    "Review submitted (ID: {}). View at {}/",
                    result.id, base_url
                );
                println!("\nUse `sashiko-cli show {}` to check progress.", result.id);
            } else {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!(
                    "Failed to queue review ({}): {}",
                    status,
                    text
                ));
            }
            return Ok(());
        }
    }
    // Cold start path: run review locally via sashiko-review subprocess
    let mut dynamic_prompt = custom_prompt;
    loop {
        eprint_phase(1, 4, &format!("Extracting patches from {}...", input));

        // Resolve commits
        let shas = if input.contains("..") {
            sashiko::git_ops::resolve_git_range(&repo_path, &input).await?
        } else {
            // Single ref — resolve to SHA
            let sha = sashiko::git_ops::get_commit_hash(&repo_path, &input).await?;
            vec![sha]
        };

        eprintln!(
            " ({} commit{})",
            shas.len(),
            if shas.len() == 1 { "" } else { "s" }
        );

        // Extract patch metadata and build ReviewInput
        let mut patches = Vec::new();
        for (i, sha) in shas.iter().enumerate() {
            let meta = sashiko::git_ops::extract_patch_metadata(&repo_path, sha)
                .await
                .with_context(|| format!("Failed to extract metadata for commit {}", sha))?;
            patches.push(sashiko::worker::PatchInput {
                index: (i + 1) as i64,
                diff: meta.diff,
                subject: Some(meta.subject),
                author: Some(meta.author),
                date: Some(meta.timestamp),
                message_id: None,
                commit_id: Some(sha.clone()),
            });
        }

        let review_input = sashiko::worker::ReviewInput {
            id: 0, // Local review, no DB ID
            subject: patches
                .first()
                .and_then(|p| p.subject.clone())
                .unwrap_or_else(|| input.clone()),
            // Local review of a git range; a cover letter is a submit-time
            // choice made against the daemon.
            cover_letter: None,
            patches,
        };

        let review_json =
            serde_json::to_string(&review_input).context("Failed to serialize review input")?;

        // Locate worker binary
        let (worker_bin, worker_subcmd) = find_worker_command()?;

        // Build subprocess args
        let baseline_ref = if let Some(b) = &baseline {
            b.clone()
        } else {
            // Default: parent of first commit
            let first_sha = &shas[0];
            format!("{}^", first_sha)
        };

        let mut args = vec![
            "--baseline".to_string(),
            baseline_ref,
            "--repo".to_string(),
            repo_path.to_string_lossy().to_string(),
        ];

        if no_ai {
            args.push("--no-ai".to_string());
        }
        if let Some(prompt) = &dynamic_prompt {
            args.push("--custom-prompt".to_string());
            args.push(prompt.clone());
        }

        eprint_phase(2, 4, "Starting review subprocess...");
        eprintln!();

        // Spawn review subprocess
        let mut cmd = tokio::process::Command::new(&worker_bin);
        if let Some(subcmd) = worker_subcmd {
            cmd.arg(subcmd);
        }
        let mut child = cmd
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env("SASHIKO_LOG_PLAIN", "1")
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("Failed to start worker binary: {:?}", worker_bin))?;

        // Write input to stdin
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin
                .write_all(format!("{}\n", review_json).as_bytes())
                .await
                .context("Failed to write to review subprocess stdin")?;
            drop(stdin);
        }

        // Stream stderr for progress in a background task
        let stderr = child.stderr.take();
        let stderr_handle = tokio::spawn(async move {
            if let Some(stderr) = stderr {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                let mut saw_applying = false;
                let mut saw_ai_review = false;
                while let Ok(Some(line)) = lines.next_line().await {
                    if !saw_applying && line.contains("Applying") {
                        saw_applying = true;
                        eprint_phase(2, 4, "Applying patches to worktree...");
                        eprintln!();
                    }
                    if !saw_ai_review && line.contains("Starting AI review") {
                        saw_ai_review = true;
                        eprint_phase(3, 4, "AI review in progress...");
                        eprintln!();
                    }
                    if line.contains("AI review completed") {
                        eprint_phase(4, 4, "Review complete.");
                        eprintln!();
                    }
                }
            }
        });

        // Capture stdout
        let output = child
            .wait_with_output()
            .await
            .context("Failed to wait for review subprocess")?;

        let _ = stderr_handle.await;

        let exit_code = output.status.code().unwrap_or(1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();

        if stdout.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "Review subprocess produced no output (exit code: {})",
                exit_code
            ));
        }

        // Parse the review output
        let result: Value =
            serde_json::from_str(stdout.trim()).context("Failed to parse review output JSON")?;

        match format {
            OutputFormat::Json => {
                println!("{}", serde_json::to_string_pretty(&result)?);
            }
            OutputFormat::Text => {
                print_local_review_results(&result, &input);
            }
        }

        // Determine exit code based on findings
        let mut has_error = false;
        if let Some(err) = result.get("error").and_then(|e| e.as_str())
            && !err.is_empty()
        {
            has_error = true;
        }
        let mut has_issues = false;
        if let Some(review) = result.get("review")
            && let Some(findings) = review.get("findings").and_then(|f| f.as_array())
        {
            let counts = count_severities(findings);
            if counts.critical.new > 0
                || counts.high.new > 0
                || counts.medium.new > 0
                || counts.low.new > 0
            {
                has_issues = true;
            }
        }

        if interactive && (has_error || has_issues) {
            println!(
                "\nIssues found. Please modify the code, or type a rebuttal here, then press Enter to re-run the review... (or Ctrl+C to exit)"
            );
            let mut rebuttal = String::new();
            std::io::stdin().read_line(&mut rebuttal)?;
            if !rebuttal.trim().is_empty() {
                dynamic_prompt = Some(rebuttal.trim().to_string());
            } else {
                dynamic_prompt = None;
            }
            continue;
        }

        if has_error {
            std::process::exit(3);
        }
        if let Some(review) = result.get("review")
            && let Some(findings) = review.get("findings").and_then(|f| f.as_array())
        {
            let counts = count_severities(findings);
            if counts.critical.new > 0 || counts.high.new > 0 {
                std::process::exit(1);
            }
        }

        break;
    } // End of loop

    Ok(())
}

fn eprint_phase(current: usize, total: usize, msg: &str) {
    eprint!("[{}/{}] {}", current, total, msg);
}

fn find_worker_command() -> Result<(PathBuf, Option<&'static str>)> {
    // Try same directory as current executable
    if let Ok(exe) = std::env::current_exe() {
        let dir = exe.parent().unwrap_or(std::path::Path::new("."));
        let candidate = dir.join("sashiko");
        if candidate.exists() {
            return Ok((candidate, Some("worker")));
        }
        if let Some(parent) = dir.parent() {
            let candidate = parent.join("sashiko");
            if candidate.exists() {
                return Ok((candidate, Some("worker")));
            }
        }
        let candidate = dir.join("sashiko-review");
        if candidate.exists() {
            return Ok((candidate, None));
        }
    }

    // Try PATH for sashiko
    if let Ok(output) = std::process::Command::new("which").arg("sashiko").output()
        && output.status.success()
    {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Ok((PathBuf::from(path), Some("worker")));
    }

    // Try PATH for sashiko-review
    if let Ok(output) = std::process::Command::new("which")
        .arg("sashiko-review")
        .output()
        && output.status.success()
    {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Ok((PathBuf::from(path), None));
    }

    Err(anyhow::anyhow!(
        "Cannot find sashiko binary.\n\
         Build it with: cargo build --bin sashiko\n\
         Or specify its location in PATH."
    ))
}

fn print_local_review_results(result: &Value, input: &str) {
    let baseline = result["baseline"].as_str().unwrap_or("unknown");
    print_colored(Color::Cyan, "Local Review Results\n");
    println!("  Input:    {}", input);
    println!("  Baseline: {}", baseline);

    // Show patch application status
    if let Some(patches) = result.get("patches").and_then(|p| p.as_array()) {
        println!("\nPatch Application:");
        for p in patches {
            let idx = p["index"].as_i64().unwrap_or(0);
            let status = p["status"].as_str().unwrap_or("unknown");
            let method = p["method"].as_str().unwrap_or("");
            let color = match status {
                "applied" => Color::Green,
                "failed" | "error" => Color::Red,
                _ => Color::Yellow,
            };
            print!("  [{}] ", idx);
            print_colored(color, status);
            if !method.is_empty() {
                print!(" ({})", method);
            }
            println!();
            if let Some(err) = p.get("error").and_then(|e| e.as_str()) {
                print_colored(Color::Red, "      Error: ");
                println!("{}", err);
            }
        }
    }

    // Show error if present
    if let Some(err) = result.get("error").and_then(|e| e.as_str())
        && !err.is_empty()
    {
        println!();
        print_colored(Color::Red, "Error: ");
        println!("{}", err);
        return;
    }

    // Show review results
    if let Some(review) = result.get("review")
        && let Some(findings) = review.get("findings").and_then(|f| f.as_array())
    {
        let inline = result.get("inline_review").and_then(|s| s.as_str());
        println!();
        let num_patches = result
            .get("patches")
            .and_then(|p| p.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let mode = if num_patches > 1 {
            SummaryMode::Patchset
        } else {
            SummaryMode::Patch
        };
        if !print_findings_summary("Review Findings:", findings, inline, mode) {
            print_colored(Color::Green, "\nNo issues found.\n");
        }
    }

    // Show token usage
    let tokens_in = result["tokens_in"].as_u64().unwrap_or(0);
    let tokens_out = result["tokens_out"].as_u64().unwrap_or(0);
    let tokens_cached = result["tokens_cached"].as_u64().unwrap_or(0);
    if tokens_in > 0 || tokens_out > 0 {
        println!(
            "\nTokens: {} in / {} out / {} cached",
            tokens_in, tokens_out, tokens_cached
        );
    }
}

#[derive(Default, Clone, Copy, Debug)]
struct BugCounts {
    new: usize,
    preexisting: usize,
}

#[derive(Default, Debug)]
struct SeverityCounts {
    critical: BugCounts,
    high: BugCounts,
    medium: BugCounts,
    low: BugCounts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SummaryMode {
    Patch,
    Patchset,
}

/// Count finding severities from a findings JSON array.
fn count_severities(findings: &[Value]) -> SeverityCounts {
    let mut counts = SeverityCounts::default();
    for f in findings {
        if let Some(sev) = f.get("severity").and_then(|s| s.as_str()) {
            let preexisting = f
                .get("preexisting")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            let entry = match sev.to_lowercase().as_str() {
                "critical" => &mut counts.critical,
                "high" => &mut counts.high,
                "medium" => &mut counts.medium,
                "low" => &mut counts.low,
                _ => continue,
            };
            if preexisting {
                entry.preexisting += 1;
            } else {
                entry.new += 1;
            }
        }
    }
    counts
}

/// Print a findings summary line with severity counts if any findings exist.
/// Returns true if findings were printed.
fn print_severity_count(label: &str, counts: BugCounts, color: Color, mode: SummaryMode) {
    print!("{}: ", label);
    print_colored(color, &counts.new.to_string());
    if matches!(mode, SummaryMode::Patch) && counts.preexisting > 0 {
        print!(" (");
        print_colored(color, &counts.preexisting.to_string());
        print!(")");
    }
}

/// Print a findings summary line with severity counts if any findings exist.
/// Returns true if findings were printed.
fn print_findings_summary(
    label: &str,
    findings: &[Value],
    inline_review: Option<&str>,
    mode: SummaryMode,
) -> bool {
    let findings_to_process = if matches!(mode, SummaryMode::Patchset) {
        findings
            .iter()
            .filter(|f| {
                !f.get("preexisting")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false)
            })
            .cloned()
            .collect::<Vec<Value>>()
    } else {
        findings.to_vec()
    };

    let counts = count_severities(&findings_to_process);
    let has_any_bugs = counts.critical.new > 0
        || counts.critical.preexisting > 0
        || counts.high.new > 0
        || counts.high.preexisting > 0
        || counts.medium.new > 0
        || counts.medium.preexisting > 0
        || counts.low.new > 0
        || counts.low.preexisting > 0;

    if !has_any_bugs {
        return false;
    }

    println!("{}", label);
    print_severity_count("Critical", counts.critical, Color::Red, mode);
    print!(" · ");
    print_severity_count("High", counts.high, Color::Red, mode);
    print!(" · ");
    print_severity_count("Medium", counts.medium, Color::Yellow, mode);
    print!(" · ");
    print_severity_count("Low", counts.low, Color::Cyan, mode);
    println!("\n");

    for f in &findings_to_process {
        let sev = f
            .get("severity")
            .and_then(|s| s.as_str())
            .unwrap_or("Unknown");
        let file = f.get("file").and_then(|s| s.as_str()).unwrap_or("");
        let line = f.get("line").and_then(|l| l.as_i64()).unwrap_or(0);
        let desc = f
            .get("problem_description")
            .and_then(|s| s.as_str())
            .unwrap_or("");
        let fix = f
            .get("recommended_fix")
            .and_then(|s| s.as_str())
            .unwrap_or("");

        let color = match sev.to_lowercase().as_str() {
            "critical" | "high" => Color::Red,
            "medium" => Color::Yellow,
            "low" => Color::Cyan,
            _ => Color::White,
        };

        let location = if !file.is_empty() {
            format!("{}:{}: ", file, line)
        } else {
            "".to_string()
        };

        print_colored(color, &format!("  {}[{}] ", location, sev));
        println!("{}", desc);
        if !fix.is_empty() {
            println!("    Fix: {}", fix);
        }
    }
    println!();

    if let Some(inline) = inline_review
        && !inline.is_empty()
        && inline != "No issues found."
    {
        println!("{}", inline.trim());
    }
    println!();
    true
}

fn print_colored(color: Color, text: &str) {
    let choice = COLOR_CHOICE.get().copied().unwrap_or(ColorChoice::Auto);
    let mut stdout = StandardStream::stdout(choice);
    let _ = stdout.set_color(ColorSpec::new().set_fg(Some(color)));
    let _ = write!(&mut stdout, "{}", text);
    let _ = stdout.reset();
}

fn format_timestamp(ts: i64) -> String {
    if ts == 0 {
        return "-".to_string();
    }
    match Utc.timestamp_opt(ts, 0) {
        chrono::LocalResult::Single(dt) => {
            let local_dt: DateTime<Local> = DateTime::from(dt);
            local_dt.format("%Y-%m-%d %H:%M:%S").to_string()
        }
        _ => ts.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pr_references_are_recognised_without_swallowing_git_revisions() {
        for (input, expected) in [
            ("#20", 20),
            (" #20 ", 20),
            ("pr/20", 20),
            ("PR20", 20),
            ("pr#20", 20),
            ("https://github.com/ricera/sashiko/pull/20", 20),
            ("https://github.com/ricera/sashiko/pull/20/files", 20),
            ("https://gitlab.com/org/repo/-/merge_requests/7", 7),
        ] {
            assert_eq!(parse_pr_reference(input), Some(expected), "{input}");
        }

        // A bare number is a valid git revision. Treating it as a pull request
        // would review something other than what was asked for, silently.
        assert_eq!(parse_pr_reference("20"), None);
        // Ordinary submit inputs must fall through to type detection untouched.
        for input in [
            "HEAD",
            "HEAD~3..HEAD",
            "ffad4278c2ac",
            "ffad4278c2ac^..11016c795eec",
            "/tmp/series.mbox",
            "20260101.abcdef@lore.kernel.org",
            "#0",
            "#-1",
            "#abc",
        ] {
            assert_eq!(parse_pr_reference(input), None, "{input}");
        }
    }

    #[test]
    fn test_format_subject_handles_multibyte_cutoff() {
        let subject = format!("{}🙂rest", "a".repeat(44));

        assert_eq!(format_subject(&subject), format!("{}...", "a".repeat(44)));
    }

    #[test]
    fn test_count_severities_mixed() {
        let findings = vec![
            json!({ "severity": "Critical", "preexisting": false }),
            json!({ "severity": "Critical", "preexisting": true }),
            json!({ "severity": "High", "preexisting": false }),
            json!({ "severity": "Medium", "preexisting": true }),
            json!({ "severity": "Low", "preexisting": false }),
            json!({ "severity": "Low", "preexisting": false }),
            json!({ "severity": "Unknown", "preexisting": false }), // Should be ignored
        ];

        let counts = count_severities(&findings);

        assert_eq!(counts.critical.new, 1);
        assert_eq!(counts.critical.preexisting, 1);
        assert_eq!(counts.high.new, 1);
        assert_eq!(counts.high.preexisting, 0);
        assert_eq!(counts.medium.new, 0);
        assert_eq!(counts.medium.preexisting, 1);
        assert_eq!(counts.low.new, 2);
        assert_eq!(counts.low.preexisting, 0);
    }

    #[test]
    fn test_count_severities_default_new() {
        let findings = vec![
            json!({ "severity": "High" }), // Missing preexisting should default to false (new)
        ];
        let counts = count_severities(&findings);
        assert_eq!(counts.high.new, 1);
        assert_eq!(counts.high.preexisting, 0);
    }

    #[test]
    fn test_review_has_new_issues_scenarios() {
        // 1. Empty review
        let r_empty = json!({});
        assert!(!review_has_new_issues(&r_empty));

        // 2. No issues found inline
        let r_clean_inline = json!({ "inline_review": "No issues found." });
        assert!(!review_has_new_issues(&r_clean_inline));

        // 3. Issues found inline, no output findings (fallback)
        let r_issues_inline = json!({ "inline_review": "Some issues." });
        assert!(review_has_new_issues(&r_issues_inline));

        // 4. Only preexisting findings in output
        let r_preexisting = json!({
            "inline_review": "Some issues.",
            "output": "{\"findings\": [{\"severity\": \"High\", \"preexisting\": true}]}"
        });
        assert!(!review_has_new_issues(&r_preexisting));

        // 5. Only new findings in output
        let r_new = json!({
            "inline_review": "Some issues.",
            "output": "{\"findings\": [{\"severity\": \"High\", \"preexisting\": false}]}"
        });
        assert!(review_has_new_issues(&r_new));

        // 6. Mixed findings in output
        let r_mixed = json!({
            "inline_review": "Some issues.",
            "output": "{\"findings\": [{\"severity\": \"High\", \"preexisting\": true}, {\"severity\": \"Low\", \"preexisting\": false}]}"
        });
        assert!(review_has_new_issues(&r_mixed));
    }
}
