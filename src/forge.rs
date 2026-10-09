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

use crate::baseline::CommitId;
use crate::finding_view::sanitize_one_line;
use crate::settings::ForgeRepo;
use anyhow::Result;
use axum::http::{HeaderMap, StatusCode};
use base64::Engine;
use bytes::Bytes;
use hmac::{Hmac, Mac, digest::KeyInit};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Validate a git commit SHA (40-char SHA-1 or 64-char SHA-256, hex digits).
pub fn is_valid_git_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Check that a URL uses an acceptable scheme for git operations.
pub fn is_valid_repo_url(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://") || url.starts_with("git@")
}

/// Check that a repository URL does not target known internal or metadata
/// endpoints. Parses the URL and checks the host component only, avoiding
/// false positives from blocklist patterns appearing in usernames or paths.
///
/// This is a best-effort blocklist, not a complete SSRF mitigation. DNS
/// rebinding can bypass host-based checks. The primary access control
/// is webhook signature verification.
pub fn is_safe_repo_url(url: &str) -> bool {
    if !is_valid_repo_url(url) {
        return false;
    }
    // For git@ URLs, extract the host portion. SSH interprets the LAST
    // '@' as the user/host separator, so we must do the same to prevent
    // injection via URLs like "git@github.com@127.0.0.1:repo.git".
    let host = if let Some(rest) = url.strip_prefix("git@") {
        let before_colon = rest.split(':').next().unwrap_or("");
        // Use the portion after the last '@' — this is what SSH resolves
        let ssh_host = before_colon.rsplit('@').next().unwrap_or(before_colon);
        ssh_host.to_ascii_lowercase()
    } else if let Ok(parsed) = url::Url::parse(url) {
        parsed.host_str().unwrap_or("").to_ascii_lowercase()
    } else {
        return false;
    };

    // Blocklist applied to the host component only
    !host.starts_with("169.254.")
        && host != "metadata.google.internal"
        && !host.starts_with("localhost")
        && !host.starts_with("127.")
        && host != "[::1]"
        && host != "0.0.0.0"
        // Decimal representations of loopback (127.0.0.0/8)
        && !(2130706432..=2130706687).contains(&host.parse::<u64>().unwrap_or(0))
        // Decimal representation of 169.254.169.254
        && host != "2852039166"
        // Hex and octal IP representations
        && !host.starts_with("0x7f")
        && !host.starts_with("0xa9fe")
        && !host.starts_with("0177")
}

/// Decode a webhook secret. If prefixed with "whsec_", strip the prefix
/// and base64-decode the remainder (Standard Webhooks convention). Otherwise
/// return the raw string bytes.
fn decode_webhook_secret(secret: &str) -> Vec<u8> {
    if let Some(encoded) = secret.strip_prefix("whsec_") {
        match base64::engine::general_purpose::STANDARD.decode(encoded) {
            Ok(key) => key,
            Err(e) => {
                tracing::warn!(
                    "webhook_secret has whsec_ prefix but base64 decode failed: {}. \
                     Check that the token was copied correctly from GitLab. \
                     Falling back to raw string bytes.",
                    e
                );
                secret.as_bytes().to_vec()
            }
        }
    } else {
        secret.as_bytes().to_vec()
    }
}

/// Verify a Standard Webhooks HMAC-SHA256 signature (GitLab 19.0+ signing
/// token). The signature header may contain multiple space-separated entries,
/// each in the format "v1,{base64(hmac)}". The HMAC is computed over
/// "{message_id}.{timestamp}.{body}".
fn verify_standard_webhook_signature(
    secret: &str,
    msg_id: &str,
    timestamp: &str,
    body: &[u8],
    signatures: &str,
) -> bool {
    let key = decode_webhook_secret(secret);
    let mut mac = match HmacSha256::new_from_slice(&key) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let preamble = format!("{}.{}.", msg_id, timestamp);
    mac.update(preamble.as_bytes());
    mac.update(body);
    let result = mac.finalize().into_bytes();
    let expected = format!(
        "v1,{}",
        base64::engine::general_purpose::STANDARD.encode(result)
    );
    signatures
        .split(' ')
        .any(|sig| expected.as_bytes().ct_eq(sig.as_bytes()).into())
}

/// Verify a GitHub HMAC-SHA256 signature. The header value has the format
/// "sha256={hex_digest}". GitHub sends lowercase hex per their documentation.
/// The received hex is normalized to lowercase before comparison for
/// robustness against forges that may use uppercase.
fn verify_github_signature(secret: &str, body: &[u8], signature_header: &str) -> bool {
    let hex_sig = match signature_header.strip_prefix("sha256=") {
        Some(s) => s.to_ascii_lowercase(),
        None => return false,
    };
    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    let result = mac.finalize().into_bytes();
    let mut computed = String::with_capacity(64);
    for b in result {
        use std::fmt::Write;
        let _ = write!(computed, "{:02x}", b);
    }
    computed.as_bytes().ct_eq(hex_sig.as_bytes()).into()
}

/// Verify a legacy GitLab secret token via constant-time comparison.
/// Note: ct_eq reveals whether the lengths differ (but not the content).
/// This is acceptable for webhook secrets with sufficient entropy.
fn verify_secret_token(secret: &str, token_header: &str) -> bool {
    secret.as_bytes().ct_eq(token_header.as_bytes()).into()
}

/// Metadata extracted from forge webhook
#[derive(Debug, Clone)]
pub struct ForgeMetadata {
    pub repo_url: Option<String>,
    pub base_sha: String,
    pub head_sha: String,
    pub pr_number: i64,
    pub pr_title: Option<String>,
    pub pr_url: Option<String>,
    pub author: Option<String>,
    /// The pull request's description as [`request_description`] reads
    /// it: trimmed, capped, and `None` when the forge sent none or a blank
    /// one. It is written by whoever opened the pull request, so it is
    /// untrusted text.
    pub pr_body: Option<String>,
}

/// The most of a pull request description that is kept, in bytes.
///
/// GitHub caps a description at 65536 characters, but a GitLab
/// description can be about 1 MB and an unsigned delivery is bounded only
/// by the request body limit, so the cap is applied here to keep every
/// stored revision, and every later rendering of it, bounded.
pub const MAX_DESCRIPTION_BYTES: usize = 65536;

/// What ends a description cut at [`MAX_DESCRIPTION_BYTES`].
const DESCRIPTION_TRUNCATED: &str = "\n[truncated]";

/// Reads a pull or merge request description from its JSON value.
///
/// The text is trimmed, and a value that is null, absent, not a string or
/// blank is `None`, so a stored description is never empty. Text longer
/// than [`MAX_DESCRIPTION_BYTES`] is cut on a character boundary and ends
/// with a line saying so, and the result, marker included, is never longer
/// than the cap.
pub fn request_description(value: &serde_json::Value) -> Option<String> {
    let text = value.as_str()?.trim();
    if text.is_empty() {
        return None;
    }
    if text.len() <= MAX_DESCRIPTION_BYTES {
        return Some(text.to_string());
    }
    let kept = crate::utils::utf8_prefix(text, MAX_DESCRIPTION_BYTES - DESCRIPTION_TRUNCATED.len());
    Some(format!("{}{DESCRIPTION_TRUNCATED}", kept.trim_end()))
}

/// Returns true if the authenticated forge username belongs to Dependabot
/// (`dependabot[bot]` or `dependabot`).
pub fn is_dependabot_author(author: &str) -> bool {
    let lower = author.trim().to_ascii_lowercase();
    lower == "dependabot" || lower == "dependabot[bot]"
}

/// Classification of a webhook request that passed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeEvent {
    /// A pull or merge request carrying commits to review.
    ChangeRequest,
    /// A provider handshake sent to confirm the endpoint is reachable. It
    /// carries nothing to review and must be acknowledged without side
    /// effects.
    Handshake,
}

/// Longest caller supplied value echoed into a log line.
const MAX_LOGGED_LEN: usize = 32;

/// Echo a caller supplied string into a log line only when it is short and
/// printable. A webhook sender controls both the event header and the
/// provider path segment, and a raw value could carry newlines that forge
/// whole log entries.
pub fn loggable(value: &str) -> &str {
    let printable = !value.is_empty()
        && value.len() <= MAX_LOGGED_LEN
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' ' | '[' | ']'));
    if printable { value } else { "(invalid)" }
}

/// Name the event a webhook carried, for diagnostics only. Validation never
/// consults this: it reads the provider specific header directly.
pub fn event_label(headers: &HeaderMap) -> &str {
    ["x-github-event", "x-gitlab-event"]
        .into_iter()
        .find_map(|name| headers.get(name).and_then(|value| value.to_str().ok()))
        .map(loggable)
        .unwrap_or("(none)")
}

/// Whether an action reported for a change request asks for a review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewIntent {
    /// The request carries commits that have not been reviewed yet.
    Review,
    /// The forge is reporting a change that leaves the commits untouched,
    /// such as a label, an assignee or a title edit.
    Skip,
}

/// Trait for forge provider implementations
pub trait ForgeProvider: Send + Sync {
    /// Provider name (e.g., "GitHub", "GitLab")
    fn name(&self) -> &str;

    /// Validate webhook event type and verify signature when a secret is
    /// configured. When `secret` is `None`, only event-type validation is
    /// performed and the request is treated as unauthenticated — callers
    /// must enforce their own access control before calling this method.
    /// Returns the kind of event on success, `UNAUTHORIZED` if the signature
    /// is missing or invalid, `BAD_REQUEST` if the event type is wrong.
    fn validate_event(
        &self,
        headers: &HeaderMap,
        body: &Bytes,
        secret: Option<&str>,
    ) -> Result<ForgeEvent, StatusCode>;

    /// Parse webhook payload and extract metadata
    fn parse_payload(&self, body: &Bytes) -> Result<(String, ForgeMetadata), StatusCode>;

    /// Decide whether the action reported by `parse_payload` describes new
    /// commits. The action vocabulary belongs to the provider, so each one
    /// answers for itself.
    fn review_intent(&self, action: &str) -> ReviewIntent;
}

/// GitHub forge provider
pub struct GitHubForge;

impl ForgeProvider for GitHubForge {
    fn name(&self) -> &str {
        "GitHub"
    }

    fn validate_event(
        &self,
        headers: &HeaderMap,
        body: &Bytes,
        secret: Option<&str>,
    ) -> Result<ForgeEvent, StatusCode> {
        let event = headers
            .get("x-github-event")
            .and_then(|v| v.to_str().ok())
            .ok_or(StatusCode::BAD_REQUEST)?;

        // A ping is what GitHub sends the moment a webhook is saved, and its
        // delivery status is what an administrator checks to confirm the
        // endpoint works. Rejecting it reports a broken integration that is
        // in fact correctly configured.
        let kind = match event {
            "pull_request" => ForgeEvent::ChangeRequest,
            "ping" => ForgeEvent::Handshake,
            _ => return Err(StatusCode::BAD_REQUEST),
        };

        if let Some(secret) = secret {
            let sig = headers
                .get("x-hub-signature-256")
                .and_then(|v| v.to_str().ok())
                .ok_or(StatusCode::UNAUTHORIZED)?;
            if !verify_github_signature(secret, body, sig) {
                return Err(StatusCode::UNAUTHORIZED);
            }
        }

        Ok(kind)
    }

