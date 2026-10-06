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
use sashiko::activity::{Phase, StageTiming, format_duration};
use sashiko::api::{
    ActivityEntry, ActivityResponse, PatchsetActivityResponse, PatchsetsResponse, SubmitRequest,
    SubmitResponse,
};
use sashiko::auth::LocalToken;
use sashiko::finding_view::{self, FindingLine};
use sashiko::project::ProjectId;
use sashiko::settings::Settings;
use sashiko::utils::utf8_prefix;
use sashiko::workflow::StageStopCause;
use sashiko::{PatchOutcome, ReviewStatus};
use serde_json::{Value, from_str};
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::OnceLock;
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};

static COLOR_CHOICE: OnceLock<ColorChoice> = OnceLock::new();

/// The project this invocation is for, resolved once in `main`.
///
/// Held the way `COLOR_CHOICE` is, for the same reason: it comes from a global
/// flag and is read far from where it is parsed, and threading it through every
/// intervening signature would say nothing the flag does not already say.
static PROJECT: OnceLock<ProjectId> = OnceLock::new();

/// The resolved project. Defaults rather than panicking, because a command that
/// never touches a project should not depend on `main` having set it.
fn project() -> ProjectId {
    PROJECT.get().copied().unwrap_or_default()
}

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

    /// The codebase to review (default: the configured project, else linux)
    #[arg(long, global = true, env = "SASHIKO_PROJECT")]
    project: Option<ProjectId>,

    /// Bearer token for authenticating API requests
    #[arg(long, global = true, env = "SASHIKO_API_TOKEN")]
    token: Option<String>,
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
    timings: bool,
    diff: Option<String>,
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

        /// Show per-stage timings and turn counts for each patch, live while a
        /// review runs and as recorded once it has finished
        #[arg(long)]
        timings: bool,

        /// Compare with another patchset ID
        #[arg(long, short = 'd')]
        diff: Option<String>,
    },
    /// Request a re-review of a completed patchset
    Rerun {
        /// ID of the patchset to re-review
        id: i64,
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

        /// Output concise machine-friendly JSON for AI agents and skip plain-text report generation
        #[arg(long)]
        agent: bool,
    },
    /// Query and triage bugs in the Sashiko bug repository
    Bugs {
        #[command(subcommand)]
        action: BugCommands,
    },
    /// Mint stateless API tokens for AI agents and automation
    Token {
        #[command(subcommand)]
        action: TokenCommands,
    },
}

#[derive(Subcommand)]
enum BugCommands {
    /// List bugs matching filters
    List {
        /// Filter by subsystem title
        #[arg(long)]
        subsystem: Option<String>,

        /// Filter by minimum severity (low, medium, high, critical)
        #[arg(long)]
        severity: Option<String>,

        /// Filter by lifecycle status (new, confirmed, fixed, dismissed, duplicate)
        #[arg(long)]
        status: Option<String>,

        /// Full-text search query
        #[arg(long, short = 'q')]
        query: Option<String>,

        /// Page number
        #[arg(long, default_value_t = 1)]
        page: usize,

        /// Items per page
        #[arg(long, default_value_t = 20)]
        per_page: usize,
    },
    /// Show full details of a bug by ID
    Show {
        /// Bug ID
        id: i64,
    },
    /// Perform a triage action or add a comment on a bug
    Action {
        /// Bug ID
        id: i64,

        /// Action to perform (comment, close, dismiss, assign, mark_duplicate)
        #[arg(long)]
        action: String,

        /// Comment text to attach
        #[arg(long)]
        comment: Option<String>,

        /// Assignee email (for assign action)
        #[arg(long)]
        assignee: Option<String>,

        /// Canonical bug ID (for mark_duplicate action)
        #[arg(long)]
        duplicate_of: Option<i64>,
    },
}

#[derive(Subcommand)]
enum TokenCommands {
    /// Mint a stateless API token
    Create {
        /// Target email address (operators only; defaults to caller's email)
        #[arg(long)]
        email: Option<String>,

        /// Maximum bug access level: read (default), comment, or manage
        #[arg(long, default_value = "read")]
        access: String,

        /// Token validity in days (1..=90)
        #[arg(long, default_value_t = 30)]
        days: u64,
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

    // Same precedence as the server: the flag, then what the settings file
    // says it is for, then the default. A missing or unreadable settings file
    // is not fatal here, since most commands only talk to a running server.
    let configured_project = Settings::new().ok().and_then(|s| s.project.kind);
    PROJECT
        .set(cli.project.or(configured_project).unwrap_or_default())
        .expect("project is set exactly once, here");

    let client = build_client(cli.token.as_deref())?;

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

/// Builds the HTTP client, presenting an explicit bearer token if supplied or
/// the local server's token when one is readable.
///
/// When `explicit_token` is `Some(...)`, the local operator token file is
/// never consulted: passing `--token ""` intentionally sheds privileges and
/// builds an unauthenticated client, and invalid header characters fail fast.
fn build_client(explicit_token: Option<&str>) -> Result<Client> {
    let bearer = match explicit_token {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Ok(Client::new());
            }
            Some(trimmed.to_owned())
        }
        None => Settings::new()
            .ok()
            .and_then(|settings| LocalToken::read_from(&settings.local_token_path()).ok())
            .map(|t| t.secret().to_owned()),
    };

    let Some(secret) = bearer else {
        return Ok(Client::new());
    };

    let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", secret))
        .context("Invalid bearer token: contains non-ASCII or control characters")?;
    value.set_sensitive(true);

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::AUTHORIZATION, value);

    Client::builder()
        .default_headers(headers)
        .build()
        .context("Failed to construct HTTP client")
}

async fn check_response(resp: reqwest::Response) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let detail = body.trim();
    if detail.is_empty() {
        anyhow::bail!("HTTP {}", status);
    } else {
        anyhow::bail!("HTTP {}: {}", status, detail);
    }
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
        Commands::Rerun { id } => handle_rerun(client, base_url, id, format).await,
        Commands::Cancel { id, force } => handle_cancel(client, base_url, id, force, format).await,
        Commands::Local {
            input,
            baseline,
            repo,
            no_ai,
            custom_prompt,
            force_local,
            interactive,
            agent,
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
                agent,
                format,
            )
            .await
        }
        Commands::Bugs { action } => handle_bugs(client, base_url, action, format).await,
        Commands::Token { action } => handle_token(client, base_url, action, format).await,
    }
}