    fn parse_payload(&self, body: &Bytes) -> Result<(String, ForgeMetadata), StatusCode> {
        use serde_json::Value;

        let payload: Value = serde_json::from_slice(body).map_err(|_| StatusCode::BAD_REQUEST)?;

        let action = payload["action"]
            .as_str()
            .ok_or(StatusCode::BAD_REQUEST)?
            .to_string();

        let pr = &payload["pull_request"];
        if pr.is_null() {
            return Err(StatusCode::BAD_REQUEST);
        }

        let head_sha = pr["head"]["sha"]
            .as_str()
            .ok_or(StatusCode::BAD_REQUEST)?
            .to_string();

        let base_sha = pr["base"]["sha"]
            .as_str()
            .ok_or(StatusCode::BAD_REQUEST)?
            .to_string();

        if !is_valid_git_sha(&head_sha) || !is_valid_git_sha(&base_sha) {
            return Err(StatusCode::BAD_REQUEST);
        }

        let pr_number = pr["number"].as_i64().ok_or(StatusCode::BAD_REQUEST)?;
        if pr_number <= 0 {
            return Err(StatusCode::BAD_REQUEST);
        }

        let pr_title = pr["title"].as_str().map(|s| s.to_string());
        let pr_url = pr["html_url"].as_str().map(|s| s.to_string());
        let pr_body = request_description(&pr["body"]);
        let author = pr["user"]["login"]
            .as_str()
            .or_else(|| payload["sender"]["login"].as_str())
            .map(|s| s.to_string());

        let repo_url = payload["repository"]["clone_url"]
            .as_str()
            .map(|s| s.to_string());

        if let Some(ref url) = repo_url
            && !is_safe_repo_url(url)
        {
            return Err(StatusCode::BAD_REQUEST);
        }

        let metadata = ForgeMetadata {
            repo_url,
            base_sha,
            head_sha,
            pr_number,
            pr_title,
            pr_url,
            author,
            pr_body,
        };

        Ok((action, metadata))
    }

    fn review_intent(&self, action: &str) -> ReviewIntent {
        // GitHub reports roughly twenty actions on a pull request and sends
        // every one of them to a subscriber. Only these four can leave the
        // head commit different from the last time it was seen.
        match action {
            "opened" | "reopened" | "synchronize" | "ready_for_review" => ReviewIntent::Review,
            _ => ReviewIntent::Skip,
        }
    }
}

/// GitLab forge provider
pub struct GitLabForge;

impl ForgeProvider for GitLabForge {
    fn name(&self) -> &str {
        "GitLab"
    }

    fn validate_event(
        &self,
        headers: &HeaderMap,
        body: &Bytes,
        secret: Option<&str>,
    ) -> Result<ForgeEvent, StatusCode> {
        let event = headers
            .get("x-gitlab-event")
            .and_then(|v| v.to_str().ok())
            .ok_or(StatusCode::BAD_REQUEST)?;

        // GitLab has no handshake event: its Test button replays a real hook,
        // so a merge request is the only thing worth accepting here.
        if event != "Merge Request Hook" {
            return Err(StatusCode::BAD_REQUEST);
        }

        if let Some(secret) = secret {
            // Try Standard Webhooks signature first (GitLab 19.0+)
            if let (Some(msg_id), Some(timestamp), Some(sig)) = (
                headers.get("webhook-id").and_then(|v| v.to_str().ok()),
                headers
                    .get("webhook-timestamp")
                    .and_then(|v| v.to_str().ok()),
                headers
                    .get("webhook-signature")
                    .and_then(|v| v.to_str().ok()),
            ) {
                if !verify_standard_webhook_signature(secret, msg_id, timestamp, body, sig) {
                    return Err(StatusCode::UNAUTHORIZED);
                }
                return Ok(ForgeEvent::ChangeRequest);
            }

            // Fallback: legacy secret token (X-Gitlab-Token)
            if let Some(token) = headers.get("x-gitlab-token").and_then(|v| v.to_str().ok()) {
                if !verify_secret_token(secret, token) {
                    return Err(StatusCode::UNAUTHORIZED);
                }
                return Ok(ForgeEvent::ChangeRequest);
            }

            // Secret configured but no auth header present
            return Err(StatusCode::UNAUTHORIZED);
        }

        Ok(ForgeEvent::ChangeRequest)
    }

    fn parse_payload(&self, body: &Bytes) -> Result<(String, ForgeMetadata), StatusCode> {
        use serde_json::Value;

        let payload: Value = serde_json::from_slice(body).map_err(|_| StatusCode::BAD_REQUEST)?;

        let action = payload["object_kind"]
            .as_str()
            .ok_or(StatusCode::BAD_REQUEST)?
            .to_string();

        let attrs = &payload["object_attributes"];
        if attrs.is_null() {
            return Err(StatusCode::BAD_REQUEST);
        }

        let head_sha = attrs["last_commit"]["id"]
            .as_str()
            .ok_or(StatusCode::BAD_REQUEST)?
            .to_string();

        let base_sha = attrs["diff_refs"]["base_sha"]
            .as_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| head_sha.clone());

        if !is_valid_git_sha(&head_sha) || !is_valid_git_sha(&base_sha) {
            return Err(StatusCode::BAD_REQUEST);
        }

        let pr_number = attrs["iid"].as_i64().ok_or(StatusCode::BAD_REQUEST)?;
        if pr_number <= 0 {
            return Err(StatusCode::BAD_REQUEST);
        }

        let pr_title = attrs["title"].as_str().map(|s| s.to_string());
        let pr_url = attrs["url"].as_str().map(|s| s.to_string());
        let pr_body = request_description(&attrs["description"]);
        let author = payload["user"]["username"].as_str().map(|s| s.to_string());

        let repo_url = payload["project"]["git_http_url"]
            .as_str()
            .map(|s| s.to_string());

        if let Some(ref url) = repo_url
            && !is_safe_repo_url(url)
        {
            return Err(StatusCode::BAD_REQUEST);
        }

        let metadata = ForgeMetadata {
            repo_url,
            base_sha,
            head_sha,
            pr_number,
            pr_title,
            pr_url,
            author,
            pr_body,
        };

        Ok((action, metadata))
    }

    fn review_intent(&self, _action: &str) -> ReviewIntent {
        // parse_payload reports the object kind here, not the action inside
        // object_attributes, so there is nothing to discriminate on yet and
        // every merge request hook is reviewed as it always was. Narrowing
        // this needs the action and oldrev fields, which is a change to
        // GitLab parsing rather than to the route.
        ReviewIntent::Review
    }
}

/// Extract repository name from a URL
pub fn extract_repo_name_from_url(url: &str) -> String {
    url.trim_end_matches('/')
        .split('/')
        .next_back()
        .map(|s| s.trim_end_matches(".git"))
        .unwrap_or("repo")
        .to_string()
}

/// Extract repository name from a GitLab MR or GitHub PR URL
pub fn extract_repo_name_from_mr_url(url: &str) -> Option<String> {
    let before_sep = url
        .split_once("/-/")
        .or_else(|| url.split_once("/pull/"))
        .map(|(prefix, _)| prefix)?;
    let name = before_sep
        .trim_end_matches('/')
        .split('/')
        .next_back()?
        .trim_end_matches(".git")
        .to_string();
    if name.is_empty() { None } else { Some(name) }
}

/// Extract the `"owner/repo"` slug from a GitHub PR or GitLab MR URL.
pub fn extract_owner_repo_from_mr_url(url: &str) -> Option<String> {
    let before_sep = url
        .split_once("/-/")
        .or_else(|| url.split_once("/pull/"))
        .map(|(prefix, _)| prefix)?;
    let after_scheme = before_sep
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(before_sep);
    let (_, path) = after_scheme.split_once('/')?;
    let slug = path.trim_matches('/').trim_end_matches(".git");
    if slug.is_empty() || !slug.contains('/') {
        None
    } else {
        Some(slug.to_string())
    }
}

/// Summary and findings for one commit in a pull request series.
#[derive(Debug, Clone)]
pub struct PatchReviewSummaryItem {
    pub part_index: usize,
    pub total_parts: usize,
    pub commit_id: Option<String>,
    pub subject: String,
    pub inline_review: Option<String>,
}

/// Maximum byte length of a GitHub issue/PR comment body (GitHub's hard limit
/// is 65 536 bytes; leave safety margin for UTF-8 boundary and trailing link).
const MAX_GITHUB_COMMENT_BYTES: usize = 60_000;

/// Compose the markdown comment posted to a pull request thread when a review
/// completes. Always produces a comment: clean pull requests receive a short
/// confirmation with a link to the full review trace.
pub fn compose_pr_review_comment(
    version: Option<u32>,
    total_commits: usize,
    series_summary: Option<&str>,
    patches: &[PatchReviewSummaryItem],
    target_url: &str,
) -> String {
    let header = match version {
        Some(v) if v > 1 => format!("### Sashiko review — v{}", v),
        _ => "### Sashiko review".to_string(),
    };

    let link_host = target_url
        .split_once("://")
        .and_then(|(_, rest)| rest.split('/').next())
        .filter(|h| !h.is_empty())
        .unwrap_or("sashiko.sashiko.dev");

    let commit_word = if total_commits == 1 {
        "1 commit".to_string()
    } else {
        format!("{} commits", total_commits)
    };

    let patches_with_findings: Vec<&PatchReviewSummaryItem> = patches
        .iter()
        .filter(|p| {
            p.inline_review.as_deref().is_some_and(|text| {
                let t = text.trim();
                !t.is_empty() && t != "No issues found."
            })
        })
        .collect();

    if patches_with_findings.is_empty() {
        return format!(
            "{header}\n\n✓ **No issues found** across {commit_word}.\n\n[Full review log on {link_host}]({target_url})\n"
        );
    }

    let mut out = String::with_capacity(2048);
    out.push_str(&header);
    out.push_str("\n\n");

    if let Some(summary) = series_summary.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str("<details>\n<summary>Series summary</summary>\n\n");
        out.push_str(summary);
        out.push_str("\n\n</details>\n\n");
    }

    for patch in patches_with_findings {
        let short_sha = patch
            .commit_id
            .as_deref()
            .filter(|s| s.len() >= 8)
            .map(|s| &s[..8]);
        let title_line = match short_sha {
            Some(sha) => format!(
                "#### Commit {}/{} — `{}` {}\n\n",
                patch.part_index, patch.total_parts, sha, patch.subject
            ),
            None => format!(
                "#### Commit {}/{} — {}\n\n",
                patch.part_index, patch.total_parts, patch.subject
            ),
        };
        out.push_str(&title_line);

        if let Some(inline) = patch.inline_review.as_deref() {
            out.push_str(inline.trim());
            out.push_str("\n\n");
        }
    }

    let footer = format!("[Full review and stage logs on {link_host}]({target_url})\n");

    if out.len() + footer.len() > MAX_GITHUB_COMMENT_BYTES {
        let mut cut = MAX_GITHUB_COMMENT_BYTES.saturating_sub(footer.len() + 64);
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str("\n\n*(comment truncated; see full report below)*\n\n");
    }

    out.push_str(&footer);
    out
}

/// Mint a short-lived RS256 JWT identifying a GitHub App (`iss = app_id`).
pub fn mint_github_app_jwt(app_id: u64, pem: &str) -> Result<String, String> {
    #[derive(serde::Serialize)]
    struct Claims {
        iat: i64,
        exp: i64,
        iss: String,
    }

    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        iat: now - 60,
        exp: now + 540,
        iss: app_id.to_string(),
    };

    let key = jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes())
        .map_err(|e| format!("invalid GitHub App RSA private key: {}", e))?;
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
    jsonwebtoken::encode(&header, &claims, &key)
        .map_err(|e| format!("failed to sign GitHub App JWT: {}", e))
}

/// Exchange a GitHub App JWT for an installation access token.
pub async fn exchange_github_installation_token(
    client: &reqwest::Client,
    api_base: &str,
    installation_id: u64,
    jwt: &str,
) -> Result<String, String> {
    let url = format!(
        "{}/app/installations/{}/access_tokens",
        api_base.trim_end_matches('/'),
        installation_id
    );
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", jwt))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "sashiko")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|e| format!("GitHub token request failed: {}", e))?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.map_err(|e| {
        format!(
            "invalid JSON from GitHub token endpoint ({}): {}",
            status, e
        )
    })?;

    if !status.is_success() {
        let msg = body["message"].as_str().unwrap_or("unknown error");
        return Err(format!(
            "GitHub token exchange returned {}: {}",
            status, msg
        ));
    }

    body["token"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "GitHub token response missing 'token' field".to_string())
}

/// Post a comment to a GitHub pull request (via the issue comments endpoint).
pub async fn post_github_pr_comment(
    client: &reqwest::Client,
    api_base: &str,
    repo: &str,
    pr_number: i64,
    token: &str,
    body: &str,
) -> Result<u16, String> {
    let url = format!(
        "{}/repos/{}/issues/{}/comments",
        api_base.trim_end_matches('/'),
        repo,
        pr_number
    );
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "sashiko")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .json(&serde_json::json!({ "body": body }))
        .send()
        .await
        .map_err(|e| format!("GitHub comment POST failed: {}", e))?;

    let status = resp.status();
    if status.is_success() {
        Ok(status.as_u16())
    } else {
        let text = resp.text().await.unwrap_or_default();
        Err(format!("GitHub returned {}: {}", status, text))
    }
}

/// Registry for forge providers
pub struct ForgeRegistry {
    providers: HashMap<String, Arc<dyn ForgeProvider>>,
}

impl ForgeRegistry {
    pub fn new() -> Self {
        let mut registry = Self {
            providers: HashMap::new(),
        };

        registry.register("github", Arc::new(GitHubForge));
        registry.register("gitlab", Arc::new(GitLabForge));

        registry
    }

    pub fn register(&mut self, name: &str, provider: Arc<dyn ForgeProvider>) {
        self.providers.insert(name.to_string(), provider);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn ForgeProvider>> {
        self.providers.get(name).cloned()
    }

    pub fn list_providers(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }
}

impl Default for ForgeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Format a pull request / merge request subject line with `#` (or `!` for GitLab)
/// and an optional `[vN]` revision tag when `version >= 2`.
pub fn format_mr_subject(mr_url: Option<&str>, number: i64, version: u32, title: &str) -> String {
    let prefix = match mr_url {
        Some(url) if url.contains("gitlab") => "!",
        _ => "#",
    };
    let clean_title = strip_existing_mr_prefix(title, number);
    if version >= 2 {
        format!("{}{} [v{}]: {}", prefix, number, version, clean_title)
    } else {
        format!("{}{}: {}", prefix, number, clean_title)
    }
}

fn strip_existing_mr_prefix(title: &str, number: i64) -> &str {
    let trimmed = title.trim();
    for pfx in ['#', '!'] {
        let base = format!("{}{}", pfx, number);
        if let Some(rest) = trimmed.strip_prefix(&base) {
            let rest = rest.trim_start();
            if let Some(after_v) = rest.strip_prefix("[v")
                && let Some((_, after_bracket)) = after_v.split_once(']')
            {
                return after_bracket
                    .trim_start()
                    .strip_prefix(':')
                    .unwrap_or(after_bracket)
                    .trim_start();
            }
            if let Some(after_colon) = rest.strip_prefix(':') {
                return after_colon.trim_start();
            }
        }
    }
    trimmed
}

/// Extract the version number from a pull request subject formatted by `format_mr_subject`.
pub fn extract_mr_version_from_subject(subject: Option<&str>, number: i64) -> Option<u32> {
    let trimmed = subject?.trim();
    for pfx in ['#', '!'] {
        let base = format!("{}{}", pfx, number);
        if let Some(rest) = trimmed.strip_prefix(&base) {
            let rest = rest.trim_start();
            if let Some(after_v) = rest.strip_prefix("[v")
                && let Some((digits, _)) = after_v.split_once(']')
                && let Ok(v) = digits.parse::<u32>()
            {
                return Some(v);
            }
            return Some(1);
        }
    }
    None
}

/// How long the daemon waits for gh to describe one pull request. gh makes one
/// API request for it, so a minute means GitHub or the network is stuck, and
/// kill_on_drop then stops gh.
const GH_TIME_LIMIT: Duration = Duration::from_secs(60);

/// The longest gh error line, or validation reason, that reaches a log line
/// or a reply, in code points.
const GH_DETAIL_MAX_CHARS: usize = 200;

/// gh's documented exit status for "authentication required".
const GH_EXIT_AUTH_REQUIRED: i32 = 4;

/// The fields asked of gh: exactly what a review of the revision needs,
/// including the description the model is given as the cover letter.
const GH_PR_FIELDS: &str = "number,title,url,baseRefOid,headRefOid,body";

/// What gh reported about one pull request, validated against the request:
/// the number is the one asked for, the URL names that pull request in
/// `repo`, and both commits are full commit ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPullRequest {
    /// The repository the number was resolved in, spelt as the forge spells
    /// it in `url`, which may differ in case from the setting's spelling.
    pub repo: ForgeRepo,
    /// The pull request number, equal to the one asked for.
    pub number: i64,
    /// The pull request's https URL, as the url crate normalises it.
    pub url: String,
    /// The title as the forge gave it. Like a webhook's, it is only stored
    /// and shown escaped.
    pub title: Option<String>,
    /// The description as [`request_description`] reads it, with the same
    /// trimming and cap as a webhook's, and `None` when gh gave none or a
    /// blank one. Whoever opened the pull request wrote it, so it is
    /// untrusted text.
    pub body: Option<String>,
    /// The commit the pull request is based on.
    pub base: CommitId,
    /// The pull request's head commit.
    pub head: CommitId,
}

impl ResolvedPullRequest {
    /// The metadata a review of this pull request's head is queued with.
    ///
    /// The repository URL is the https clone URL GitHub's webhook sends as
    /// clone_url, in the forge's own spelling, so the fetcher derives the
    /// same fetcher-<hash> remote, and uses the same credentials, as it does
    /// for a webhook-driven review of the same repository. The description
    /// is carried, so the revision records it as a webhook-driven one does.
    /// gh is not asked for the author, because the Dependabot skip it feeds
    /// does not apply to a review the operator asked for, so none is
    /// recorded.
    pub fn to_forge_metadata(&self) -> ForgeMetadata {
        ForgeMetadata {
            repo_url: Some(format!(
                "https://{}/{}/{}.git",
                self.repo.host, self.repo.owner, self.repo.name
            )),
            base_sha: self.base.as_str().to_string(),
            head_sha: self.head.as_str().to_string(),
            pr_number: self.number,
            pr_title: self.title.clone(),
            pr_url: Some(self.url.clone()),
            author: None,
            pr_body: self.body.clone(),
        }
    }
}

/// Why gh could not resolve a pull request. Each message is safe to log and
/// to return to the caller: `Failed` and `Invalid` hold one line reduced by
/// `sanitize_one_line`, never gh's raw output.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GhError {
    /// gh could not be started because no such program was found.
    #[error("gh is not installed on the daemon host, or is not on the daemon's PATH")]
    NotInstalled,
    /// gh exited with its documented status for missing authentication.
    #[error("gh is not logged in as the user that runs the daemon; run gh auth login as that user")]
    NotAuthenticated,
    /// gh did not finish within GH_TIME_LIMIT and was stopped.
    #[error("gh did not answer within {} seconds", GH_TIME_LIMIT.as_secs())]
    TimedOut,
    /// gh could not be run to completion, or failed for another reason;
    /// `detail` is the first line of its error output.
    #[error("gh failed: {detail}")]
    Failed { detail: String },
    /// gh succeeded, but what it printed is not the pull request asked for.
    #[error("gh's answer is not usable: {reason}")]
    Invalid { reason: String },
    /// The server was built without a resolver, so gh was not run.
    #[error(
        "this server is not set up to resolve pull request numbers; the daemon resolves them with gh"
    )]
    NotConfigured,
}

impl GhError {
    fn failed(detail: &str) -> Self {
        Self::Failed {
            detail: sanitize_one_line(detail, GH_DETAIL_MAX_CHARS),
        }
    }

    fn invalid(reason: &str) -> Self {
        Self::Invalid {
            reason: sanitize_one_line(reason, GH_DETAIL_MAX_CHARS),
        }
    }
}

/// Turns a pull request number into the commits a review of its current head
/// needs. The daemon holds one as a trait object, so tests can stand in for
/// gh without running it.
#[async_trait::async_trait]
pub trait PullRequestResolver: Send + Sync {
    /// Resolves pull request `number` of `repo`.
    async fn resolve(&self, repo: &ForgeRepo, number: i64) -> Result<ResolvedPullRequest, GhError>;
}

/// Resolves pull requests with the gh command line tool, logged in as the
/// user that runs the daemon.
#[derive(Debug, Clone, Copy, Default)]
pub struct GhCli;

#[async_trait::async_trait]
impl PullRequestResolver for GhCli {
    async fn resolve(&self, repo: &ForgeRepo, number: i64) -> Result<ResolvedPullRequest, GhError> {
        resolve_with(
            gh_pr_view_command(repo, number),
            GH_TIME_LIMIT,
            repo,
            number,
        )
        .await
    }
}

/// Resolves no pull request, and runs nothing. It is the resolver of a
/// server built without a daemon behind it, such as a test's router, so
/// that nothing such a server serves can run gh or reach the network.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoResolver;

#[async_trait::async_trait]
impl PullRequestResolver for NoResolver {
    async fn resolve(
        &self,
        _repo: &ForgeRepo,
        _number: i64,
    ) -> Result<ResolvedPullRequest, GhError> {
        Err(GhError::NotConfigured)
    }
}

/// Refuses a number no pull request can have before anything is run with it.
fn check_pull_request_number(number: i64) -> Result<(), GhError> {
    if number > 0 {
        Ok(())
    } else {
        Err(GhError::invalid(&format!(
            "{number} is not a pull request number; they start at 1"
        )))
    }
}