async fn handle_bugs(
    client: &Client,
    base_url: &str,
    action: BugCommands,
    format: OutputFormat,
) -> Result<()> {
    match action {
        BugCommands::List {
            subsystem,
            severity,
            status,
            query,
            page,
            per_page,
        } => {
            let mut req = client.get(format!("{}/api/bugs", base_url)).query(&[
                ("page", page.to_string()),
                ("per_page", per_page.to_string()),
            ]);
            if let Some(sub) = subsystem {
                req = req.query(&[("subsystem", sub)]);
            }
            if let Some(sev) = severity {
                req = req.query(&[("min_severity", sev)]);
            }
            if let Some(st) = status {
                req = req.query(&[("lifecycle_status", st)]);
            }
            if let Some(q) = query {
                req = req.query(&[("q", q)]);
            }
            let resp = check_response(req.send().await?).await?;
            let body: Value = resp.json().await?;
            match format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&body)?);
                }
                OutputFormat::Text => {
                    let bugs = body
                        .get("items")
                        .or_else(|| body.get("bugs"))
                        .and_then(|v| v.as_array())
                        .ok_or_else(|| anyhow::anyhow!("API response missing 'items' array"))?;
                    let total = body["total"].as_u64().unwrap_or(bugs.len() as u64);
                    println!("Bugs ({} total):", total);
                    for bug in bugs {
                        let id = bug["internal_id"]
                            .as_i64()
                            .or_else(|| bug["id"].as_i64())
                            .unwrap_or_default();
                        let sev = bug["severity"].as_str().unwrap_or("unknown");
                        let st = bug["lifecycle_status"].as_str().unwrap_or("unknown");
                        let title = bug["title"].as_str().unwrap_or("(untitled)");
                        println!("  #{:<6} [{:<8}] ({:<9}) {}", id, sev, st, title);
                    }
                }
            }
            Ok(())
        }
        BugCommands::Show { id } => {
            let resp = check_response(
                client
                    .get(format!("{}/api/bug", base_url))
                    .query(&[("id", id.to_string())])
                    .send()
                    .await?,
            )
            .await?;
            let body: Value = resp.json().await?;
            match format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&body)?);
                }
                OutputFormat::Text => {
                    let bug_id = body["internal_id"]
                        .as_i64()
                        .or_else(|| body["id"].as_i64())
                        .unwrap_or(id);
                    let title = body["title"].as_str().unwrap_or("(untitled)");
                    let sev = body["severity"].as_str().unwrap_or("unknown");
                    let st = body["lifecycle_status"].as_str().unwrap_or("unknown");
                    println!("Bug #{}: {}", bug_id, title);
                    println!("Severity: {} | Status: {}", sev, st);
                    if let Some(desc) = body["description"]
                        .as_str()
                        .or_else(|| body["report"].as_str())
                    {
                        println!("\n{}", desc);
                    }
                }
            }
            Ok(())
        }
        BugCommands::Action {
            id,
            action,
            comment,
            assignee,
            duplicate_of,
        } => {
            let mut map = serde_json::Map::new();
            map.insert("action".to_string(), Value::String(action.clone()));
            match action.as_str() {
                "comment" => {
                    if assignee.is_some() || duplicate_of.is_some() {
                        anyhow::bail!(
                            "Action 'comment' does not accept --assignee or --duplicate-of"
                        );
                    }
                    let text = comment.filter(|s| !s.trim().is_empty()).ok_or_else(|| {
                        anyhow::anyhow!("Action 'comment' requires non-empty --comment")
                    })?;
                    map.insert("content".to_string(), Value::String(text));
                }
                "close" | "dismiss" => {
                    if assignee.is_some() || duplicate_of.is_some() {
                        anyhow::bail!(
                            "Action '{}' does not accept --assignee or --duplicate-of",
                            action
                        );
                    }
                    if let Some(c) = comment {
                        map.insert("reason".to_string(), Value::String(c));
                    }
                }
                "mark_duplicate" => {
                    if assignee.is_some() {
                        anyhow::bail!("Action 'mark_duplicate' does not accept --assignee");
                    }
                    let dup = duplicate_of.ok_or_else(|| {
                        anyhow::anyhow!("Action 'mark_duplicate' requires --duplicate-of <ID>")
                    })?;
                    map.insert("duplicate_of_id".to_string(), Value::from(dup));
                    if let Some(c) = comment {
                        map.insert("reasoning".to_string(), Value::String(c));
                    }
                }
                "assign" => {
                    if duplicate_of.is_some() {
                        anyhow::bail!("Action 'assign' does not accept --duplicate-of");
                    }
                    let raw = assignee.ok_or_else(|| {
                        anyhow::anyhow!(
                            "Action 'assign' requires --assignee <EMAIL> (or --assignee \"\" to unassign)"
                        )
                    })?;
                    let assignee_val = if raw.trim().is_empty() {
                        Value::Null
                    } else {
                        Value::String(raw)
                    };
                    map.insert("assignee".to_string(), assignee_val);
                    if let Some(c) = comment {
                        map.insert("reason".to_string(), Value::String(c));
                    }
                }
                _ => {
                    anyhow::bail!(
                        "Unsupported bug action '{}'. Valid actions: comment, close, dismiss, mark_duplicate, assign",
                        action
                    );
                }
            }
            let resp = check_response(
                client
                    .post(format!("{}/api/bug/action", base_url))
                    .query(&[("id", id.to_string())])
                    .json(&Value::Object(map))
                    .send()
                    .await?,
            )
            .await?;
            let body: Value = resp.json().await?;
            match format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&body)?);
                }
                OutputFormat::Text => {
                    println!("Bug #{} updated ({}).", id, action);
                }
            }
            Ok(())
        }
    }
}