/// Builds `gh pr view` for one pull request.
///
/// The repository is named with --repo, in the "host/owner/name" form, so gh
/// neither reads git remotes nor runs git, and neither the daemon's
/// repository nor an inherited GIT_DIR plays any part. The value is joined to
/// the option with "=" so that it can never be read as an option of its own.
/// resolve_with refuses a number below 1 before the command runs, so the
/// number cannot be read as one either.
/// GH_PROMPT_DISABLED keeps gh from waiting for an answer nobody can give.
fn gh_pr_view_command(repo: &ForgeRepo, number: i64) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("gh");
    command
        .arg("pr")
        .arg("view")
        .arg(number.to_string())
        .arg(format!("--repo={}/{}/{}", repo.host, repo.owner, repo.name))
        .arg("--json")
        .arg(GH_PR_FIELDS)
        .env("GH_PROMPT_DISABLED", "1");
    command
}

/// Runs a gh command to completion and reads the pull request it prints.
/// A number no pull request can have is refused before the command runs,
/// because gh would read a negative one as an option.
async fn resolve_with(
    command: tokio::process::Command,
    time_limit: Duration,
    repo: &ForgeRepo,
    number: i64,
) -> Result<ResolvedPullRequest, GhError> {
    check_pull_request_number(number)?;
    let output = run_gh(command, time_limit).await?;
    check_gh_status(&output)?;
    parse_gh_pull_request(&output.stdout, repo, number)
}

/// Runs a gh command with stdin closed, giving up after `time_limit`.
/// kill_on_drop stops gh when the wait is given up, or when the request that
/// started it is dropped.
async fn run_gh(
    mut command: tokio::process::Command,
    time_limit: Duration,
) -> Result<std::process::Output, GhError> {
    let child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => GhError::NotInstalled,
            _ => GhError::failed(&format!("could not start gh: {err}")),
        })?;
    tokio::time::timeout(time_limit, child.wait_with_output())
        .await
        .map_err(|_| GhError::TimedOut)?
        .map_err(|err| GhError::failed(&format!("could not read gh's output: {err}")))
}

/// Types a gh run that did not succeed: its documented authentication status
/// is `NotAuthenticated`, anything else `Failed` with the first line of its
/// error output, or with its exit status when it wrote none.
fn check_gh_status(output: &std::process::Output) -> Result<(), GhError> {
    if output.status.success() {
        return Ok(());
    }
    if output.status.code() == Some(GH_EXIT_AUTH_REQUIRED) {
        return Err(GhError::NotAuthenticated);
    }
    let detail = sanitize_one_line(
        &String::from_utf8_lossy(&output.stderr),
        GH_DETAIL_MAX_CHARS,
    );
    if detail.is_empty() {
        return Err(GhError::failed(&format!(
            "gh {} with no message",
            output.status
        )));
    }
    Err(GhError::Failed { detail })
}

/// The JSON object `gh pr view --json` prints for GH_PR_FIELDS.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPullRequest {
    number: i64,
    #[serde(default)]
    title: Option<String>,
    url: String,
    base_ref_oid: String,
    head_ref_oid: String,
    /// Read as a JSON value, not a string, so that request_description
    /// decides what an absent, null or non-string description means, as it
    /// does for a webhook's.
    #[serde(default)]
    body: serde_json::Value,
}

/// Reads and validates what gh printed for pull request `number` of `repo`.
/// gh's output is the forge's data, so nothing in it is trusted until it is
/// checked: the number must be the one asked for, the commits must be full
/// commit ids, and the URL must name this pull request of this repository.
fn parse_gh_pull_request(
    stdout: &[u8],
    repo: &ForgeRepo,
    number: i64,
) -> Result<ResolvedPullRequest, GhError> {
    let raw: GhPullRequest = serde_json::from_slice(stdout)
        .map_err(|err| GhError::invalid(&format!("gh printed no pull request JSON: {err}")))?;
    if raw.number != number {
        return Err(GhError::invalid(&format!(
            "gh described pull request {}, not {number}",
            raw.number
        )));
    }
    let commit = |role: &str, value: &str| {
        CommitId::parse(value).ok_or_else(|| {
            GhError::invalid(&format!(
                "the {role} commit {value:?} is not a full lowercase commit id"
            ))
        })
    };
    let base = commit("base", &raw.base_ref_oid)?;
    let head = commit("head", &raw.head_ref_oid)?;
    let (url, repo) =
        pull_request_url(&raw.url, repo, number).map_err(|reason| GhError::invalid(&reason))?;
    Ok(ResolvedPullRequest {
        repo,
        number,
        url,
        title: raw.title,
        body: request_description(&raw.body),
        base,
        head,
    })
}