async fn handle_token(
    client: &Client,
    base_url: &str,
    action: TokenCommands,
    format: OutputFormat,
) -> Result<()> {
    match action {
        TokenCommands::Create {
            email,
            access,
            days,
        } => {
            let mut map = serde_json::Map::new();
            map.insert("max_bug_access".to_string(), Value::String(access));
            map.insert("expires_in_days".to_string(), Value::from(days));
            if let Some(em) = email {
                map.insert("email".to_string(), Value::String(em));
            }
            let resp = check_response(
                client
                    .post(format!("{}/api/auth/token", base_url))
                    .json(&Value::Object(map))
                    .send()
                    .await?,
            )
            .await?;
            let body: Value = resp.json().await?;
            match format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&body)?);
                }
                OutputFormat::Text => {
                    let token = body
                        .get("token")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| anyhow::anyhow!("API response missing 'token' field"))?;
                    println!("{}", token);
                }
            }
            Ok(())
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
    format: OutputFormat,
) -> Result<()> {
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
            }
        }
        SubmitType::Range => {
            let repo_path = repo.map(|p| p.to_string_lossy().to_string());

            SubmitRequest::RemoteRange {
                sha: target,
                repo: repo_path,
                skip_subjects: skip_subjects.clone(),
                only_subjects: only_subjects.clone(),
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
/// reported progress recently, which is the signature of a wedged fetch or
/// review. Best-effort: a daemon without the endpoint prints nothing extra.
async fn print_live_activity(client: &Client, base_url: &str) {
    let url = format!("{}/api/activity", base_url);
    let Ok(resp) = client.get(&url).send().await else {
        return;
    };
    if !resp.status().is_success() {
        return;
    }
    let Ok(data) = resp.json::<ActivityResponse>().await else {
        return;
    };

    println!();
    if data.entries.is_empty() {
        print_colored(Color::Cyan, "Live Activity:");
        println!(" (idle)");
        return;
    }

    print_colored(Color::Cyan, "Live Activity:\n");
    println!("  {:<36} {:<48} {:>10}", "WORK", "PHASE", "IDLE");
    for entry in &data.entries {
        println!(
            "  {:<36} {:<48} {:>10}",
            entry.key,
            entry.description,
            format_duration(entry.idle_seconds.unwrap_or(0))
        );
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

                    print!("{:<10} ", item.id);
                    print_colored(
                        patchset_status_color(status_str),
                        &format!("{:<18}", status_str),
                    );

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

/// Whether a status says its review ran out of time: `Partial` kept what the
/// stages that finished found, and `TimedOut` kept nothing. The spellings are
/// taken from `PatchOutcome`, the type the daemon writes them from, so the two
/// cannot drift apart.
fn ran_out_of_time(status: &str) -> bool {
    [PatchOutcome::Partial, PatchOutcome::TimedOut]
        .iter()
        .any(|outcome| outcome.as_str() == status)
}

/// The colour a patchset status is listed in. A status that ran out of time
/// is yellow, between a full review's green and a failure's red, because its
/// review is neither.
fn patchset_status_color(status: &str) -> Color {
    match status {
        "Reviewed" => Color::Green,
        "Embargoed" => Color::Magenta,
        "Failed" | "Error" | "Failed To Apply" | "Cancelled" => Color::Red,
        "Pending" | "In Review" => Color::Yellow,
        s if ran_out_of_time(s) => Color::Yellow,
        _ => Color::White,
    }
}

/// The colour a patch status is printed in. Any status not listed prints
/// green, so a status that ran out of time has its own arm: green would make
/// a patch whose review stopped early read as fully reviewed.
fn patch_status_color(status: &str) -> Color {
    match status {
        "Failed" | "Failed To Apply" | "Error" => Color::Red,
        "Embargoed" => Color::Magenta,
        s if ran_out_of_time(s) => Color::Yellow,
        _ => Color::Green,
    }
}

/// The colour of a review's badge. A Partial review is yellow whether or not
/// it found issues: its coverage is incomplete, so not finding any does not
/// make it clean.
fn review_badge_color(review: &Value) -> Color {
    let status = review_status(review);
    if status == Some(ReviewStatus::Partial) || review_has_new_issues(review) {
        Color::Yellow
    } else if status == Some(ReviewStatus::Failed) {
        Color::Red
    } else {
        Color::Green
    }
}

/// A review's status, or `None` when it is missing or one this build does
/// not know.
fn review_status(review: &Value) -> Option<ReviewStatus> {
    review.get("status")?.as_str()?.parse().ok()
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

/// How well a review stands for its patch, weakest first, so that the
/// derived order ranks them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ReviewStanding {
    /// Any other status: failed, cancelled, or not finished yet.
    Other,
    /// The review ran out of time and kept what the stages that finished
    /// found. It says more about the patch than a failure does, and less
    /// than a full review.
    Partial,
    /// A full review.
    Reviewed,
}

impl ReviewStanding {
    fn of(review: &Value) -> Self {
        match review_status(review) {
            Some(ReviewStatus::Reviewed) => Self::Reviewed,
            Some(ReviewStatus::Partial) => Self::Partial,
            _ => Self::Other,
        }
    }
}

/// The review that stands for a patch: the latest Reviewed one, else the
/// latest Partial one, else the latest of any status. Every view that shows
/// one review per patch picks it here, so they all agree.
///
/// The server lists reviews oldest first, and `max_by_key` returns the last
/// of several equal keys, so the latest review of the best standing wins.
fn find_best_review_for_patch_refs<'a>(patch_id: i64, reviews: &[&'a Value]) -> Option<&'a Value> {
    reviews
        .iter()
        .copied()
        .filter(|r| r.get("patch_id").and_then(|id| id.as_i64()) == Some(patch_id))
        .max_by_key(|r| ReviewStanding::of(r))
}

/// The one-line result of a review. The status is read before the findings,
/// so that a Partial review with findings never reads as a complete review
/// that found issues.
fn review_result_label(review: &Value) -> &str {
    let has_new_issues = review_has_new_issues(review);
    match review_status(review) {
        Some(ReviewStatus::Partial) if has_new_issues => "Partial, Issues Found",
        Some(ReviewStatus::Partial) => "Partial",
        _ if has_new_issues => "Issues Found",
        _ => review.get("status").and_then(|s| s.as_str()).unwrap_or(""),
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
        print_colored(patch_status_color(status), status);
        print!(")");
    }

    if let Some(rev) = review {
        print!(" [");
        print_colored(review_badge_color(rev), review_result_label(rev));
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

/// Notes how long a review took, and how many attempts it needed when that
/// was more than one.
///
/// The live attempt counter in the activity view vanishes the moment the review
/// ends, so without this there is no way to see after the fact that a result
/// took three goes to produce.
fn print_attempt_note(review: &Value) {
    if let Some(note) = attempt_note(review) {
        println!("  ({})", note);
    }
}

/// The text of `print_attempt_note`, or `None` when the review carries
/// neither a duration nor a retry worth noting.
///
/// Duration and attempt are stamped on failed and Partial attempts too, so
/// the wording follows the status, as the web page's does: neither of them
/// "succeeded", and a Partial review says it stopped because time ran out.
fn attempt_note(review: &Value) -> Option<String> {
    let status = review_status(review);
    let attempt = review.get("attempt").and_then(|a| a.as_u64());
    let duration = review.get("duration_seconds").and_then(|d| d.as_u64());

    let mut parts = Vec::new();
    if let Some(d) = duration {
        let d = format_duration(d);
        parts.push(match status {
            Some(ReviewStatus::Partial) => format!("stopped after {} (out of time)", d),
            Some(ReviewStatus::Failed) => format!("failed after {}", d),
            _ => format!("reviewed in {}", d),
        });
    }
    // Only worth saying when it took more than one go; annotating every review
    // with "attempt 1" would bury the case worth noticing.
    if let Some(a) = attempt.filter(|a| *a > 1) {
        parts.push(match status {
            Some(ReviewStatus::Partial | ReviewStatus::Failed) => format!("on attempt {}", a),
            _ => format!("succeeded on attempt {}", a),
        });
    }

    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Warns when a review did not run every stage it was supposed to.
///
/// The concurrent review stages run best-effort, so one failure does not abort
/// the review; that is only acceptable if the missing coverage is stated. A
/// review that skipped the security stage but reads as clean is worse than one
/// that failed outright.
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
            "Incomplete coverage: {} stage(s) did not complete\n",
            failures.len()
        ),
    );
    for f in failures {
        let stage = f["stage"].as_str().unwrap_or("?");
        let reason = f["reason"].as_str().unwrap_or("unknown");
        println!("  Stage {} {}: {}", stage, stage_failure_label(f), reason);
    }
}

/// What one stored stage failure says about why the stage stopped, in the
/// label the web page and the activity descriptions use for the same cause.
///
/// Rows written before the cause existed carry only `cancelled`. A legacy
/// `cancelled: true` reads "stopped" rather than "cancelled", because the flag
/// was set mostly for stages that ran out of time or were still running when
/// the worker died, and "stopped" is what those rows have always read. A cause
/// this build does not know also reads "stopped", which claims no fault.
fn stage_failure_label(failure: &Value) -> &'static str {
    match failure.get("cause").filter(|c| !c.is_null()) {
        Some(cause) => serde_json::from_value::<StageStopCause>(cause.clone())
            .map_or("stopped", StageStopCause::label),
        None if failure["cancelled"].as_bool().unwrap_or(false) => "stopped",
        None => "failed",
    }
}

/// Live stage activity for one patchset, grouped by the patch it describes.
struct PatchActivity {
    /// False once the daemon has stopped: the entries then say where the work
    /// got to, not what it is doing.
    live: bool,
    /// In the order the daemon sent them, which is the order its pipeline runs
    /// them. This side does not know the daemon's project, so it does not
    /// re-sort.
    by_patch: std::collections::HashMap<i64, Vec<ActivityEntry>>,
}

async fn fetch_patchset_activity(
    client: &Client,
    base_url: &str,
    patchset_id: &str,
) -> Option<PatchsetActivityResponse> {
    let url = format!("{}/api/patchset/activity?id={}", base_url, patchset_id);
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

async fn fetch_patch_activity(
    client: &Client,
    base_url: &str,
    patchset_id: &str,
) -> Option<PatchActivity> {
    let data = fetch_patchset_activity(client, base_url, patchset_id).await?;
    let mut by_patch: std::collections::HashMap<i64, Vec<ActivityEntry>> =
        std::collections::HashMap::new();
    for entry in data.entries {
        // Entries with no patch describe the patchset as a whole and belong in
        // the activity summary, not under any one patch.
        if let Some(patch_id) = entry.patch_id {
            by_patch.entry(patch_id).or_default().push(entry);
        }
    }
    Some(PatchActivity {
        live: data.live,
        by_patch,
    })
}

/// How many turns, for a table column.
fn turns_cell(turns: u64) -> String {
    match turns {
        0 => "—".to_string(),
        1 => "1 turn".to_string(),
        n => format!("{} turns", n),
    }
}

/// One row of a patch's stage table.
///
/// Built from the deserialised `Phase` rather than the server's rendered
/// description, so the columns line up and the CLI does not carry a second
/// copy of the wording that can drift from the daemon's.
fn stage_row(entry: &ActivityEntry, live: bool) -> String {
    let stage = entry.stage.as_deref().unwrap_or("this patch");
    let dash = "—".to_string();
    let describe_or_stored = |p: Option<&Phase>| {
        p.map(Phase::describe)
            .unwrap_or_else(|| entry.description.clone())
    };

    // A persisted entry has no clocks: it records where the work got to, not
    // how long it has been there.
    if !live {
        return format!(
            "    {:<20} {:>10}  {:>12}  {}",
            stage,
            dash,
            dash,
            describe_or_stored(entry.phase.as_ref())
        );
    }

    let age = entry.age_seconds.unwrap_or(0);
    let (elapsed, turns, state) = match &entry.phase {
        Some(Phase::StageDone { seconds, turns, .. }) => (
            format_duration(*seconds),
            turns_cell(*turns),
            "done".to_string(),
        ),
        Some(phase @ Phase::StageFailed { .. }) => (
            format!("{} ago", format_duration(age)),
            dash.clone(),
            phase.describe(),
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
                    stage: String::new(),
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
        // The patch's own entry: screening, planning, or running its stages.
        other => (
            format_duration(age),
            dash.clone(),
            describe_or_stored(other.as_ref()),
        ),
    };

    let idle = entry.idle_seconds.unwrap_or(0);
    let stalled = if idle > 300 && !matches!(entry.phase, Some(Phase::StageDone { .. })) {
        format!("  — no progress for {}", format_duration(idle))
    } else {
        String::new()
    };

    format!(
        "    {:<20} {:>10}  {:>12}  {}{}",
        stage, elapsed, turns, state, stalled
    )
}

/// One row of a finished review's stage table.
///
/// A stage that did not run to completion says what stopped it, in the label
/// the coverage warning uses, so its time is not read as a finished stage's.
fn recorded_stage_row(timing: &StageTiming) -> String {
    let row = format!(
        "    {:<20} {:>10}  {:>12}",
        timing.stage,
        format_duration(timing.seconds),
        turns_cell(timing.turns)
    );
    match timing.stopped_by {
        Some(cause) => format!("{}  {}", row, cause.label()),
        None => row,
    }
}

/// Footnote for a finished review's stage table.
///
/// The rows will not add up to the patch's review time, because the stages
/// overlap. The longest stage beside the sum makes the gap visible, and both
/// numbers are about this review. With one stage there is nothing to overlap
/// and nothing to sum.
fn stage_breakdown_note(stages: &[StageTiming]) -> String {
    if let [only] = stages {
        return format!("1 stage, {}", format_duration(only.seconds));
    }

    format!(
        "{} stages overlapping: longest {}, {} summed",
        stages.len(),
        format_duration(stages.iter().map(|s| s.seconds).max().unwrap_or(0)),
        format_duration(stages.iter().map(|s| s.seconds).sum())
    )
}

/// Per-patch stage tables.
///
/// Printed whatever the patchset's status. The recorded breakdown only exists
/// once a review has finished, and the most interesting moment to ask what the
/// stages are doing is while they are still doing it.
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
        // Stored by the daemon in pipeline order, which this side cannot
        // reconstruct from the names.
        let recorded: Option<Vec<StageTiming>> = find_best_review_for_patch_refs(p_id, reviews)
            .and_then(|r| r.get("stage_durations"))
            .and_then(|d| serde_json::from_value(d.clone()).ok())
            .filter(|d: &Vec<StageTiming>| !d.is_empty());

        match (live_entries, recorded) {
            // Live wins: it is the current truth, and the recorded breakdown for
            // an earlier attempt would contradict it.
            (Some(entries), _) => {
                let live = activity.map(|a| a.live).unwrap_or(true);
                println!(
                    "    {:<20} {:>10}  {:>12}  STATE",
                    "STAGE", "ELAPSED", "TURNS"
                );
                for entry in entries {
                    println!("{}", stage_row(entry, live));
                }
            }
            (None, Some(stages)) => {
                println!("    {:<20} {:>10}  {:>12}", "STAGE", "ELAPSED", "TURNS");
                for st in &stages {
                    println!("{}", recorded_stage_row(st));
                }
                println!("    ({})", stage_breakdown_note(&stages));
            }
            // Said plainly rather than left blank, which is indistinguishable
            // from the flag not working.
            (None, None) => println!("    no stage timings recorded"),
        }
    }
}