/// Checks that `url` is https://<host>/<owner>/<name>/pull/<number> for
/// `repo`, with no credentials, port, query or fragment. Returns the URL as
/// the url crate normalises it, and the repository as the URL spells it.
/// Host, owner and name compare without ASCII case, as GitHub treats them,
/// because gh prints the repository's own spelling while the setting may
/// spell it differently.
fn pull_request_url(
    url: &str,
    repo: &ForgeRepo,
    number: i64,
) -> Result<(String, ForgeRepo), String> {
    let expected = format!(
        "https://{}/{}/{}/pull/{number}",
        repo.host, repo.owner, repo.name
    );
    // The expected URL comes first, so a long foreign URL that is cut to fit a
    // reply still leaves the operator what the URL had to be.
    let mismatch = || format!("the pull request URL is not {expected} but {url:?}");
    let parsed = url::Url::parse(url).map_err(|_| mismatch())?;
    let plain = parsed.scheme() == "https"
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.port().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none();
    let number = number.to_string();
    let segments: Vec<&str> = parsed
        .path_segments()
        .map(Iterator::collect)
        .unwrap_or_default();
    let named = match (parsed.host_str(), segments.as_slice()) {
        (Some(host), [owner, name, "pull", n])
            if plain
                && host.eq_ignore_ascii_case(&repo.host)
                && owner.eq_ignore_ascii_case(&repo.owner)
                && name.eq_ignore_ascii_case(&repo.name)
                && *n == number =>
        {
            ForgeRepo {
                host: host.to_string(),
                owner: owner.to_string(),
                name: name.to_string(),
            }
        }
        _ => return Err(mismatch()),
    };
    Ok((parsed.into(), named))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_mr_subject_versions() {
        assert_eq!(
            format_mr_subject(
                Some("https://github.com/sashiko-dev/sashiko/pull/513"),
                513,
                1,
                "baseline: route iwl-net and iwl-next series to dev-queue"
            ),
            "#513: baseline: route iwl-net and iwl-next series to dev-queue"
        );
        assert_eq!(
            format_mr_subject(
                Some("https://github.com/sashiko-dev/sashiko/pull/513"),
                513,
                5,
                "baseline: route iwl-net and iwl-next series to dev-queue"
            ),
            "#513 [v5]: baseline: route iwl-net and iwl-next series to dev-queue"
        );
        assert_eq!(
            format_mr_subject(
                Some("https://github.com/sashiko-dev/sashiko/pull/513"),
                513,
                2,
                "#513: baseline: route iwl-net and iwl-next series to dev-queue"
            ),
            "#513 [v2]: baseline: route iwl-net and iwl-next series to dev-queue"
        );
        assert_eq!(
            format_mr_subject(
                Some("https://gitlab.com/org/repo/-/merge_requests/42"),
                42,
                3,
                "fix race condition"
            ),
            "!42 [v3]: fix race condition"
        );
    }

    #[test]
    fn test_is_valid_git_sha_40_char() {
        assert!(is_valid_git_sha("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"));
        assert!(is_valid_git_sha("0000000000000000000000000000000000000000"));
        assert!(is_valid_git_sha("abcdef0123456789abcdef0123456789abcdef01"));
    }

    #[test]
    fn test_is_valid_git_sha_rejects_non_hex() {
        assert!(!is_valid_git_sha(
            "g1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"
        ));
        assert!(!is_valid_git_sha("../../etc/passwd/../../../../etc/shadow"));
        // Uppercase hex is valid — git accepts both cases
        assert!(is_valid_git_sha("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
    }

    #[test]
    fn test_is_valid_git_sha_64_char() {
        let sha256 = "a".repeat(64);
        assert!(is_valid_git_sha(&sha256));
    }

    #[test]
    fn test_is_valid_git_sha_rejects_wrong_length() {
        assert!(!is_valid_git_sha("abc123"));
        assert!(!is_valid_git_sha("a".repeat(39).as_str()));
        assert!(!is_valid_git_sha("a".repeat(41).as_str()));
        assert!(!is_valid_git_sha(""));
    }

    #[test]
    fn test_is_valid_repo_url_accepts_valid_schemes() {
        assert!(is_valid_repo_url("https://gitlab.com/org/repo.git"));
        assert!(is_valid_repo_url("http://gitlab.internal/org/repo.git"));
        assert!(is_valid_repo_url("git@github.com:org/repo.git"));
    }

    #[test]
    fn test_is_valid_repo_url_rejects_invalid_schemes() {
        assert!(!is_valid_repo_url("ftp://files.example.com/repo.tar"));
        assert!(!is_valid_repo_url("file:///etc/passwd"));
        assert!(!is_valid_repo_url("javascript:alert(1)"));
        assert!(!is_valid_repo_url(""));
    }

    #[test]
    fn test_is_safe_repo_url_blocks_ssrf() {
        assert!(!is_safe_repo_url(
            "http://169.254.169.254/latest/meta-data/"
        ));
        assert!(!is_safe_repo_url("http://metadata.google.internal/"));
        assert!(!is_safe_repo_url("http://localhost:5432/"));
        assert!(!is_safe_repo_url("http://localhost.localdomain/repo"));
        assert!(!is_safe_repo_url("http://127.0.0.1:8080/repo"));
        assert!(!is_safe_repo_url("http://127.1/repo"));
        assert!(!is_safe_repo_url("http://[::1]:8080/repo"));
        assert!(!is_safe_repo_url("http://0.0.0.0/repo"));
        // Decimal and hex IP representations of 127.0.0.1
        assert!(!is_safe_repo_url("http://2130706433/repo"));
        assert!(!is_safe_repo_url("http://0x7f000001/repo"));
        // Octal representation
        assert!(!is_safe_repo_url("http://0177.0.0.1/repo"));
        // Decimal representation of 169.254.169.254
        assert!(!is_safe_repo_url("http://2852039166/latest/"));
        // Hex representation of 169.254.x.x
        assert!(!is_safe_repo_url("http://0xa9fea9fe/latest/"));
        // Decimal for 127.0.0.2 (other loopback addresses in 127.0.0.0/8)
        assert!(!is_safe_repo_url("http://2130706434/repo"));
    }

    #[test]
    fn test_is_safe_repo_url_blocks_ssh_injection() {
        // SSH resolves the LAST '@' as user/host separator
        assert!(!is_safe_repo_url("git@github.com@127.0.0.1:repo.git"));
        assert!(!is_safe_repo_url("git@github.com@localhost:repo.git"));
        assert!(!is_safe_repo_url("git@legit.com@169.254.169.254:repo.git"));
    }

    #[test]
    fn test_is_safe_repo_url_no_false_positives_on_path() {
        // Blocklist patterns in username or path should NOT trigger rejection
        assert!(is_safe_repo_url(
            "https://github.com/user-127.0.0.1/repo.git"
        ));
        assert!(is_safe_repo_url(
            "https://github.com/org/localhost-tools.git"
        ));
        assert!(is_safe_repo_url("git@github.com:0x7f-labs/project.git"));
    }

    #[test]
    fn test_is_safe_repo_url_accepts_legitimate() {
        assert!(is_safe_repo_url("https://gitlab.com/org/repo.git"));
        assert!(is_safe_repo_url("https://github.com/org/repo.git"));
        assert!(is_safe_repo_url("git@gitlab.example.com:org/repo.git"));
        assert!(is_safe_repo_url(
            "http://gitlab.internal:8929/group/project.git"
        ));
    }

    #[test]
    fn test_github_parse_payload_rejects_invalid_sha() {
        let forge = GitHubForge;
        let payload = serde_json::json!({
            "action": "opened",
            "pull_request": {
                "head": {"sha": "not-a-valid-sha"},
                "base": {"sha": "also-not-valid"},
                "number": 1,
                "title": "test"
            },
            "repository": {"clone_url": "https://github.com/org/repo.git"}
        });
        let body = Bytes::from(serde_json::to_vec(&payload).unwrap());
        assert_eq!(
            forge.parse_payload(&body).unwrap_err(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_github_parse_payload_rejects_negative_pr() {
        let forge = GitHubForge;
        let valid_sha = "a".repeat(40);
        let payload = serde_json::json!({
            "action": "opened",
            "pull_request": {
                "head": {"sha": &valid_sha},
                "base": {"sha": &valid_sha},
                "number": -1,
                "title": "test"
            },
            "repository": {"clone_url": "https://github.com/org/repo.git"}
        });
        let body = Bytes::from(serde_json::to_vec(&payload).unwrap());
        assert_eq!(
            forge.parse_payload(&body).unwrap_err(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_github_parse_payload_rejects_ssrf_url() {
        let forge = GitHubForge;
        let valid_sha = "a".repeat(40);
        let payload = serde_json::json!({
            "action": "opened",
            "pull_request": {
                "head": {"sha": &valid_sha},
                "base": {"sha": &valid_sha},
                "number": 1,
                "title": "test"
            },
            "repository": {"clone_url": "http://169.254.169.254/latest/meta-data/"}
        });
        let body = Bytes::from(serde_json::to_vec(&payload).unwrap());
        assert_eq!(
            forge.parse_payload(&body).unwrap_err(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_github_parse_payload_accepts_valid() {
        let forge = GitHubForge;
        let valid_sha = "a".repeat(40);
        let base_sha = "b".repeat(40);
        let payload = serde_json::json!({
            "action": "opened",
            "pull_request": {
                "head": {"sha": &valid_sha},
                "base": {"sha": &base_sha},
                "number": 42,
                "title": "Fix something",
                "html_url": "https://github.com/org/repo/pull/42"
            },
            "repository": {"clone_url": "https://github.com/org/repo.git"}
        });
        let body = Bytes::from(serde_json::to_vec(&payload).unwrap());
        let (action, metadata) = forge.parse_payload(&body).unwrap();
        assert_eq!(action, "opened");
        assert_eq!(metadata.pr_number, 42);
        assert_eq!(metadata.head_sha, valid_sha);
    }

    #[test]
    fn test_gitlab_parse_payload_rejects_invalid_sha() {
        let forge = GitLabForge;
        let payload = serde_json::json!({
            "object_kind": "merge_request",
            "object_attributes": {
                "last_commit": {"id": "../../etc/passwd"},
                "diff_refs": {"base_sha": "invalid"},
                "iid": 1,
                "title": "test"
            },
            "project": {"git_http_url": "https://gitlab.com/org/repo.git"}
        });
        let body = Bytes::from(serde_json::to_vec(&payload).unwrap());
        assert_eq!(
            forge.parse_payload(&body).unwrap_err(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_gitlab_parse_payload_rejects_zero_iid() {
        let forge = GitLabForge;
        let valid_sha = "a".repeat(40);
        let payload = serde_json::json!({
            "object_kind": "merge_request",
            "object_attributes": {
                "last_commit": {"id": &valid_sha},
                "diff_refs": {"base_sha": &valid_sha},
                "iid": 0,
                "title": "test"
            },
            "project": {"git_http_url": "https://gitlab.com/org/repo.git"}
        });
        let body = Bytes::from(serde_json::to_vec(&payload).unwrap());
        assert_eq!(
            forge.parse_payload(&body).unwrap_err(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_gitlab_parse_payload_accepts_valid() {
        let forge = GitLabForge;
        let valid_sha = "c".repeat(40);
        let payload = serde_json::json!({
            "object_kind": "merge_request",
            "object_attributes": {
                "last_commit": {"id": &valid_sha},
                "diff_refs": {"base_sha": &valid_sha},
                "iid": 10,
                "title": "Fix bug",
                "url": "https://gitlab.com/org/repo/-/merge_requests/10"
            },
            "project": {"git_http_url": "https://gitlab.com/org/repo.git"}
        });
        let body = Bytes::from(serde_json::to_vec(&payload).unwrap());
        let (action, metadata) = forge.parse_payload(&body).unwrap();
        assert_eq!(action, "merge_request");
        assert_eq!(metadata.pr_number, 10);
    }

    /// The description each parser reads from a delivery whose description
    /// field is `description`, or that has none when it is `None`.
    fn parsed_descriptions(description: Option<serde_json::Value>) -> [Option<String>; 2] {
        let mut github = serde_json::json!({
            "action": "opened",
            "pull_request": {
                "head": {"sha": "a".repeat(40)},
                "base": {"sha": "b".repeat(40)},
                "number": 42,
            },
            "repository": {"clone_url": "https://github.com/org/repo.git"}
        });
        let mut gitlab = serde_json::json!({
            "object_kind": "merge_request",
            "object_attributes": {
                "last_commit": {"id": "c".repeat(40)},
                "iid": 10,
            },
            "project": {"git_http_url": "https://gitlab.com/org/repo.git"}
        });
        if let Some(description) = description {
            github["pull_request"]["body"] = description.clone();
            gitlab["object_attributes"]["description"] = description;
        }
        let parse = |forge: &dyn ForgeProvider, payload: &serde_json::Value| {
            let body = Bytes::from(serde_json::to_vec(payload).unwrap());
            forge.parse_payload(&body).unwrap().1.pr_body
        };
        [parse(&GitHubForge, &github), parse(&GitLabForge, &gitlab)]
    }

    #[test]
    fn request_description_trims_and_treats_blank_as_none() {
        let text = "Fixes the widget.\r\n\r\n- one\n- two";
        assert_eq!(
            parsed_descriptions(Some(serde_json::json!(format!("\n  {text} \n\t")))),
            [Some(text.to_string()), Some(text.to_string())],
            "GitHub's body and GitLab's description are read the same way"
        );
        for absent in [
            None,
            Some(serde_json::Value::Null),
            Some(serde_json::json!("")),
            Some(serde_json::json!("  ")),
            Some(serde_json::json!(" \r\n\t ")),
            Some(serde_json::json!(42)),
            Some(serde_json::json!(["a description"])),
            Some(serde_json::json!({"text": "a description"})),
        ] {
            assert_eq!(
                parsed_descriptions(absent.clone()),
                [None, None],
                "{absent:?}"
            );
        }
    }

    #[test]
    fn an_oversized_description_is_capped() {
        // A GitLab description of about 1 MB, of a three-byte character
        // repeated, so that no cut at a fixed byte offset can be assumed to
        // land on a character boundary.
        let huge = "\u{20ac}".repeat(350_000);
        let [github, gitlab] = parsed_descriptions(Some(serde_json::json!(huge)));
        assert_eq!(github, gitlab);
        let capped = gitlab.unwrap();
        assert!(capped.len() <= MAX_DESCRIPTION_BYTES, "{}", capped.len());
        let kept = capped.strip_suffix("\n[truncated]").unwrap();
        assert!(kept.chars().all(|c| c == '\u{20ac}'));
        assert!(
            MAX_DESCRIPTION_BYTES - capped.len() < '\u{20ac}'.len_utf8(),
            "the cut keeps every whole character that fits"
        );
        assert!(huge.starts_with(kept));

        // The cut in the middle of a character steps back to its start.
        let budget = MAX_DESCRIPTION_BYTES - DESCRIPTION_TRUNCATED.len();
        let straddling = format!("{}\u{20ac}{}", "a".repeat(budget - 1), "t".repeat(64));
        assert!(straddling.len() > MAX_DESCRIPTION_BYTES);
        let capped = request_description(&serde_json::json!(straddling)).unwrap();
        assert_eq!(capped, format!("{}\n[truncated]", "a".repeat(budget - 1)));

        // Text at the cap is kept whole.
        let at_cap = "b".repeat(MAX_DESCRIPTION_BYTES);
        assert_eq!(
            request_description(&serde_json::json!(at_cap)).as_deref(),
            Some(at_cap.as_str())
        );
    }

    // --- HMAC verification tests ---

    #[test]
    fn test_verify_github_signature_known_vector() {
        // Test vector from GitHub docs:
        // secret: "It's a Secret to Everybody"
        // payload: "Hello, World!"
        // expected: sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17
        let secret = "It's a Secret to Everybody";
        let payload = b"Hello, World!";
        let sig = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
        assert!(verify_github_signature(secret, payload, sig));
    }

    #[test]
    fn test_verify_github_signature_rejects_invalid() {
        let secret = "my-secret";
        let payload = b"test body";
        let sig = "sha256=0000000000000000000000000000000000000000000000000000000000000000";
        assert!(!verify_github_signature(secret, payload, sig));
    }

    #[test]
    fn test_verify_github_signature_rejects_missing_prefix() {
        let secret = "my-secret";
        let payload = b"test body";
        let sig = "md5=abcdef";
        assert!(!verify_github_signature(secret, payload, sig));
    }

    #[test]
    fn test_verify_standard_webhook_signature() {
        let secret = "test-secret-key";
        let msg_id = "msg-123";
        let timestamp = "1720000000";
        let body = b"test body";

        // Compute the expected signature manually
        let key = decode_webhook_secret(secret);
        let mut mac = HmacSha256::new_from_slice(&key).unwrap();
        let preamble = format!("{}.{}.", msg_id, timestamp);
        mac.update(preamble.as_bytes());
        mac.update(body);
        let result = mac.finalize().into_bytes();
        let sig = format!(
            "v1,{}",
            base64::engine::general_purpose::STANDARD.encode(result)
        );

        assert!(verify_standard_webhook_signature(
            secret, msg_id, timestamp, body, &sig
        ));
    }

    #[test]
    fn test_verify_standard_webhook_signature_rejects_tampered() {
        let secret = "test-secret";
        let msg_id = "msg-456";
        let timestamp = "1720000000";
        let body = b"original body";
        let sig = "v1,AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
        assert!(!verify_standard_webhook_signature(
            secret, msg_id, timestamp, body, sig
        ));
    }

    #[test]
    fn test_verify_standard_webhook_multiple_signatures() {
        let secret = "multi-test";
        let msg_id = "msg-789";
        let timestamp = "1720000000";
        let body = b"multi sig body";

        let key = decode_webhook_secret(secret);
        let mut mac = HmacSha256::new_from_slice(&key).unwrap();
        mac.update(format!("{}.{}.", msg_id, timestamp).as_bytes());
        mac.update(body);
        let result = mac.finalize().into_bytes();
        let valid_sig = format!(
            "v1,{}",
            base64::engine::general_purpose::STANDARD.encode(result)
        );

        // Multiple signatures separated by space — one valid, one garbage
        let header = format!("v1,garbage_signature {}", valid_sig);
        assert!(verify_standard_webhook_signature(
            secret, msg_id, timestamp, body, &header
        ));
    }

    #[test]
    fn test_verify_secret_token() {
        assert!(verify_secret_token("my-secret", "my-secret"));
        assert!(!verify_secret_token("my-secret", "wrong-secret"));
        assert!(!verify_secret_token("my-secret", "my-secre")); // length differs
    }

    #[test]
    fn test_decode_webhook_secret_whsec_prefix() {
        // "whsec_" + base64("test-key") = "whsec_dGVzdC1rZXk="
        let decoded = decode_webhook_secret("whsec_dGVzdC1rZXk=");
        assert_eq!(decoded, b"test-key");
    }

    #[test]
    fn test_decode_webhook_secret_plain() {
        let decoded = decode_webhook_secret("my-plain-secret");
        assert_eq!(decoded, b"my-plain-secret");
    }

    #[test]
    fn test_decode_webhook_secret_invalid_base64_falls_back() {
        // Invalid base64 after whsec_ prefix — falls back to raw bytes
        let decoded = decode_webhook_secret("whsec_!!!invalid!!!");
        assert_eq!(decoded, b"whsec_!!!invalid!!!");
    }

    #[test]
    fn test_github_validate_event_accepts_without_secret() {
        let forge = GitHubForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "pull_request".parse().unwrap());
        let body = Bytes::from("{}");
        assert!(forge.validate_event(&headers, &body, None).is_ok());
    }

    #[test]
    fn test_github_validate_event_rejects_missing_signature() {
        let forge = GitHubForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "pull_request".parse().unwrap());
        let body = Bytes::from("{}");
        assert_eq!(
            forge
                .validate_event(&headers, &body, Some("my-secret"))
                .unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn test_gitlab_validate_event_accepts_legacy_token() {
        let forge = GitLabForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-event", "Merge Request Hook".parse().unwrap());
        headers.insert("x-gitlab-token", "shared-secret".parse().unwrap());
        let body = Bytes::from("{}");
        assert!(
            forge
                .validate_event(&headers, &body, Some("shared-secret"))
                .is_ok()
        );
    }

    #[test]
    fn test_gitlab_validate_event_rejects_wrong_token() {
        let forge = GitLabForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-event", "Merge Request Hook".parse().unwrap());
        headers.insert("x-gitlab-token", "wrong-token".parse().unwrap());
        let body = Bytes::from("{}");
        assert_eq!(
            forge
                .validate_event(&headers, &body, Some("correct-token"))
                .unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn test_gitlab_validate_event_rejects_no_auth_headers() {
        let forge = GitLabForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-event", "Merge Request Hook".parse().unwrap());
        let body = Bytes::from("{}");
        assert_eq!(
            forge
                .validate_event(&headers, &body, Some("my-secret"))
                .unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn test_gitlab_validate_event_accepts_without_secret() {
        let forge = GitLabForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-event", "Merge Request Hook".parse().unwrap());
        let body = Bytes::from("{}");
        assert!(forge.validate_event(&headers, &body, None).is_ok());
    }

    #[test]
    fn test_loggable_passes_plain_values() {
        assert_eq!(loggable("pull_request"), "pull_request");
        assert_eq!(loggable("Merge Request Hook"), "Merge Request Hook");
        assert_eq!(loggable("github"), "github");
    }

    #[test]
    fn test_loggable_rejects_forged_log_lines() {
        assert_eq!(loggable("ping\nRejected nothing"), "(invalid)");
        assert_eq!(loggable("ping\r\n"), "(invalid)");
        assert_eq!(loggable(""), "(invalid)");
        assert_eq!(loggable(&"a".repeat(MAX_LOGGED_LEN + 1)), "(invalid)");
    }

    #[test]
    fn test_event_label_reports_the_event_header() {
        let mut headers = HeaderMap::new();
        assert_eq!(event_label(&headers), "(none)");

        headers.insert("x-github-event", "ping".parse().unwrap());
        assert_eq!(event_label(&headers), "ping");

        let mut gitlab = HeaderMap::new();
        gitlab.insert("x-gitlab-event", "Merge Request Hook".parse().unwrap());
        assert_eq!(event_label(&gitlab), "Merge Request Hook");
    }

    /// Build the header value GitHub sends for a given body and secret.
    fn github_signature(secret: &str, body: &[u8]) -> String {
        use std::fmt::Write;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let mut hex = String::from("sha256=");
        for b in mac.finalize().into_bytes() {
            let _ = write!(hex, "{:02x}", b);
        }
        hex
    }

    #[test]
    fn test_github_validate_event_classifies_ping_as_handshake() {
        let forge = GitHubForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "ping".parse().unwrap());
        let body = Bytes::from(r#"{"zen":"Design for failure."}"#);
        assert_eq!(
            forge.validate_event(&headers, &body, None).unwrap(),
            ForgeEvent::Handshake
        );
    }

    #[test]
    fn test_github_validate_event_classifies_pull_request_as_change_request() {
        let forge = GitHubForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "pull_request".parse().unwrap());
        let body = Bytes::from("{}");
        assert_eq!(
            forge.validate_event(&headers, &body, None).unwrap(),
            ForgeEvent::ChangeRequest
        );
    }

    #[test]
    fn test_github_validate_event_accepts_signed_ping() {
        let forge = GitHubForge;
        let body = Bytes::from(r#"{"zen":"Design for failure."}"#);
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "ping".parse().unwrap());
        headers.insert(
            "x-hub-signature-256",
            github_signature("my-secret", &body).parse().unwrap(),
        );
        assert_eq!(
            forge
                .validate_event(&headers, &body, Some("my-secret"))
                .unwrap(),
            ForgeEvent::Handshake
        );
    }

    #[test]
    fn test_github_validate_event_rejects_unsigned_ping() {
        // A handshake is acknowledged with 200, so it must prove the sender
        // just like any other event when a secret is configured.
        let forge = GitHubForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "ping".parse().unwrap());
        let body = Bytes::from(r#"{"zen":"Design for failure."}"#);
        assert_eq!(
            forge
                .validate_event(&headers, &body, Some("my-secret"))
                .unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn test_github_validate_event_rejects_unrelated_event() {
        let forge = GitHubForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", "push".parse().unwrap());
        let body = Bytes::from("{}");
        assert_eq!(
            forge.validate_event(&headers, &body, None).unwrap_err(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_gitlab_validate_event_rejects_ping() {
        let forge = GitLabForge;
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-event", "ping".parse().unwrap());
        let body = Bytes::from("{}");
        assert_eq!(
            forge.validate_event(&headers, &body, None).unwrap_err(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn test_github_review_intent_accepts_commit_changing_actions() {
        let forge = GitHubForge;
        for action in ["opened", "reopened", "synchronize", "ready_for_review"] {
            assert_eq!(
                forge.review_intent(action),
                ReviewIntent::Review,
                "{action} should be reviewed"
            );
        }
    }

    #[test]
    fn test_github_review_intent_skips_metadata_actions() {
        let forge = GitHubForge;
        for action in [
            "labeled",
            "unlabeled",
            "edited",
            "assigned",
            "review_requested",
            "closed",
            "converted_to_draft",
            // An action GitHub has not invented yet is skipped rather than
            // charged a review on the guess that it moved the head commit.
            "some_future_action",
        ] {
            assert_eq!(
                forge.review_intent(action),
                ReviewIntent::Skip,
                "{action} should be skipped"
            );
        }
    }

    #[test]
    fn test_gitlab_review_intent_reviews_every_merge_request_hook() {
        let forge = GitLabForge;
        assert_eq!(forge.review_intent("merge_request"), ReviewIntent::Review);
    }

    #[test]
    fn test_extract_repo_name_from_mr_url() {
        assert_eq!(
            extract_repo_name_from_mr_url("https://gitlab.com/org/repo/-/merge_requests/10"),
            Some("repo".to_string())
        );
        assert_eq!(
            extract_repo_name_from_mr_url("https://github.com/sashiko-dev/sashiko/pull/501"),
            Some("sashiko".to_string())
        );
        assert_eq!(
            extract_repo_name_from_mr_url("https://example.com/not-a-pr"),
            None
        );
    }

    #[test]
    fn test_extract_owner_repo_from_mr_url() {
        assert_eq!(
            extract_owner_repo_from_mr_url("https://github.com/sashiko-dev/sashiko/pull/501"),
            Some("sashiko-dev/sashiko".to_string())
        );
        assert_eq!(
            extract_owner_repo_from_mr_url("https://gitlab.com/org/sub/repo/-/merge_requests/10"),
            Some("org/sub/repo".to_string())
        );
        assert_eq!(
            extract_owner_repo_from_mr_url("https://example.com/not-a-pr"),
            None
        );
    }

    #[test]
    fn test_compose_pr_review_comment_clean() {
        let patches = vec![
            PatchReviewSummaryItem {
                part_index: 1,
                total_parts: 2,
                commit_id: Some("1234567890abcdef1234567890abcdef12345678".to_string()),
                subject: "first clean patch".to_string(),
                inline_review: None,
            },
            PatchReviewSummaryItem {
                part_index: 2,
                total_parts: 2,
                commit_id: Some("fedcba0987654321fedcba0987654321fedcba09".to_string()),
                subject: "second clean patch".to_string(),
                inline_review: Some("No issues found.".to_string()),
            },
        ];
        let body = compose_pr_review_comment(
            Some(1),
            2,
            None,
            &patches,
            "https://sashiko.sashiko.dev/#/patchset/mr-501-abc..def",
        );
        assert!(body.starts_with("### Sashiko review\n\n"));
        assert!(body.contains("✓ **No issues found** across 2 commits."));
        assert!(
            body.contains(
                "[Full review log on sashiko.sashiko.dev](https://sashiko.sashiko.dev/#/patchset/mr-501-abc..def)"
            )
        );
    }

    #[test]
    fn test_compose_pr_review_comment_with_findings_and_version() {
        let patches = vec![
            PatchReviewSummaryItem {
                part_index: 1,
                total_parts: 2,
                commit_id: Some("1234567890abcdef1234567890abcdef12345678".to_string()),
                subject: "clean commit".to_string(),
                inline_review: None,
            },
            PatchReviewSummaryItem {
                part_index: 2,
                total_parts: 2,
                commit_id: Some("fedcba0987654321fedcba0987654321fedcba09".to_string()),
                subject: "commit with bug".to_string(),
                inline_review: Some("Severity: HIGH\nMissing check on input buffer.".to_string()),
            },
        ];
        let body = compose_pr_review_comment(
            Some(3),
            2,
            Some("Series summary text here."),
            &patches,
            "https://sashiko.sashiko.dev/#/patchset/mr-501-v3",
        );
        assert!(body.starts_with("### Sashiko review — v3\n\n"));
        assert!(body.contains("<details>\n<summary>Series summary</summary>\n\nSeries summary text here.\n\n</details>"));
        assert!(!body.contains("clean commit"));
        assert!(body.contains("#### Commit 2/2 — `fedcba09` commit with bug"));
        assert!(body.contains("Severity: HIGH\nMissing check on input buffer."));
        assert!(
            body.contains(
                "[Full review and stage logs on sashiko.sashiko.dev](https://sashiko.sashiko.dev/#/patchset/mr-501-v3)"
            )
        );
    }

    #[test]
    fn test_mint_github_app_jwt_valid_rsa_key() {
        let test_pem = "-----BEGIN PRIVATE KEY-----\n\
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDpmekVGfs9pOaJ\n\
Fwiha1czG7AKrJOV3vMY5TOTpUdS78e9K0x5aq7GiVesTQE+SWpL6Z+YJ34kfP7F\n\
IMYJdyWe2i8nZIunMsZixNr9ljiGC19FZZeUelgWf5YyocJ3JfJfuqYnif0gZuUL\n\
HevyBwEItFXMmj6ZGVCTvHHeo8g4eSqvwCcD7c1TfcRgHc/hy82+nALsuifyvxjo\n\
vkXNKsJCm+W/FEhsXNxQ0Ros4H8BcDpJw1HI2XHJHcX+ouonEjTzMX7lzxdvbNp6\n\
XbPf1ANJJPtq/KVjNFTUrrMeLdph0J0KDJtDK3bK/Uls+2VS12YKQHJs0ylq05l8\n\
Bavic5wZAgMBAAECggEAGDKYkaur+h9iFK1NeDsVlfZg7pogfOkn/ATcqjH4CL/3\n\
IWyiHVRPCm3LpnjLj4Isqp8RUxeJhN9rHKG1IeHfrxbMGk6F+38noa+L57IZ5M4P\n\
blwvBB2wO5Ride2KUVadnAufjn+dYtqFu028KnP4nXLguF2kl638ShwTx4uQ/+21\n\
M3SYoKl+FQCUs4kts1pYgzGa3NpV0LCJnfGz1eGwajO2saYLCBaimrX/hprexzmm\n\
Th2ITsZab75tDaMKbpyVZIfN3wOCxthV9byXOIFndVaDmzrghzPBL1iGKWcMCnMT\n\
onlB27jVXnrGlTGkVLH9qKQ7Z6Lyj9GV6xddx9oQSQKBgQD2ayKONlQGwzLUB8s5\n\
eKRT6d/uhTl9WeNX0KXAsiYjDH8vXZqDR2YUPs3hSqABpQuH+EEeF8h/5BMmq/da\n\
M9+sZyqsH/B27N5JbnhseOnYccd+4PsfRISEwkLa9DkKS2tQZy2A1lnLWo6TAe37\n\
8bq2LXAPZiOeEEc7GCt0yVDpfwKBgQDyrzEDrQPJr/6MvHWi4qDumSYK2+fXNP1B\n\
FqnP3L09C0TTvj1Ia5HLHJ/CeiHq6suC8D+QrDbiu01DbyACBeNq9y2fzv5cixZx\n\
V58aNZd7w45SqmSieYgnCi+z/YN6VDkTNVp6Pyn4ZK/I8oxWAkWm1q8ZjAoxBaua\n\
OQQKVAtWZwKBgQCSB8+Eo6GMGGWoza2bs2j+6ZxxR7ZYGMrnoZh455o+Lwu4UCpf\n\
HhLacJWlq4nDL8HzpCVC5ilF0S2gP0zowdEN5F2ff5YLhDf/IF5xOf6q7FKjWES5\n\
tOsrmcvw4cZj2WoRTfPjZCP2pQXVDNGx+wEBMVA1b/wvkcoEtUAbh6pRlQKBgQDe\n\
uK2xA+4AAYcJvkPv0zGDCAaD3MHvHfB29ceuvpTmGxt1gJhZiG9rCsAMCW5rXESd\n\
zMNpkMNmXiNQigHEGYdXObYjfiKu5+8W4iVgNmLp8NUDROHKwuKTgaO5+iXZ9MXU\n\
vRhmLOXl0vII56CnpropnclhFsabquqMRVtR50PobQKBgBhRj6Wi9QDQ/6/kk5VG\n\
2evCz2SRP0Mua25y3+gNDNcjfIVaiQdCd5lJMG3G9esdc3SJ2lbz+QQUbk1U5Jcz\n\
NS77VgBQLugIAhcS11DAtF4vd29/Jc1kDsQQ30Or5ONNGjMe0x0WN+uGRzrmLI6U\n\
bfBnKqGjJguuHd5ta5Vh5B51\n\
-----END PRIVATE KEY-----";

        let jwt = mint_github_app_jwt(4982337, test_pem).expect("should sign JWT");
        assert_eq!(jwt.split('.').count(), 3);
    }

    #[test]
    fn test_is_dependabot_author_and_github_payload() {
        assert!(is_dependabot_author("dependabot[bot]"));
        assert!(is_dependabot_author("dependabot"));
        assert_eq!(loggable("dependabot[bot]"), "dependabot[bot]");
        assert!(!is_dependabot_author("kfree"));
        assert!(!is_dependabot_author(
            "Roman Gushchin <roman.gushchin@linux.dev>"
        ));

        let body = Bytes::from(
            serde_json::json!({
                "action": "opened",
                "pull_request": {
                    "number": 42,
                    "title": "Bump tokio from 1.40.0 to 1.41.0",
                    "html_url": "https://github.com/sashiko-dev/sashiko/pull/42",
                    "user": { "login": "dependabot[bot]" },
                    "head": { "sha": "1111111111111111111111111111111111111111" },
                    "base": { "sha": "2222222222222222222222222222222222222222" }
                },
                "repository": {
                    "clone_url": "https://github.com/sashiko-dev/sashiko.git"
                }
            })
            .to_string(),
        );

        let forge = GitHubForge;
        let (_action, metadata) = forge.parse_payload(&body).expect("valid payload");
        assert_eq!(metadata.author.as_deref(), Some("dependabot[bot]"));
        assert!(metadata.author.as_deref().is_some_and(is_dependabot_author));
    }

    fn ionic() -> ForgeRepo {
        "example-org/driver-a"
            .parse()
            .expect("the repository name is valid")
    }

    const GH_BASE: &str = "0123456789abcdef0123456789abcdef01234567";
    const GH_HEAD: &str = "89abcdef0123456789abcdef0123456789abcdef";
    const GH_URL: &str = "https://github.com/example-org/driver-a/pull/967";

    /// A `gh pr view --json number,title,url,baseRefOid,headRefOid,body`
    /// answer in the shape gh prints when stdout is not a terminal: one
    /// compact object with its keys sorted, then a newline. The body is
    /// empty, as gh prints it for a pull request with no description.
    fn gh_answer(number: i64, url: &str, base: &str, head: &str) -> String {
        let answer = serde_json::json!({
            "baseRefOid": base,
            "body": "",
            "headRefOid": head,
            "number": number,
            "title": "ionic: fix the \"rx\" ring \u{00e9}",
            "url": url,
        });
        format!("{answer}\n")
    }

    fn output(raw_status: i32, stdout: &str, stderr: &str) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(raw_status),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    /// A wait status for a process that exited with `code`.
    fn exited(code: i32) -> i32 {
        code << 8
    }

    fn sh(script: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    /// What gh 2.102.0 printed, byte for byte, with stdout not a terminal,
    /// for `gh pr view 14462 --repo=github.com/cli/cli --json
    /// number,title,url,baseRefOid,headRefOid`. A public repository's pull
    /// request is recorded so that the source holds no private repository's
    /// data; the title's quotes show gh's JSON escaping. It was recorded
    /// before body was asked for, so it also stands for an answer with no
    /// body key, which reads as no description.
    const GH_RECORDED_ANSWER: &str = r#"{"baseRefOid":"9eb4db2ba89017ea9dbcaed4f712dc4881f7f37b","headRefOid":"189706bccb1686c68fb73c5bd9013358320a9e71","number":14462,"title":"Revert \"Use internal Apple codesign action\"","url":"https://github.com/cli/cli/pull/14462"}
"#;

    #[test]
    fn gh_output_is_parsed() {
        let cli: ForgeRepo = "cli/cli".parse().expect("a valid name");
        let resolved = parse_gh_pull_request(GH_RECORDED_ANSWER.as_bytes(), &cli, 14462)
            .expect("the recorded answer describes the pull request asked for");
        assert_eq!(
            resolved,
            ResolvedPullRequest {
                repo: cli,
                number: 14462,
                url: "https://github.com/cli/cli/pull/14462".to_string(),
                title: Some("Revert \"Use internal Apple codesign action\"".to_string()),
                body: None,
                base: CommitId::parse("9eb4db2ba89017ea9dbcaed4f712dc4881f7f37b")
                    .expect("a full commit id"),
                head: CommitId::parse("189706bccb1686c68fb73c5bd9013358320a9e71")
                    .expect("a full commit id"),
            }
        );

        let untitled = serde_json::json!({
            "baseRefOid": GH_BASE,
            "headRefOid": GH_HEAD,
            "number": 967,
            "url": GH_URL,
        })
        .to_string();
        let resolved =
            parse_gh_pull_request(untitled.as_bytes(), &ionic(), 967).expect("a title is optional");
        assert_eq!(resolved.title, None);

        let enterprise: ForgeRepo = "ghe.example.com/team/repo".parse().expect("a valid name");
        let answer = gh_answer(
            5,
            "https://ghe.example.com/team/repo/pull/5",
            GH_BASE,
            GH_HEAD,
        );
        assert!(parse_gh_pull_request(answer.as_bytes(), &enterprise, 5).is_ok());
    }

    /// gh's body is read by the same rule as a webhook's description:
    /// trimmed, with an empty, null, absent or non-string one as none, and
    /// an oversized one capped. The queued metadata carries what was read,
    /// so a revision reviewed by number records its description as a
    /// webhook-driven one does.
    #[test]
    fn gh_reads_the_description_as_a_webhook_does() {
        let with_body = |body: serde_json::Value| {
            let mut answer: serde_json::Value =
                serde_json::from_str(&gh_answer(967, GH_URL, GH_BASE, GH_HEAD))
                    .expect("gh_answer prints JSON");
            answer["body"] = body;
            parse_gh_pull_request(answer.to_string().as_bytes(), &ionic(), 967)
                .expect("the description never decides whether gh's answer is usable")
        };
        let crlf = "\r\n## Changes\r\n\r\n- Fix the \u{00e9} ring.\r\n  ";
        let resolved = with_body(serde_json::json!(crlf));
        assert_eq!(resolved.body, request_description(&serde_json::json!(crlf)));
        assert_eq!(
            resolved.body.as_deref(),
            Some("## Changes\r\n\r\n- Fix the \u{00e9} ring.")
        );
        assert_eq!(resolved.to_forge_metadata().pr_body, resolved.body);

        for blank in [
            serde_json::json!(""),
            serde_json::json!(" \r\n\t"),
            serde_json::Value::Null,
            serde_json::json!(42),
            serde_json::json!({"text": "not a string"}),
        ] {
            let resolved = with_body(blank.clone());
            assert_eq!(resolved.body, None, "{blank}");
            assert_eq!(resolved.to_forge_metadata().pr_body, None, "{blank}");
        }
        let absent = gh_answer(967, GH_URL, GH_BASE, GH_HEAD).replace(r#""body":"","#, "");
        assert!(!absent.contains("body"));
        let resolved = parse_gh_pull_request(absent.as_bytes(), &ionic(), 967)
            .expect("an answer without a body is usable");
        assert_eq!(resolved.body, None);

        let oversized = "\u{00e9}".repeat(MAX_DESCRIPTION_BYTES);
        let capped = with_body(serde_json::json!(oversized))
            .body
            .expect("an oversized description is capped, not dropped");
        assert!(capped.len() <= MAX_DESCRIPTION_BYTES);
        assert!(capped.ends_with(DESCRIPTION_TRUNCATED));
    }

    /// gh prints the repository's own spelling, which may differ in case from
    /// the setting's; GitHub treats the two as one repository. The forge's
    /// spelling is kept, so the clone URL a review is queued with is the one
    /// GitHub's webhook sends for the same repository.
    #[test]
    fn a_resolved_pull_request_is_queued_in_the_forge_spelling() {
        let setting: ForgeRepo = "GitHub.com/Example-Org/Driver-A"
            .parse()
            .expect("a valid name");
        let answer = gh_answer(967, GH_URL, GH_BASE, GH_HEAD);
        let resolved = parse_gh_pull_request(answer.as_bytes(), &setting, 967)
            .expect("names compare without case");
        assert_eq!(resolved.repo, ionic());
        assert_eq!(resolved.url, GH_URL);

        let metadata = resolved.to_forge_metadata();
        assert_eq!(
            metadata.repo_url.as_deref(),
            Some("https://github.com/example-org/driver-a.git")
        );
        assert_eq!(metadata.base_sha, GH_BASE);
        assert_eq!(metadata.head_sha, GH_HEAD);
        assert_eq!(metadata.pr_number, 967);
        assert_eq!(
            metadata.pr_title.as_deref(),
            Some("ionic: fix the \"rx\" ring \u{00e9}")
        );
        assert_eq!(metadata.pr_url.as_deref(), Some(GH_URL));
        assert_eq!(metadata.author, None);
        assert_eq!(metadata.pr_body, None);
        assert!(metadata.repo_url.as_deref().is_some_and(is_safe_repo_url));
    }

    #[test]
    fn resolved_pull_request_rejects() {
        let rejected =
            |answer: String| match parse_gh_pull_request(answer.as_bytes(), &ionic(), 967) {
                Err(GhError::Invalid { reason }) => reason,
                other => panic!("{answer} was accepted as {other:?}"),
            };
        let short = &GH_HEAD[..12];
        assert!(rejected(gh_answer(967, GH_URL, GH_BASE, short)).contains("head commit"));
        assert!(rejected(gh_answer(967, GH_URL, short, GH_HEAD)).contains("base commit"));
        let upper = GH_HEAD.to_uppercase();
        assert!(rejected(gh_answer(967, GH_URL, GH_BASE, &upper)).contains("head commit"));
        assert!(
            rejected(gh_answer(968, GH_URL, GH_BASE, GH_HEAD))
                .contains("pull request 968, not 967")
        );
        for url in [
            "https://github.com/example-org/driver-b/pull/967",
            "https://github.com/other/driver-a/pull/967",
            "https://gitlab.com/example-org/driver-a/pull/967",
            "https://github.com/example-org/driver-a/pull/968",
            "https://github.com/example-org/driver-a/issues/967",
            "https://github.com/example-org/driver-a/pull/967/files",
            "http://github.com/example-org/driver-a/pull/967",
            "git@github.com:example-org/driver-a.git",
            "https://user:secret@github.com/example-org/driver-a/pull/967",
            "https://github.com:8443/example-org/driver-a/pull/967",
            "https://github.com/example-org/driver-a/pull/967?x=1",
            "https://github.com/example-org/driver-a/pull/967#top",
            "not a url",
        ] {
            let reason = rejected(gh_answer(967, url, GH_BASE, GH_HEAD));
            assert!(
                reason.contains("is not https://github.com/example-org/driver-a/pull/967"),
                "{url}: {reason}"
            );
        }
        assert!(rejected("Not Found\n".to_string()).contains("no pull request JSON"));
        let no_commits = serde_json::json!({"number": 967, "url": GH_URL}).to_string();
        assert!(rejected(no_commits).contains("no pull request JSON"));
    }

    /// A value gh printed reaches a reason only within one sanitised line,
    /// so a crafted URL can neither forge a log entry nor reorder its text.
    #[test]
    fn an_invalid_answer_is_reported_on_one_safe_line() {
        let url = format!(
            "https://evil.example/\u{1b}[2J\nINFO forged\u{202E}{}",
            "x".repeat(500)
        );
        let answer = gh_answer(967, &url, GH_BASE, GH_HEAD);
        let Err(GhError::Invalid { reason }) =
            parse_gh_pull_request(answer.as_bytes(), &ionic(), 967)
        else {
            panic!("a foreign URL must be refused");
        };
        assert!(
            !reason
                .chars()
                .any(crate::finding_view::is_unsafe_for_terminal),
            "{reason:?}"
        );
        assert!(reason.ends_with("..."), "{reason:?}");
        assert_eq!(
            reason.chars().count(),
            GH_DETAIL_MAX_CHARS + 3,
            "{reason:?}"
        );
        assert!(
            reason.starts_with(
                "the pull request URL is not https://github.com/example-org/driver-a/pull/967 but "
            ),
            "{reason:?}"
        );
    }

    #[test]
    fn gh_errors_are_typed_and_sanitized() {
        assert_eq!(check_gh_status(&output(exited(0), "{}", "")), Ok(()));
        assert_eq!(
            check_gh_status(&output(
                exited(4),
                "",
                "To get started with GitHub CLI, please run:  gh auth login\n"
            )),
            Err(GhError::NotAuthenticated)
        );
        let not_found = "GraphQL: Could not resolve to a PullRequest with the number of 99999. \
                         (repository.pullRequest)";
        assert_eq!(
            check_gh_status(&output(exited(1), "", &format!("{not_found}\n"))),
            Err(GhError::Failed {
                detail: not_found.to_string()
            })
        );
        assert_eq!(
            check_gh_status(&output(
                exited(1),
                "",
                "\u{1b}[31mGraphQL:\u{202E}bad\r\nINFO forged entry\n"
            )),
            Err(GhError::Failed {
                detail: "[31mGraphQL: bad".to_string()
            })
        );
        let long = "x".repeat(1000);
        assert_eq!(
            check_gh_status(&output(exited(1), "", &long)),
            Err(GhError::Failed {
                detail: format!("{}...", "x".repeat(GH_DETAIL_MAX_CHARS))
            })
        );
        assert_eq!(
            check_gh_status(&output(exited(1), "", "\n")),
            Err(GhError::Failed {
                detail: "gh exit status: 1 with no message".to_string()
            })
        );
        // SIGKILL: no exit code, so never mistaken for status 4.
        assert!(matches!(
            check_gh_status(&output(9, "", "")),
            Err(GhError::Failed { .. })
        ));
        assert_eq!(
            GhError::Failed {
                detail: not_found.to_string()
            }
            .to_string(),
            format!("gh failed: {not_found}")
        );
    }

    #[test]
    fn gh_is_asked_for_one_pull_request_of_the_named_repository() {
        let enterprise: ForgeRepo = "ghe.example.com/team/repo".parse().expect("a valid name");
        for (repo, expected) in [
            (ionic(), "--repo=github.com/example-org/driver-a"),
            (enterprise, "--repo=ghe.example.com/team/repo"),
        ] {
            let command = gh_pr_view_command(&repo, 967);
            let command = command.as_std();
            assert_eq!(command.get_program(), "gh");
            let args: Vec<&std::ffi::OsStr> = command.get_args().collect();
            // The field list is spelled out rather than read from
            // GH_PR_FIELDS, so dropping a field (body above all, which
            // carries the cover letter) fails this test.
            assert_eq!(
                args,
                [
                    "pr",
                    "view",
                    "967",
                    expected,
                    "--json",
                    "number,title,url,baseRefOid,headRefOid,body"
                ]
            );
            assert!(
                command
                    .get_envs()
                    .any(|(key, value)| key == "GH_PROMPT_DISABLED" && value == Some("1".as_ref()))
            );
        }
    }

    /// The refusal is in the path every resolution takes, ahead of any
    /// process: a stand-in that would answer NotAuthenticated if it ran is
    /// never asked, and GhCli refuses without running gh.
    #[tokio::test]
    async fn a_number_below_one_is_refused_before_gh_runs() {
        let repo = ionic();
        let refused = |result: &Result<ResolvedPullRequest, GhError>| {
            matches!(result, Err(GhError::Invalid { reason })
                if reason.contains("is not a pull request number"))
        };
        for number in [0, -1, i64::MIN] {
            let stand_in = resolve_with(sh("exit 4"), GH_TIME_LIMIT, &repo, number).await;
            assert!(refused(&stand_in), "{number}: {stand_in:?}");
            let gh = GhCli.resolve(&repo, number).await;
            assert!(refused(&gh), "{number}: {gh:?}");
        }
        assert_eq!(check_pull_request_number(1), Ok(()));
    }

    /// The process handling, run against sh standing in for gh: gh itself is
    /// never run in tests.
    #[tokio::test]
    async fn gh_runs_are_typed() {
        let repo = ionic();
        let answer = gh_answer(967, GH_URL, GH_BASE, GH_HEAD);
        let mut printer = sh("printf '%s' \"$1\"");
        printer.arg("sh").arg(&answer);
        let resolved = resolve_with(printer, GH_TIME_LIMIT, &repo, 967)
            .await
            .expect("the printed answer resolves");
        assert_eq!(resolved.head.as_str(), GH_HEAD);

        let resolve = |command| resolve_with(command, GH_TIME_LIMIT, &repo, 967);
        assert_eq!(resolve(sh("exit 4")).await, Err(GhError::NotAuthenticated));
        assert_eq!(
            resolve(sh(
                "echo 'HTTP 502: Bad Gateway' >&2; echo more >&2; exit 1"
            ))
            .await,
            Err(GhError::Failed {
                detail: "HTTP 502: Bad Gateway".to_string()
            })
        );
        let missing = tokio::process::Command::new("/nonexistent/sashiko-test/gh");
        assert_eq!(resolve(missing).await, Err(GhError::NotInstalled));
    }

    /// A gh that does not answer in time is reported as TimedOut and is
    /// stopped rather than left running: the stand-in records its process id,
    /// and that process must be gone, or a zombie waiting to be reaped, soon
    /// after the wait is given up.
    #[tokio::test]
    async fn a_stuck_gh_is_timed_out_and_stopped() {
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let pid_file = scratch.path().join("gh.pid");
        let mut stuck = sh("echo $$ > \"$1\"; exec sleep 30");
        stuck.arg("sh").arg(&pid_file);
        let started = std::time::Instant::now();
        let result = resolve_with(stuck, Duration::from_secs(2), &ionic(), 967).await;
        assert_eq!(result, Err(GhError::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(20));

        let pid = std::fs::read_to_string(&pid_file).expect("the stand-in recorded its pid");
        let pid = pid.trim();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while process_is_running(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "the stand-in {pid} was left running"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Whether `pid` names a live process. A zombie has already stopped and
    /// only waits to be reaped.
    fn process_is_running(pid: &str) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(") ")
                .is_some_and(|(_, fields)| !fields.starts_with('Z'))
        })
    }
}