/// One-line summary of everything a patchset is currently doing.
///
/// Live entries report both clocks: `age` is how long that activity has been
/// running, `idle` is how long since it last moved. A large idle beside a
/// growing age is the signature of a wedged stage.
///
/// When nothing is live the daemon returns the last state recorded before it
/// stopped, prefixed "stopped while" -- so it is neither mistaken for something
/// still running, nor read as the cause of a failure.
///
/// Best-effort: a daemon without the endpoint yields `None` rather than
/// breaking `show --watch`.
async fn fetch_activity_summary(
    client: &Client,
    base_url: &str,
    patchset_id: &str,
) -> Option<String> {
    let data = fetch_patchset_activity(client, base_url, patchset_id).await?;
    if data.entries.is_empty() {
        return None;
    }

    let parts: Vec<String> = data
        .entries
        .iter()
        .map(|e| match (e.age_seconds, e.idle_seconds) {
            (Some(age), Some(idle)) if data.live => format!(
                "{} [{}, idle {}]",
                e.description,
                format_duration(age),
                format_duration(idle)
            ),
            _ => e.description.clone(),
        })
        .collect();

    let joined = parts.join(" | ");
    if data.live {
        Some(joined)
    } else {
        // An observation, not a diagnosis: the work stopped during this
        // activity, which is not the same as failing because of it.
        Some(format!("stopped while: {}", joined))
    }
}

/// Whether `show --watch` stops polling a patchset in this status. Partial is
/// finished: a review that ran out of time is not resumed, so watching it any
/// longer would poll for ever.
fn is_watch_terminal(status: &str) -> bool {
    matches!(
        status,
        "Reviewed" | "Partial" | "Failed" | "Error" | "Cancelled" | "Embargoed" | "Failed To Apply"
    )
}

/// Whether a finished patchset in this status has a review log to show. A
/// Partial patchset has one: its salvaged review kept its conversation.
fn has_review_log(status: &str) -> bool {
    matches!(
        status,
        "Reviewed" | "Partial" | "Failed" | "Failed To Apply"
    )
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

            let is_terminal = is_watch_terminal(&status);

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
                // run. The activity feed is what shows it is actually moving --
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
            if has_review_log(&status) {
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

/// How the patches of a patchset stand, each counted by the review that
/// stands for it (`find_best_review_for_patch_refs`).
#[derive(Debug, Default, PartialEq, Eq)]
struct PatchsetSummary {
    reviewed_clean: usize,
    reviewed_issues: usize,
    /// Patches whose review ran out of time and kept what the stages that
    /// finished found. They are neither reviewed nor still in review.
    partial: usize,
    in_review: usize,
    not_started: usize,
    failed: usize,
    /// The newest review of any patch, whichever review stands for it. A
    /// caller passes it back as `--since`, so a retry newer than the review
    /// that stands for its patch must not be reported again on every poll.
    latest_review_id: i64,
    /// The `(part_index, subject)` of every patch whose Reviewed review found
    /// new issues, one per `reviewed_issues`. A Partial patch is counted in
    /// `partial` alone, so it is not listed here; `--issues` lists it with its
    /// "Partial, Issues Found" label.
    issues: Vec<(i64, String)>,
}

impl PatchsetSummary {
    fn tally(patches: &[Value], reviews: &[&Value]) -> Self {
        let mut summary = Self::default();
        for patch in patches {
            let p_id = patch["id"].as_i64().unwrap_or(0);
            summary.latest_review_id = summary
                .latest_review_id
                .max(latest_review_id_for_patch(p_id, reviews));
            match find_best_review_for_patch_refs(p_id, reviews) {
                Some(review) => summary.count(patch, review),
                None => summary.not_started += 1,
            }
        }
        summary
    }

    fn count(&mut self, patch: &Value, review: &Value) {
        let status = review_status(review);
        let has_new_issues = review_has_new_issues(review);
        if status == Some(ReviewStatus::Reviewed) && has_new_issues {
            let idx = patch["part_index"].as_i64().unwrap_or(0);
            let subject = patch["subject"].as_str().unwrap_or("").to_string();
            self.issues.push((idx, subject));
        }

        match status {
            Some(ReviewStatus::Reviewed) if has_new_issues => self.reviewed_issues += 1,
            Some(ReviewStatus::Reviewed) => self.reviewed_clean += 1,
            Some(ReviewStatus::Partial) => self.partial += 1,
            Some(ReviewStatus::Failed) => self.failed += 1,
            // "Error" is not a `ReviewStatus` and nothing in this tree writes
            // it, but the summary has always counted it as a failure.
            _ if review.get("status").and_then(|s| s.as_str()) == Some("Error") => self.failed += 1,
            _ => self.in_review += 1,
        }
    }
}

/// The largest review id among a patch's reviews, or 0 when it has none.
fn latest_review_id_for_patch(patch_id: i64, reviews: &[&Value]) -> i64 {
    reviews
        .iter()
        .filter(|r| r.get("patch_id").and_then(|id| id.as_i64()) == Some(patch_id))
        .filter_map(|r| r.get("id").and_then(|i| i.as_i64()))
        .max()
        .unwrap_or(0)
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
    let summary = PatchsetSummary::tally(patches, reviews);

    match format {
        OutputFormat::Json => {
            let issues_json: Vec<Value> = summary
                .issues
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
                "reviewed_clean": summary.reviewed_clean,
                "reviewed_issues": summary.reviewed_issues,
                "partial": summary.partial,
                "in_review": summary.in_review,
                "not_started": summary.not_started,
                "failed": summary.failed,
                "latest_review_id": summary.latest_review_id,
                "issues": issues_json,
            });
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        OutputFormat::Text => {
            println!("PS {} ({} patches) — Status: {}", ps_id, total, ps_status);
            println!(
                "  Reviewed:    {:>3}  (clean: {}, issues: {})",
                summary.reviewed_clean + summary.reviewed_issues,
                summary.reviewed_clean,
                summary.reviewed_issues
            );
            println!("  Partial:     {:>3}", summary.partial);
            println!("  In Review:   {:>3}", summary.in_review);
            println!("  Not Started: {:>3}", summary.not_started);
            println!("  Failed:      {:>3}", summary.failed);

            // Time spent reviewing, not time since submission: a patchset can
            // sit in the queue for a while before a worker picks it up.
            if let Some(secs) = details
                .get("review_duration_seconds")
                .and_then(|d| d.as_u64())
            {
                println!("  Review Time: {:>3}", format_duration(secs));
            }

            if !summary.issues.is_empty() && !opts.issues {
                println!("\nIssues Found:");
                for (idx, subject) in &summary.issues {
                    println!("  [{}] {}", idx, subject);
                }
            }

            if opts.issues {
                println!();
                for patch in patches {
                    let p_id = patch["id"].as_i64().unwrap_or(0);
                    if let Some(rev) = find_best_review_for_patch_refs(p_id, reviews)
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
                print_colored(Color::Green, "Rerun queued: ");
                println!("Patchset {} has been re-added to the review queue.", id);
            }
        }
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Rerun failed ({}): {}", status, text));
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
    agent: bool,
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

    // Check if server is running (unless --force-local or --agent)
    if !force_local
        && !agent
        && let Ok(settings) = Settings::new()
    {
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
                SubmitType::Range => SubmitRequest::RemoteRange {
                    sha: input.clone(),
                    repo: repo_str,
                    skip_subjects: None,
                    only_subjects: None,
                },
                _ => SubmitRequest::Remote {
                    sha: input.clone(),
                    repo: repo_str,
                    skip_subjects: None,
                    only_subjects: None,
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
            // The worker resolves its own prompts, and without being told the
            // project it resolves the default one's.
            "--project".to_string(),
            project().as_str().to_string(),
        ];

        if no_ai {
            args.push("--no-ai".to_string());
        }
        if agent {
            args.push("--agent".to_string());
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

        // Stream stderr for progress, while keeping a bounded tail of the
        // subprocess's log lines. The progress UI consumes these lines, so
        // without the tail a failed review would collapse to a bare "no output"
        // message with its actual cause (a stage error, an auth/quota failure)
        // discarded. On failure we replay the tail so the run explains itself.
        // The worker's own progress lines (stage started, turn n/m, finished)
        // are shown as they arrive: they are the only sign of life during the
        // AI phase. Turn ticks overwrite one line on a terminal and are kept
        // out of the tail, which they would otherwise fill.
        let stderr = child.stderr.take();
        let stderr_is_terminal = std::io::stderr().is_terminal();
        let stderr_handle = tokio::spawn(async move {
            use sashiko::local_review::PROGRESS_LINE_PREFIX;
            use std::collections::VecDeque;
            const STDERR_TAIL_LINES: usize = 100;
            let mut tail: VecDeque<String> = VecDeque::with_capacity(STDERR_TAIL_LINES);
            if let Some(stderr) = stderr {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                let mut saw_applying = false;
                let mut saw_ai_review = false;
                let mut turn_line_open = false;
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
                    if let Some(progress) = line.strip_prefix(PROGRESS_LINE_PREFIX) {
                        if progress.contains(" turn ") {
                            if stderr_is_terminal {
                                eprint!("\r\x1b[2K      {progress}");
                                turn_line_open = true;
                            }
                            continue;
                        }
                        if turn_line_open {
                            eprint!("\r\x1b[2K");
                            turn_line_open = false;
                        }
                        eprintln!("      {progress}");
                    }
                    if line.contains("AI review completed") {
                        eprint_phase(4, 4, "Review complete.");
                        eprintln!();
                    }
                    if tail.len() == STDERR_TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
                if turn_line_open {
                    eprintln!();
                }
            }
            tail
        });

        // Capture stdout
        let output = child
            .wait_with_output()
            .await
            .context("Failed to wait for review subprocess")?;

        let stderr_tail = stderr_handle.await.unwrap_or_default();

        let exit_code = output.status.code().unwrap_or(1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();

        // A subprocess that failed before emitting a result, or emitted
        // something that is not a result, has its cause in the log tail;
        // replay it rather than reporting a bare exit code or parse error.
        let replay_tail = || {
            for line in &stderr_tail {
                eprintln!("{line}");
            }
        };
        if stdout.trim().is_empty() {
            replay_tail();
            return Err(anyhow::anyhow!(
                "Review subprocess produced no output (exit code: {})",
                exit_code
            ));
        }

        // Parse the review output
        let result: Value = match serde_json::from_str(stdout.trim()) {
            Ok(value) => value,
            Err(err) => {
                replay_tail();
                return Err(anyhow::anyhow!(
                    "Failed to parse review output JSON: {} (exit code: {})",
                    err,
                    exit_code
                ));
            }
        };

        if agent {
            println!(
                "{}",
                serde_json::to_string_pretty(&sashiko::local_review::format_agent_review_output(
                    &result
                ))?
            );
        } else {
            match format {
                OutputFormat::Json => {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                }
                OutputFormat::Text => {
                    print_local_review_results(&result, &input);
                }
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

        if interactive && !agent && (has_error || has_issues) {
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

    print_finding_lines(&findings_to_process);
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

/// Prints one line per finding. When no finding says whether it is
/// speculative, a note after the list says so once. Write errors are ignored,
/// as in `print_colored`, because a closed stdout leaves nothing to report to.
fn print_finding_lines(findings: &[Value]) {
    let mut stdout = colored_stdout();
    for finding in findings {
        let _ = finding_view::write_finding_line(&mut stdout, &FindingLine::from_json(finding));
    }
    let _ = finding_view::write_not_marked_note(&mut stdout, findings);
    let _ = stdout.flush();
}

/// Stdout with the colour choice the command line selected.
fn colored_stdout() -> StandardStream {
    StandardStream::stdout(COLOR_CHOICE.get().copied().unwrap_or(ColorChoice::Auto))
}

fn print_colored(color: Color, text: &str) {
    let mut stdout = colored_stdout();
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
    fn test_a_stage_failure_is_labelled_by_its_cause_or_its_legacy_flag() {
        for (failure, expected) in [
            (serde_json::json!({"cause": "error"}), "failed"),
            (serde_json::json!({"cause": "cancelled"}), "cancelled"),
            (
                serde_json::json!({"cause": "out_of_time"}),
                "stopped: out of time",
            ),
            (serde_json::json!({"cause": "cut_short"}), "cut short"),
            (
                serde_json::json!({"cause": "interrupted"}),
                "still running when the review stopped",
            ),
            // The cause wins over a legacy flag written beside it.
            (
                serde_json::json!({"cause": "error", "cancelled": true}),
                "failed",
            ),
            (serde_json::json!({"cause": "no_such_cause"}), "stopped"),
            (serde_json::json!({"cancelled": true}), "stopped"),
            (serde_json::json!({"cancelled": false}), "failed"),
            (
                serde_json::json!({"cause": null, "cancelled": true}),
                "stopped",
            ),
            (serde_json::json!({}), "failed"),
            // A whole stored entry, as either writer stores it, with the
            // fields the API adds beside it. Only `cause` decides the label,
            // so this and the next case pin that the stored shape and an
            // older daemon row without `patch_index` keep reading the same.
            (
                serde_json::json!({"stage": "security", "reason": "r", "cause": "out_of_time",
                                   "patch_index": 2, "order": 3, "stage_label": "Security"}),
                "stopped: out of time",
            ),
            // A daemon row written before the record named its patch.
            (
                serde_json::json!({"stage": "security", "reason": "r", "cause": "interrupted"}),
                "still running when the review stopped",
            ),
        ] {
            assert_eq!(stage_failure_label(&failure), expected, "{failure}");
        }
    }

    /// A recorded stage that did not run to completion is printed with the
    /// label of what stopped it, so its time is not read as a finished
    /// stage's, while a finished stage's row is printed as before.
    #[test]
    fn test_a_recorded_stage_says_what_stopped_it() {
        let timing = |stopped_by| StageTiming {
            stage: "deduplication".to_string(),
            seconds: 18,
            turns: 3,
            stopped_by,
        };
        let finished = recorded_stage_row(&timing(None));
        assert_eq!(finished.trim_end(), finished, "{finished:?}");
        assert!(finished.ends_with("3 turns"), "{finished:?}");
        let stopped = recorded_stage_row(&timing(Some(StageStopCause::OutOfTime)));
        assert_eq!(stopped, format!("{finished}  stopped: out of time"));
    }

    /// `show --watch` stops on a Partial patchset and shows its review log,
    /// as it does for a Reviewed one, and keeps polling one still in review.
    #[test]
    fn test_watch_stops_on_a_partial_patchset() {
        for (status, terminal, log) in [
            ("Reviewed", true, true),
            ("Partial", true, true),
            ("Failed", true, true),
            ("Failed To Apply", true, true),
            ("Cancelled", true, false),
            ("Embargoed", true, false),
            ("In Review", false, false),
            ("Pending", false, false),
            ("Fetching", false, false),
        ] {
            assert_eq!(is_watch_terminal(status), terminal, "{status}");
            assert_eq!(has_review_log(status), log, "{status}");
        }
    }

    /// A review row as `/api/patch` lists it, with one new finding when
    /// `issues` is set.
    fn review(id: i64, patch_id: i64, status: &str, issues: bool) -> Value {
        let findings = if issues {
            json!([{ "severity": "High", "preexisting": false }])
        } else {
            json!([])
        };
        json!({
            "id": id,
            "patch_id": patch_id,
            "status": status,
            "output": json!({ "findings": findings }).to_string(),
        })
    }

    /// The label reads the status first: a Partial review is never labelled
    /// as a complete review that found issues, and every other status keeps
    /// the label it had.
    #[test]
    fn test_a_partial_review_is_labelled_partial_first() {
        for (status, issues, expected) in [
            ("Partial", true, "Partial, Issues Found"),
            ("Partial", false, "Partial"),
            ("Reviewed", true, "Issues Found"),
            ("Reviewed", false, "Reviewed"),
            ("Failed", false, "Failed"),
            ("In Review", false, "In Review"),
        ] {
            let r = review(1, 1, status, issues);
            assert_eq!(review_result_label(&r), expected, "{status} {issues}");
        }
    }

    /// Partial and TimedOut are yellow wherever a status is printed, so that
    /// neither reads as a full review, and a Partial review's badge is yellow
    /// even when it found nothing.
    #[test]
    fn test_statuses_that_ran_out_of_time_are_yellow() {
        for status in ["Partial", "TimedOut"] {
            assert_eq!(patchset_status_color(status), Color::Yellow, "{status}");
            assert_eq!(patch_status_color(status), Color::Yellow, "{status}");
        }
        assert_eq!(patchset_status_color("Reviewed"), Color::Green);
        assert_eq!(patch_status_color("Reviewed"), Color::Green);
        assert_eq!(patch_status_color("Failed"), Color::Red);

        for (status, issues, expected) in [
            ("Partial", false, Color::Yellow),
            ("Partial", true, Color::Yellow),
            ("Reviewed", false, Color::Green),
            ("Reviewed", true, Color::Yellow),
            ("Failed", false, Color::Red),
        ] {
            let r = review(1, 1, status, issues);
            assert_eq!(review_badge_color(&r), expected, "{status} {issues}");
        }
    }

    /// A patch is shown by its latest Reviewed review, else its latest
    /// Partial one, else its latest review of any status, and never by
    /// another patch's review.
    #[test]
    fn test_the_best_review_prefers_reviewed_then_partial_then_latest() {
        let id_of_best = |rows: &[Value]| {
            let refs: Vec<&Value> = rows.iter().collect();
            find_best_review_for_patch_refs(7, &refs).and_then(|r| r["id"].as_i64())
        };
        // Each patch's reviews as (id, status), oldest first, and the id of
        // the review expected to stand for it.
        type Case<'a> = (&'a [(i64, &'a str)], Option<i64>);
        let cases: [Case; 6] = [
            (&[(1, "Failed"), (2, "Partial"), (3, "Failed")], Some(2)),
            (&[(1, "Reviewed"), (2, "Partial")], Some(1)),
            (&[(1, "Partial"), (2, "Reviewed"), (3, "Partial")], Some(2)),
            (&[(1, "Partial"), (2, "Partial")], Some(2)),
            (&[(1, "Failed"), (2, "Cancelled")], Some(2)),
            (&[], None),
        ];
        for (statuses, expected) in cases {
            let mut rows: Vec<Value> = statuses
                .iter()
                .map(|(id, status)| review(*id, 7, status, false))
                .collect();
            // Another patch's Reviewed review is never chosen.
            rows.push(review(99, 8, "Reviewed", false));
            assert_eq!(id_of_best(&rows), expected, "{statuses:?}");
        }
    }

    /// `show --summary` counts a patch by the same review every other view
    /// shows for it, counts a Partial patch on its own rather than as in
    /// review, lists only Reviewed patches under issues so the list matches
    /// `reviewed_issues`, and reports the newest review id even when it is a
    /// retry that does not stand for its patch.
    #[test]
    fn test_the_summary_counts_partial_patches_on_their_own() {
        let patch = |id: i64| json!({ "id": id, "part_index": id, "subject": format!("p{id}") });
        let patches: Vec<Value> = (1..=6).map(patch).collect();
        let rows = [
            review(10, 1, "Reviewed", false),
            review(11, 2, "Reviewed", true),
            // A later failed retry does not hide the salvaged review.
            review(12, 3, "Partial", true),
            review(16, 3, "Failed", false),
            review(14, 4, "Failed", false),
            review(15, 5, "In Review", false),
        ];
        let refs: Vec<&Value> = rows.iter().collect();

        let summary = PatchsetSummary::tally(&patches, &refs);

        assert_eq!(
            summary,
            PatchsetSummary {
                reviewed_clean: 1,
                reviewed_issues: 1,
                partial: 1,
                in_review: 1,
                not_started: 1,
                failed: 1,
                latest_review_id: 16,
                issues: vec![(2, "p2".to_string())],
            }
        );
    }

    /// A Partial review says it stopped because it ran out of time, and
    /// neither a Partial nor a failed review claims to have succeeded.
    #[test]
    fn test_the_attempt_note_follows_the_status() {
        let note = |status: &str, attempt: u64| {
            attempt_note(&json!({
                "status": status,
                "duration_seconds": 90,
                "attempt": attempt,
            }))
        };
        let d = format_duration(90);
        assert_eq!(
            note("Partial", 1),
            Some(format!("stopped after {d} (out of time)"))
        );
        assert_eq!(
            note("Partial", 2),
            Some(format!("stopped after {d} (out of time), on attempt 2"))
        );
        assert_eq!(
            note("Failed", 2),
            Some(format!("failed after {d}, on attempt 2"))
        );
        assert_eq!(
            note("Reviewed", 2),
            Some(format!("reviewed in {d}, succeeded on attempt 2"))
        );
        assert_eq!(attempt_note(&json!({ "status": "Reviewed" })), None);
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

    #[test]
    fn test_cli_bugs_and_token_subcommand_parsing() {
        let parsed = Cli::try_parse_from([
            "sashiko-cli",
            "--token",
            "jwt-token",
            "bugs",
            "list",
            "--subsystem",
            "BTRFS FILE SYSTEM",
            "--severity",
            "high",
        ])
        .unwrap();
        assert_eq!(parsed.token.as_deref(), Some("jwt-token"));
        assert!(matches!(
            parsed.command,
            Commands::Bugs {
                action: BugCommands::List { .. }
            }
        ));

        let show = Cli::try_parse_from(["sashiko-cli", "bugs", "show", "42"]).unwrap();
        assert!(matches!(
            show.command,
            Commands::Bugs {
                action: BugCommands::Show { id: 42 }
            }
        ));

        let token_cmd = Cli::try_parse_from([
            "sashiko-cli",
            "token",
            "create",
            "--access",
            "comment",
            "--days",
            "14",
        ])
        .unwrap();
        assert!(matches!(
            token_cmd.command,
            Commands::Token {
                action: TokenCommands::Create { days: 14, .. }
            }
        ));

        let local_cmd = Cli::try_parse_from(["sashiko-cli", "local", "HEAD~1..HEAD", "--agent"])
            .expect("local --agent should parse");
        assert!(matches!(
            local_cmd.command,
            Commands::Local {
                ref input,
                agent: true,
                ..
            } if input == "HEAD~1..HEAD"
        ));
    }
}
