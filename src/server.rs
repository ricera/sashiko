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

use crate::access::{BugAccess, OptionalPrincipal, Principal, SectionTitle};
use crate::api::*;
use crate::baseline::CommitId;
use crate::db::Database;
use crate::events::{Event, MessageSource};
use crate::fetcher::FetchRequest;
use crate::mbox::LoreMboxClient;
use axum::{
    Json, Router,
    extract::{ConnectInfo, Path, Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Redirect},
    routing::{get, get_service, post},
};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower_http::services::{ServeDir, ServeFile};
use tracing::{error, info, warn};

use std::time::{Duration, Instant};
use tokio::sync::RwLock;

struct CachedValue<T> {
    value: T,
    timestamp: Instant,
}

struct AsyncCache<T> {
    inner: RwLock<Option<CachedValue<T>>>,
    ttl: Duration,
}

impl<T: Clone> AsyncCache<T> {
    fn new(ttl: Duration) -> Self {
        Self {
            inner: RwLock::new(None),
            ttl,
        }
    }

    async fn get_or_fetch<F, Fut, E>(&self, fetch: F) -> Result<T, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
    {
        if let Some(cached) = self.inner.read().await.as_ref()
            && cached.timestamp.elapsed() < self.ttl
        {
            return Ok(cached.value.clone());
        }

        let mut write_guard = self.inner.write().await;
        if let Some(cached) = write_guard.as_ref()
            && cached.timestamp.elapsed() < self.ttl
        {
            return Ok(cached.value.clone());
        }

        let value = fetch().await?;
        *write_guard = Some(CachedValue {
            value: value.clone(),
            timestamp: Instant::now(),
        });
        Ok(value)
    }

    async fn invalidate(&self) {
        *self.inner.write().await = None;
    }
}

struct AsyncMapCache<K, V> {
    inner: RwLock<std::collections::HashMap<K, CachedValue<V>>>,
    ttl: Duration,
}

impl<K: std::hash::Hash + Eq + Clone, V: Clone> AsyncMapCache<K, V> {
    fn new(ttl: Duration) -> Self {
        Self {
            inner: RwLock::new(std::collections::HashMap::new()),
            ttl,
        }
    }

    async fn get_or_fetch<F, Fut, E>(&self, key: K, fetch: F) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<V, E>>,
    {
        if let Some(cached) = self.inner.read().await.get(&key)
            && cached.timestamp.elapsed() < self.ttl
        {
            return Ok(cached.value.clone());
        }

        let mut write_guard = self.inner.write().await;
        if let Some(cached) = write_guard.get(&key)
            && cached.timestamp.elapsed() < self.ttl
        {
            return Ok(cached.value.clone());
        }

        let value = fetch().await?;
        write_guard.insert(
            key,
            CachedValue {
                value: value.clone(),
                timestamp: Instant::now(),
            },
        );
        Ok(value)
    }
}

/// In-process rate limiter for sign-in link requests.
///
/// Limits:
/// - Per address: 3 requests per 15 minutes, 10 requests per 24 hours (caps mail bombs)
/// - Per source IP: 10 requests per 15 minutes (caps enumeration sweeps)
/// - Global: 100 requests per 1 hour (caps total outbound login mail)
#[derive(Default)]
pub struct SignInLinkRateLimiter {
    state: std::sync::Mutex<SignInLinkRateLimiterState>,
}

#[derive(Default)]
struct SignInLinkRateLimiterState {
    per_address: std::collections::HashMap<String, Vec<Instant>>,
    per_ip: std::collections::HashMap<String, Vec<Instant>>,
    global: Vec<Instant>,
}

impl SignInLinkRateLimiter {
    pub fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(SignInLinkRateLimiterState::default()),
        }
    }

    /// Checks if a request is allowed by all rate-limit buckets.
    /// If allowed, records the attempt in all buckets and returns true.
    /// If any limit is exceeded, returns false without recording an outbound grant.
    pub fn check_and_record(&self, email: &str, client_ip: &str) -> bool {
        let now = Instant::now();
        let fifteen_mins = Duration::from_secs(15 * 60);
        let one_hour = Duration::from_secs(60 * 60);
        let one_day = Duration::from_secs(24 * 60 * 60);

        let mut guard = match self.state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        // 1. Per client IP: 10 per 15 minutes
        let ip_attempts = guard.per_ip.entry(client_ip.to_string()).or_default();
        ip_attempts.retain(|&t| now.saturating_duration_since(t) < fifteen_mins);
        if ip_attempts.len() >= 10 {
            return false;
        }

        // 2. Per address: 3 per 15 minutes, 10 per day
        let normalized_email = email.trim().to_lowercase();
        if !normalized_email.is_empty() {
            let addr_attempts = guard
                .per_address
                .entry(normalized_email.clone())
                .or_default();
            addr_attempts.retain(|&t| now.saturating_duration_since(t) < one_day);
            let in_15m = addr_attempts
                .iter()
                .filter(|&&t| now.saturating_duration_since(t) < fifteen_mins)
                .count();
            if in_15m >= 3 || addr_attempts.len() >= 10 {
                return false;
            }
        }

        // 3. Global: 100 per hour
        guard
            .global
            .retain(|&t| now.saturating_duration_since(t) < one_hour);
        if guard.global.len() >= 100 {
            return false;
        }

        // Passed all limits; record the attempt
        guard
            .per_ip
            .entry(client_ip.to_string())
            .or_default()
            .push(now);
        if !normalized_email.is_empty() {
            guard
                .per_address
                .entry(normalized_email)
                .or_default()
                .push(now);
        }
        guard.global.push(now);

        true
    }
}

/// What the router needs to know about the process it runs in.
///
/// These are facts about this invocation rather than configuration, which is
/// why they arrive separately from Settings. They are grouped because passing
/// them positionally made three consecutive bools that no call site could be
/// read against.
#[derive(Clone)]
pub struct ServerOptions {
    /// Accepts mutations from any address without authentication. Unsafe, and
    /// only set by an explicit command line flag.
    pub allow_all_submit: bool,
    pub smtp_enabled: bool,
    pub dry_run: bool,
    /// The credential this process published for local tooling to present.
    ///
    /// Absent when the token could not be written, in which case local tools
    /// authenticate the same way anyone else does.
    pub local_token: Option<crate::auth::LocalToken>,
    /// Shared UNIX timestamp updated on each iteration of `EmailWorker::run`.
    pub email_worker_heartbeat: Option<Arc<std::sync::atomic::AtomicI64>>,
    /// What running fetches and reviews are doing, shared with the reviewer
    /// and the fetcher that report into it. The default is an empty registry
    /// with nothing reporting, which is what a router built without a daemon
    /// behind it should serve.
    pub activity: Arc<crate::activity::ActivityRegistry>,
    /// What turns a pull request number into its commits for
    /// /api/pr/review. The daemon passes gh. The default resolves nothing,
    /// so that a router built without a daemon behind it, as every test's
    /// is, can never run gh or reach the network.
    pub pull_request_resolver: Arc<dyn crate::forge::PullRequestResolver>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            allow_all_submit: false,
            smtp_enabled: false,
            dry_run: false,
            local_token: None,
            activity: Arc::default(),
            pull_request_resolver: Arc::new(crate::forge::NoResolver),
            email_worker_heartbeat: None,
        }
    }
}

pub struct AppState {
    pub settings: Arc<crate::settings::Settings>,
    pub db: Arc<Database>,
    pub sender: mpsc::Sender<Event>,
    pub fetch_sender: mpsc::Sender<FetchRequest>,
    pub forge_registry: Arc<crate::forge::ForgeRegistry>,
    pub read_only: bool,
    pub allow_all_submit: bool,
    pub smtp_enabled: bool,
    pub dry_run: bool,
    pub local_token: Option<crate::auth::LocalToken>,
    pub email_worker_heartbeat: Option<Arc<std::sync::atomic::AtomicI64>>,
    pub activity: Arc<crate::activity::ActivityRegistry>,
    pub pull_request_resolver: Arc<dyn crate::forge::PullRequestResolver>,
    pub sign_in_link_rate_limiter: SignInLinkRateLimiter,
    stats_timeline_cache: AsyncMapCache<Option<i64>, serde_json::Value>,
    stats_reviews_cache: AsyncCache<serde_json::Value>,
    stats_tools_cache: AsyncCache<serde_json::Value>,
    messages_count_cache: AsyncCache<usize>,
    patchsets_count_cache: AsyncCache<usize>,
    patchsets_homepage_cache: AsyncCache<Vec<crate::db::PatchsetRow>>,
    messages_homepage_cache: AsyncCache<Vec<crate::db::MessageRow>>,
    bug_subsystems_cache: AsyncMapCache<Option<String>, Vec<serde_json::Value>>,
}

async fn redirect_www(req: Request, next: Next) -> impl IntoResponse {
    if let Some(host) = req.headers().get("host").and_then(|h| h.to_str().ok()) {
        let host_without_port = host.split(':').next().unwrap_or("");
        if host_without_port == "www.sashiko.dev" {
            let uri = req.uri();
            let new_uri = format!(
                "https://sashiko.dev{}{}",
                uri.path(),
                uri.query().map(|q| format!("?{}", q)).unwrap_or_default()
            );
            return Redirect::permanent(&new_uri).into_response();
        }
    }
    next.run(req).await
}

/// Build the API router with all routes and shared state.
///
/// Extracted from [`run_server`] so that integration tests can construct the
/// router independently (e.g. bind to port 0 for random-port testing).
pub fn build_router(
    settings: Arc<crate::settings::Settings>,
    db: Arc<Database>,
    sender: mpsc::Sender<Event>,
    fetch_sender: mpsc::Sender<FetchRequest>,
    options: ServerOptions,
) -> Router {
    let forge_registry = Arc::new(crate::forge::ForgeRegistry::new());
    let read_only = settings.server.read_only;

    let state = Arc::new(AppState {
        settings: settings.clone(),
        db,
        sender,
        fetch_sender,
        read_only,
        forge_registry,
        allow_all_submit: options.allow_all_submit,
        smtp_enabled: options.smtp_enabled,
        dry_run: options.dry_run,
        local_token: options.local_token,
        email_worker_heartbeat: options.email_worker_heartbeat,
        activity: options.activity,
        pull_request_resolver: options.pull_request_resolver,
        sign_in_link_rate_limiter: SignInLinkRateLimiter::new(),
        stats_timeline_cache: AsyncMapCache::new(Duration::from_secs(60)),
        stats_reviews_cache: AsyncCache::new(Duration::from_secs(60)),
        stats_tools_cache: AsyncCache::new(Duration::from_secs(60)),
        messages_count_cache: AsyncCache::new(Duration::from_secs(30)),
        patchsets_count_cache: AsyncCache::new(Duration::from_secs(30)),
        patchsets_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
        messages_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
        bug_subsystems_cache: AsyncMapCache::new(Duration::from_secs(5)),
    });

    Router::new()
        .route("/health", get(health_check))
        .route("/api/health", get(health_check))
        .route("/api/config", get(get_config))
        .route("/api/lists", get(list_mailing_lists))
        .route("/api/patchsets", get(list_patchsets))
        .route("/api/messages", get(list_messages))
        .route("/api/patch", get(get_patchset))
        .route("/api/patchset", get(get_patchset_summary))
        .route("/api/message", get(get_message))
        .route("/api/review", get(get_review))
        .route("/api/review_log", get(get_review_log))
        // Read-only and public, like the patchset and review routes beside
        // them. They carry progress metadata only -- stage and tool names, turn
        // counts, tool call counts per patch with the names of the references
        // attached to it, redacted remote URLs, one-line failure reasons with
        // any quoted model output removed (`activity::public_reason`) -- and no
        // review text, so they need no capability, embargoed patchsets
        // included. Reference names come from the operator's configuration,
        // and which are attached follows from the patch's file list, which
        // the patchset page already shows. That has to be revisited if live
        // conversation text is ever added to these payloads. The live
        // conversation itself is kept out by `activity::progress_update`,
        // which discards every `LogEntry`, so it never reaches the
        // registry these read; it is served as `live_log` from the review
        // routes above instead.
        .route("/api/activity", get(get_activity))
        .route("/api/patchset/activity", get(get_patchset_activity))
        .route("/api/stats", get(get_stats))
        .route("/api/stats/timeline", get(stats_timeline))
        .route("/api/stats/reviews", get(stats_reviews))
        .route("/api/stats/tools", get(stats_tools))
        .route("/api/submit", post(submit_patch))
        .route("/api/auth/request-link", post(request_link))
        .route("/api/auth/verify", get(verify_link))
        .route("/api/auth/refresh", post(refresh_token))
        .route("/api/auth/token", post(create_auth_token))
        .route("/api/patchset/rerun", post(rerun_patchset))
        .route(
            "/api/patchset/release-embargo",
            post(release_patchset_embargo),
        )
        .route("/api/patchset/cancel", post(cancel_patchset))
        .route("/api/patch/rerun", post(rerun_patch))
        .route("/api/pr/review", post(review_pull_request))
        .route("/api/bug", get(get_bug))
        .route("/api/bugs", get(list_bugs))
        .route("/api/bugs/subsystems", get(list_bug_subsystems))
        .route("/api/subsystems", get(list_bug_subsystems))
        .route("/api/bug/logs", get(get_bug_logs))
        .route("/api/bug/raw", get(get_bug_raw))
        .route("/api/bug/input", get(get_bug_input))
        .route("/api/bug/enrichments", get(get_bug_enrichments))
        .route("/api/bug/analyze", post(analyze_bug))
        .route("/api/bug/action", post(bug_action))
        .route("/bug/{bugid}", get(redirect_bug))
        .route("/api/webhook/{provider}", post(forge_webhook))
        .route("/", get_service(ServeFile::new("static/index.html")))
        .route(
            "/auth/verify",
            get_service(ServeFile::new("static/index.html")),
        )
        .nest_service("/static", ServeDir::new("static"))
        .layer(middleware::from_fn(redirect_www))
        .layer(axum::extract::DefaultBodyLimit::max(25 * 1024 * 1024))
        .with_state(state)
}

pub async fn run_server(
    settings: Arc<crate::settings::Settings>,
    db: Arc<Database>,
    sender: mpsc::Sender<Event>,
    fetch_sender: mpsc::Sender<FetchRequest>,
    options: ServerOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let app = build_router(settings.clone(), db, sender, fetch_sender, options);

    let bind_addr = format!("{}:{}", settings.server.host, settings.server.port);
    let addrs: Vec<SocketAddr> = bind_addr
        .to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("invalid bind address '{}': {}", bind_addr, e))?
        .collect();
    info!("Web API listening on {:?}", addrs);

    let listener = TcpListener::bind(addrs.as_slice()).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

fn normalize_thread_message_id(message_id: &str) -> String {
    message_id.trim_matches(['<', '>']).to_string()
}

fn generate_synthetic_id(prefix: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let start = SystemTime::now();
    let since_the_epoch = start
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards");
    // e.g. sashiko-local-1715890000-12345
    format!(
        "sashiko-{}-{}-{}@sashiko.local",
        prefix,
        since_the_epoch.as_secs(),
        fastrand::u32(..)
    )
}

async fn submit_patch(
    auth: crate::auth::OptionalAuthUser,
    // Logged on a refusal to give an operator something to grep for. It is
    // deliberately not passed to is_authorized: where a request comes from
    // decides nothing.
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<SubmitRequest>,
) -> Result<Json<SubmitResponse>, StatusCode> {
    if state.read_only {
        return Err(StatusCode::FORBIDDEN);
    }

    if !is_authorized(
        &state,
        &headers,
        auth.0.as_ref(),
        crate::settings::Permission::Ingest,
    ) {
        info!("Refused unauthorized patch submission from {}", addr);
        return Err(StatusCode::FORBIDDEN);
    }

    match payload {
        SubmitRequest::Inject {
            raw,
            base_commit,
            skip_subjects,
            only_subjects,
        } => {
            // A baseline that names no commit would be resolved as a ref,
            // or reach git and the review log verbatim.
            if raw.trim().is_empty()
                || base_commit
                    .as_deref()
                    .is_some_and(|b| CommitId::parse(b).is_none())
            {
                return Err(StatusCode::BAD_REQUEST);
            }
            // Basic guardrail
            if !raw.contains("From ") && !raw.contains("Subject:") {
                return Err(StatusCode::BAD_REQUEST);
            }

            let id = generate_synthetic_id("inject");
            info!("Received raw mbox injection: {} (len: {})", id, raw.len());

            let submitted_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .ok();

            let event = Event::RawMboxSubmitted {
                raw,
                submission_id: id.clone(),
                source: MessageSource::ApiInject,
                group: "api-submit".to_string(),
                baseline: base_commit,
                skip_subjects,
                only_subjects,
                submitted_at,
            };

            if let Err(e) = state.sender.send(event).await {
                error!("Failed to send raw mbox to queue: {}", e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }

            Ok(Json(SubmitResponse {
                status: "accepted".to_string(),
                id,
            }))
        }
        SubmitRequest::Remote {
            sha,
            repo,
            skip_subjects,
            only_subjects,
            b4_cover_letter,
        }
        | SubmitRequest::RemoteRange {
            sha,
            repo,
            skip_subjects,
            only_subjects,
            b4_cover_letter,
        } => {
            let has_invalid_ref = sha.trim_matches('.').is_empty()
                || sha.chars().any(char::is_whitespace)
                || sha.split("..").count() > 2
                || sha.split("..").any(|part| {
                    let part = part.trim_start_matches('.');
                    part.starts_with('-') || part.starts_with('+') || part.contains(':')
                });
            if has_invalid_ref {
                return Err(StatusCode::BAD_REQUEST);
            }
            let id = sha.clone();
            let repo_display = repo.as_deref().unwrap_or("local");
            info!(
                "Received remote fetch request: {} from {}",
                sha, repo_display
            );

            // Optimistic check: If we already have this patchset in the DB,
            // skip creating placeholder and skip fetch queue entirely.
            match state.db.has_patchset_by_msgid(&id).await {
                Ok(true) => {
                    info!(
                        "Remote fetch request for already ingested SHA {}, skipping placeholder and fetch",
                        id
                    );
                    return Ok(Json(SubmitResponse {
                        status: "accepted".to_string(),
                        id,
                    }));
                }
                Err(e) => {
                    error!("Failed to check if patchset exists: {}", e);
                }
                _ => {}
            }

            // Create a placeholder record in the DB so the user can track status
            if let Err(e) = state
                .db
                .create_fetching_patchset(
                    &format!("{}@sashiko.local", id),
                    &format!("Fetching {} from {}...", sha, repo_display),
                    skip_subjects.as_ref(),
                    only_subjects.as_ref(),
                    None,
                    None,
                    None,
                    None,
                )
                .await
            {
                error!("Failed to create placeholder patchset: {}", e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }

            let req = FetchRequest {
                repo_url: repo,
                commit_hash: sha,
                mr_url: None,
                mr_title: None,
                mr_number: None,
                b4_cover_letter,
            };

            if let Err(e) = state.fetch_sender.send(req).await {
                error!("Failed to send fetch request to queue: {}", e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }

            Ok(Json(SubmitResponse {
                status: "accepted".to_string(),
                id,
            }))
        }
        SubmitRequest::Thread { msgid } => {
            let id = generate_synthetic_id("thread");
            // Keep the canonical message ID in application state. The Lore
            // client encodes it exactly once when constructing the URL.
            let clean_msgid = normalize_thread_message_id(&msgid);
            info!(
                "Received thread fetch request: {} (msgid: {})",
                id, clean_msgid
            );

            // Create a placeholder record in the DB so the user can track status
            if let Err(e) = state
                .db
                .create_fetching_patchset(
                    &clean_msgid,
                    &format!("Fetching thread {}...", clean_msgid),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                )
                .await
            {
                tracing::error!("Failed to create placeholder patchset: {}", e);
                // Non-fatal, just continue
            }

            let msgid_clone = clean_msgid.clone();
            let sender = state.sender.clone();

            tokio::spawn(async move {
                if let Err(e) = fetch_and_inject_thread(&msgid_clone, sender.clone()).await {
                    tracing::error!("Failed to fetch thread {}: {}", msgid_clone, e);
                    let _ = sender
                        .send(Event::IngestionFailed {
                            article_id: msgid_clone.clone(),
                            error: format!("Failed to fetch thread: {}", e),
                            source: MessageSource::ApiFetchThread,
                        })
                        .await;
                }
            });

            Ok(Json(SubmitResponse {
                status: "accepted".to_string(),
                id, // The client might expect this ID
            }))
        }
    }
}

async fn fetch_and_inject_thread(
    msgid: &str,
    sender: tokio::sync::mpsc::Sender<Event>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let raw = LoreMboxClient::new()?.fetch_thread(msgid).await?;
    let raw = tokio::task::spawn_blocking(move || String::from_utf8(raw)).await??;

    let submitted_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .ok();

    let event = Event::RawMboxSubmitted {
        raw,
        submission_id: msgid.to_string(),
        source: MessageSource::ApiFetchThread,
        group: "api-submit".to_string(),
        baseline: None,
        skip_subjects: None,
        only_subjects: None,
        submitted_at,
    };

    sender.send(event).await?;
    Ok(())
}

async fn list_mailing_lists(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    let lists = state.db.get_mailing_lists().await.map_err(|e| {
        error!("Failed to get mailing lists: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let result = lists
        .into_iter()
        .map(|(name, group)| {
            serde_json::json!({
                "name": name,
                "group": group
            })
        })
        .collect();

    Ok(Json(result))
}

async fn list_patchsets(
    auth: crate::auth::OptionalAuthUser,
    OptionalPrincipal(principal): OptionalPrincipal,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(pagination): Query<Pagination>,
) -> Result<Json<PatchsetsResponse>, StatusCode> {
    let page = pagination.page.unwrap_or(1).max(1);
    let per_page = pagination.per_page.unwrap_or(50).clamp(1, 100);
    let offset = (page - 1) * per_page;

    let mut items = if pagination.q.is_none()
        && pagination.mailing_list.is_none()
        && page == 1
        && per_page == 50
    {
        state
            .patchsets_homepage_cache
            .get_or_fetch(|| async {
                state
                    .db
                    .get_patchsets(per_page, offset, None, None)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
            })
            .await?
    } else {
        state
            .db
            .get_patchsets(
                per_page,
                offset,
                pagination.q.clone(),
                pagination.mailing_list.clone(),
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };
    populate_embargoed_patchset_findings(&state, &headers, auth.0.as_ref(), &principal, &mut items)
        .await?;

    let total = if pagination.q.is_none() && pagination.mailing_list.is_none() {
        state
            .patchsets_count_cache
            .get_or_fetch(|| async {
                state
                    .db
                    .count_patchsets(None, None)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
            })
            .await?
    } else {
        state
            .db
            .count_patchsets(pagination.q.clone(), pagination.mailing_list.clone())
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };

    Ok(Json(PatchsetsResponse {
        items,
        total,
        page,
        per_page,
    }))
}

async fn list_messages(
    State(state): State<Arc<AppState>>,
    Query(pagination): Query<Pagination>,
) -> Result<Json<MessagesResponse>, StatusCode> {
    let page = pagination.page.unwrap_or(1).max(1);
    let per_page = pagination.per_page.unwrap_or(50).clamp(1, 100);
    let offset = (page - 1) * per_page;

    let items = if pagination.q.is_none()
        && pagination.mailing_list.is_none()
        && page == 1
        && per_page == 50
    {
        state
            .messages_homepage_cache
            .get_or_fetch(|| async {
                state
                    .db
                    .get_messages(per_page, offset, None, None)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
            })
            .await?
    } else {
        state
            .db
            .get_messages(
                per_page,
                offset,
                pagination.q.clone(),
                pagination.mailing_list.clone(),
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };
    let total = if pagination.q.is_none() && pagination.mailing_list.is_none() {
        state
            .messages_count_cache
            .get_or_fetch(|| async {
                state
                    .db
                    .count_messages(None, None)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
            })
            .await?
    } else {
        state
            .db
            .count_messages(pagination.q.clone(), pagination.mailing_list.clone())
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };

    Ok(Json(MessagesResponse {
        items,
        total,
        page,
        per_page,
    }))
}

fn is_auth_blocklisted(state: &AppState, auth: Option<&crate::auth::AuthUser>) -> bool {
    if let Some(user) = auth {
        if state.settings.server.acl.is_blocklisted(&user.email) {
            return true;
        }
        if user
            .sid
            .as_deref()
            .is_some_and(|sid| state.settings.server.acl.is_blocklisted(sid))
        {
            return true;
        }
    }
    false
}

/// Resolves `(can_view_embargoed, can_release_embargo)` for an embargoed
/// patchset in a single pass.
///
/// Viewing embargoed results is a read operation: it is permitted in
/// `read_only` mode and for read-scoped API tokens (`BugAccess::Read`), whereas
/// releasing an embargo mutates state and requires `!state.read_only` and
/// `BugAccess::Manage` (or `Permission::Review`).
async fn resolve_patchset_embargo_access(
    state: &Arc<AppState>,
    headers: &axum::http::HeaderMap,
    auth: Option<&crate::auth::AuthUser>,
    principal: &Principal,
    patchset_id: i64,
) -> Result<(bool, bool), StatusCode> {
    if is_auth_blocklisted(state, auth) {
        return Ok((false, false));
    }
    if is_authorized(state, headers, auth, crate::settings::Permission::Review) {
        return Ok((true, !state.read_only));
    }
    if !principal.has_any_embargo_visibility() {
        return Ok((false, false));
    }
    if principal.has_global_embargo_visibility() {
        let can_release = !state.read_only && principal.may_release_embargo(&[]);
        return Ok((true, can_release));
    }
    let sections = state
        .db
        .authorizing_sections_for_patchset(patchset_id)
        .await
        .map_err(|e| {
            tracing::error!("Database error resolving patchset authority: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let titles: Vec<SectionTitle> = sections.iter().map(|s| SectionTitle::new(s)).collect();
    let can_view = principal.may_view_embargoed(&titles);
    let can_release = !state.read_only && principal.may_release_embargo(&titles);
    Ok((can_view, can_release))
}

async fn can_release_patchset_embargo(
    state: &Arc<AppState>,
    headers: &axum::http::HeaderMap,
    auth: Option<&crate::auth::AuthUser>,
    principal: &Principal,
    patchset_id: i64,
) -> Result<bool, StatusCode> {
    let (_, can_release) =
        resolve_patchset_embargo_access(state, headers, auth, principal, patchset_id).await?;
    Ok(can_release)
}

/// Populates `findings_low/medium/high/critical` on embargoed rows in `items`
/// when the caller is authorized to view embargoed results for those patchsets.
async fn populate_embargoed_patchset_findings(
    state: &Arc<AppState>,
    headers: &axum::http::HeaderMap,
    auth: Option<&crate::auth::AuthUser>,
    principal: &Principal,
    items: &mut [crate::db::PatchsetRow],
) -> Result<(), StatusCode> {
    if is_auth_blocklisted(state, auth) {
        return Ok(());
    }
    let now = chrono::Utc::now().timestamp();
    let embargoed_ids: Vec<i64> = items
        .iter()
        .filter(|row| {
            row.status.as_deref() == Some("Embargoed")
                || row.embargo_until.is_some_and(|until| until > now)
        })
        .map(|row| row.id)
        .collect();
    if embargoed_ids.is_empty() {
        return Ok(());
    }

    let visible_ids: std::collections::HashSet<i64> =
        if is_authorized(state, headers, auth, crate::settings::Permission::Review)
            || principal.has_global_embargo_visibility()
        {
            embargoed_ids.iter().copied().collect()
        } else if !principal.has_any_embargo_visibility() {
            return Ok(());
        } else {
            let sections = state
                .db
                .authorizing_sections_for_patchsets(&embargoed_ids)
                .await
                .map_err(|e| {
                    tracing::error!("Database error resolving patchsets authority: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
            embargoed_ids
                .iter()
                .copied()
                .filter(|id| {
                    let titles: Vec<SectionTitle> = sections
                        .get(id)
                        .map(|names| names.iter().map(|s| SectionTitle::new(s)).collect())
                        .unwrap_or_default();
                    principal.may_view_embargoed(&titles)
                })
                .collect()
        };

    if visible_ids.is_empty() {
        return Ok(());
    }

    let visible_vec: Vec<i64> = visible_ids.iter().copied().collect();
    let counts = state
        .db
        .get_patchset_finding_counts(&visible_vec)
        .await
        .map_err(|e| {
            tracing::error!("Database error fetching embargoed finding counts: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    for row in items.iter_mut() {
        if visible_ids.contains(&row.id) {
            let counts = counts.get(&row.id).copied().unwrap_or_default();
            row.findings_low = Some(counts.low);
            row.findings_medium = Some(counts.medium);
            row.findings_high = Some(counts.high);
            row.findings_critical = Some(counts.critical);
            row.findings_first_reported = Some(counts.first_reported);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn attach_patchset_flags(
    state: &Arc<AppState>,
    headers: &axum::http::HeaderMap,
    auth: Option<&crate::auth::AuthUser>,
    principal: &Principal,
    details: &mut serde_json::Value,
    page: Option<u32>,
    per_page: Option<u32>,
    include_logs: bool,
) -> Result<(), StatusCode> {
    let now = chrono::Utc::now().timestamp();
    let is_embargoed = details
        .get("embargo_until")
        .and_then(|v| v.as_i64())
        .is_some_and(|until| until > now);
    let (can_view, can_release) = if is_embargoed
        && let Some(ps_id) = details.get("id").and_then(|v| v.as_i64())
    {
        let (view, release) =
            resolve_patchset_embargo_access(state, headers, auth, principal, ps_id).await?;
        if view {
            let reviews = state
                .db
                .get_embargoed_patchset_reviews(ps_id, page, per_page, include_logs)
                .await
                .map_err(|e| {
                    tracing::error!("Database error fetching embargoed patchset reviews: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
            if let Some(obj) = details.as_object_mut() {
                obj.insert("reviews".to_string(), serde_json::Value::Array(reviews));
            }
        }
        (view, release)
    } else {
        (false, false)
    };
    if let Some(obj) = details.as_object_mut() {
        obj.insert(
            "smtp_enabled".to_string(),
            serde_json::Value::Bool(state.smtp_enabled),
        );
        obj.insert(
            "dry_run".to_string(),
            serde_json::Value::Bool(state.dry_run),
        );
        obj.insert(
            "can_view_embargoed".to_string(),
            serde_json::Value::Bool(can_view),
        );
        obj.insert(
            "can_release_embargo".to_string(),
            serde_json::Value::Bool(can_release),
        );
    }
    Ok(())
}

/// An error reply with an empty body, for a miss or a failure that has no
/// reason to give the caller.
fn bare(status: StatusCode) -> (StatusCode, String) {
    (status, String::new())
}

/// The 500 a database failure behind a patchset lookup gets. The error is
/// logged, not sent: it can name tables and values the caller has no
/// business seeing.
fn lookup_failed(e: anyhow::Error) -> (StatusCode, String) {
    info!("Database error: {}", e);
    bare(StatusCode::INTERNAL_SERVER_ERROR)
}

/// The patchset the id of a /api/patch or /api/patchset request names.
///
/// A number is a patchset id. An id with a `-` and no `@` is a stored slug
/// first, and only when no slug matches a pull request revision address
/// (`<repo>-<N>-<sha>`), so a stored slug of that shape keeps its meaning.
/// Anything else is a message id. `Ok(None)` is a miss with nothing more to
/// say; an address that finds no single revision is a 404 saying why.
async fn locate_patchset(state: &AppState, id: &str) -> Result<Option<i64>, (StatusCode, String)> {
    if let Ok(id) = id.parse::<i64>() {
        return Ok(Some(id));
    }
    if !id.contains('-') || id.contains('@') {
        info!("Looking up patchset by msgid: {:?}", id);
        return state
            .db
            .find_patchset_id_by_msgid(id)
            .await
            .map_err(lookup_failed);
    }
    info!("Looking up patchset by slug: {:?}", id);
    if let Some(found) = state
        .db
        .find_patchset_id_by_slug(id)
        .await
        .map_err(lookup_failed)?
    {
        return Ok(Some(found));
    }
    let Some(address) = crate::db::PrRevisionAddress::parse(id) else {
        return Ok(None);
    };
    let lookup = state
        .db
        .resolve_pr_revision_address(&address)
        .await
        .map_err(lookup_failed)?;
    revision_lookup_reply(&address, lookup)
}

/// The patchset a pull request revision address found, or the 404 that
/// says why it found none. The reason repeats only the validated hex
/// prefix, never the rest of the caller's text.
fn revision_lookup_reply(
    address: &crate::db::PrRevisionAddress,
    lookup: crate::db::RevisionLookup,
) -> Result<Option<i64>, (StatusCode, String)> {
    use crate::db::RevisionLookup;
    let prefix = &address.head_prefix;
    let reason = match lookup {
        RevisionLookup::Found(id) => return Ok(Some(id)),
        RevisionLookup::UnknownPullRequest => {
            "no pull request revision has this address".to_string()
        }
        RevisionLookup::NoHeadMatches => {
            format!("no revision of this pull request has a head commit starting {prefix}")
        }
        RevisionLookup::Ambiguous { matches } => format!(
            "{prefix} matches {matches} revisions of this pull request; give more of the commit"
        ),
    };
    Err((StatusCode::NOT_FOUND, reason))
}

async fn get_patchset(
    auth: crate::auth::OptionalAuthUser,
    OptionalPrincipal(principal): OptionalPrincipal,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(query): Query<PatchQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let result = match locate_patchset(&state, &query.id).await? {
        Some(id) => {
            info!("Fetching details for patchset id: {}", id);
            state
                .db
                .get_patchset_details(id, query.page, query.page_size())
                .await
        }
        None => Ok(None),
    };

    match result {
        Ok(Some(mut details)) => {
            attach_patchset_flags(
                &state,
                &headers,
                auth.0.as_ref(),
                &principal,
                &mut details,
                query.page,
                query.page_size(),
                true,
            )
            .await
            .map_err(bare)?;
            redact_embedded_bugs(&state, &principal, &mut details)
                .await
                .map_err(bare)?;
            annotate_review_stages(&state, &mut details);
            Ok(Json(details))
        }
        Ok(None) => {
            info!("Patchset not found: {:?}", query.id);
            Err(bare(StatusCode::NOT_FOUND))
        }
        Err(e) => Err(lookup_failed(e)),
    }
}

async fn enforce_review_embargo(
    state: &Arc<AppState>,
    headers: &axum::http::HeaderMap,
    auth: Option<&crate::auth::AuthUser>,
    principal: &Principal,
    details: &serde_json::Value,
) -> Result<(), StatusCode> {
    let now = chrono::Utc::now().timestamp();
    if let Some(until) = details.get("embargo_until").and_then(|v| v.as_i64())
        && until > now
    {
        let Some(ps_id) = details.get("patchset_id").and_then(|v| v.as_i64()) else {
            return Err(StatusCode::NOT_FOUND);
        };
        let (can_view, _) =
            resolve_patchset_embargo_access(state, headers, auth, principal, ps_id).await?;
        if !can_view {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    Ok(())
}

async fn get_review(
    auth: crate::auth::OptionalAuthUser,
    OptionalPrincipal(principal): OptionalPrincipal,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(query): Query<ReviewQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let result = if let Some(ps_id) = query.patchset_id {
        info!("Fetching latest review for patchset id: {}", ps_id);
        state.db.get_latest_review_for_patchset(ps_id).await
    } else if let Some(id) = query.id {
        info!("Fetching details for review id: {}", id);
        state.db.get_review_details(id).await
    } else {
        return Err(StatusCode::BAD_REQUEST);
    };

    match result {
        Ok(Some(mut details)) => {
            enforce_review_embargo(&state, &headers, auth.0.as_ref(), &principal, &details).await?;
            redact_embedded_bugs(&state, &principal, &mut details).await?;
            crate::activity::annotate_stage_records(
                &mut details,
                state.settings.project.kind.unwrap_or_default(),
            );
            attach_live_log(&state, &mut details, query.after_seq).await?;
            Ok(Json(details))
        }
        Ok(None) => {
            info!("Review not found");
            Err(StatusCode::NOT_FOUND)
        }
        Err(e) => {
            info!("Database error: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_patchset_summary(
    auth: crate::auth::OptionalAuthUser,
    OptionalPrincipal(principal): OptionalPrincipal,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(query): Query<PatchQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let result = match locate_patchset(&state, &query.id).await? {
        Some(id) => {
            info!("Fetching summary for patchset id: {}", id);
            state
                .db
                .get_patchset_summary(id, query.page, query.page_size())
                .await
        }
        None => Ok(None),
    };

    match result {
        Ok(Some(mut details)) => {
            attach_patchset_flags(
                &state,
                &headers,
                auth.0.as_ref(),
                &principal,
                &mut details,
                query.page,
                query.page_size(),
                false,
            )
            .await
            .map_err(bare)?;
            annotate_review_stages(&state, &mut details);
            Ok(Json(details))
        }
        Ok(None) => {
            info!("Patchset not found: {:?}", query.id);
            Err(bare(StatusCode::NOT_FOUND))
        }
        Err(e) => Err(lookup_failed(e)),
    }
}

/// Everything in flight across the daemon, least idle first.
async fn get_activity(State(state): State<Arc<AppState>>) -> Json<crate::api::ActivityResponse> {
    let mut entries: Vec<crate::api::ActivityEntry> = state
        .activity
        .snapshot()
        .into_iter()
        .map(Into::into)
        .collect();
    crate::activity::annotate(
        &mut entries,
        state.settings.project.kind.unwrap_or_default(),
    );
    Json(crate::api::ActivityResponse {
        count: entries.len(),
        entries,
    })
}

/// Statuses in which a patchset still has work outstanding. Persisted activity
/// is only an explanation for a patchset in one of these: one that has
/// finished has nothing left to explain, whatever rows outlived it. Every
/// other status is finished, `Partial` included: a review that ran out of
/// time is not resumed.
///
/// A patchset that the startup sweep failed because a restart stranded its
/// fetch is finished too, and it has no persisted row: the activity writer
/// deletes every fetch row at startup. Its `failed_reason` already says where
/// the fetch stopped, so status alone is the right test.
const UNFINISHED_PATCHSET_STATUSES: &[&str] = &["Fetching", "Pending", "Incomplete", "In Review"];

/// What one patchset is doing now, or failing that what it was doing when the
/// daemon last stopped.
async fn get_patchset_activity(
    State(state): State<Arc<AppState>>,
    Query(query): Query<crate::api::PatchsetActivityQuery>,
) -> Result<Json<crate::api::PatchsetActivityResponse>, StatusCode> {
    let db_error = |e: anyhow::Error| {
        info!("Database error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    };
    let project = state.settings.project.kind.unwrap_or_default();

    let mut entries: Vec<crate::api::ActivityEntry> = state
        .activity
        .patchset_snapshot(query.id)
        .into_iter()
        .map(Into::into)
        .collect();
    // A fetch is keyed by what it fetches, not by patchset, so it has to be
    // looked up through the placeholder the patchset was created with.
    // Without this a patchset that is fetching right now read as stopped.
    if let Some(commit) = state
        .db
        .get_patchset_fetch_commit(query.id)
        .await
        .map_err(db_error)?
        && let Some(fetch) = state
            .activity
            .get(&crate::activity::ActivityKey::Commit(commit))
    {
        entries.push(fetch.into());
    }

    // Read whether or not anything is live: it also tells the page when to
    // stop polling.
    let status = state
        .db
        .get_patchset_status(query.id)
        .await
        .map_err(db_error)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let unfinished = UNFINISHED_PATCHSET_STATUSES.contains(&status.as_str());
    let live = !entries.is_empty();
    if !live && unfinished {
        entries = state
            .db
            .get_patchset_activity(query.id)
            .await
            .map_err(db_error)?;
    }

    crate::activity::sort_for_display(&mut entries, project);
    let tool_calls = activity_tool_calls(&state.db, query.id, &entries).await;
    Ok(Json(crate::api::PatchsetActivityResponse {
        live,
        entries,
        finished: !unfinished,
        tool_calls,
    }))
}

/// Tool calls of each patch the activity entries name, over the patch's
/// current run.
///
/// Only the patches with an entry are counted, because the counts sit on
/// their stage tables and a patch with no entry has no table to show them
/// on. With no entry nothing is read, so polling a finished patchset costs
/// nothing more than before.
///
/// The counts are advisory: a database error is logged and gives none, so
/// the stage tables they sit beside are still served.
async fn activity_tool_calls(
    db: &Database,
    patchset_id: i64,
    entries: &[crate::api::ActivityEntry],
) -> Vec<crate::api::PatchToolCalls> {
    let patch_ids: Vec<i64> = entries
        .iter()
        .filter_map(|e| e.patch_id)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if patch_ids.is_empty() {
        return Vec::new();
    }
    db.patch_tool_calls(patchset_id, &patch_ids)
        .await
        .unwrap_or_else(|e| {
            warn!(
                "Failed to count the tool calls of patchset {}: {}",
                patchset_id, e
            );
            Vec::new()
        })
}

/// Gives every review in a patchset payload the stage order and labels the
/// live entries carry, so recorded and live stage rows sort on one field.
fn annotate_review_stages(state: &AppState, details: &mut serde_json::Value) {
    let project = state.settings.project.kind.unwrap_or_default();
    if let Some(reviews) = details.get_mut("reviews").and_then(|r| r.as_array_mut()) {
        for review in reviews.iter_mut() {
            crate::activity::annotate_stage_records(review, project);
        }
    }
}

/// Adds a running review's streamed conversation to its payload as
/// `live_log`, with `live_log_capped` saying whether streaming stopped at the
/// cap and `live_log_first_seq` naming the run the entries belong to.
///
/// `live_log_first_seq` is the lowest seq stored for the review, whatever
/// `after_seq` asked for, or null when nothing is stored. A review reset to
/// Pending by a restart reuses its row and numbers the new run's entries past
/// the old run's, so this changes when a new run replaces the old one's
/// entries. A page appending after its cursor would otherwise splice the two
/// conversations together; it redraws from the start instead.
///
/// Only while the review is Pending or In Review and has no final log: once
/// `logs` is set it is the record, and a review that ended without one has
/// nothing more coming. Liveness is decided here, once, so the page keeps no
/// list of statuses: the key's presence is the answer. When present it is
/// always an array, possibly empty, so a poll can tell "nothing new" from "no
/// live log".
///
/// Access: `live_log` is transcript, the same in kind as `logs`. Of the
/// transcripts this daemon serves, only bug transcripts are gated so far
/// (`transcript_bug` refuses the raw bug-transcript routes without global bug
/// visibility); review transcripts are served by `/api/review`,
/// `/api/review_log` and `/api/patch` to anyone who can read the review.
/// `live_log` follows the review-transcript rule: it is served only by
/// `/api/review` and `/api/review_log`, behind the same extractor as `logs`,
/// so it is no more exposed than the finished log is. It is deliberately not
/// added to `/api/patch`, `/api/patchset` or the activity routes. When the
/// transcript enforcement of DESIGN_PATCHSET_SUBSYSTEM_ACCESS_CONTROL §4.6
/// lands, `live_log`, `live_log_capped` and `live_log_first_seq` must be
/// stripped or refused together with `logs`: they belong on its one list of
/// transcript keys. The design's §5 move of `renderLogView` from
/// `/api/review` to `/api/review_log` applies to the page's live-log poll
/// too, which reads `/api/review?id=&after_seq=` today.
async fn attach_live_log(
    state: &AppState,
    details: &mut serde_json::Value,
    after_seq: Option<i64>,
) -> Result<(), StatusCode> {
    let has_final_log = details.get("logs").is_some_and(|l| !l.is_null());
    let running = details
        .get("status")
        .and_then(|s| s.as_str())
        .and_then(|s| s.parse::<crate::ReviewStatus>().ok())
        .is_some_and(|s| {
            matches!(
                s,
                crate::ReviewStatus::Pending | crate::ReviewStatus::InReview
            )
        });
    let Some(id) = details.get("id").and_then(|v| v.as_i64()) else {
        return Ok(());
    };
    if has_final_log || !running {
        return Ok(());
    }
    let db_error = |e: anyhow::Error| {
        info!("Database error: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    };
    let mut entries = state
        .db
        .get_review_log_entries(id, after_seq)
        .await
        .map_err(db_error)?;
    let project = state.settings.project.kind.unwrap_or_default();
    for entry in &mut entries {
        entry.label_stage(project);
    }
    let span = state.db.review_live_log_span(id).await.map_err(db_error)?;
    if let Some(obj) = details.as_object_mut() {
        obj.insert(
            "live_log".to_string(),
            serde_json::to_value(entries).map_err(|e| db_error(e.into()))?,
        );
        obj.insert(
            "live_log_capped".to_string(),
            serde_json::Value::Bool(span.capped),
        );
        obj.insert(
            "live_log_first_seq".to_string(),
            serde_json::json!(span.first_seq),
        );
    }
    Ok(())
}

async fn get_review_log(
    auth: crate::auth::OptionalAuthUser,
    OptionalPrincipal(principal): OptionalPrincipal,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(query): Query<ReviewQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let result = if let Some(ps_id) = query.patchset_id {
        info!("Fetching latest review log for patchset id: {}", ps_id);
        state.db.get_latest_review_for_patchset(ps_id).await
    } else if let Some(id) = query.id {
        info!("Fetching details for review id: {}", id);
        state.db.get_review_details(id).await
    } else {
        return Err(StatusCode::BAD_REQUEST);
    };

    match result {
        Ok(Some(mut details)) => {
            enforce_review_embargo(&state, &headers, auth.0.as_ref(), &principal, &details).await?;
            redact_embedded_bugs(&state, &principal, &mut details).await?;
            crate::activity::annotate_stage_records(
                &mut details,
                state.settings.project.kind.unwrap_or_default(),
            );
            attach_live_log(&state, &mut details, query.after_seq).await?;
            Ok(Json(details))
        }
        Ok(None) => {
            info!("Review not found");
            Err(StatusCode::NOT_FOUND)
        }
        Err(e) => {
            info!("Database error: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Resolves what the caller may do to one bug, from the MAINTAINERS sections
/// attributed to it. Subsystem names derived from a path prefix or supplied by
/// the reporter name nobody and are excluded by the accessor.
async fn bug_access(
    state: &AppState,
    principal: &Principal,
    bug_id: i64,
) -> Result<BugAccess, StatusCode> {
    bug_access_for_query(state, principal, bug_id, false).await
}

/// Resolves what the caller may do to one bug, accounting for whether the
/// request addressed the bug by its `bugid` / `slug`.
async fn bug_access_for_query(
    state: &AppState,
    principal: &Principal,
    bug_id: i64,
    by_bugid: bool,
) -> Result<BugAccess, StatusCode> {
    let sections = state
        .db
        .authorizing_sections_for_bug(bug_id)
        .await
        .map_err(|e| {
            tracing::error!("Database error resolving bug authority: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let titles: Vec<SectionTitle> = sections.iter().map(|s| SectionTitle::new(s)).collect();
    Ok(principal.access_to_with_bugid(&titles, by_bugid))
}

/// Resolves the caller's standing for a single-bug read route.
///
/// Addressing a bug by its `bugid` / `slug` is a capability read, so an
/// unauthenticated caller (or one holding an expired session) resolves to the
/// anonymous principal rather than being rejected with 401. Addressing a bug
/// by its sequential numeric `id` requires a valid session or API token.
fn resolve_bug_read_principal(
    principal: Result<Principal, (StatusCode, &'static str)>,
    query: &BugQuery,
) -> Result<Principal, StatusCode> {
    match principal {
        Ok(p) => Ok(p),
        Err(_) if query.by_bugid() => Ok(Principal::anonymous()),
        Err((status, _)) => Err(status),
    }
}

/// The subset of the given bugs the caller may read, resolved in one query.
async fn readable_bug_ids(
    state: &AppState,
    principal: &Principal,
    bug_ids: &[i64],
) -> Result<std::collections::HashSet<i64>, StatusCode> {
    let sections = state
        .db
        .authorizing_sections_for_bugs(bug_ids)
        .await
        .map_err(|e| {
            tracing::error!("Database error resolving bug authority: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(bug_ids
        .iter()
        .copied()
        .filter(|id| {
            let titles: Vec<SectionTitle> = sections
                .get(id)
                .map(|names| names.iter().map(|s| SectionTitle::new(s)).collect())
                .unwrap_or_default();
            principal.access_to(&titles).can_read()
        })
        .collect())
}

async fn lookup_bug(state: &AppState, query: &BugQuery) -> Result<crate::db::Bug, StatusCode> {
    if let Some(id) = query.id {
        state.db.get_bug(id).await
    } else if let Some(bugid) = query.effective_bugid() {
        state.db.get_bug_by_bugid(bugid).await
    } else {
        return Err(StatusCode::BAD_REQUEST);
    }
    .map_err(|e| {
        tracing::error!("Database error fetching bug: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?
    .ok_or(StatusCode::NOT_FOUND)
}

/// Loads the bug the query names and confirms the caller holds subsystem or
/// global authority to read it.
///
/// A bug outside the caller's authority answers 404, byte for byte the same as
/// a bug that does not exist, so the response does not disclose which of the
/// two it was.
async fn readable_bug(
    state: &AppState,
    principal: &Principal,
    query: &BugQuery,
) -> Result<crate::db::Bug, StatusCode> {
    let bug = lookup_bug(state, query).await?;
    if !bug_access(state, principal, bug.id).await?.can_read() {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(bug)
}

/// Loads the bug for `GET /api/bug`, granting capability read access when the
/// caller addresses the bug by its `bugid` / `slug`.
async fn readable_bug_for_view(
    state: &AppState,
    principal: &Principal,
    query: &BugQuery,
) -> Result<(crate::db::Bug, BugAccess), StatusCode> {
    let bug = lookup_bug(state, query).await?;
    let access = bug_access_for_query(state, principal, bug.id, query.by_bugid()).await?;
    if !access.can_read() {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok((bug, access))
}

/// Returns all bug IDs referenced by the bug's current or historical duplicate
/// relations (both `bug.duplicate_of_id` and any `old`/`new` targets recorded
/// in `duplicate_of_id` audit enrichments).
fn referenced_duplicate_ids(bug: &crate::db::Bug) -> Vec<i64> {
    let mut ids = Vec::new();
    if let Some(id) = bug.duplicate_of_id {
        ids.push(id);
    }
    for enrichment in &bug.enrichments {
        if enrichment.kind == "audit"
            && let Some(data) = &enrichment.data_json
            && data["field"].as_str() == Some("duplicate_of_id")
        {
            for key in ["old", "new"] {
                if let Some(id) = data[key]
                    .as_i64()
                    .or_else(|| data[key].as_str().and_then(|s| s.parse::<i64>().ok()))
                {
                    ids.push(id);
                }
            }
        }
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Redacts `audit` (`duplicate_of_id`) and `deduplication` enrichments on a
/// bug unless all current and historical duplicate target bugs are within the
/// caller's read authority, preventing deduplication reasoning or audit fields
/// from leaking an inaccessible counterpart's ID or problem statement.
fn redact_inaccessible_duplicate_enrichments(
    bug: &mut crate::db::Bug,
    readable_targets: &std::collections::HashSet<i64>,
) {
    let refs = referenced_duplicate_ids(bug);
    if refs.iter().all(|id| readable_targets.contains(id)) {
        return;
    }
    for enrichment in &mut bug.enrichments {
        if enrichment.kind == "audit"
            && enrichment
                .data_json
                .as_ref()
                .is_some_and(|d| d["field"].as_str() == Some("duplicate_of_id"))
        {
            enrichment.content = Some("Duplicate relationship updated".to_string());
            enrichment.data_json = Some(serde_json::json!({ "field": "duplicate_of_id" }));
        } else if enrichment.kind == "deduplication" {
            enrichment.content = Some("Matched an existing bug".to_string());
            enrichment.data_json = None;
        }
    }
}

/// Loads the bug for one of the raw transcript endpoints.
///
/// Transcripts are narrower than the bug itself. The deduplication stage
/// compares a bug against every other bug in the database and the prompt
/// embeds each candidate's problem statement, so a transcript discloses
/// unrelated bugs by construction. Only a principal who can already read every
/// bug learns nothing new from one; for a subsystem-scoped maintainer it would
/// cross exactly the boundary the rest of this model draws.
///
/// The refusal is 403 rather than 404 because the caller reached this point by
/// passing the read check, so the bug's existence is already known to them.
async fn transcript_bug(
    state: &AppState,
    principal: &Principal,
    query: &BugQuery,
) -> Result<crate::db::Bug, StatusCode> {
    let bug = readable_bug(state, principal, query).await?;
    if !principal.has_global_bug_visibility() {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(bug)
}

/// Removes embedded bug summaries the caller may not read.
///
/// The patchset and review payloads carry each bug's problem statement,
/// severity and inline review. That is bug data and answers to the same
/// authority as `/api/bug`, so it is filtered here rather than being left to
/// the single-bug endpoints to protect.
async fn redact_embedded_bugs(
    state: &AppState,
    principal: &Principal,
    payload: &mut serde_json::Value,
) -> Result<(), StatusCode> {
    let Some(bugs) = payload.get("bugs").and_then(|b| b.as_array()) else {
        return Ok(());
    };
    if bugs.is_empty() {
        return Ok(());
    };
    let ids: Vec<i64> = bugs.iter().filter_map(|b| b["id"].as_i64()).collect();
    let readable = readable_bug_ids(state, principal, &ids).await?;
    if let Some(array) = payload.get_mut("bugs").and_then(|b| b.as_array_mut()) {
        array.retain(|b| b["id"].as_i64().is_some_and(|id| readable.contains(&id)));
    }
    Ok(())
}

async fn get_bug(
    principal: Result<Principal, (StatusCode, &'static str)>,
    State(state): State<Arc<AppState>>,
    Query(query): Query<BugQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let principal = resolve_bug_read_principal(principal, &query)?;
    let (bug, access) = readable_bug_for_view(&state, &principal, &query).await?;
    let mut val = serde_json::to_value(&bug).unwrap_or(serde_json::json!({}));
    val["slug"] = serde_json::Value::String(bug.bugid.clone());
    val["problem"] = serde_json::Value::String(bug.problem().to_string());
    val["severity"] = serde_json::to_value(bug.severity()).unwrap();
    val["severity_explanation"] = serde_json::to_value(bug.severity_explanation()).unwrap();
    val["description"] = serde_json::to_value(bug.description()).unwrap();
    val["inline_review"] = serde_json::Value::String(bug.inline_review());
    val["locations"] = serde_json::to_value(bug.locations()).unwrap();
    val["source_files"] = serde_json::to_value(bug.source_files()).unwrap();
    val["introduced_in_commit"] = serde_json::to_value(bug.introduced_in_commit()).unwrap();
    val["verified_on_sha"] = serde_json::to_value(bug.verified_on_sha()).unwrap();
    val["is_fixed"] = serde_json::Value::Bool(bug.is_fixed());
    val["fixed_in_commit"] = serde_json::to_value(bug.fixed_in_commit()).unwrap();
    val["raw_input"] = serde_json::to_value(bug.raw_input()).unwrap();
    val["tokens_in"] = serde_json::Value::Number(bug.tokens_in().into());
    val["tokens_out"] = serde_json::Value::Number(bug.tokens_out().into());
    val["tokens_cached"] = serde_json::Value::Number(bug.tokens_cached().into());

    attach_duplicate_relations(&state, &principal, &bug, &mut val).await?;
    val["can_comment"] = serde_json::Value::Bool(!state.read_only && access.can_comment());
    val["can_manage"] = serde_json::Value::Bool(!state.read_only && access.can_manage());
    let mut family = state
        .db
        .bug_family(bug.id, false)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut check_ids: Vec<i64> = family.iter().map(|member| member.id).collect();
    for member in &family {
        check_ids.extend(referenced_duplicate_ids(member));
    }
    check_ids.sort_unstable();
    check_ids.dedup();
    let readable_by_principal = readable_bug_ids(&state, &principal, &check_ids).await?;
    let mut readable_family = readable_by_principal.clone();
    readable_family.insert(bug.id);
    family.retain(|member| readable_family.contains(&member.id));
    for member in &mut family {
        redact_inaccessible_duplicate_enrichments(member, &readable_by_principal);
    }
    let mut evidence = state
        .db
        .bug_evidence(&family)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if !principal.has_global_bug_visibility()
        && let Some(activity) = evidence.get_mut("activity").and_then(|a| a.as_array_mut())
    {
        activity.retain(|e| {
            !matches!(
                e.get("kind").and_then(|k| k.as_str()),
                Some("candidate" | "raw_candidate")
            )
        });
    }
    val["evidence"] = evidence;
    if let Some(obj) = val.as_object_mut() {
        for key in [
            "raw_input",
            "vector_json",
            "enrichments",
            "tokens_in",
            "tokens_out",
            "tokens_cached",
        ] {
            obj.remove(key);
        }
    }
    Ok(Json(val))
}

/// Attaches the bug's duplicate relations, omitting any counterpart the caller
/// may not read. A pointer to an invisible bug would disclose both that it
/// exists and what it is about.
async fn attach_duplicate_relations(
    state: &AppState,
    principal: &Principal,
    bug: &crate::db::Bug,
    val: &mut serde_json::Value,
) -> Result<(), StatusCode> {
    if bug.lifecycle_status == crate::db::BugLifecycleStatus::Duplicate {
        let Some(canonical_id) = bug.duplicate_of_id else {
            return Ok(());
        };
        if !bug_access(state, principal, canonical_id).await?.can_read() {
            if let Some(obj) = val.as_object_mut() {
                obj.remove("duplicate_of_id");
            }
            return Ok(());
        }
        if let Some(canonical) = state.db.get_bug(canonical_id).await.ok().flatten() {
            val["duplicate_of"] = serde_json::json!({
                "id": canonical.id,
                "bugid": canonical.bugid,
                "problem": canonical.problem(),
            });
        }
        return Ok(());
    }

    let Ok(duplicates) = state.db.list_duplicates_for_bug(bug.id).await else {
        return Ok(());
    };
    let ids: Vec<i64> = duplicates.iter().map(|d| d.id).collect();
    let readable = readable_bug_ids(state, principal, &ids).await?;
    let summaries: Vec<serde_json::Value> = duplicates
        .iter()
        .filter(|d| readable.contains(&d.id))
        .map(|d| {
            serde_json::json!({
                "id": d.id,
                "bugid": d.bugid,
                "problem": d.problem(),
                "created_at": d.created_at,
            })
        })
        .collect();
    val["duplicates"] = serde_json::Value::Array(summaries);
    Ok(())
}

async fn get_bug_raw(
    principal: Principal,
    State(state): State<Arc<AppState>>,
    Query(query): Query<BugQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let bug = transcript_bug(&state, &principal, &query).await?;
    let records = state
        .db
        .bug_family(bug.id, true)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(
        serde_json::json!({ "bugid": bug.bugid, "records": records }),
    ))
}

/// Serves the payload the bug workflow was started with for a single bug.
/// Duplicates keep their own candidate records, so each one resolves to the
/// input that produced it rather than the canonical bug's input.
async fn get_bug_input(
    principal: Principal,
    State(state): State<Arc<AppState>>,
    Query(query): Query<BugQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let bug = transcript_bug(&state, &principal, &query).await?;

    let inputs: Vec<serde_json::Value> = bug
        .enrichments
        .iter()
        .filter(|e| e.kind == "candidate" || e.kind == "raw_candidate")
        .map(|e| {
            serde_json::json!({
                "enrichment_id": e.id,
                "created_at": e.created_at,
                "author": e.author,
                "tool": e.tool,
                "model": e.model,
                "input": e.data_json,
                "content": e.content,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "bugid": bug.bugid,
        "title": bug.title,
        "inputs": inputs,
    })))
}

async fn get_bug_enrichments(
    principal: Principal,
    State(state): State<Arc<AppState>>,
    Query(query): Query<BugQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut bug = readable_bug(&state, &principal, &query).await?;
    if !principal.has_global_bug_visibility() {
        bug.enrichments
            .retain(|e| e.kind != "candidate" && e.kind != "raw_candidate");
    }
    let refs = referenced_duplicate_ids(&bug);
    let readable_refs = readable_bug_ids(&state, &principal, &refs).await?;
    redact_inaccessible_duplicate_enrichments(&mut bug, &readable_refs);
    Ok(Json(
        serde_json::to_value(&bug.enrichments).unwrap_or_default(),
    ))
}

async fn get_bug_logs(
    principal: Principal,
    State(state): State<Arc<AppState>>,
    Query(query): Query<BugQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let bug = transcript_bug(&state, &principal, &query).await?;
    let result = state.db.get_bug_logs(bug.id).await;

    match result {
        Ok(Some(logs_str)) => {
            let parsed: serde_json::Value =
                serde_json::from_str(&logs_str).unwrap_or(serde_json::Value::String(logs_str));
            Ok(Json(parsed))
        }
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("Database error fetching bug logs: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn analyze_bug(
    principal: Principal,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    State(state): State<Arc<AppState>>,
    Json(payload): Json<AnalyzeBugPayload>,
) -> Result<Json<BugOutcome>, (StatusCode, String)> {
    if state.read_only {
        return Err((
            StatusCode::FORBIDDEN,
            "Server is running in read-only mode.".to_string(),
        ));
    }

    // Filing a bug runs an LLM analysis, so it costs money and is granted
    // explicitly rather than falling out of any read or review capability.
    if !principal.may_create() {
        return Err((
            StatusCode::FORBIDDEN,
            "You don't have permissions to file bugs.".to_string(),
        ));
    }

    if !state.settings.linux_bug.enabled {
        return Err((
            StatusCode::NOT_FOUND,
            "Linux bug tracking is disabled on this server.".to_string(),
        ));
    }

    // Uncached, as this asked for before the cached constructor grew to take the
    // AI settings and a database path: filing a bug is a one-off analysis.
    let provider = match crate::ai::create_provider(&state.settings) {
        Ok(p) => p,
        Err(e) => {
            // The reason names the provider and its configuration, so it stays
            // in the log rather than going back over the wire.
            tracing::error!("Failed to create AI provider: {}", e);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Analysis is unavailable.".to_string(),
            ));
        }
    };

    let project = state.settings.project.kind.unwrap_or_default();
    let repo_path = std::path::PathBuf::from(&state.settings.git.repository_path);
    let tools = if repo_path.exists() {
        let mut mainline_sha = None;
        for ref_name in ["origin/master", "origin/main", "master", "main", "HEAD"] {
            if let Ok(sha) = crate::git_ops::get_commit_hash(&repo_path, ref_name).await {
                mainline_sha = Some(sha);
                break;
            }
        }
        let mut tb = crate::toolbox::ToolBox::new(repo_path, None);
        if let Some(m_sha) = mainline_sha {
            tb.set_virtual_head(m_sha);
        }
        Some(std::sync::Arc::new(tb))
    } else {
        None
    };

    let source_tool = payload
        .tool
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "api".into());
    let source_model = payload
        .model
        .filter(|s| !s.trim().is_empty())
        .or_else(|| Some(provider.get_capabilities().model_name));
    let mut payload = payload.input;
    if payload.subsystems.is_empty() && !payload.source_files.is_empty() {
        if project.uses_maintainers() {
            let matched = if let Some(mindex) = crate::maintainers::get_global_maintainers() {
                mindex.match_files(&payload.source_files)
            } else if let Ok(mindex) = crate::maintainers::MaintainersIndex::from_top_of_trunk(
                &state.settings.git.repository_path,
            ) {
                mindex.match_files(&payload.source_files)
            } else {
                Vec::new()
            };
            payload.subsystems = matched
                .into_iter()
                .map(crate::db::AttributedSubsystem::from_maintainers)
                .collect();
        } else {
            payload.subsystems =
                crate::workflows::linux_bug::extract_directory_subsystems_for_project(
                    &payload.source_files,
                    project,
                )
                .into_iter()
                .map(crate::db::AttributedSubsystem::from_path_prefix)
                .collect();
        }
    }

    let actor = if principal.email().is_empty() {
        format!("authorized client ({})", addr.ip())
    } else {
        principal.email().to_string()
    };
    let attributed_db = state.db.with_bug_actor(&actor, &source_tool, source_model);
    match crate::workflows::linux_bug::process_issue_for_project(
        provider.as_ref(),
        tools,
        &attributed_db,
        payload,
        Some("api_analyze"),
        project,
    )
    .await
    {
        Ok(outcome) => Ok(Json(outcome)),
        Err(e) => {
            // The failure quotes paths, prompts and provider responses, none of
            // which belong in a response body.
            tracing::error!("Pre-existing bug analysis failed: {}", e);
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Analysis failed.".to_string(),
            ))
        }
    }
}

async fn redirect_bug(Path(bugid): Path<String>) -> impl IntoResponse {
    Redirect::temporary(&format!("/#/bug/{}", bugid))
}

async fn get_message(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PatchQuery>,
) -> Result<Json<crate::db::MessageRow>, StatusCode> {
    let result = if let Ok(id_val) = query.id.parse::<i64>() {
        info!("Fetching details for message id: {}", id_val);
        state.db.get_message_details(id_val).await
    } else {
        info!("Fetching details for message msgid: {}", query.id);
        state.db.get_message_details_by_msgid(&query.id).await
    };

    match result {
        Ok(Some(mut details)) => {
            if (details.body.is_none() || details.body.as_deref() == Some(""))
                && let (Some(hash), Some(group)) = (&details.git_blob_hash, &details.mailing_list)
            {
                let repo_root = std::path::PathBuf::from("archives").join(group);

                // 1. Find all potential repo paths (root + epochs)
                let mut candidate_paths = Vec::new();

                // Check epochs first (most likely for recent messages)
                if let Ok(mut entries) = tokio::fs::read_dir(&repo_root).await {
                    let mut epochs = Vec::new();
                    while let Ok(Some(entry)) = entries.next_entry().await {
                        if let Ok(ft) = entry.file_type().await
                            && ft.is_dir()
                            && let Ok(name) = entry.file_name().into_string()
                            && let Ok(num) = name.parse::<i32>()
                        {
                            epochs.push(num);
                        }
                    }
                    epochs.sort_by(|a, b| b.cmp(a)); // Descending

                    for epoch in epochs {
                        candidate_paths.push(repo_root.join(epoch.to_string()));
                    }
                }

                // Add root as fallback
                candidate_paths.push(repo_root.clone());

                // 2. Search for blob
                for path in candidate_paths {
                    if let Ok(raw) = crate::git_ops::read_blob(&path, hash).await
                        && let Ok((metadata, _)) = crate::patch::parse_email(&raw)
                    {
                        details.body = Some(metadata.body);
                        break;
                    }
                }
            }
            Ok(Json(details))
        }
        Ok(None) => {
            info!("Message not found: {}", query.id);
            Err(StatusCode::NOT_FOUND)
        }
        Err(e) => {
            info!("Database error: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn get_stats(
    State(_state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let pending = crate::metrics::get_pending_patches();
    let reviewing = crate::metrics::get_reviewing_patches();
    let messages = crate::metrics::get_messages();
    let patchsets = crate::metrics::get_patchsets();
    let repo_packs = crate::metrics::get_repo_packs();
    let repo_pack_bytes = crate::metrics::get_repo_pack_bytes();

    Ok(Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "build": env!("BUILD_DESCRIBE"),
        "pending": pending,
        "reviewing": reviewing,
        "messages": messages,
        "patchsets": patchsets,
        "repo_packs": repo_packs,
        "repo_pack_bytes": repo_pack_bytes
    })))
}

async fn stats_timeline(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SubsystemQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let data = state
        .stats_timeline_cache
        .get_or_fetch(params.subsystem_id, || async {
            state
                .db
                .get_timeline_stats(params.subsystem_id)
                .await
                .map_err(|e| {
                    info!("Error getting timeline stats: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                })
        })
        .await?;
    Ok(Json(data))
}

async fn stats_reviews(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let data = state
        .stats_reviews_cache
        .get_or_fetch(|| async {
            state.db.get_review_stats().await.map_err(|e| {
                info!("Error getting review stats: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })
        })
        .await?;
    Ok(Json(data))
}

async fn stats_tools(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let data = state
        .stats_tools_cache
        .get_or_fetch(|| async {
            state.db.get_tool_usage_stats().await.map_err(|e| {
                info!("Error getting tool stats: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })
        })
        .await?;
    Ok(Json(data))
}

async fn rerun_patchset(
    auth: crate::auth::OptionalAuthUser,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(query): Query<PatchQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if state.read_only {
        return Err((
            StatusCode::FORBIDDEN,
            "Server is running in read-only mode.".into(),
        ));
    }

    if !is_authorized(
        &state,
        &headers,
        auth.0.as_ref(),
        crate::settings::Permission::Review,
    ) {
        return Err((
            StatusCode::FORBIDDEN,
            "You don't have permissions to rerun patchsets.".into(),
        ));
    }

    let id = query
        .id
        .parse::<i64>()
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid id parameter".into()))?;

    state.db.rerun_patchset(id).await.map_err(|e| {
        error!("Failed to rerun patchset {}: {}", id, e);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to rerun patchset".into(),
        )
    })?;

    Ok(Json(serde_json::json!({ "status": "accepted" })))
}

async fn release_patchset_embargo(
    auth: crate::auth::OptionalAuthUser,
    OptionalPrincipal(principal): OptionalPrincipal,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(query): Query<PatchQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if state.read_only {
        return Err((
            StatusCode::FORBIDDEN,
            "Server is running in read-only mode.".into(),
        ));
    }

    let id = query
        .id
        .parse::<i64>()
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid id parameter".into()))?;

    let allowed = can_release_patchset_embargo(&state, &headers, auth.0.as_ref(), &principal, id)
        .await
        .map_err(|status| {
            (
                status,
                "Failed to verify permissions to release embargo.".into(),
            )
        })?;
    if !allowed {
        return Err((
            StatusCode::FORBIDDEN,
            "You don't have permissions to release the embargo on this patchset.".into(),
        ));
    }

    let now = chrono::Utc::now().timestamp();
    let released = state
        .db
        .release_patchset_embargo(id, now)
        .await
        .map_err(|e| {
            error!("Failed to release embargo on patchset {}: {}", id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to release embargo".into(),
            )
        })?;
    if released {
        info!(
            "Released embargo on patchset {} (actor: {})",
            id,
            if principal.email().is_empty() {
                "operator/acl"
            } else {
                principal.email()
            }
        );
        state.patchsets_homepage_cache.invalidate().await;
    }

    Ok(Json(serde_json::json!({
        "status": "released",
        "released": released,
    })))
}

async fn cancel_patchset(
    auth: crate::auth::OptionalAuthUser,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(query): Query<CancelQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if state.read_only {
        return Err((
            StatusCode::FORBIDDEN,
            "Server is running in read-only mode.".into(),
        ));
    }

    if !is_authorized(
        &state,
        &headers,
        auth.0.as_ref(),
        crate::settings::Permission::Cancel,
    ) {
        return Err((
            StatusCode::FORBIDDEN,
            "You don't have permissions to cancel patchsets.".into(),
        ));
    }

    let cancelled = state
        .db
        .cancel_patchset(query.id, query.force)
        .await
        .map_err(|e| {
            error!("Failed to cancel patchset {}: {}", query.id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to cancel patchset".into(),
            )
        })?;

    if cancelled {
        info!("Patchset {} cancelled (force={})", query.id, query.force);
        Ok(Json(serde_json::json!({ "status": "cancelled" })))
    } else {
        let reason = if query.force {
            "Patchset is not in a cancellable state (must be Pending, Incomplete, or In Review)"
        } else {
            "Patchset is not in a cancellable state (must be Pending or Incomplete; use force=true for In Review)"
        };
        Ok(Json(serde_json::json!({
            "status": "not_modified",
            "reason": reason
        })))
    }
}

async fn rerun_patch(
    auth: crate::auth::OptionalAuthUser,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Query(query): Query<RerunPatchQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if state.read_only {
        return Err((
            StatusCode::FORBIDDEN,
            "Server is running in read-only mode.".into(),
        ));
    }

    if !is_authorized(
        &state,
        &headers,
        auth.0.as_ref(),
        crate::settings::Permission::Review,
    ) {
        return Err((
            StatusCode::FORBIDDEN,
            "You don't have permissions to rerun patches.".into(),
        ));
    }

    state
        .db
        .rerun_patch(query.patchset_id, query.patch_id)
        .await
        .map_err(|e| {
            error!(
                "Failed to rerun patch {} in patchset {}: {}",
                query.patch_id, query.patchset_id, e
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to rerun patch".into(),
            )
        })?;

    Ok(Json(serde_json::json!({ "status": "accepted" })))
}

const EMAIL_WORKER_STALE_SECONDS: i64 = 300;

async fn health_check(State(state): State<Arc<AppState>>) -> StatusCode {
    if let Some(hb) = &state.email_worker_heartbeat {
        let last = hb.load(std::sync::atomic::Ordering::Relaxed);
        let now = chrono::Utc::now().timestamp();
        if last > 0 && now.saturating_sub(last) > EMAIL_WORKER_STALE_SECONDS {
            error!(
                "Health check failed: EmailWorker heartbeat is stale ({}s old)",
                now.saturating_sub(last)
            );
            return StatusCode::SERVICE_UNAVAILABLE;
        }
    }
    StatusCode::OK
}

async fn get_config(
    auth: crate::auth::OptionalAuthUser,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let is_auth = |perm| is_authorized(&state, &headers, auth.0.as_ref(), perm);
    let can_review = !state.read_only && is_auth(crate::settings::Permission::Review);
    let can_cancel = !state.read_only && is_auth(crate::settings::Permission::Cancel);
    let can_ingest = !state.read_only && is_auth(crate::settings::Permission::Ingest);

    let project = state.settings.project.kind.unwrap_or_default();

    Ok(Json(serde_json::json!({
        "project_name": state.settings.project.name,
        "project": project.display_name(),
        "project_description": state.settings.project.description,
        "project_domain": state.settings.project.domain,
        "attribution": state.settings.project.attribution(),
        "forge_enabled": state.settings.forge.enabled,
        "bugs_enabled": state.settings.linux_bug.enabled,
        "bug_fix_check_enabled": state.settings.linux_bug.fix_check_enabled,
        "read_only": state.read_only,
        "permissions": {
            "review": can_review,
            "cancel": can_cancel,
            "ingest": can_ingest,
        },
        "user": {
            "email": auth.0.as_ref().map(|u| &u.email),
            "is_authenticated": auth.0.is_some(),
        },
        "version": env!("CARGO_PKG_VERSION"),
        "build": env!("BUILD_DESCRIBE"),
        "git_hash": env!("GIT_HASH"),
    })))
}

async fn forge_webhook(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if state.read_only {
        return Err(StatusCode::FORBIDDEN);
    }

    let webhook_secret = state.settings.forge.webhook_secret.as_deref();
    let has_secret = webhook_secret.is_some();

    // Access control: when webhook_secret is configured, signature
    // verification in validate_event is the sole access control for ALL
    // requests. This is critical for reverse proxy deployments where all
    // traffic arrives from loopback — the signature check cannot be
    // bypassed by source IP.
    //
    // Without a secret there is nothing to verify, so the only callers left
    // are ones that can prove they share this machine by presenting the local
    // token, plus the explicit --enable-unsafe-all-submit flag. A forge cannot
    // do either, which is the point: an endpoint a forge can reach
    // unauthenticated is an endpoint anyone can reach, and arriving on
    // loopback never distinguished the two behind a reverse proxy.
    if !has_secret && !presents_local_token(&headers, &state) && !state.allow_all_submit {
        info!(
            "Refused {} webhook from {}: configure webhook_secret or use --enable-unsafe-all-submit",
            crate::forge::loggable(&provider),
            addr
        );
        return Err(StatusCode::FORBIDDEN);
    }

    let forge = state.forge_registry.get(&provider).ok_or_else(|| {
        warn!(
            "Unknown forge provider: {}",
            crate::forge::loggable(&provider)
        );
        StatusCode::NOT_FOUND
    })?;

    // A rejected delivery is reported by the forge as a bare status code, so
    // the log line is the only place an administrator can learn which event
    // was refused and why.
    let event = forge
        .validate_event(&headers, &body, webhook_secret)
        .inspect_err(|status| {
            warn!(
                "Rejected {} webhook event {} from {}: {}",
                forge.name(),
                crate::forge::event_label(&headers),
                addr,
                status
            );
        })?;

    if event == crate::forge::ForgeEvent::Handshake {
        info!(
            "{} webhook handshake from {} acknowledged",
            forge.name(),
            addr
        );
        return Ok(Json(serde_json::json!({
            "status": "ok",
            "message": format!("{} webhook endpoint is configured", forge.name())
        })));
    }

    let (action, metadata) = forge.parse_payload(&body).inspect_err(|status| {
        warn!(
            "Rejected {} webhook payload from {}: {} ({} bytes)",
            forge.name(),
            addr,
            status,
            body.len()
        );
    })?;

    info!(
        "{} {}: {} - {}",
        forge.name(),
        crate::forge::loggable(&action),
        metadata.pr_title.as_deref().unwrap_or("(no title)"),
        metadata.pr_url.as_deref().unwrap_or("(no url)")
    );

    // A forge reports far more than new commits, and the base sha it sends
    // moves with the target branch. Reviewing an action that changed only a
    // label would spend a full review re-reading code that did not change.
    if forge.review_intent(&action) == crate::forge::ReviewIntent::Skip {
        info!(
            "{} {} carries no new commits, skipping review",
            forge.name(),
            crate::forge::loggable(&action)
        );
        return Ok(Json(serde_json::json!({
            "status": "ignored",
            "message": format!("{} action carries no new commits", forge.name())
        })));
    }

    if let Some(ref author) = metadata.author
        && crate::forge::is_dependabot_author(author)
    {
        info!(
            "{} PR #{} by {} is from Dependabot, skipping review",
            forge.name(),
            metadata.pr_number,
            crate::forge::loggable(author)
        );
        return Ok(Json(serde_json::json!({
            "status": "ignored",
            "message": "Dependabot pull requests are not reviewed"
        })));
    }

    enqueue_pull_request(&state, forge.name(), &metadata, ExistingRevision::Refetch).await?;

    Ok(Json(serde_json::json!({
        "status": "accepted",
        "message": format!("{} {} queued for review", forge.name(), action)
    })))
}

/// What to do about a pull request revision whose placeholder already
/// existed and was not reset to Fetching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingRevision {
    /// Send the fetch request whatever the row was. A webhook delivery
    /// queues a fetch every time, and sharing the enqueue path does not
    /// change that.
    Refetch,
    /// Send it only for a row that was created, reset, or is still
    /// Fetching, and that the auto-cancel of superseded revisions kept. A
    /// row that is pending, in review or finished already has its patches,
    /// and a superseded one was left as it was or, while in progress,
    /// cancelled, so fetching either would do nothing for an operator who
    /// asked for the revision by number.
    Leave,
}

impl ExistingRevision {
    /// Whether a fetch request is sent for `placeholder` after queuing. A
    /// row that was already Fetching is fetched again in both modes,
    /// because a fetch request lost to a daemon restart is recovered only
    /// by a new one.
    fn sends_fetch(self, placeholder: &crate::db::FetchingPlaceholder) -> bool {
        use crate::db::PlaceholderState;
        match self {
            Self::Refetch => true,
            Self::Leave => {
                placeholder.standing.superseded_by.is_none()
                    && matches!(
                        placeholder.state,
                        PlaceholderState::Created
                            | PlaceholderState::Reset { .. }
                            | PlaceholderState::AlreadyFetching
                    )
            }
        }
    }
}

/// Creates or finds the Fetching placeholder of one pull request revision
/// and queues its fetch when `existing` says so.
///
/// The webhook and the manual review route both come through here, so a
/// revision reaches one placeholder id, subject, slug and description
/// whichever way it arrived. The placeholder id is built only here.
async fn enqueue_pull_request(
    state: &AppState,
    forge_name: &str,
    metadata: &crate::forge::ForgeMetadata,
    existing: ExistingRevision,
) -> Result<crate::db::FetchingPlaceholder, StatusCode> {
    let default_subject = format!("{} #{}", forge_name, metadata.pr_number);
    let subject = metadata.pr_title.as_deref().unwrap_or(&default_subject);

    let commit_range = format!("{}..{}", metadata.base_sha, metadata.head_sha);
    let placeholder_id = format!("mr-{}-{}@sashiko.local", metadata.pr_number, commit_range);

    let slug = metadata.repo_url.as_ref().map(|url| {
        let repo = crate::forge::extract_repo_name_from_url(url);
        format!("{}-{}", repo, metadata.pr_number)
    });

    let placeholder = state
        .db
        .queue_fetching_patchset(
            &placeholder_id,
            &format!("Fetching {} PR/MR: {}", forge_name, subject),
            None,
            None,
            metadata.pr_url.as_deref(),
            Some(subject),
            Some(metadata.pr_number),
            slug.as_deref(),
            metadata.pr_body.as_deref(),
        )
        .await
        .map_err(|e| {
            error!("Failed to create placeholder patchset: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    if !existing.sends_fetch(&placeholder) {
        return Ok(placeholder);
    }

    let req = FetchRequest {
        repo_url: metadata.repo_url.clone(),
        commit_hash: commit_range,
        mr_url: metadata.pr_url.clone(),
        mr_title: metadata.pr_title.clone(),
        mr_number: Some(metadata.pr_number),
        // A pull request's tracker is read by b4's marker, never by flag.
        b4_cover_letter: false,
    };

    state.fetch_sender.send(req).await.map_err(|e| {
        error!("Failed to send fetch request to queue: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(placeholder)
}

/// The reply to a request the route refuses: a status and one line saying
/// why, which the CLI prints as it is.
type PrReviewRefusal = (StatusCode, String);

/// Reviews a pull request of the configured GitHub repository by its number
/// (`POST /api/pr/review`).
///
/// gh resolves the number to the pull request's current base and head, and
/// the head revision is queued through the same path a webhook delivery
/// takes, so it gets the same placeholder, subject, slug and auto-cancel of
/// older revisions. Unlike a delivery, a revision that is already pending,
/// under review or finished is not fetched again; RerunHead reruns a
/// finished one instead. A head that auto-cancel ranks below another
/// revision is refused, because it would be cancelled rather than reviewed.
async fn review_pull_request(
    auth: crate::auth::OptionalAuthUser,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<AppState>>,
    Json(request): Json<PrReviewRequest>,
) -> Result<Json<PrReviewResponse>, PrReviewRefusal> {
    authorize_pr_review(&state, &headers, auth.0.as_ref(), request.mode)?;
    let repo = pr_review_repository(&state.settings.forge, &request)?;
    let resolved = match state
        .pull_request_resolver
        .resolve(repo, request.number)
        .await
    {
        Ok(resolved) => resolved,
        Err(err) => return Err(unresolved_pull_request(&state, repo, request.number, &err).await),
    };

    let forge_name = crate::forge::ForgeProvider::name(&crate::forge::GitHubForge);
    let metadata = resolved.to_forge_metadata();
    let placeholder = enqueue_pull_request(&state, forge_name, &metadata, ExistingRevision::Leave)
        .await
        .map_err(|status| (status, "Failed to queue the pull request".to_string()))?;

    if let Some(newer) = placeholder.standing.superseded_by {
        return Err(superseded_head(&placeholder, newer, &resolved));
    }
    let outcome = match pr_review_step(&placeholder.state, request.mode) {
        PrReviewStep::Report(outcome) => outcome,
        PrReviewStep::Rerun(observed) => {
            rerun_head(&state, &placeholder, observed, &resolved).await?
        }
    };

    let response = pr_review_response(outcome, &placeholder, &resolved);
    info!("{}", response.message);
    Ok(Json(response))
}

/// The first 8 characters of a pull request's head commit, for messages.
fn short_head(resolved: &crate::forge::ResolvedPullRequest) -> &str {
    let head = resolved.head.as_str();
    head.get(..8).unwrap_or(head)
}

/// The refusal for a head the auto-cancel of superseded revisions ranks
/// below another revision of the pull request. Versions count rows, not the
/// order of heads, so a pull request whose head went back to an earlier
/// revision lands here. The auto-cancel cancels such a head when it is in
/// progress and would cancel it again after a rerun, so the route reports
/// that rather than a review that will not happen.
fn superseded_head(
    placeholder: &crate::db::FetchingPlaceholder,
    newer: crate::db::KnownRevision,
    resolved: &crate::forge::ResolvedPullRequest,
) -> PrReviewRefusal {
    let number = resolved.number;
    let id = placeholder.id;
    let was = placeholder
        .state
        .previous_status()
        .map(|status| format!(", was {status}"))
        .unwrap_or_default();
    let message = format!(
        "PR #{number} head {} is patchset {id}{}{was}, but patchset {}, v{} of PR #{number} \
         is newer here, and the auto-cancel of superseded revisions keeps only the newest, \
         so this head was not queued",
        short_head(resolved),
        version_label(&placeholder.standing),
        newer.id,
        newer.version,
    );
    warn!("{}", message);
    (StatusCode::CONFLICT, message)
}

/// ", vK" for a revision whose version is known, else nothing.
fn version_label(standing: &crate::db::RevisionStanding) -> String {
    standing
        .version
        .map(|version| format!(", v{version}"))
        .unwrap_or_default()
}

/// Reruns the head revision, which queuing found finished in `observed`,
/// as /api/patchset/rerun does. The rerun happens only while the row is
/// still `observed`: another request may have rerun it since, and a
/// reviewer may already hold it, and resetting it then would pull the row
/// out from under that review. Such a row is refused with 409.
async fn rerun_head(
    state: &AppState,
    placeholder: &crate::db::FetchingPlaceholder,
    observed: crate::ReviewStatus,
    resolved: &crate::forge::ResolvedPullRequest,
) -> Result<PrReviewOutcome, PrReviewRefusal> {
    let rerun = state
        .db
        .rerun_patchset_from(placeholder.id, observed)
        .await
        .map_err(|e| {
            error!("Failed to rerun patchset {}: {}", placeholder.id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to rerun patchset".to_string(),
            )
        })?;
    if rerun {
        return Ok(PrReviewOutcome::RerunQueued);
    }
    Err((
        StatusCode::CONFLICT,
        format!(
            "PR #{} head {} as patchset {} is no longer {}, so it was not rerun; \
             ask again to see where it stands",
            resolved.number,
            short_head(resolved),
            placeholder.id,
            observed.as_str()
        ),
    ))
}

/// The permissions a request to review a pull request by number needs.
/// Reviewing the head ingests its patches, so it needs Ingest. A rerun is
/// the act /api/patchset/rerun performs, which needs Review, so RerunHead
/// needs both; otherwise an Ingest-only caller could rerun any review by
/// naming its pull request.
fn pr_review_permissions(mode: PrReviewMode) -> &'static [crate::settings::Permission] {
    use crate::settings::Permission;
    match mode {
        PrReviewMode::ReviewHead => &[Permission::Ingest],
        PrReviewMode::RerunHead => &[Permission::Ingest, Permission::Review],
    }
}

/// Refuses a request to review a pull request by number on a read-only
/// server, or from a caller missing a permission `mode` needs. Where the
/// request comes from is not consulted, as for every other mutation.
fn authorize_pr_review(
    state: &Arc<AppState>,
    headers: &axum::http::HeaderMap,
    auth: Option<&crate::auth::AuthUser>,
    mode: PrReviewMode,
) -> Result<(), PrReviewRefusal> {
    if state.read_only {
        return Err((
            StatusCode::FORBIDDEN,
            "Server is running in read-only mode.".into(),
        ));
    }
    match pr_review_permissions(mode)
        .iter()
        .find(|perm| !is_authorized(state, headers, auth, **perm))
    {
        Some(perm) => Err((
            StatusCode::FORBIDDEN,
            format!("You don't have the {perm:?} permission this request needs."),
        )),
        None => Ok(()),
    }
}

/// The repository a request's number belongs to: the one
/// forge.github_repository names, never one read from a git remote, since
/// the daemon's repository also holds fetcher remotes whose URLs come from
/// requests. A request naming another repository is refused rather than
/// reviewed against this daemon's tree.
fn pr_review_repository<'a>(
    forge: &'a crate::settings::ForgeSettings,
    request: &PrReviewRequest,
) -> Result<&'a crate::settings::ForgeRepo, PrReviewRefusal> {
    if request.number <= 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "{} is not a pull request number; they start at 1",
                request.number
            ),
        ));
    }
    if forge
        .provider
        .as_deref()
        .is_some_and(|provider| provider.trim().eq_ignore_ascii_case("gitlab"))
    {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "reviewing a merge request by number needs gh, which is GitHub only".into(),
        ));
    }
    let Some(repo) = forge.github_repository.as_ref() else {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "set forge.github_repository to review a pull request by number".into(),
        ));
    };
    match request.repo.as_deref() {
        Some(given) if !names_repository(given, repo) => Err((
            StatusCode::BAD_REQUEST,
            format!(
                "the pull request belongs to {}, but this daemon reviews pull requests of {}/{}/{}",
                crate::finding_view::sanitize_one_line(given, 200),
                repo.host,
                repo.owner,
                repo.name
            ),
        )),
        _ => Ok(repo),
    }
}

/// Whether `given`, the "host/owner/name" (or "owner/name" on github.com)
/// taken from a pull request URL, names `repo`. The host is compared too,
/// because a repository of the same name on another forge host is another
/// repository, whose numbers name other changes. GitHub compares hosts,
/// owners and names without case, so this does too.
fn names_repository(given: &str, repo: &crate::settings::ForgeRepo) -> bool {
    given
        .parse::<crate::settings::ForgeRepo>()
        .is_ok_and(|given| {
            given.host.eq_ignore_ascii_case(&repo.host)
                && given.owner.eq_ignore_ascii_case(&repo.owner)
                && given.name.eq_ignore_ascii_case(&repo.name)
        })
}

/// The refusal for a number gh could not resolve. It names the newest
/// revision of that pull request already known here, if any, and how to
/// rerun it, rather than rerunning it: reviewing an older head when the
/// operator asked for the pull request would be a surprise, and a rerun by
/// patchset id does it explicitly. Every part of the message is safe to log:
/// GhError's text is already one sanitized line.
async fn unresolved_pull_request(
    state: &AppState,
    repo: &crate::settings::ForgeRepo,
    number: i64,
    err: &crate::forge::GhError,
) -> PrReviewRefusal {
    let mut message = format!(
        "could not resolve pull request #{number} of {}/{}: {err}",
        repo.owner, repo.name
    );
    match state
        .db
        .newest_pull_request_revision(&repo.owner, &repo.name, number)
        .await
    {
        Ok(Some(known)) => message.push_str(&format!(
            "; the newest known revision is patchset {}, v{}; use sashiko-cli rerun {} to review it again",
            known.id, known.version, known.id
        )),
        Ok(None) => {}
        Err(e) => error!(
            "Failed to look up the known revisions of pull request #{}: {}",
            number, e
        ),
    }
    warn!("{}", message);
    (StatusCode::BAD_GATEWAY, message)
}

/// What the route does once the head revision is queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrReviewStep {
    /// Nothing more; the outcome is reported as it is.
    Report(PrReviewOutcome),
    /// Rerun the revision, which queuing found finished in this status, and
    /// report RerunQueued.
    Rerun(crate::ReviewStatus),
}

/// Maps what queuing the head revision found to what the route does. A row
/// still Fetching, waiting or under review is in progress, and so is one
/// with a status no writer stores, which is not known to have finished. A
/// Reviewed or Skipped row has finished, and RerunHead reruns it. Queuing
/// resets a Cancelled, Partial, Failed or FailedToApply row, so Kept never
/// holds one; were it to, the row ended without being queued again, which is
/// reported as finished too, so that RerunHead can rerun it.
fn pr_review_step(found: &crate::db::PlaceholderState, mode: PrReviewMode) -> PrReviewStep {
    use crate::ReviewStatus::*;
    use crate::db::PlaceholderState::*;
    match (found, mode) {
        (Created | Reset { .. }, _) => PrReviewStep::Report(PrReviewOutcome::Queued),
        (AlreadyFetching | KeptUnknown { .. } | Kept(Incomplete | Pending | InReview), _) => {
            PrReviewStep::Report(PrReviewOutcome::AlreadyInProgress)
        }
        (
            Kept(Reviewed | Skipped | Cancelled | Partial | Failed | FailedToApply),
            PrReviewMode::ReviewHead,
        ) => PrReviewStep::Report(PrReviewOutcome::AlreadyFinished),
        (
            Kept(status @ (Reviewed | Skipped | Cancelled | Partial | Failed | FailedToApply)),
            PrReviewMode::RerunHead,
        ) => PrReviewStep::Rerun(*status),
    }
}

/// The reply for a queued head revision, with one line for the operator.
/// The message names the pull request by number and commit only: the title
/// is the forge's text and is shown escaped where it is stored instead.
fn pr_review_response(
    outcome: PrReviewOutcome,
    placeholder: &crate::db::FetchingPlaceholder,
    resolved: &crate::forge::ResolvedPullRequest,
) -> PrReviewResponse {
    let number = resolved.number;
    let id = placeholder.id;
    let head = resolved.head.as_str();
    let short = short_head(resolved);
    let previous_status = placeholder.state.previous_status().map(str::to_string);
    let was = match previous_status.as_deref() {
        Some("") | None => "no status".to_string(),
        Some(status) => status.to_string(),
    };
    let message = match (outcome, &placeholder.state) {
        (PrReviewOutcome::Queued, crate::db::PlaceholderState::Created) => {
            format!("Queued PR #{number} (head {short}) as patchset {id}")
        }
        (PrReviewOutcome::Queued, _) => format!(
            "PR #{number} head {short} had {was} as patchset {id}{}; queued again",
            version_label(&placeholder.standing)
        ),
        (PrReviewOutcome::AlreadyInProgress, _) => {
            format!("PR #{number} head {short} is already being reviewed as patchset {id} ({was})")
        }
        (PrReviewOutcome::AlreadyFinished, _) => format!(
            "PR #{number} head {short} already finished as patchset {id} ({was}); \
             use rerun --pr {number} to review it again"
        ),
        (PrReviewOutcome::RerunQueued, _) => {
            format!("Rerun of PR #{number} queued as patchset {id}")
        }
    };
    PrReviewResponse {
        outcome,
        patchset_id: id,
        number,
        head: head.to_string(),
        previous_status,
        message,
    }
}

async fn list_bugs(
    principal: Principal,
    State(state): State<Arc<AppState>>,
    Query(query): Query<BugListQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let scope = principal.maintained_section_names();
    let page = query.page.unwrap_or(1).max(1);
    let per_page = query.per_page.unwrap_or(50).clamp(1, 100);

    let min_sev = query
        .min_severity
        .as_deref()
        .or(query.severity.as_deref())
        .map(crate::db::Severity::from_str);

    let parsed_subsystems: Option<Vec<String>> = if let Some(ref subs_str) = query.subsystems {
        let subs: Vec<String> = if subs_str.trim().starts_with('[') {
            serde_json::from_str(subs_str).unwrap_or_else(|_| {
                subs_str
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
        } else {
            subs_str
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        Some(subs)
    } else if let Some(ref sub_str) = query.subsystem
        && sub_str.contains(',')
    {
        let subs: Vec<String> = sub_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Some(subs)
    } else {
        None
    };

    // Unknown filter values are rejected outright: silently returning an empty
    // list would look identical to "no bugs match" and hide the typo.
    let lifecycle_status = match query.lifecycle_status.as_deref().map(str::parse) {
        Some(Ok(status)) => Some(status),
        Some(Err(_)) => return Err(StatusCode::BAD_REQUEST),
        None => None,
    };
    let pipeline_state = match query.pipeline_state.as_deref().map(str::parse) {
        Some(Ok(state)) => Some(state),
        Some(Err(_)) => return Err(StatusCode::BAD_REQUEST),
        None => None,
    };
    let assignee = query
        .assignee
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|who| {
            if who.eq_ignore_ascii_case("none") {
                crate::db::AssigneeFilter::Unassigned
            } else {
                crate::db::AssigneeFilter::Is(who)
            }
        });

    match state
        .db
        .list_bugs(crate::db::ListBugsParams {
            page: Some(page as u32),
            limit: Some(per_page as u32),
            min_severity: min_sev,
            subsystem: if parsed_subsystems.is_none() {
                query.subsystem.as_deref()
            } else {
                None
            },
            subsystems: parsed_subsystems.as_deref(),
            lifecycle_status,
            pipeline_state,
            assignee,
            search: query.q.as_deref(),
            sort_by: query.sort_by.as_deref(),
            sort_order: query.sort_order.as_deref(),
            visibility: principal.visibility(&scope),
        })
        .await
    {
        Ok((items, total)) => {
            let ids: Vec<_> = items.iter().map(|b| b.id).collect();
            let mut summaries = state
                .db
                .bug_discovery_summaries(&ids)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let mut serialized_items = Vec::new();
            for bug in &items {
                let mut val = serde_json::to_value(bug).unwrap_or(serde_json::json!({}));
                val["slug"] = serde_json::Value::String(bug.bugid.clone());
                val["problem"] = serde_json::Value::String(bug.problem().to_string());
                val["severity"] = serde_json::to_value(bug.severity()).unwrap();
                val["severity_explanation"] =
                    serde_json::to_value(bug.severity_explanation()).unwrap();
                val["description"] = serde_json::to_value(bug.description()).unwrap();
                val["inline_review"] = serde_json::Value::String(bug.inline_review());
                val["locations"] = serde_json::to_value(bug.locations()).unwrap();
                val["source_files"] = serde_json::to_value(bug.source_files()).unwrap();
                val["introduced_in_commit"] =
                    serde_json::to_value(bug.introduced_in_commit()).unwrap();
                val["verified_on_sha"] = serde_json::to_value(bug.verified_on_sha()).unwrap();
                val["is_fixed"] = serde_json::Value::Bool(bug.is_fixed());
                val["fixed_in_commit"] = serde_json::to_value(bug.fixed_in_commit()).unwrap();
                val["raw_input"] = serde_json::to_value(bug.raw_input()).unwrap();
                val["tokens_in"] = serde_json::Value::Number(bug.tokens_in().into());
                val["tokens_out"] = serde_json::Value::Number(bug.tokens_out().into());
                val["tokens_cached"] = serde_json::Value::Number(bug.tokens_cached().into());
                val.as_object_mut().map(|obj| obj.remove("enrichments"));
                val["evidence"] = summaries.remove(&bug.id).unwrap_or_else(|| serde_json::json!({"count": 0, "models": [], "tools": [], "unknown_models": 0}));
                val.as_object_mut().unwrap().remove("raw_input");
                val.as_object_mut().unwrap().remove("vector_json");
                serialized_items.push(val);
            }
            Ok(Json(serde_json::json!({
                "items": serialized_items,
                "total": total,
                "page": page,
                "per_page": per_page
            })))
        }
        Err(e) => {
            tracing::error!("Failed to fetch bugs list: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn list_bug_subsystems(
    principal: Principal,
    State(state): State<Arc<AppState>>,
    Query(query): Query<BugSubsystemsQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let lifecycle_status = match query.lifecycle_status.as_deref().map(str::parse) {
        Some(Ok(status)) => status,
        Some(Err(_)) => return Err(StatusCode::BAD_REQUEST),
        None => crate::db::BugLifecycleStatus::Open,
    };
    let scope = principal.maintained_section_names();
    let visibility = principal.visibility(&scope);
    // Key the cache on the parsed status so the alias and the canonical
    // spelling do not each get their own entry, and on the principal's scope so
    // that one caller's counts are never served to a caller with a different
    // reach. Every principal who reads everything shares one entry.
    let scope_key = match visibility {
        crate::db::BugVisibility::Unrestricted => "*".to_string(),
        crate::db::BugVisibility::Sections(sections) => sections.join("\n"),
    };
    let cache_key = Some(format!("{}|{}", lifecycle_status.as_str(), scope_key));

    let res = state
        .bug_subsystems_cache
        .get_or_fetch(cache_key, || async {
            let db = state.db.clone();
            let counts = db
                .get_subsystems_bug_counts(Some(lifecycle_status), visibility)
                .await
                .map_err(|e| {
                    tracing::error!("Failed to fetch bug subsystem counts: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;

            let list: Vec<serde_json::Value> = counts
                .into_iter()
                .map(|(name, count)| {
                    serde_json::json!({
                        "name": name,
                        "count": count,
                        "open_bugs": count,
                    })
                })
                .collect();
            Ok::<Vec<serde_json::Value>, StatusCode>(list)
        })
        .await?;

    Ok(Json(serde_json::Value::Array(res)))
}

/// Checks that a string looks like an email address.
///
/// This deliberately stops well short of RFC validation. Its only job is to
/// catch a value that clearly is not an address, since assignees are matched
/// by exact string and a malformed one can never be filtered for.
fn is_plausible_email(value: &str) -> bool {
    match value.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !value.chars().any(char::is_whitespace)
        }
        None => false,
    }
}

/// The authority each action demands.
///
/// Commenting is a conversation and is open to anyone who may read the bug,
/// which includes the kernel security list. Everything else rewrites the bug's
/// state and belongs to whoever maintains the affected code.
fn required_access(action: &BugAction) -> BugAccess {
    match action {
        BugAction::Comment { .. } => BugAccess::Comment,
        BugAction::Close { .. }
        | BugAction::Dismiss { .. }
        | BugAction::MarkDuplicate { .. }
        | BugAction::Assign { .. } => BugAccess::Manage,
    }
}

/// Renders a failed bug lookup for an endpoint that answers with a message.
///
/// A bug the caller has no authority over is reported as absent, exactly as
/// the read endpoints report it, so that the two cases stay indistinguishable.
fn bug_lookup_denial(status: StatusCode) -> (StatusCode, String) {
    let message = match status {
        StatusCode::BAD_REQUEST => "Missing id or bugid param",
        StatusCode::NOT_FOUND => "Bug not found",
        StatusCode::UNAUTHORIZED => "Please log in to access bugs repository.",
        _ => "Database error",
    };
    (status, message.to_string())
}

/// Longest part of a bug title, in bytes, that an assignment notice's
/// subject carries.
const ASSIGNMENT_SUBJECT_TITLE_BYTES: usize = 80;

/// Longest part of the assigner's note, in bytes, that an assignment notice
/// carries. The whole note is on the bug page as a comment.
const ASSIGNMENT_NOTE_BYTES: usize = 4096;

/// Renders the subject and body of the notice sent to a bug's new assignee.
fn build_bug_assignment_email(
    assigner: &str,
    title: &str,
    link: &str,
    note: Option<&str>,
    base_url: &str,
) -> (String, String) {
    // Titles are model output and can span lines, which a subject cannot.
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let short_title = crate::utils::utf8_prefix(&title, ASSIGNMENT_SUBJECT_TITLE_BYTES);
    let ellipsis = if short_title.len() < title.len() {
        "..."
    } else {
        ""
    };
    let subject = format!("[sashiko] {assigner} assigned you a bug: {short_title}{ellipsis}");

    let note_block = match note.map(str::trim).filter(|n| !n.is_empty()) {
        Some(note) => {
            let shown = crate::utils::utf8_prefix(note, ASSIGNMENT_NOTE_BYTES);
            let mut indented: Vec<String> = shown
                .lines()
                .map(|line| match line.trim_end() {
                    "" => String::new(),
                    line => format!("  {line}"),
                })
                .collect();
            if shown.len() < note.len() {
                indented.push("  [...] The full note is on the bug page.".to_string());
            }
            format!("Their note:\n\n{}\n\n", indented.join("\n"))
        }
        None => String::new(),
    };
    let body = format!(
        "{assigner} assigned you this Sashiko bug report:\n\n\
         \x20 {title}\n\
         \x20 {link}\n\n\
         {note_block}\
         The link opens the report without signing in, for anyone who has it.\n\n\
         Questions about the assignment go to {assigner}, who is on Cc.\n\n\
         -- \n\
         Sashiko AI review · {base_url}"
    );
    (subject, body)
}

/// Whether an assignment notice may go to `address`.
///
/// Email to a list address lands in the list's public archive with the bug's
/// link, and an address that is not a plain one, Sashiko's own, or a
/// blocklisted one is not a person to notify.
fn is_notifiable_address(
    state: &AppState,
    smtp: &crate::settings::SmtpSettings,
    address: &str,
) -> bool {
    let Ok(parsed) = address.parse::<lettre::Address>() else {
        return false;
    };
    let domain = parsed.domain().to_ascii_lowercase();
    !crate::email_router::KNOWN_MAILING_LIST_DOMAINS.contains(&domain.as_str())
        && crate::email_policy::extract_bare_email(&smtp.sender_address)
            != address.to_ascii_lowercase()
        && !state.settings.server.acl.is_blocklisted(address)
}

/// The notice to queue with an assignment.
struct AssignmentNotice {
    subject: String,
    body: String,
    status: &'static str,
}

/// Builds the email to a bug's new assignee, naming who assigned it, or
/// None when no notice is due. assign_bug() drops it when the assignee is
/// unchanged.
fn assignment_notice(
    state: &AppState,
    bug: &crate::db::Bug,
    assigner: &str,
    assignee: &str,
    note: Option<&str>,
) -> Option<AssignmentNotice> {
    let smtp = state.settings.smtp.as_ref()?;
    // Nobody to name, or an assignee who already knows. Addresses that
    // differ only in case name one person.
    if assigner.is_empty() || assignee.eq_ignore_ascii_case(assigner) {
        return None;
    }
    if !is_notifiable_address(state, smtp, assignee) {
        tracing::warn!(
            "Not emailing the new assignee of bug {}: {} is a list, Sashiko's own, \
             blocklisted or not a plain address",
            bug.bugid,
            assignee
        );
        return None;
    }

    let Some(base_url) = state.settings.server.mailed_link_base_url() else {
        tracing::error!(
            "Not emailing the new assignee of bug {}: server.public_base_url is \
             not a reachable URL",
            bug.bugid
        );
        return None;
    };
    let link = format!("{}/bug/{}", base_url, bug.bugid);
    let (subject, body) = build_bug_assignment_email(assigner, &bug.title, &link, note, &base_url);
    let status = if smtp.dry_run { "Dry-Run" } else { "Pending" };
    Some(AssignmentNotice {
        subject,
        body,
        status,
    })
}

async fn bug_action(
    principal: Principal,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<BugQuery>,
    axum::extract::Json(payload): axum::extract::Json<BugActionPayload>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if state.read_only {
        return Err((
            StatusCode::FORBIDDEN,
            "Server is running in read-only mode.".into(),
        ));
    }

    // The check can only run once the bug is loaded, because what the caller
    // may do depends on which subsystems the bug is attributed to.
    let bug = readable_bug(&state, &principal, &query)
        .await
        .map_err(bug_lookup_denial)?;
    let access = bug_access(&state, &principal, bug.id)
        .await
        .map_err(bug_lookup_denial)?;
    let required = required_access(&payload.action);
    if access < required {
        return Err((
            StatusCode::FORBIDDEN,
            format!(
                "This action requires {} authority over the bug.",
                required.describe()
            ),
        ));
    }

    // Testing mode resolves an operator that proved no address, so fall back to
    // naming the peer rather than attributing the change to nobody.
    let actor = if principal.email().is_empty() {
        format!("authorized client ({})", addr.ip())
    } else {
        principal.email().to_string()
    };
    let tool = payload
        .tool
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("api");
    let model = payload.model.filter(|s| !s.trim().is_empty());
    let db = state.db.with_bug_actor(&actor, tool, model);
    match payload.action {
        BugAction::Comment { content } => {
            if content.trim().is_empty() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Comment content is required".into(),
                ));
            }
            db.add_bug_enrichment(
                bug.id,
                &crate::db::NewBugEnrichment {
                    kind: "comment".into(),
                    content: Some(content),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        BugAction::Close { reason } => {
            db.change_bug_status_with_reason(
                bug.id,
                crate::db::BugLifecycleStatus::Closed,
                reason.as_deref(),
            )
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        BugAction::Dismiss { reason } => {
            db.change_bug_status_with_reason(
                bug.id,
                crate::db::BugLifecycleStatus::Dismissed,
                reason.as_deref(),
            )
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        BugAction::MarkDuplicate {
            duplicate_of_id,
            duplicate_of_bugid,
            reasoning,
        } => {
            let target = match (duplicate_of_id, duplicate_of_bugid) {
                (Some(id), None) => db.get_bug(id).await,
                (None, Some(bugid)) => db.get_bug_by_bugid(bugid.trim()).await,
                _ => {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        "Provide one existing bug ID".into(),
                    ));
                }
            }
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            let target = target
                .filter(|b| b.id != bug.id && b.duplicate_of_id.is_none())
                .ok_or_else(|| {
                    (
                        StatusCode::BAD_REQUEST,
                        "Choose an existing canonical bug, distinct from this bug".into(),
                    )
                })?;
            // Marking a bug duplicate of one the caller cannot manage would
            // both disclose that the target exists and rewrite its history, so
            // that case answers exactly like a target that is not there.
            let target_access = bug_access(&state, &principal, target.id)
                .await
                .map_err(bug_lookup_denial)?;
            if !target_access.can_manage() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Choose an existing canonical bug, distinct from this bug".into(),
                ));
            }
            let duplicate_of_id = target.id;
            db.mark_bug_as_duplicate(crate::db::MarkDuplicateBugParams {
                ephemeral_id: bug.id,
                canonical_id: duplicate_of_id,
                reasoning: reasoning.as_deref().unwrap_or("Marked as duplicate"),
                ..Default::default()
            })
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        BugAction::Assign { assignee, reason } => {
            let assignee = assignee.as_deref().map(str::trim).filter(|s| !s.is_empty());
            // A bare sanity check, not full validation: assignees are matched
            // by exact string, so a value that is obviously not an address
            // would create an assignment nobody can ever filter for.
            if let Some(who) = assignee
                && !is_plausible_email(who)
            {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "Assignee must be an email address".into(),
                ));
            }
            let assigner = [principal.email()];
            let notice = assignee.and_then(|who| {
                assignment_notice(&state, &bug, assigner[0], who, reason.as_deref())
                    .map(|notice| (who, notice))
            });
            let email = notice
                .as_ref()
                .map(|(who, notice)| crate::db::TransactionalEmail {
                    kind: crate::db::EmailKind::BugAssignment,
                    status: notice.status,
                    to_address: who,
                    cc_addresses: &assigner,
                    subject: &notice.subject,
                    body: &notice.body,
                });
            db.assign_bug(bug.id, assignee, reason.as_deref(), email.as_ref())
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
    }

    Ok(Json(serde_json::json!({ "status": "success" })))
}
#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    /// The patchset page sends its page size as limit=, the API's own name is
    /// per_page=; the route reads either, decoded as axum decodes it, bounded
    /// as the list endpoints bound theirs, and an edited address that is not
    /// a number falls back to the default rather than failing the request.
    #[test]
    fn patch_query_reads_limit_as_the_page_size() {
        let page_size = |uri: &str| {
            let parsed: axum::http::Uri = uri.parse().unwrap();
            let Query(query) = Query::<PatchQuery>::try_from_uri(&parsed)
                .unwrap_or_else(|e| panic!("{uri} was refused: {e}"));
            assert_eq!(query.page, Some(2), "{uri}");
            query.page_size()
        };
        let max = Some(crate::api::MAX_PATCHSET_PAGE_SIZE);
        for (query, expected) in [
            ("limit=10", Some(10)),
            ("per_page=10", Some(10)),
            ("limit=4294967295", max),
            ("limit=99999999999999999999", max),
            ("limit=101", max),
            ("limit=0", Some(1)),
            ("limit=-5", Some(1)),
            ("limit=-99999999999999999999", Some(1)),
            ("limit=%2B7", Some(7)),
            ("limit=abc", None),
            ("limit=7abc", None),
            ("limit=-", None),
            ("limit=", None),
            ("x=1", None),
            ("per_page=20&limit=10", Some(20)),
            ("limit=10&per_page=20", Some(20)),
            ("per_page=abc&limit=10", Some(10)),
        ] {
            let uri = format!("/api/patchset?id=7&page=2&{query}");
            assert_eq!(page_size(&uri), expected, "{uri}");
        }
    }

    #[test]
    fn test_thread_message_id_remains_unencoded() {
        assert_eq!(
            normalize_thread_message_id("<message/part%25@example.com>"),
            "message/part%25@example.com"
        );
    }

    #[test]
    fn test_generate_synthetic_id_format() {
        let id = generate_synthetic_id("test");
        assert!(id.starts_with("sashiko-test-"));
        assert!(id.ends_with("@sashiko.local"));
    }

    const PR_BASE: &str = "1111111111111111111111111111111111111111";

    /// A state over an in-memory database whose fetch requests the test
    /// reads from the returned receiver.
    async fn pull_request_state() -> (AppState, mpsc::Receiver<FetchRequest>) {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();
        let (sender, _) = mpsc::channel(1);
        let (fetch_sender, fetch_rx) = mpsc::channel(16);
        let mut settings = crate::settings::Settings::new().unwrap();
        settings.forge.webhook_secret = None;
        let state = AppState {
            settings: Arc::new(settings),
            db,
            sender,
            fetch_sender,
            read_only: false,
            allow_all_submit: false,
            smtp_enabled: false,
            dry_run: true,
            forge_registry: Arc::new(crate::forge::ForgeRegistry::new()),
            sign_in_link_rate_limiter: SignInLinkRateLimiter::new(),
            stats_timeline_cache: AsyncMapCache::new(Duration::from_secs(60)),
            stats_reviews_cache: AsyncCache::new(Duration::from_secs(60)),
            stats_tools_cache: AsyncCache::new(Duration::from_secs(60)),
            messages_count_cache: AsyncCache::new(Duration::from_secs(30)),
            patchsets_count_cache: AsyncCache::new(Duration::from_secs(30)),
            patchsets_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
            messages_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
            bug_subsystems_cache: AsyncMapCache::new(Duration::from_secs(5)),
            local_token: None,
            activity: Default::default(),
            email_worker_heartbeat: None,
            pull_request_resolver: Arc::new(FakeResolver::default()),
        };
        (state, fetch_rx)
    }

    /// Pull request `number` of acme/widget, at a head named after it.
    fn widget_pull_request(number: i64) -> crate::forge::ForgeMetadata {
        crate::forge::ForgeMetadata {
            repo_url: Some("https://github.com/acme/widget.git".to_string()),
            base_sha: PR_BASE.to_string(),
            head_sha: format!("{number:040x}"),
            pr_number: number,
            pr_title: Some("Fix the widget".to_string()),
            pr_url: Some(format!("https://github.com/acme/widget/pull/{number}")),
            author: None,
            pr_body: None,
        }
    }

    /// How many fetch requests are waiting on the channel, taking them off.
    fn fetches_sent(fetch_rx: &mut mpsc::Receiver<FetchRequest>) -> usize {
        std::iter::from_fn(|| fetch_rx.try_recv().ok()).count()
    }

    /// Whether an operator asking for a revision by number fetches it
    /// again once its row is in `status`. Only a resubmittable row is reset
    /// and fetched; the others already have their patches. The match is
    /// exhaustive so that a new status must be placed here.
    fn manual_path_fetches_again(status: crate::ReviewStatus) -> bool {
        use crate::ReviewStatus::*;
        match status {
            Incomplete | Pending | InReview | Skipped | Reviewed => false,
            Cancelled | Partial | Failed | FailedToApply => true,
        }
    }

    /// Queues a fresh revision with `existing`, stores `status` on its row
    /// when one is given (otherwise it stays Fetching), queues it again,
    /// and returns what the second call found and how many fetches it sent.
    /// The first call always creates the row and sends one fetch.
    async fn requeue_revision(
        state: &AppState,
        fetch_rx: &mut mpsc::Receiver<FetchRequest>,
        number: i64,
        status: Option<&str>,
        existing: ExistingRevision,
    ) -> (crate::db::PlaceholderState, usize) {
        let metadata = widget_pull_request(number);
        let first = enqueue_pull_request(state, "GitHub", &metadata, existing)
            .await
            .unwrap();
        assert_eq!(first.state, crate::db::PlaceholderState::Created);
        assert_eq!(fetches_sent(fetch_rx), 1, "a new revision is fetched");
        store_status(state, first.id, status).await;
        let again = enqueue_pull_request(state, "GitHub", &metadata, existing)
            .await
            .unwrap();
        assert_eq!(again.id, first.id, "one revision has one row");
        (again.state, fetches_sent(fetch_rx))
    }

    /// Stores `status` on patchset `id` when one is given, as the stage
    /// that would have moved the row there does; `None` leaves it Fetching.
    async fn store_status(state: &AppState, id: i64, status: Option<&str>) {
        let Some(status) = status else {
            return;
        };
        state
            .db
            .conn
            .execute(
                "UPDATE patchsets SET status = ? WHERE id = ?",
                libsql::params![status, id],
            )
            .await
            .unwrap();
    }

    /// The stored statuses a revision is queued again from, with whether
    /// the manual path fetches it again from each. Fetching is the row a
    /// lost request left behind; "Embargoed" is a status no writer stores.
    fn requeued_statuses() -> Vec<(Option<&'static str>, bool)> {
        crate::ReviewStatus::ALL
            .iter()
            .map(|status| (Some(status.as_str()), manual_path_fetches_again(*status)))
            .chain([(None, true), (Some("Embargoed"), false)])
            .collect()
    }

    /// A webhook delivery queues a fetch whatever the revision's row was,
    /// as it did before the enqueue path was shared.
    #[tokio::test]
    async fn the_webhook_still_refetches_every_revision() {
        let (state, mut fetch_rx) = pull_request_state().await;
        for (number, (status, _)) in (1..).zip(requeued_statuses()) {
            let (found, sent) = requeue_revision(
                &state,
                &mut fetch_rx,
                number,
                status,
                ExistingRevision::Refetch,
            )
            .await;
            assert_eq!(sent, 1, "{status:?} was found as {found:?}");
        }
    }

    /// The manual path fetches a revision again only when its row was
    /// reset or is still Fetching, and leaves a pending, running or
    /// finished one alone.
    #[tokio::test]
    async fn the_manual_path_does_not_refetch_a_kept_revision() {
        let (state, mut fetch_rx) = pull_request_state().await;
        for (number, (status, fetches_again)) in (1..).zip(requeued_statuses()) {
            let (found, sent) = requeue_revision(
                &state,
                &mut fetch_rx,
                number,
                status,
                ExistingRevision::Leave,
            )
            .await;
            assert_eq!(
                sent,
                usize::from(fetches_again),
                "{status:?} was found as {found:?}"
            );
        }
    }

    /// A revision is queued with its pull request's description on its
    /// placeholder. Queued again, a new description replaces the stored one
    /// and a delivery without one leaves it, on both paths. A head that a
    /// newer revision outranks keeps the description it had, because the
    /// one sent now describes the pull request at its newer head.
    #[tokio::test]
    async fn a_queued_revision_records_its_description() {
        let (state, mut fetch_rx) = pull_request_state().await;
        let description = |id| {
            let db = state.db.clone();
            async move { db.get_patchset_mr_body(id).await.unwrap() }
        };
        let mut earlier = widget_pull_request(7);
        earlier.pr_body = Some("First description.".to_string());
        let id = enqueue_pull_request(&state, "GitHub", &earlier, ExistingRevision::Refetch)
            .await
            .unwrap()
            .id;
        assert_eq!(description(id).await.as_deref(), Some("First description."));

        earlier.pr_body = None;
        enqueue_pull_request(&state, "GitHub", &earlier, ExistingRevision::Leave)
            .await
            .unwrap();
        assert_eq!(description(id).await.as_deref(), Some("First description."));

        earlier.pr_body = Some("Edited description.".to_string());
        enqueue_pull_request(&state, "GitHub", &earlier, ExistingRevision::Leave)
            .await
            .unwrap();
        assert_eq!(
            description(id).await.as_deref(),
            Some("Edited description.")
        );

        let mut pushed = widget_pull_request(7);
        pushed.head_sha = format!("{:040x}", 0x99);
        pushed.pr_body = Some("Newest description.".to_string());
        let newer = enqueue_pull_request(&state, "GitHub", &pushed, ExistingRevision::Refetch)
            .await
            .unwrap()
            .id;
        assert_eq!(
            description(newer).await.as_deref(),
            Some("Newest description.")
        );

        earlier.pr_body = Some("Late description.".to_string());
        let outranked = enqueue_pull_request(&state, "GitHub", &earlier, ExistingRevision::Refetch)
            .await
            .unwrap();
        assert_eq!(outranked.id, id);
        assert_eq!(
            outranked.standing.superseded_by.map(|revision| revision.id),
            Some(newer)
        );
        assert_eq!(
            description(id).await.as_deref(),
            Some("Edited description.")
        );
        assert_eq!(
            description(newer).await.as_deref(),
            Some("Newest description.")
        );
        fetches_sent(&mut fetch_rx);
    }

    /// A revision queued again from any stored status, on either path,
    /// records the description it is queued with, whether its row is kept
    /// or reset, so that a rerun of a reviewed head reads the description
    /// as of the latest delivery.
    #[tokio::test]
    async fn a_requeued_revision_records_its_new_description_in_every_status() {
        let (state, mut fetch_rx) = pull_request_state().await;
        let cases = requeued_statuses().into_iter().flat_map(|(status, _)| {
            [ExistingRevision::Refetch, ExistingRevision::Leave].map(|path| (status, path))
        });
        for (number, (status, existing)) in (1..).zip(cases) {
            let mut metadata = widget_pull_request(number);
            metadata.pr_body = Some("Queued description.".to_string());
            let id = enqueue_pull_request(&state, "GitHub", &metadata, existing)
                .await
                .unwrap()
                .id;
            store_status(&state, id, status).await;
            metadata.pr_body = Some("Requeued description.".to_string());
            let again = enqueue_pull_request(&state, "GitHub", &metadata, existing)
                .await
                .unwrap();
            assert_eq!(again.id, id, "one revision has one row");
            assert_eq!(
                state.db.get_patchset_mr_body(id).await.unwrap().as_deref(),
                Some("Requeued description."),
                "{status:?} on the {existing:?} path was found as {:?}",
                again.state
            );
            // The fetch channel is bounded, so each case takes its fetches
            // off before the next one queues more.
            fetches_sent(&mut fetch_rx);
        }
    }

    /// A submitted range carries its --b4-cover-letter flag to the fetcher
    /// as the request gave it, false when the key is absent, as it is in
    /// every request sent before the flag existed; a range submitted as
    /// "remote" carries it the same way. Each case names its own range,
    /// because a range already queued is not fetched again.
    #[tokio::test]
    async fn a_submitted_range_passes_its_b4_flag_to_the_fetcher() {
        let (mut state, mut fetch_rx) = pull_request_state().await;
        state.allow_all_submit = true;
        let state = Arc::new(state);
        let cases = [
            ("remote-range", None, false),
            ("remote-range", Some(true), true),
            ("remote", Some(true), true),
        ];
        for (head, (kind, flag, flagged)) in (1..).zip(cases) {
            let range = format!("{PR_BASE}..{head:040x}");
            let mut payload = serde_json::json!({"type": kind, "sha": range});
            if let Some(flag) = flag {
                payload["b4_cover_letter"] = serde_json::json!(flag);
            }
            let request: SubmitRequest = serde_json::from_value(payload.clone()).unwrap();
            let Json(reply) = submit_patch(
                crate::auth::OptionalAuthUser(None),
                ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))),
                axum::http::HeaderMap::new(),
                State(state.clone()),
                Json(request),
            )
            .await
            .unwrap_or_else(|status| panic!("{payload} was refused with {status}"));
            assert_eq!(reply.status, "accepted", "{payload}");
            let sent = fetch_rx.try_recv().ok();
            assert_eq!(
                sent.map(|request| (
                    request.commit_hash,
                    request.mr_number,
                    request.b4_cover_letter
                )),
                Some((range, None, flagged)),
                "{payload}"
            );
        }
    }

    /// A GitHub delivery still produces the placeholder id, subject, slug
    /// and fetch request it did before the enqueue path was shared, and a
    /// second delivery for a finished revision still queues a fetch while
    /// leaving the row as it was.
    #[tokio::test]
    async fn the_webhook_queues_the_same_placeholder_subject_and_slug() {
        let (mut state, mut fetch_rx) = pull_request_state().await;
        // Without a webhook secret the gate admits only the local token or
        // the unsafe flag; the flag is the simpler of the two to set here.
        state.allow_all_submit = true;
        let state = Arc::new(state);
        let head = format!("{:040x}", 7);
        let payload = serde_json::json!({
            "action": "opened",
            "pull_request": {
                "number": 7,
                "title": "Fix the widget",
                "html_url": "https://github.com/acme/widget/pull/7",
                "head": { "sha": head },
                "base": { "sha": PR_BASE },
                "user": { "login": "alice" },
            },
            "repository": { "clone_url": "https://github.com/acme/widget.git" },
        });
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-github-event", "pull_request".parse().unwrap());
        let deliver = || {
            forge_webhook(
                ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))),
                State(state.clone()),
                Path("github".to_string()),
                headers.clone(),
                axum::body::Bytes::from(payload.to_string()),
            )
        };

        let Json(reply) = deliver().await.unwrap_or_else(|status| {
            panic!("webhook refused with {status}");
        });
        assert_eq!(reply["status"], "accepted");
        let placeholder_id = format!("mr-7-{PR_BASE}..{head}@sashiko.local");
        let expected_fetch = FetchRequest {
            repo_url: Some("https://github.com/acme/widget.git".to_string()),
            commit_hash: format!("{PR_BASE}..{head}"),
            mr_url: Some("https://github.com/acme/widget/pull/7".to_string()),
            mr_title: Some("Fix the widget".to_string()),
            mr_number: Some(7),
            b4_cover_letter: false,
        };
        assert_eq!(fetch_rx.try_recv().ok(), Some(expected_fetch.clone()));

        let mut rows = state
            .db
            .conn
            .query(
                "SELECT id, subject, slug, mr_number, status FROM patchsets \
                 WHERE cover_letter_message_id = ?",
                libsql::params![placeholder_id],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().expect("the placeholder row");
        let id: i64 = row.get(0).unwrap();
        assert_eq!(row.get::<String>(1).unwrap(), "#7: Fix the widget");
        assert_eq!(row.get::<String>(2).unwrap(), "widget-7");
        assert_eq!(row.get::<i64>(3).unwrap(), 7);
        assert_eq!(row.get::<String>(4).unwrap(), "Fetching");

        state
            .db
            .conn
            .execute(
                "UPDATE patchsets SET status = 'Reviewed' WHERE id = ?",
                libsql::params![id],
            )
            .await
            .unwrap();
        let Json(again) = deliver().await.unwrap();
        assert_eq!(again["status"], "accepted");
        assert_eq!(fetch_rx.try_recv().ok(), Some(expected_fetch));
        let status: String = state
            .db
            .conn
            .query(
                "SELECT status FROM patchsets WHERE id = ?",
                libsql::params![id],
            )
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
            .expect("the row")
            .get(0)
            .unwrap();
        assert_eq!(status, "Reviewed");
    }

    /// Stands in for gh. It answers pull request N of any repository with
    /// base PR_BASE, head N in hex and the description fake_description(N),
    /// or with `failure` when one is set, and counts the questions it is
    /// asked. No test runs gh.
    #[derive(Default)]
    struct FakeResolver {
        failure: Option<crate::forge::GhError>,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl FakeResolver {
        fn failing(failure: crate::forge::GhError) -> Self {
            Self {
                failure: Some(failure),
                ..Default::default()
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl crate::forge::PullRequestResolver for FakeResolver {
        async fn resolve(
            &self,
            repo: &crate::settings::ForgeRepo,
            number: i64,
        ) -> Result<crate::forge::ResolvedPullRequest, crate::forge::GhError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(failure) = &self.failure {
                return Err(failure.clone());
            }
            let commit = |hex: &str| CommitId::parse(hex).expect("a full commit id");
            Ok(crate::forge::ResolvedPullRequest {
                repo: repo.clone(),
                number,
                url: format!(
                    "https://{}/{}/{}/pull/{number}",
                    repo.host, repo.owner, repo.name
                ),
                title: Some("Fix the widget".to_string()),
                body: Some(fake_description(number)),
                base: commit(PR_BASE),
                head: commit(&format!("{number:040x}")),
            })
        }
    }

    /// The description FakeResolver gives pull request `number`.
    fn fake_description(number: i64) -> String {
        format!("Pull request {number} makes the widget reentrant.")
    }

    /// A state for /api/pr/review over acme/widget that admits every caller
    /// and asks `resolver` instead of gh, with `configure` applied to its
    /// settings last.
    async fn pr_route_state(
        resolver: Arc<FakeResolver>,
        configure: impl FnOnce(&mut crate::settings::Settings),
    ) -> (Arc<AppState>, mpsc::Receiver<FetchRequest>) {
        let (mut state, fetch_rx) = pull_request_state().await;
        let mut settings = (*state.settings).clone();
        settings.forge.github_repository = Some("acme/widget".parse().unwrap());
        configure(&mut settings);
        state.settings = Arc::new(settings);
        state.allow_all_submit = true;
        state.pull_request_resolver = resolver;
        (Arc::new(state), fetch_rx)
    }

    fn pr_request(number: i64, mode: PrReviewMode) -> PrReviewRequest {
        PrReviewRequest {
            number,
            repo: None,
            mode,
        }
    }

    /// Sends `request` to the route as an anonymous caller.
    async fn ask_for_pr(
        state: &Arc<AppState>,
        request: PrReviewRequest,
    ) -> Result<PrReviewResponse, PrReviewRefusal> {
        review_pull_request(
            crate::auth::OptionalAuthUser(None),
            axum::http::HeaderMap::new(),
            State(state.clone()),
            Json(request),
        )
        .await
        .map(|Json(response)| response)
    }

    async fn set_patchset_status(state: &AppState, id: i64, status: &str) {
        state
            .db
            .conn
            .execute(
                "UPDATE patchsets SET status = ? WHERE id = ?",
                libsql::params![status, id],
            )
            .await
            .unwrap();
    }

    /// Every state queuing can find maps to the outcome section 4.2.3's
    /// table gives it, in both modes. Kept is walked over every status, so a
    /// status added later must be placed.
    #[test]
    fn pr_review_follows_the_state_table() {
        use crate::db::PlaceholderState;
        use PrReviewOutcome::*;
        let report = PrReviewStep::Report;
        let fixed = [
            (PlaceholderState::Created, report(Queued), report(Queued)),
            (
                PlaceholderState::Reset {
                    was: crate::ReviewStatus::Failed,
                },
                report(Queued),
                report(Queued),
            ),
            (
                PlaceholderState::AlreadyFetching,
                report(AlreadyInProgress),
                report(AlreadyInProgress),
            ),
            (
                PlaceholderState::KeptUnknown {
                    status: String::new(),
                },
                report(AlreadyInProgress),
                report(AlreadyInProgress),
            ),
        ];
        let kept = crate::ReviewStatus::ALL.map(|status| {
            use crate::ReviewStatus::*;
            let (review, rerun) = match status {
                Incomplete | Pending | InReview => {
                    (report(AlreadyInProgress), report(AlreadyInProgress))
                }
                Reviewed | Skipped | Cancelled | Partial | Failed | FailedToApply => {
                    (report(AlreadyFinished), PrReviewStep::Rerun(status))
                }
            };
            (PlaceholderState::Kept(status), review, rerun)
        });
        for (found, review, rerun) in fixed.into_iter().chain(kept) {
            assert_eq!(
                pr_review_step(&found, PrReviewMode::ReviewHead),
                review,
                "{found:?}"
            );
            assert_eq!(
                pr_review_step(&found, PrReviewMode::RerunHead),
                rerun,
                "{found:?}"
            );
        }
    }

    /// A state whose ACL lets one user ingest, one review and one do both,
    /// and which publishes `token`.
    async fn pr_auth_state(read_only: bool, token: &crate::auth::LocalToken) -> Arc<AppState> {
        let (mut state, _) = pull_request_state().await;
        let mut settings = (*state.settings).clone();
        settings.server.testing_mode = false;
        settings.server.acl.ingest = vec![
            "ingester@example.org".to_string(),
            "both@example.org".to_string(),
        ];
        settings.server.acl.review = vec![
            "reviewer@example.org".to_string(),
            "both@example.org".to_string(),
        ];
        state.settings = Arc::new(settings);
        state.read_only = read_only;
        state.local_token = Some(token.clone());
        Arc::new(state)
    }

    /// Who may make a request in `mode`: the local token, each ACL user,
    /// and nobody at all, on a writable server and on a read-only one.
    async fn pr_review_admits(mode: PrReviewMode) -> Vec<(&'static str, bool, bool)> {
        let token = crate::auth::LocalToken::generate().unwrap();
        let mut with_token = axum::http::HeaderMap::new();
        with_token.insert(
            "authorization",
            format!("Bearer {}", token.secret()).parse().unwrap(),
        );
        let bare = axum::http::HeaderMap::new();
        let writable = pr_auth_state(false, &token).await;
        let read_only = pr_auth_state(true, &token).await;
        let callers = [
            ("token", &with_token, None),
            ("ingester", &bare, Some("ingester@example.org")),
            ("reviewer", &bare, Some("reviewer@example.org")),
            ("both", &bare, Some("both@example.org")),
            ("nobody", &bare, None),
        ];
        callers
            .into_iter()
            .map(|(who, headers, email)| {
                let user = email.map(crate::auth::AuthUser::new);
                let admits = |state: &Arc<AppState>| {
                    authorize_pr_review(state, headers, user.as_ref(), mode).is_ok()
                };
                (who, admits(&writable), admits(&read_only))
            })
            .collect()
    }

    /// Reviewing a head needs Ingest alone; read-only refuses everyone.
    #[tokio::test]
    async fn review_pr_needs_ingest() {
        assert_eq!(
            pr_review_admits(PrReviewMode::ReviewHead).await,
            vec![
                ("token", true, false),
                ("ingester", true, false),
                ("reviewer", false, false),
                ("both", true, false),
                ("nobody", false, false),
            ]
        );
    }

    /// Rerunning a head needs Review as well as Ingest, so neither an
    /// ingest-only nor a review-only user may; read-only refuses everyone.
    #[tokio::test]
    async fn rerun_pr_needs_review_permission() {
        assert_eq!(
            pr_review_admits(PrReviewMode::RerunHead).await,
            vec![
                ("token", true, false),
                ("ingester", false, false),
                ("reviewer", false, false),
                ("both", true, false),
                ("nobody", false, false),
            ]
        );
    }

    /// Runs git in `dir`, failing the test if git does.
    fn git_in(dir: &std::path::Path, args: &[&str]) {
        let status = crate::git_cmd::in_dir(dir)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    /// The daemon's repository can say which GitHub repository it came
    /// from, and holds a fetcher remote on GitHub, but without
    /// forge.github_repository the route refuses with 422 and asks gh
    /// nothing: no remote decides what a number means.
    #[tokio::test]
    async fn a_number_is_never_resolved_against_a_remote() {
        let repo = tempfile::tempdir().unwrap();
        git_in(repo.path(), &["init", "--quiet"]);
        git_in(
            repo.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://gitlab.example.org/acme/widget.git",
            ],
        );
        git_in(
            repo.path(),
            &[
                "remote",
                "add",
                "fetcher-0123456789abcdef",
                "https://github.com/acme/widget.git",
            ],
        );
        let gh = Arc::new(FakeResolver::default());
        let (state, mut fetch_rx) = pr_route_state(gh.clone(), |settings| {
            settings.git.repository_path = repo.path().display().to_string();
            settings.forge.github_repository = None;
        })
        .await;

        let (status, reason) = ask_for_pr(&state, pr_request(7, PrReviewMode::ReviewHead))
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(reason.contains("forge.github_repository"), "{reason}");
        assert_eq!(gh.calls(), 0, "gh was asked");
        assert_eq!(fetches_sent(&mut fetch_rx), 0);
    }

    /// A request that cannot name a pull request of the configured
    /// repository is refused before gh is asked: a number below 1, a GitLab
    /// forge, and a URL of another repository or of the same name on another
    /// host, whose refusal names both. The URL's host and repository are
    /// compared without case, as GitHub does.
    #[tokio::test]
    async fn a_pr_request_is_checked_before_gh_is_asked() {
        let gh = Arc::new(FakeResolver::default());
        let (state, mut fetch_rx) = pr_route_state(gh.clone(), |_| {}).await;
        let refusal = |request| {
            let state = state.clone();
            async move { ask_for_pr(&state, request).await.unwrap_err() }
        };

        for number in [0, -1] {
            let (status, _) = refusal(pr_request(number, PrReviewMode::ReviewHead)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "#{number}");
        }
        let (status, reason) = refusal(PrReviewRequest {
            repo: Some("other/widget".to_string()),
            ..pr_request(7, PrReviewMode::ReviewHead)
        })
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(reason.contains("other/widget"), "{reason}");
        assert!(reason.contains("github.com/acme/widget"), "{reason}");
        for foreign in [
            "ghe.example.com/acme/widget",
            "github.com:8443/acme/widget",
            "acme/widget/extra",
        ] {
            let (status, reason) = refusal(PrReviewRequest {
                repo: Some(foreign.to_string()),
                ..pr_request(7, PrReviewMode::ReviewHead)
            })
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{foreign}");
            assert!(reason.contains(foreign), "{reason}");
        }

        let (gitlab_state, _) = pr_route_state(gh.clone(), |settings| {
            settings.forge.provider = Some("gitlab".to_string());
        })
        .await;
        let (status, reason) = ask_for_pr(&gitlab_state, pr_request(7, PrReviewMode::ReviewHead))
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(reason.contains("GitHub only"), "{reason}");
        assert_eq!(gh.calls(), 0, "gh was asked");

        for (number, given) in [(7, "ACME/Widget"), (8, "GitHub.com/acme/WIDGET")] {
            let queued = ask_for_pr(
                &state,
                PrReviewRequest {
                    repo: Some(given.to_string()),
                    ..pr_request(number, PrReviewMode::ReviewHead)
                },
            )
            .await
            .unwrap();
            assert_eq!(queued.outcome, PrReviewOutcome::Queued, "{given}");
            assert_eq!(fetches_sent(&mut fetch_rx), 1, "{given}");
        }
        assert_eq!(gh.calls(), 2);
    }

    /// The route's outcome, reply and fetches for one head revision as its
    /// row moves through the statuses: queued when new, in progress while
    /// Fetching (with the fetch sent again, to recover a lost request) and
    /// while Pending (with none), finished once Reviewed, rerun under
    /// RerunHead, and queued again after a failure.
    #[tokio::test]
    async fn the_pr_route_reports_what_it_did_with_the_head() {
        let gh = Arc::new(FakeResolver::default());
        let (state, mut fetch_rx) = pr_route_state(gh.clone(), |_| {}).await;
        let ask = |mode| {
            let state = state.clone();
            async move { ask_for_pr(&state, pr_request(7, mode)).await.unwrap() }
        };
        use PrReviewMode::*;
        use PrReviewOutcome::*;

        let first = ask(ReviewHead).await;
        let id = first.patchset_id;
        assert_eq!(
            first,
            PrReviewResponse {
                outcome: Queued,
                patchset_id: id,
                number: 7,
                head: format!("{:040x}", 7),
                previous_status: None,
                message: format!("Queued PR #7 (head 00000000) as patchset {id}"),
            }
        );
        assert_eq!(fetches_sent(&mut fetch_rx), 1);

        let fetching = ask(ReviewHead).await;
        assert_eq!(
            (fetching.outcome, fetching.previous_status.as_deref()),
            (AlreadyInProgress, Some("Fetching"))
        );
        assert_eq!(fetches_sent(&mut fetch_rx), 1, "a lost fetch is recovered");

        // The rows below are each set by hand, then asked about once; the
        // expected fetch count says whether the route fetched again.
        let steps: [(&str, PrReviewMode, PrReviewOutcome, usize, &str); 5] = [
            (
                "Pending",
                ReviewHead,
                AlreadyInProgress,
                0,
                "is already being reviewed",
            ),
            ("Pending", RerunHead, AlreadyInProgress, 0, "(Pending)"),
            (
                "Reviewed",
                ReviewHead,
                AlreadyFinished,
                0,
                "use rerun --pr 7",
            ),
            (
                "Reviewed",
                RerunHead,
                RerunQueued,
                0,
                "Rerun of PR #7 queued",
            ),
            ("Failed", ReviewHead, Queued, 1, "had Failed as patchset"),
        ];
        for (status, mode, outcome, fetches, says) in steps {
            set_patchset_status(&state, id, status).await;
            let reply = ask(mode).await;
            assert_eq!(reply.patchset_id, id, "one head has one row");
            assert_eq!(reply.outcome, outcome, "{status} under {mode:?}");
            assert_eq!(reply.previous_status.as_deref(), Some(status));
            assert!(reply.message.contains(says), "{}", reply.message);
            assert_eq!(
                fetches_sent(&mut fetch_rx),
                fetches,
                "{status} under {mode:?}"
            );
        }
        assert_eq!(gh.calls(), 7);
        let requeued = format!("had Failed as patchset {id}, v1; queued again");
        set_patchset_status(&state, id, "Failed").await;
        let reply = ask(ReviewHead).await;
        assert!(reply.message.contains(&requeued), "{}", reply.message);
    }

    /// A head reviewed by number records the description gh gave, on the
    /// placeholder the route queued, so the review reads it as its cover
    /// letter as it would after a webhook delivery.
    #[tokio::test]
    async fn a_pull_request_reviewed_by_number_records_its_description() {
        let (state, mut fetch_rx) = pr_route_state(Arc::new(FakeResolver::default()), |_| {}).await;
        let queued = ask_for_pr(&state, pr_request(7, PrReviewMode::ReviewHead))
            .await
            .unwrap();
        assert_eq!(queued.outcome, PrReviewOutcome::Queued);
        assert_eq!(
            state
                .db
                .get_patchset_mr_body(queued.patchset_id)
                .await
                .unwrap(),
            Some(fake_description(7))
        );
        assert_eq!(fetches_sent(&mut fetch_rx), 1);
    }

    /// Concurrent requests to the route for one pull request head share one
    /// placeholder: one is queued, every other reports the fetch in
    /// progress, none fails, and a later request still finds the head in
    /// progress instead of being refused. Each round asks about a pull
    /// request of its own, so that each is a new head.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_pr_requests_for_one_head_share_one_placeholder() {
        const ROUNDS: i64 = 50;
        const CALLERS: usize = 8;
        let gh = Arc::new(FakeResolver::default());
        let (state, mut fetch_rx) = pr_route_state(gh, |_| {}).await;
        for number in 1..=ROUNDS {
            let barrier = Arc::new(tokio::sync::Barrier::new(CALLERS));
            let mut callers = tokio::task::JoinSet::new();
            for _ in 0..CALLERS {
                let state = state.clone();
                let barrier = Arc::clone(&barrier);
                callers.spawn(async move {
                    barrier.wait().await;
                    ask_for_pr(&state, pr_request(number, PrReviewMode::ReviewHead)).await
                });
            }
            let replies: Vec<PrReviewResponse> = callers
                .join_all()
                .await
                .into_iter()
                .map(|reply| reply.unwrap_or_else(|e| panic!("PR {number}: {e:?}")))
                .collect();
            fetches_sent(&mut fetch_rx);

            let outcomes =
                |wanted: PrReviewOutcome| replies.iter().filter(|r| r.outcome == wanted).count();
            assert_eq!(outcomes(PrReviewOutcome::Queued), 1, "PR {number}");
            assert_eq!(
                outcomes(PrReviewOutcome::AlreadyInProgress),
                CALLERS - 1,
                "PR {number}"
            );
            let id = replies[0].patchset_id;
            assert!(
                replies.iter().all(|r| r.patchset_id == id),
                "PR {number}: {replies:?}"
            );
            let rows: i64 = state
                .db
                .conn
                .query(
                    "SELECT COUNT(*) FROM patchsets WHERE mr_number = ?",
                    libsql::params![number],
                )
                .await
                .unwrap()
                .next()
                .await
                .unwrap()
                .expect("a count")
                .get(0)
                .unwrap();
            assert_eq!(rows, 1, "PR {number}");

            let later = ask_for_pr(&state, pr_request(number, PrReviewMode::ReviewHead))
                .await
                .unwrap_or_else(|e| panic!("PR {number} asked again: {e:?}"));
            assert_eq!(
                (later.outcome, later.patchset_id),
                (PrReviewOutcome::AlreadyInProgress, id),
                "PR {number}"
            );
            fetches_sent(&mut fetch_rx);
        }
    }

    /// The rerun under RerunHead leaves the finished row Pending for the
    /// reviewer, as /api/patchset/rerun does.
    #[tokio::test]
    async fn a_rerun_by_pr_number_queues_the_finished_row() {
        let (state, _fetch_rx) = pr_route_state(Arc::new(FakeResolver::default()), |_| {}).await;
        let id = ask_for_pr(&state, pr_request(7, PrReviewMode::ReviewHead))
            .await
            .unwrap()
            .patchset_id;
        set_patchset_status(&state, id, "Reviewed").await;
        let reply = ask_for_pr(&state, pr_request(7, PrReviewMode::RerunHead))
            .await
            .unwrap();
        assert_eq!(reply.outcome, PrReviewOutcome::RerunQueued);
        let status: String = state
            .db
            .conn
            .query(
                "SELECT status FROM patchsets WHERE id = ?",
                libsql::params![id],
            )
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
            .expect("the row")
            .get(0)
            .unwrap();
        assert_eq!(status, "Pending");
    }

    /// The status patchset `id` holds now.
    async fn patchset_status(state: &AppState, id: i64) -> String {
        state
            .db
            .get_patchset_status(id)
            .await
            .unwrap()
            .expect("the row")
    }

    /// RerunHead reruns the head only while its row still holds the status
    /// queuing found. A row that has moved on since, here one a reviewer
    /// picked up after another request reran it, is refused with 409 and
    /// left under its review; a row still finished is rerun.
    #[tokio::test]
    async fn a_rerun_leaves_a_head_that_moved_on() {
        use crate::forge::PullRequestResolver;
        let (state, _fetch_rx) = pr_route_state(Arc::new(FakeResolver::default()), |_| {}).await;
        let repo = "acme/widget".parse().unwrap();
        let resolved = FakeResolver::default().resolve(&repo, 7).await.unwrap();
        let placeholder = enqueue_pull_request(
            &state,
            "GitHub",
            &resolved.to_forge_metadata(),
            ExistingRevision::Leave,
        )
        .await
        .unwrap();
        let id = placeholder.id;

        set_patchset_status(&state, id, "In Review").await;
        let (status, reason) = rerun_head(
            &state,
            &placeholder,
            crate::ReviewStatus::Reviewed,
            &resolved,
        )
        .await
        .unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(reason.contains("is no longer Reviewed"), "{reason}");
        assert_eq!(patchset_status(&state, id).await, "In Review");

        set_patchset_status(&state, id, "Reviewed").await;
        let rerun = rerun_head(
            &state,
            &placeholder,
            crate::ReviewStatus::Reviewed,
            &resolved,
        )
        .await;
        assert_eq!(rerun, Ok(PrReviewOutcome::RerunQueued));
        assert_eq!(patchset_status(&state, id).await, "Pending");
    }

    /// The status, failure reason and slug of patchset `id`, and the
    /// statuses of its patches by id, NULL as None.
    async fn stored_revision(
        state: &AppState,
        id: i64,
    ) -> (String, Option<String>, Option<String>, Vec<Option<String>>) {
        let mut rows = state
            .db
            .conn
            .query(
                "SELECT status, failed_reason, slug FROM patchsets WHERE id = ?",
                libsql::params![id],
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().expect("the row");
        let columns = (
            row.get(0).unwrap(),
            row.get(1).unwrap(),
            row.get(2).unwrap(),
        );
        let mut rows = state
            .db
            .conn
            .query(
                "SELECT status FROM patches WHERE patchset_id = ? ORDER BY id",
                libsql::params![id],
            )
            .await
            .unwrap();
        let mut patches = Vec::new();
        while let Some(row) = rows.next().await.unwrap() {
            patches.push(row.get(0).unwrap());
        }
        (columns.0, columns.1, columns.2, patches)
    }

    /// A head the auto-cancel ranks below a newer revision, as when a pull
    /// request goes back to an earlier head, is refused with 409 naming the
    /// newer revision, in both modes. It is neither fetched nor rerun, and
    /// neither row changes: the older one keeps its status, failure reason,
    /// held outcomes and slug, and the newer one keeps the bare slug.
    #[tokio::test]
    async fn a_head_the_auto_cancel_supersedes_is_refused() {
        let (state, mut fetch_rx) = pr_route_state(Arc::new(FakeResolver::default()), |_| {}).await;
        let earlier = ask_for_pr(&state, pr_request(7, PrReviewMode::ReviewHead))
            .await
            .unwrap()
            .patchset_id;
        state
            .db
            .ensure_thread_for_message("held@sashiko.local", 0)
            .await
            .unwrap();
        let held = state
            .db
            .create_patch(earlier, "held@sashiko.local", 1, "diff")
            .await
            .unwrap();
        state
            .db
            .update_patch_status(held, crate::PatchOutcome::TimedOut)
            .await
            .unwrap();
        state
            .db
            .update_patchset_error(
                &format!("mr-7-{PR_BASE}..{:040x}@sashiko.local", 7u32),
                "boom",
            )
            .await
            .unwrap();
        let mut pushed = widget_pull_request(7);
        pushed.head_sha = format!("{:040x}", 0x99);
        let newer = enqueue_pull_request(&state, "GitHub", &pushed, ExistingRevision::Refetch)
            .await
            .unwrap()
            .id;
        fetches_sent(&mut fetch_rx);
        let before = stored_revision(&state, earlier).await;
        assert_eq!(before.0, "Failed");
        assert_eq!(before.1.as_deref(), Some("boom"));
        assert_eq!(
            before.3,
            [Some(crate::PatchOutcome::TimedOut.as_str().to_string())]
        );
        assert_eq!(
            stored_revision(&state, newer).await.2.as_deref(),
            Some("widget-7")
        );

        let (status, reason) = ask_for_pr(&state, pr_request(7, PrReviewMode::ReviewHead))
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            reason.contains(&format!(
                "is patchset {earlier}, v1, was Failed, but patchset {newer}, v2 of PR #7"
            )),
            "{reason}"
        );
        assert_eq!(fetches_sent(&mut fetch_rx), 0);
        assert_eq!(stored_revision(&state, earlier).await, before);
        assert_eq!(
            stored_revision(&state, newer).await.2.as_deref(),
            Some("widget-7")
        );

        set_patchset_status(&state, earlier, "Reviewed").await;
        let (status, _) = ask_for_pr(&state, pr_request(7, PrReviewMode::RerunHead))
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(patchset_status(&state, earlier).await, "Reviewed");
        assert_eq!(patchset_status(&state, newer).await, "Fetching");
        assert_eq!(fetches_sent(&mut fetch_rx), 0);
    }

    /// A router built with the default options resolves no pull request:
    /// its resolver refuses every number without running anything, so no
    /// test that builds one can run gh.
    #[tokio::test]
    async fn the_default_options_resolve_no_pull_request() {
        let repo = "acme/widget".parse().unwrap();
        let resolved = ServerOptions::default()
            .pull_request_resolver
            .resolve(&repo, 7)
            .await;
        assert!(
            matches!(resolved, Err(crate::forge::GhError::NotConfigured)),
            "{resolved:?}"
        );
    }

    /// When gh cannot resolve a number, the refusal is a 502 carrying gh's
    /// line and naming the newest revision of that pull request in this
    /// repository, by patchset and version, with the command that reruns it.
    /// A pull request with the same number in another repository is not
    /// named, though its row is newer and its version higher, and nothing is
    /// queued.
    #[tokio::test]
    async fn an_unresolved_number_names_the_newest_known_revision() {
        let detail = "GraphQL: Could not resolve to a PullRequest with the number of 7.";
        let gh = Arc::new(FakeResolver::failing(crate::forge::GhError::Failed {
            detail: detail.to_string(),
        }));
        let (state, mut fetch_rx) = pr_route_state(gh, |_| {}).await;
        let mut revision = widget_pull_request(7);
        let mut queue = |head: u32, repo: &str| {
            revision.head_sha = format!("{head:040x}");
            revision.repo_url = Some(format!("https://github.com/{repo}.git"));
            revision.pr_url = Some(format!("https://github.com/{repo}/pull/7"));
            let (state, metadata) = (state.clone(), revision.clone());
            async move {
                enqueue_pull_request(&state, "GitHub", &metadata, ExistingRevision::Leave)
                    .await
                    .unwrap()
                    .id
            }
        };
        queue(0x71, "acme/widget").await;
        let newest = queue(0x72, "acme/widget").await;
        queue(0x73, "other/widget").await;
        fetches_sent(&mut fetch_rx);

        let (status, reason) = ask_for_pr(&state, pr_request(7, PrReviewMode::ReviewHead))
            .await
            .unwrap_err();
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(reason.contains(detail), "{reason}");
        assert!(
            reason.contains(&format!(
                "the newest known revision is patchset {newest}, v2; \
                 use sashiko-cli rerun {newest}"
            )),
            "{reason}"
        );
        assert_eq!(fetches_sent(&mut fetch_rx), 0);

        let (_, unknown) = ask_for_pr(&state, pr_request(8, PrReviewMode::ReviewHead))
            .await
            .unwrap_err();
        assert!(!unknown.contains("newest known revision"), "{unknown}");
    }

    /// Serves a router with `activity` behind it, read-only so that nothing
    /// mutating could be what lets a request through.
    async fn serve_activity(
        activity: Arc<crate::activity::ActivityRegistry>,
    ) -> (SocketAddr, Arc<Database>) {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();
        let mut settings = crate::settings::Settings::new().unwrap();
        settings.server.testing_mode = false;
        settings.server.read_only = true;
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = mpsc::channel(10);
        let app = build_router(
            Arc::new(settings),
            db.clone(),
            event_tx,
            fetch_tx,
            ServerOptions {
                activity,
                ..Default::default()
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        (addr, db)
    }

    async fn patchset_activity(addr: SocketAddr, id: i64) -> crate::api::PatchsetActivityResponse {
        let resp = reqwest::get(format!("http://{addr}/api/patchset/activity?id={id}"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        resp.json().await.unwrap()
    }

    /// Both activity routes are public reads, like the patchset pages they sit
    /// beside, and answer without credentials even on a read-only server.
    #[tokio::test]
    async fn test_activity_routes_answer_without_credentials() {
        use crate::activity::{ActivityKey, ActivityRegistry, Phase};

        let activity = ActivityRegistry::new();
        activity.update(
            ActivityKey::Patchset(3),
            Phase::ReviewingPatches { patches: 2 },
        );
        let (addr, _db) = serve_activity(activity).await;

        let resp = reqwest::get(format!("http://{addr}/api/activity"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let all: crate::api::ActivityResponse = resp.json().await.unwrap();
        assert_eq!(all.count, 1);
        assert_eq!(all.entries[0].description, "reviewing 2 patches");
        assert!(all.entries[0].idle_seconds.is_some());

        // A patchset the database has never heard of, with nothing live.
        let resp = reqwest::get(format!("http://{addr}/api/patchset/activity?id=999"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// Both routes that open a patchset by id, as the page and the CLI use
    /// them.
    const PATCHSET_ROUTES: [&str; 2] = ["/api/patch", "/api/patchset"];

    /// Queues the revision of PR `number` whose head is `head`, under
    /// `slug`, and returns its patchset.
    async fn queue_revision_at(db: &Database, number: i64, head: &str, slug: &str) -> i64 {
        db.create_fetching_patchset(
            &format!("mr-{number}-{}..{head}@sashiko.local", "a".repeat(40)),
            "Fetching PR",
            None,
            None,
            Some(&format!("https://github.com/acme/frob/pull/{number}")),
            Some("Fix the frobnicator"),
            Some(number),
            Some(slug),
        )
        .await
        .unwrap()
    }

    /// GETs `route` for `id` and returns the status and the body text.
    async fn open_patchset(addr: SocketAddr, route: &str, id: &str) -> (StatusCode, String) {
        let resp = reqwest::get(format!("http://{addr}{route}?id={id}"))
            .await
            .unwrap();
        (resp.status(), resp.text().await.unwrap())
    }

    /// A stored slug that has the shape of an address keeps naming the row
    /// that holds it: repository repo-12's PR 1234567 is not PR 12 of repo.
    #[tokio::test]
    async fn an_exact_slug_wins_over_the_address() {
        let (addr, db) = serve_activity(crate::activity::ActivityRegistry::new()).await;
        let head = format!("1234567{}", "b".repeat(33));
        queue_revision_at(&db, 12, &head, "repo-12").await;
        let bare = queue_revision_at(&db, 1234567, &"c".repeat(40), "repo-12-1234567").await;

        for route in PATCHSET_ROUTES {
            let (status, body) = open_patchset(addr, route, "repo-12-1234567").await;
            assert_eq!(status, StatusCode::OK, "{route}: {body}");
            let details: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(details["id"], bare, "{route}");
            // The lookup only finds the id; the reply still went through
            // the embargo and maintainer access step.
            assert_eq!(details["can_view_embargoed"], false, "{route}");
        }
    }

    /// A revision under embargo opened by its address is gated exactly as
    /// when it is opened by number: a public caller sees no reviews.
    #[tokio::test]
    async fn an_embargoed_revision_opened_by_its_address_shows_no_reviews() {
        let (addr, db) = serve_activity(crate::activity::ActivityRegistry::new()).await;
        let id = queue_revision_at(&db, 5, &format!("feed123{}", "4".repeat(33)), "frob-5").await;
        let until = chrono::Utc::now().timestamp() + 3600;
        db.execute(
            "UPDATE patchsets SET embargo_until = ? WHERE id = ?",
            libsql::params![until, id],
        )
        .await
        .unwrap();

        for route in PATCHSET_ROUTES {
            let (status, body) = open_patchset(addr, route, "frob-5-feed123").await;
            assert_eq!(status, StatusCode::OK, "{route}: {body}");
            let details: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(details["id"], id, "{route}");
            assert_eq!(details["can_view_embargoed"], false, "{route}");
            assert_eq!(
                details["reviews"].as_array().map(Vec::len),
                Some(0),
                "{route}: {body}"
            );
        }
    }

    /// An address opens its revision on both routes after a newer one took
    /// the bare slug; each way it can miss is a 404 whose body says why and
    /// repeats nothing but the hex prefix; an id that is no address at all
    /// is a 404 with no body, as before.
    #[tokio::test]
    async fn an_ambiguous_or_unknown_prefix_is_a_404_that_says_why() {
        let (addr, db) = serve_activity(crate::activity::ActivityRegistry::new()).await;
        let v1 = queue_revision_at(&db, 7, &format!("abcdef1{}", "1".repeat(33)), "frob-7").await;
        queue_revision_at(&db, 7, &format!("abcdef1{}", "2".repeat(33)), "frob-7").await;

        let misses = [
            (
                "frob-9-ABCDEF1",
                "no pull request revision has this address",
            ),
            (
                "frob-7-0000000",
                "no revision of this pull request has a head commit starting 0000000",
            ),
            (
                "frob-7-ABCDEF1",
                "abcdef1 matches 2 revisions of this pull request; give more of the commit",
            ),
            ("frob-seven", ""),
        ];
        for route in PATCHSET_ROUTES {
            let (status, body) = open_patchset(addr, route, "frob-7-abcdef11").await;
            assert_eq!(status, StatusCode::OK, "{route}: {body}");
            let details: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(details["id"], v1, "{route}");

            for (id, reason) in misses {
                let (status, body) = open_patchset(addr, route, id).await;
                assert_eq!(status, StatusCode::NOT_FOUND, "{route} {id}");
                assert_eq!(body, reason, "{route} {id}");
            }
        }
    }

    /// A fetch is keyed by what it fetches, so the patchset has to be bridged to
    /// it. Without that a patchset fetching right now read as stopped.
    #[tokio::test]
    async fn test_a_fetch_in_progress_reads_as_live() {
        use crate::activity::{ActivityKey, ActivityRegistry, Phase};

        let activity = ActivityRegistry::new();
        let (addr, db) = serve_activity(activity.clone()).await;
        let id = db
            .create_fetching_patchset(
                "mr-4-aaa..bbb@sashiko.local",
                "Fetching PR",
                None,
                None,
                None,
                Some("Title"),
                Some(4),
                None,
            )
            .await
            .unwrap();
        activity.update(
            ActivityKey::Commit("aaa..bbb".to_string()),
            Phase::Fetching {
                remote: "https://example.org/repo".to_string(),
                commits: 2,
            },
        );

        let resp = patchset_activity(addr, id).await;
        assert!(resp.live, "a fetch running now is live");
        assert_eq!(resp.entries.len(), 1);
        assert_eq!(resp.entries[0].key, "commit:aaa..bbb");
        assert!(resp.entries[0].age_seconds.is_some());
    }

    /// Persisted activity explains a patchset the daemon stopped in the middle
    /// of. It is shown only for one still unfinished, and in the same order the
    /// live entries would be.
    #[tokio::test]
    async fn test_last_known_activity_is_shown_only_for_unfinished_work() {
        use crate::activity::{ActivityKey, ActivityRegistry, Phase};

        let (addr, db) = serve_activity(ActivityRegistry::new()).await;
        let thread_id = db.create_thread("act", "Subject", 1000).await.unwrap();
        let id = db
            .create_patchset(
                thread_id, None, "act", "Subject", "Author", 1000, 1, 1, "", "", None, 1, None,
                false, None, None,
            )
            .await
            .unwrap()
            .unwrap();
        db.update_patchset_status(id, "In Review").await.unwrap();

        // What a registry would have written, newest first, as a restart would
        // find it.
        let rows = [
            (
                ActivityKey::PatchsetStage {
                    patchset_id: id,
                    patch_id: 8,
                    stage: "report".to_string(),
                },
                Phase::Queued,
            ),
            (
                ActivityKey::PatchsetStage {
                    patchset_id: id,
                    patch_id: 8,
                    stage: "goal".to_string(),
                },
                Phase::Queued,
            ),
            (
                ActivityKey::Patchset(id),
                Phase::ReviewingPatches { patches: 1 },
            ),
        ];
        for (key, phase) in rows {
            db.upsert_activity(
                &key.to_string(),
                key.patchset_id(),
                &serde_json::to_string(&phase).unwrap(),
                &phase.describe(),
                1000,
            )
            .await
            .unwrap();
        }

        let resp = patchset_activity(addr, id).await;
        assert!(!resp.live);
        let keys: Vec<&str> = resp.entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                format!("patchset:{id}"),
                format!("patchset:{id}/patch:8/stage:goal"),
                format!("patchset:{id}/patch:8/stage:report"),
            ]
        );
        assert!(resp.entries[1].order < resp.entries[2].order);
        assert_eq!(
            resp.entries[1].stage_label.as_deref(),
            Some("Goal Analysis")
        );

        // Finished, the same rows explain nothing.
        db.update_patchset_status(id, "Reviewed").await.unwrap();
        let resp = patchset_activity(addr, id).await;
        assert!(!resp.live);
        assert!(resp.entries.is_empty());
    }

    /// A patchset with one patch and one review of it, for the review route
    /// tests. Returns (patchset, patch, review).
    async fn reviewed(db: &Database, root: &str) -> (i64, i64, i64) {
        let thread_id = db.create_thread(root, "Subject", 1000).await.unwrap();
        let msg = format!("{root}_p1");
        db.create_message(
            &msg, thread_id, None, "Author", "Subject", 1000, "Body", "", "", None, None,
        )
        .await
        .unwrap();
        let ps = db
            .create_patchset(
                thread_id, None, root, "Subject", "Author", 1000, 1, 1, "", "", None, 1, None,
                false, None, None,
            )
            .await
            .unwrap()
            .unwrap();
        let patch = db.create_patch(ps, &msg, 1, "diff").await.unwrap();
        let review = db
            .create_review(ps, Some(patch), "mock", "mock", None, None, None)
            .await
            .unwrap();
        (ps, patch, review)
    }

    async fn get_json(addr: SocketAddr, path: &str) -> serde_json::Value {
        let resp = reqwest::get(format!("http://{addr}{path}")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        resp.json().await.unwrap()
    }

    fn message_json(role: &str, content: &str) -> String {
        serde_json::json!({ "role": role, "content": content }).to_string()
    }

    /// A running review's conversation is served beside its (absent) final
    /// log, from both review routes, and only while the review runs.
    #[tokio::test]
    async fn test_live_log_is_served_only_while_the_review_runs() {
        let (addr, db) = serve_activity(crate::activity::ActivityRegistry::new()).await;
        let (_, _, review) = reviewed(&db, "live").await;

        // Pending with nothing streamed yet: live, and empty rather than absent.
        let body = get_json(addr, &format!("/api/review?id={review}")).await;
        assert_eq!(body["live_log"], serde_json::json!([]), "{body}");
        assert_eq!(body["live_log_capped"], false);
        assert!(body["live_log_first_seq"].is_null(), "{body}");

        db.update_review_status(review, "In Review", None)
            .await
            .unwrap();
        db.append_review_log_entry(review, 0, None, "system", &message_json("system", "sys"))
            .await
            .unwrap();
        db.append_review_log_entry(review, 1, Some("goal"), "user", &message_json("user", "go"))
            .await
            .unwrap();

        let body = get_json(addr, &format!("/api/review?id={review}")).await;
        let live = body["live_log"].as_array().expect("live_log");
        assert_eq!(live.len(), 2);
        assert_eq!(live[1]["role"], "user");
        assert_eq!(live[1]["content"], "go");
        assert_eq!(live[1]["seq"], 1);
        assert_eq!(live[1]["stage"], "goal");
        assert_eq!(
            live[1]["stage_label"],
            serde_json::json!(crate::workflows::stage_short_label(
                crate::project::ProjectId::default(),
                "goal"
            )),
            "the label comes from the daemon, as for activity entries"
        );
        assert!(live[1]["stage_label"].is_string(), "{body}");
        assert!(live[0]["stage"].is_null());
        assert!(live[0]["stage_label"].is_null());
        assert_eq!(body["live_log_first_seq"], 0);
        assert_eq!(
            get_json(addr, &format!("/api/review_log?id={review}")).await["live_log"],
            body["live_log"]
        );

        // Only the tail after a sequence already held.
        let tail = get_json(addr, &format!("/api/review?id={review}&after_seq=0")).await;
        assert_eq!(tail["live_log"].as_array().unwrap().len(), 1);
        assert_eq!(tail["live_log"][0]["seq"], 1);
        let none = get_json(addr, &format!("/api/review?id={review}&after_seq=1")).await;
        assert_eq!(none["live_log"], serde_json::json!([]));
        assert_eq!(
            none["live_log_first_seq"], 0,
            "the run marker does not depend on the cursor"
        );

        // A rerun of the row clears the old run's entries and continues past
        // its seqs; the marker moves, so a page holding the old cursor can see
        // the conversation started over.
        db.delete_review_log_entries(review).await.unwrap();
        db.append_review_log_entry(review, 2, None, "system", &message_json("system", "again"))
            .await
            .unwrap();
        let rerun = get_json(addr, &format!("/api/review?id={review}&after_seq=1")).await;
        assert_eq!(rerun["live_log_first_seq"], 2, "{rerun}");
        assert_eq!(rerun["live_log"][0]["content"], "again");

        // A review that failed without reaching complete_review keeps its rows
        // until the startup sweep, but they are no longer served as live.
        db.update_review_status(review, "Failed", None)
            .await
            .unwrap();
        let body = get_json(addr, &format!("/api/review?id={review}")).await;
        assert!(body.get("live_log").is_none(), "{body}");

        // The final log landing ends it too, and takes the rows with it.
        db.update_review_status(review, "In Review", None)
            .await
            .unwrap();
        db.complete_review(review, "Reviewed", "ok", None, None, None, Some("[]"))
            .await
            .unwrap();
        for path in ["/api/review", "/api/review_log"] {
            let body = get_json(addr, &format!("{path}?id={review}")).await;
            assert!(body.get("live_log").is_none(), "{body}");
            assert_eq!(body["logs"], "[]");
        }
    }

    /// The live conversation is transcript. It never reaches the activity
    /// routes, which are public for embargoed patchsets too, nor the patchset
    /// payloads.
    #[tokio::test]
    async fn test_no_conversation_reaches_the_activity_routes() {
        use crate::activity::{ActivityRegistry, ProgressWire, StageTracker, progress_update};

        let activity = ActivityRegistry::new();
        let (addr, db) = serve_activity(activity.clone()).await;
        let (ps, patch, review) = reviewed(&db, "secret").await;
        db.update_patchset_status(ps, "In Review").await.unwrap();
        db.update_review_status(review, "In Review", None)
            .await
            .unwrap();

        const SECRET: &str = "conversation-text-that-must-stay-put";
        let message = crate::ai::AiMessage {
            role: crate::ai::AiRole::Assistant,
            content: Some(SECRET.to_string()),
            thought: None,
            thought_signature: None,
            tool_calls: None,
            tool_call_id: None,
        };
        // What the daemon does with a worker's progress, for a stage start and
        // for a log entry.
        let mut tracker = StageTracker::new(activity.clone());
        for wire in [
            ProgressWire::StageStarted {
                stage: "goal".to_string(),
            },
            ProgressWire::LogEntry {
                stage: Some("goal".to_string()),
                message: message.clone(),
            },
        ] {
            if let Some(update) =
                progress_update(crate::project::ProjectId::Linux, ps, patch, &wire, 1, 1)
            {
                tracker.apply(update);
            }
        }
        db.append_review_log_entry(
            review,
            0,
            Some("goal"),
            "assistant",
            &serde_json::to_string(&message).unwrap(),
        )
        .await
        .unwrap();

        let activity_body = reqwest::get(format!("http://{addr}/api/patchset/activity?id={ps}"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(activity_body.contains("goal"), "{activity_body}");
        for path in [
            format!("/api/patchset/activity?id={ps}"),
            "/api/activity".to_string(),
            format!("/api/patchset?id={ps}"),
            format!("/api/patch?id={ps}"),
        ] {
            let body = reqwest::get(format!("http://{addr}{path}"))
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert!(!body.contains(SECRET), "{path}: {body}");
            assert!(!body.contains("live_log"), "{path}: {body}");
        }
        // Where the finished log is served, the live one is.
        let body = get_json(addr, &format!("/api/review?id={review}")).await;
        assert_eq!(body["live_log"][0]["content"], SECRET);
    }

    /// The page stops polling when the daemon says the patchset is finished,
    /// rather than keeping a list of statuses of its own.
    #[tokio::test]
    async fn test_patchset_activity_says_when_it_is_finished() {
        let (addr, db) = serve_activity(crate::activity::ActivityRegistry::new()).await;
        let (ps, _, _) = reviewed(&db, "fin").await;
        db.update_patchset_status(ps, "In Review").await.unwrap();
        assert!(!patchset_activity(addr, ps).await.finished);
        for status in ["Reviewed", "Partial", "Failed", "Skipped"] {
            db.update_patchset_status(ps, status).await.unwrap();
            assert!(patchset_activity(addr, ps).await.finished, "{status}");
        }
        db.update_patchset_status(ps, "Pending").await.unwrap();
        assert!(!patchset_activity(addr, ps).await.finished);
        // Last: a cancelled patchset never leaves that status.
        db.update_patchset_status(ps, "Cancelled").await.unwrap();
        assert!(patchset_activity(addr, ps).await.finished);
    }

    /// A patchset of two patches, each with one review on its first attempt
    /// and kernel attached, and each review with these calls to kernel and
    /// to the code under review. Returns (patchset, patches).
    async fn two_reviewed_patches(
        db: &Database,
        root: &str,
        calls: [(usize, usize); 2],
    ) -> (i64, [i64; 2]) {
        let thread_id = db.create_thread(root, "Subject", 1000).await.unwrap();
        let ps = db
            .create_patchset(
                thread_id, None, root, "Subject", "Author", 1000, 2, 2, "", "", None, 1, None,
                false, None, None,
            )
            .await
            .unwrap()
            .unwrap();
        let record = serde_json::json!({
            "attached": [{"name": "kernel", "kind": "kernel", "revision": "v6.18",
                          "commit": "0".repeat(40), "why": "always"}],
            "dormant": [],
        })
        .to_string();
        let mut patches = [0; 2];
        for (index, (kernel, review_tree)) in calls.into_iter().enumerate() {
            let part = u32::try_from(index).unwrap() + 1;
            let msg = format!("{root}_p{part}");
            db.create_message(
                &msg, thread_id, None, "Author", "Subject", 1000, "Body", "", "", None, None,
            )
            .await
            .unwrap();
            patches[index] = db.create_patch(ps, &msg, part, "diff").await.unwrap();
            let review = db
                .create_review(
                    ps,
                    Some(patches[index]),
                    "mock",
                    "mock",
                    None,
                    None,
                    Some(1),
                )
                .await
                .unwrap();
            db.set_review_reference_repos(review, &record)
                .await
                .unwrap();
            let kernel_calls = std::iter::repeat_n(r#"{"pattern":"k","repo":"kernel"}"#, kernel);
            let review_calls = std::iter::repeat_n(r#"{"pattern":"r"}"#, review_tree);
            for arguments in kernel_calls.chain(review_calls) {
                db.create_tool_usage(crate::db::ToolUsage {
                    review_id: review,
                    provider: "mock".to_string(),
                    model: "mock".to_string(),
                    tool_name: "git_grep".to_string(),
                    arguments: Some(arguments.to_string()),
                    output_length: 0,
                })
                .await
                .unwrap();
            }
        }
        (ps, patches)
    }

    /// The poll carries the tool calls of the patches its entries name, from
    /// the database, and of no other patch.
    #[tokio::test]
    async fn test_patchset_activity_carries_tool_calls_of_patches_with_entries() {
        use crate::activity::{ActivityKey, ActivityRegistry, Phase};

        let activity = ActivityRegistry::new();
        let (addr, db) = serve_activity(activity.clone()).await;
        let (ps, [first, _]) = two_reviewed_patches(&db, "tc_live", [(3, 4), (5, 6)]).await;
        db.update_patchset_status(ps, "In Review").await.unwrap();
        activity.update(
            ActivityKey::PatchsetStage {
                patchset_id: ps,
                patch_id: first,
                stage: "goal".to_string(),
            },
            Phase::Queued,
        );

        let resp = patchset_activity(addr, ps).await;
        assert!(resp.live);
        assert_eq!(
            resp.tool_calls,
            [crate::api::PatchToolCalls {
                patch_id: first,
                attempts: 1,
                counts: crate::api::ToolCallCounts {
                    total: 7,
                    by_reference: Some(crate::api::ReferenceSplit {
                        references: vec![crate::api::ReferenceCalls {
                            name: "kernel".to_string(),
                            calls: 3,
                        }],
                        unattached: 0,
                    }),
                },
            }]
        );
    }

    /// With no entry there is no stage table to put counts on, so none are
    /// sent, however many calls the reviews made.
    #[tokio::test]
    async fn test_patchset_activity_without_entries_carries_no_tool_calls() {
        let (addr, db) = serve_activity(crate::activity::ActivityRegistry::new()).await;
        let (ps, _) = two_reviewed_patches(&db, "tc_idle", [(1, 2), (3, 4)]).await;
        db.update_patchset_status(ps, "Reviewed").await.unwrap();

        let resp = patchset_activity(addr, ps).await;
        assert!(resp.entries.is_empty());
        assert!(resp.tool_calls.is_empty());
    }

    /// The counts are advisory: when they cannot be read the stage table is
    /// still served, with no counts.
    #[tokio::test]
    async fn test_patchset_activity_survives_a_failed_tool_call_count() {
        use crate::activity::{ActivityKey, ActivityRegistry, Phase};

        let activity = ActivityRegistry::new();
        let (addr, db) = serve_activity(activity.clone()).await;
        let (ps, [first, _]) = two_reviewed_patches(&db, "tc_fail", [(1, 1), (1, 1)]).await;
        activity.update(
            ActivityKey::PatchsetStage {
                patchset_id: ps,
                patch_id: first,
                stage: "goal".to_string(),
            },
            Phase::Queued,
        );
        // Without the table the count fails.
        db.conn.execute("DROP TABLE tool_usages", ()).await.unwrap();

        let resp = patchset_activity(addr, ps).await;
        assert_eq!(resp.entries.len(), 1);
        assert!(resp.tool_calls.is_empty());
    }

    /// Recorded stage rows carry the order and label live entries carry, from
    /// every route that serves them.
    #[tokio::test]
    async fn test_recorded_stage_rows_carry_order_and_label() {
        let (addr, db) = serve_activity(crate::activity::ActivityRegistry::new()).await;
        let (ps, _, review) = reviewed(&db, "rec").await;
        db.set_review_stage_durations(
            review,
            r#"[{"stage":"locking","seconds":5,"turns":1},{"stage":"goal","seconds":9,"turns":2},
                {"stage":"no-such-stage","seconds":1,"turns":1}]"#,
        )
        .await
        .unwrap();
        db.set_review_stage_failures(
            review,
            r#"[{"stage":"security","reason":"x","cause":"out_of_time"}]"#,
        )
        .await
        .unwrap();

        let reviews = |body: serde_json::Value| body["reviews"][0].clone();
        for (path, r) in [
            (
                "review",
                get_json(addr, &format!("/api/review?id={review}")).await,
            ),
            (
                "review_log",
                get_json(addr, &format!("/api/review_log?id={review}")).await,
            ),
            (
                "patchset",
                reviews(get_json(addr, &format!("/api/patchset?id={ps}")).await),
            ),
            (
                "patch",
                reviews(get_json(addr, &format!("/api/patch?id={ps}")).await),
            ),
        ] {
            let durations = r["stage_durations"].as_array().expect(path);
            let goal = durations.iter().find(|d| d["stage"] == "goal").expect(path);
            let locking = durations
                .iter()
                .find(|d| d["stage"] == "locking")
                .expect(path);
            assert_eq!(goal["stage_label"], "Goal Analysis", "{path}");
            assert!(
                goal["order"].as_u64() < locking["order"].as_u64(),
                "{path}: {r}"
            );
            let unknown = durations
                .iter()
                .find(|d| d["stage"] == "no-such-stage")
                .expect(path);
            assert!(unknown["order"].is_null(), "{path}");
            assert!(unknown["stage_label"].is_null(), "{path}");
            assert!(r["stage_failures"][0]["order"].is_u64(), "{path}");
        }
    }

    #[tokio::test]
    async fn test_api_token_endpoint_minting_and_attenuation() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let jwt_secret = "unit-test-jwt-secret".to_string();
        let mut settings = crate::settings::Settings::new().unwrap();
        settings.server.testing_mode = false;
        settings.server.jwt_secret = Some(jwt_secret.clone());
        settings.server.acl = crate::settings::AclSettings {
            admins: vec!["operator@example.org".to_string()],
            security: vec!["security@example.org".to_string()],
            blocklist: vec!["revoked-sid-999".to_string()],
            ..Default::default()
        };
        let settings = Arc::new(settings);

        let local_token = crate::auth::LocalToken::generate().unwrap();
        let local_token_secret = local_token.secret().to_string();
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = mpsc::channel(10);
        let app = build_router(
            settings,
            db,
            event_tx,
            fetch_tx,
            ServerOptions {
                local_token: Some(local_token),
                ..Default::default()
            },
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let op_session = crate::auth::create_token(
            "operator@example.org",
            &jwt_secret,
            Some("session".to_string()),
            3600,
        )
        .unwrap();
        let sec_session = crate::auth::create_token(
            "security@example.org",
            &jwt_secret,
            Some("session".to_string()),
            3600,
        )
        .unwrap();

        let client = reqwest::Client::new();
        // Mint default read-only token for self.
        let resp = client
            .post(format!("http://{}/api/auth/token", addr))
            .bearer_auth(&op_session)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["email"], "operator@example.org");
        assert_eq!(body["max_bug_access"], "read");
        assert_eq!(body["expires_in_days"], 30);
        let minted_token = body["token"].as_str().unwrap().to_string();

        // Minted api_token can list bugs.
        let list_resp = client
            .get(format!("http://{}/api/bugs", addr))
            .bearer_auth(&minted_token)
            .send()
            .await
            .unwrap();
        assert_eq!(list_resp.status(), StatusCode::OK);

        // Minted api_token cannot be refreshed at /api/auth/refresh.
        let refresh_resp = client
            .post(format!("http://{}/api/auth/refresh", addr))
            .bearer_auth(&minted_token)
            .send()
            .await
            .unwrap();
        assert_eq!(refresh_resp.status(), StatusCode::FORBIDDEN);

        // Minted api_token cannot mint another token at /api/auth/token.
        let re_mint_resp = client
            .post(format!("http://{}/api/auth/token", addr))
            .bearer_auth(&minted_token)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(re_mint_resp.status(), StatusCode::FORBIDDEN);

        // Non-operator cannot mint token for another email.
        let cross_resp = client
            .post(format!("http://{}/api/auth/token", addr))
            .bearer_auth(&sec_session)
            .json(&serde_json::json!({ "email": "operator@example.org" }))
            .send()
            .await
            .unwrap();
        assert_eq!(cross_resp.status(), StatusCode::FORBIDDEN);

        // Operator CAN mint token for another email.
        let op_cross_resp = client
            .post(format!("http://{}/api/auth/token", addr))
            .bearer_auth(&op_session)
            .json(&serde_json::json!({
                "email": "security@example.org",
                "max_bug_access": "comment",
                "expires_in_days": 7
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(op_cross_resp.status(), StatusCode::OK);

        // Local operator token (sashiko-cli auth token) CAN mint token for an email without a JWT session.
        let local_op_resp = client
            .post(format!("http://{}/api/auth/token", addr))
            .bearer_auth(&local_token_secret)
            .json(&serde_json::json!({
                "email": "security@example.org",
                "max_bug_access": "read",
                "expires_in_days": 30
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(local_op_resp.status(), StatusCode::OK);

        // Revoked sid in acl.blocklist is rejected by refresh_token and is_authorized.
        let revoked_session = crate::auth::create_token_with_session(
            "operator@example.org",
            &jwt_secret,
            Some("session".to_string()),
            3600,
            Some(100),
            Some("revoked-sid-999".to_string()),
        )
        .unwrap();
        let revoked_refresh = client
            .post(format!("http://{}/api/auth/refresh", addr))
            .bearer_auth(&revoked_session)
            .send()
            .await
            .unwrap();
        assert_eq!(revoked_refresh.status(), StatusCode::FORBIDDEN);
        let revoked_cancel = client
            .post(format!("http://{}/api/patchset/cancel?id=1", addr))
            .bearer_auth(&revoked_session)
            .send()
            .await
            .unwrap();
        assert_eq!(revoked_cancel.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_local_token_authorizes_ingest_but_grants_no_identity() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        // Everything that could authorize the request for another reason is
        // switched off, so only the token can be what admits it.
        let mut settings = crate::settings::Settings::new().unwrap();
        settings.server.testing_mode = false;
        settings.server.acl = crate::settings::AclSettings::default();
        let settings = Arc::new(settings);

        let local_token = crate::auth::LocalToken::generate().unwrap();
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = mpsc::channel(10);
        let app = build_router(
            settings,
            db.clone(),
            event_tx,
            fetch_tx,
            ServerOptions {
                local_token: Some(local_token.clone()),
                ..Default::default()
            },
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let submit = format!("http://{}/api/submit", addr);
        let payload = serde_json::json!({
            "type": "remote",
            "sha": "1234567890abcdef1234567890abcdef12345678",
            "repo": "https://example.org/linux.git",
        });

        let anonymous = client
            .post(&submit)
            .json(&payload)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(anonymous, 403, "loopback alone must not ingest");

        let wrong = client
            .post(&submit)
            .bearer_auth("0".repeat(64))
            .json(&payload)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(
            wrong, 403,
            "a token this server did not write must not ingest"
        );

        let accepted = client
            .post(&submit)
            .bearer_auth(local_token.secret())
            .json(&payload)
            .send()
            .await
            .unwrap()
            .status();
        assert!(accepted.is_success(), "token was refused: {}", accepted);

        // The token is an authority, not an identity, so it cannot act on a
        // bug however local its holder is.
        let bug_action = client
            .post(format!("http://{}/api/bug/action?bugid=linux-1", addr))
            .bearer_auth(local_token.secret())
            .json(&serde_json::json!({"action": {"kind": "close"}}))
            .send()
            .await
            .unwrap()
            .status();
        assert!(
            !bug_action.is_success(),
            "token reached a bug route: {}",
            bug_action
        );
    }

    #[tokio::test]
    async fn test_bug_input_endpoint_serves_per_bug_payload() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let make_bug = |bugid: &str, title: &str| crate::db::NewBug {
            bugid: bugid.to_string(),
            title: title.to_string(),
            lifecycle_status: crate::db::BugLifecycleStatus::New,
            pipeline_state: crate::db::BugPipelineState::Pending,
            assignee: None,
            reporter: "sashiko".to_string(),
            reported_at: 100,
            discovered_in_patchset_id: None,
            discovered_in_patch_id: None,
            discovered_in_commit: None,
            source_ref: None,
            vector_json: None,
            duplicate_of_id: None,
            subsystems: vec![],
        };

        let canonical = db
            .create_bug_with_enrichment(
                &make_bug("linux-canonical", "UAF in canonical path"),
                Some(&crate::db::NewBugEnrichment {
                    kind: "candidate".to_string(),
                    tool: "sashiko:linux_patch_review".to_string(),
                    model: Some("test-model".to_string()),
                    created_at: 100,
                    content: Some("canonical reasoning".to_string()),
                    data_json: Some(serde_json::json!({"problem": "canonical problem"})),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        let duplicate = db
            .create_bug_with_enrichment(
                &make_bug("linux-duplicate", "UAF spotted again"),
                Some(&crate::db::NewBugEnrichment {
                    kind: "candidate".to_string(),
                    tool: "sashiko:linux_patch_review".to_string(),
                    model: Some("test-model".to_string()),
                    created_at: 200,
                    content: Some("duplicate reasoning".to_string()),
                    data_json: Some(serde_json::json!({"problem": "duplicate problem"})),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        db.mark_bug_as_duplicate(crate::db::MarkDuplicateBugParams {
            preserve_triage: false,
            ephemeral_id: duplicate,
            canonical_id: canonical,
            reasoning: "same defect",
            logs: None,
            tokens_in: None,
            tokens_out: None,
            tokens_cached: None,
        })
        .await
        .unwrap();

        // Bug routes require a resolved principal. Testing mode supplies an
        // operator one, so that this test exercises the handler rather than
        // the authorization layer, which is covered separately.
        let mut settings = crate::settings::Settings::new().unwrap();
        settings.server.testing_mode = true;
        let settings = Arc::new(settings);
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = mpsc::channel(10);
        let app = build_router(
            settings,
            db.clone(),
            event_tx,
            fetch_tx,
            ServerOptions::default(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let canonical_input: serde_json::Value = reqwest::get(format!(
            "http://{}/api/bug/input?bugid=linux-canonical",
            addr
        ))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
        assert_eq!(canonical_input["bugid"], "linux-canonical");
        let inputs = canonical_input["inputs"].as_array().unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0]["input"]["problem"], "canonical problem");

        // A duplicate resolves to the payload that produced it, not to the
        // canonical bug's payload.
        let duplicate_input: serde_json::Value =
            reqwest::get(format!("http://{}/api/bug/input?id={}", addr, duplicate))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        let inputs = duplicate_input["inputs"].as_array().unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0]["input"]["problem"], "duplicate problem");
        assert_eq!(inputs[0]["model"], "test-model");

        let missing = reqwest::get(format!("http://{}/api/bug/input?id=999999", addr))
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);
    }

    #[tokio::test]
    async fn test_bug_endpoints() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let bug_id = db
            .create_bug(&crate::db::NewBug {
                bugid: "linux-12345678".to_string(),
                title: "UAF in test_device".to_string(),
                lifecycle_status: crate::db::BugLifecycleStatus::New,
                pipeline_state: crate::db::BugPipelineState::Pending,
                assignee: None,
                reporter: "sashiko".to_string(),
                reported_at: 123456,
                discovered_in_patchset_id: None,
                discovered_in_patch_id: None,
                discovered_in_commit: None,
                source_ref: None,
                vector_json: None,
                duplicate_of_id: None,
                subsystems: vec![crate::db::AttributedSubsystem::from_maintainers(
                    "drivers/net",
                )],
            })
            .await
            .unwrap();

        db.add_bug_enrichment(
            bug_id,
            &crate::db::NewBugEnrichment {
                kind: "severity_calibration".to_string(),
                tool: "sashiko".to_string(),
                model: None,
                author: None,
                created_at: 123456,
                content: Some("Trace".to_string()),
                data_json: Some(serde_json::json!({
                    "severity": "Critical",
                    "severity_int": 4,
                })),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        db.add_bug_enrichment(
            bug_id,
            &crate::db::NewBugEnrichment {
                kind: "report".to_string(),
                tool: "sashiko".to_string(),
                model: None,
                author: None,
                created_at: 123457,
                content: Some("Inline review".to_string()),
                data_json: None,
                tokens_in: None,
                tokens_out: None,
                tokens_cached: None,
                logs: Some("[{\"role\":\"user\",\"content\":\"test\"}]".to_string()),
            },
        )
        .await
        .unwrap();

        // Bug reads need a principal with authority over the bug. The security
        // list reads every bug and holds no other capability, which keeps the
        // capability assertions further down honest.
        const SECRET: &str = "bug-test-secret-12345678901234567890";
        let mut settings = crate::settings::Settings::new().unwrap();
        settings.project.kind = Some(crate::project::ProjectId::Linux);
        settings.server.jwt_secret = Some(SECRET.to_string());
        settings.server.acl.security = vec!["security@example.org".to_string()];
        settings.server.acl.admins = vec!["operator@example.org".to_string()];
        settings.server.acl.blocklist = Vec::new();

        let settings = Arc::new(settings);
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = mpsc::channel(10);

        // The capability assertions further down turn on this, since an
        // identity in the security list holds no capability of its own.
        let local_token = crate::auth::LocalToken::generate().unwrap();

        let app = build_router(
            settings.clone(),
            db.clone(),
            event_tx,
            fetch_tx,
            ServerOptions {
                dry_run: true,
                local_token: Some(local_token.clone()),
                ..Default::default()
            },
        );

        let token = crate::auth::create_token(
            "security@example.org",
            SECRET,
            Some("session".to_string()),
            3600,
        )
        .unwrap();
        let mut auth_headers = reqwest::header::HeaderMap::new();
        auth_headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", token).parse().unwrap(),
        );
        let client = reqwest::Client::builder()
            .default_headers(auth_headers)
            .build()
            .unwrap();
        let get = |url: String| client.get(url).send();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        // Test 1: get_bug by bugid (logs omitted)
        let res = get(format!("http://{}/api/bug?bugid=linux-12345678", addr))
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json: serde_json::Value = res.json().await.unwrap();
        assert_eq!(json["bugid"], "linux-12345678");
        assert_eq!(json["problem"], "UAF in test_device");
        assert!(json["logs"].is_null());
        assert!(json.get("enrichments").is_none());
        assert!(json.get("raw_input").is_none());
        assert_eq!(json["evidence"]["count"], 1);
        assert_eq!(json["evidence"]["activity"].as_array().unwrap().len(), 4);

        // Test 1c: get_bug_enrichments
        let res_enrich = get(format!(
            "http://{}/api/bug/enrichments?bugid=linux-12345678",
            addr
        ))
        .await
        .unwrap();
        assert_eq!(res_enrich.status(), 200);
        let enrich_json: serde_json::Value = res_enrich.json().await.unwrap();
        assert_eq!(enrich_json.as_array().unwrap().len(), 4);
        assert!(
            enrich_json
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["kind"] == "report")
        );

        // Test 1b: get_bug by legacy slug param
        let res_slug = get(format!("http://{}/api/bug?slug=linux-12345678", addr))
            .await
            .unwrap();
        assert_eq!(res_slug.status(), 200);

        // Test 2: get_bug_logs by bugid
        let res_logs = get(format!("http://{}/api/bug/logs?bugid=linux-12345678", addr))
            .await
            .unwrap();
        assert_eq!(res_logs.status(), 200);
        let logs_json: serde_json::Value = res_logs.json().await.unwrap();
        assert!(logs_json.is_array());
        assert_eq!(logs_json[0]["role"], "user");

        // Test 2b: get_bug_logs by legacy slug param
        let res_logs_slug = get(format!("http://{}/api/bug/logs?slug=linux-12345678", addr))
            .await
            .unwrap();
        assert_eq!(res_logs_slug.status(), 200);

        // Test 2b: get_bug_logs by id
        let res_logs_id = get(format!("http://{}/api/bug/logs?id={}", addr, bug_id))
            .await
            .unwrap();
        assert_eq!(res_logs_id.status(), 200);

        // Test 3: list_bugs with subsystem filter
        let res_sub = get(format!("http://{}/api/bugs?subsystem=drivers/net", addr))
            .await
            .unwrap();
        assert_eq!(res_sub.status(), 200);
        let list_json: serde_json::Value = res_sub.json().await.unwrap();
        assert_eq!(list_json["total"], 1);
        assert_eq!(list_json["items"].as_array().unwrap().len(), 1);
        assert!(list_json["items"][0]["logs"].is_null());

        // Test 3a: list_bugs with hierarchical parent subsystem filter ("drivers" matches "drivers/net")
        let res_hier = get(format!("http://{}/api/bugs?subsystem=drivers", addr))
            .await
            .unwrap();
        assert_eq!(res_hier.status(), 200);
        let hier_json: serde_json::Value = res_hier.json().await.unwrap();
        assert_eq!(hier_json["total"], 1);

        // Test 3b: list_bugs with non-matching subsystem filter
        let res_other_sub = get(format!("http://{}/api/bugs?subsystem=btrfs", addr))
            .await
            .unwrap();
        assert_eq!(res_other_sub.status(), 200);
        let empty_list: serde_json::Value = res_other_sub.json().await.unwrap();
        assert_eq!(empty_list["total"], 0);

        // Test 3c: list_bugs with lifecycle filter
        let res_status = get(format!("http://{}/api/bugs?lifecycle_status=new", addr))
            .await
            .unwrap();
        assert_eq!(res_status.status(), 200);
        let status_json: serde_json::Value = res_status.json().await.unwrap();
        assert_eq!(status_json["total"], 1);

        // Test 3d: list_bugs with lifecycle filter mismatch
        let res_open = get(format!("http://{}/api/bugs?lifecycle_status=open", addr))
            .await
            .unwrap();
        assert_eq!(res_open.status(), 200);
        let open_json: serde_json::Value = res_open.json().await.unwrap();
        assert_eq!(open_json["total"], 0);

        // Test 3d1: the historical status spelling still selects the lifecycle
        // axis, so bookmarked URLs keep working.
        let res_alias = get(format!("http://{}/api/bugs?status=new", addr))
            .await
            .unwrap();
        assert_eq!(res_alias.status(), 200);
        let alias_json: serde_json::Value = res_alias.json().await.unwrap();
        assert_eq!(alias_json["total"], 1);

        // Test 3d2: the pipeline axis is filterable independently.
        let res_pipeline = get(format!("http://{}/api/bugs?pipeline_state=pending", addr))
            .await
            .unwrap();
        assert_eq!(res_pipeline.status(), 200);
        let pipeline_json: serde_json::Value = res_pipeline.json().await.unwrap();
        assert_eq!(pipeline_json["total"], 1);

        // Test 3d3: an unknown state is rejected rather than silently matching
        // nothing, which would look identical to an empty result set.
        let res_bogus = get(format!("http://{}/api/bugs?lifecycle_status=raw", addr))
            .await
            .unwrap();
        assert_eq!(res_bogus.status(), 400);

        // Test 3e: list_bugs with sorting
        let res_sort = get(format!(
            "http://{}/api/bugs?sort_by=severity&sort_order=desc",
            addr
        ))
        .await
        .unwrap();
        assert_eq!(res_sort.status(), 200);

        // Test 3f: list_bugs with multi-subsystem filter (subsystems parameter)
        let res_multi = get(format!(
            "http://{}/api/bugs?subsystems=drivers/net,btrfs",
            addr
        ))
        .await
        .unwrap();
        assert_eq!(res_multi.status(), 200);
        let multi_json: serde_json::Value = res_multi.json().await.unwrap();
        assert_eq!(multi_json["total"], 1);

        // Test 3g: list_bugs with comma-separated single subsystem parameter
        let res_comma = get(format!("http://{}/api/bugs?subsystem=drivers/net,fs", addr))
            .await
            .unwrap();
        assert_eq!(res_comma.status(), 200);
        let comma_json: serde_json::Value = res_comma.json().await.unwrap();
        assert_eq!(comma_json["total"], 1);

        // Test 3h: list_bugs with non-matching multi-subsystem
        let res_multi_none = get(format!("http://{}/api/bugs?subsystems=btrfs,ext4", addr))
            .await
            .unwrap();
        assert_eq!(res_multi_none.status(), 200);
        let multi_none_json: serde_json::Value = res_multi_none.json().await.unwrap();
        assert_eq!(multi_none_json["total"], 0);

        // Test 3i: GET /api/bugs/subsystems with lifecycle_status=new
        let res_subs_api = get(format!(
            "http://{}/api/bugs/subsystems?lifecycle_status=new",
            addr
        ))
        .await
        .unwrap();
        assert_eq!(res_subs_api.status(), 200);
        let subs_json: serde_json::Value = res_subs_api.json().await.unwrap();
        let subs_arr = subs_json.as_array().unwrap();
        assert_eq!(subs_arr.len(), 1);
        assert_eq!(subs_arr[0]["name"], "drivers/net");
        assert_eq!(subs_arr[0]["count"], 1);
        assert_eq!(subs_arr[0]["open_bugs"], 1);

        // Test 3j: GET /api/subsystems alias
        let res_subs_alias = get(format!(
            "http://{}/api/subsystems?lifecycle_status=new",
            addr
        ))
        .await
        .unwrap();
        assert_eq!(res_subs_alias.status(), 200);

        // Test 3k: GET /api/bugs/subsystems with lifecycle_status=open (should
        // skip 0-bug entries)
        let res_subs_open = get(format!(
            "http://{}/api/bugs/subsystems?lifecycle_status=open",
            addr
        ))
        .await
        .unwrap();
        assert_eq!(res_subs_open.status(), 200);
        let open_subs_json: serde_json::Value = res_subs_open.json().await.unwrap();
        assert_eq!(open_subs_json.as_array().unwrap().len(), 0);

        // Test 3l: duplicate linking on get_bug
        let dup_id = db
            .create_bug(&crate::db::NewBug {
                bugid: "linux-dup-endpoint".to_string(),
                title: "Duplicate issue".to_string(),
                lifecycle_status: crate::db::BugLifecycleStatus::New,
                pipeline_state: crate::db::BugPipelineState::Pending,
                assignee: None,
                reporter: "sashiko".to_string(),
                reported_at: 123457,
                discovered_in_patchset_id: None,
                discovered_in_patch_id: None,
                discovered_in_commit: None,
                source_ref: None,
                vector_json: None,
                duplicate_of_id: None,
                subsystems: vec![crate::db::AttributedSubsystem::from_maintainers(
                    "drivers/net",
                )],
            })
            .await
            .unwrap();
        db.mark_bug_as_duplicate(crate::db::MarkDuplicateBugParams {
            preserve_triage: false,
            ephemeral_id: dup_id,
            canonical_id: bug_id,
            reasoning: "Dup of test_device",
            logs: None,
            tokens_in: None,
            tokens_out: None,
            tokens_cached: None,
        })
        .await
        .unwrap();

        // Check duplicate_of on the duplicate bug
        let res_dup = get(format!("http://{}/api/bug?id={}", addr, dup_id))
            .await
            .unwrap();
        assert_eq!(res_dup.status(), 200);
        let dup_resp: serde_json::Value = res_dup.json().await.unwrap();
        assert_eq!(dup_resp["duplicate_of"]["id"], bug_id);
        assert_eq!(dup_resp["duplicate_of"]["bugid"], "linux-12345678");

        // Check duplicates array on canonical bug
        let res_canon = get(format!("http://{}/api/bug?id={}", addr, bug_id))
            .await
            .unwrap();
        assert_eq!(res_canon.status(), 200);
        let canon_resp: serde_json::Value = res_canon.json().await.unwrap();
        assert_eq!(canon_resp["duplicates"].as_array().unwrap().len(), 1);
        assert_eq!(canon_resp["duplicates"][0]["id"], dup_id);

        // Test 4: redirect_bug
        let no_redirect = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let res = no_redirect
            .get(format!("http://{}/bug/pb-12345678", addr))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 307);
        assert_eq!(
            res.headers().get("location").unwrap().to_str().unwrap(),
            "/#/bug/pb-12345678"
        );

        // Test 5: bug_action. The security list may comment on any bug.
        let action_res = client
            .post(format!("http://{}/api/bug/action?id={}", addr, bug_id))
            .json(&serde_json::json!({
                "action": "comment",
                "content": "A new test comment via API"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(action_res.status(), 200);

        // Closing rewrites the bug's state, which the security list may not do
        // outside its own subsystems.
        let refused = client
            .post(format!("http://{}/api/bug/action?id={}", addr, bug_id))
            .json(&serde_json::json!({"action":"close", "reason":"Fixed upstream"}))
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), 403);
        assert_eq!(
            refused.text().await.unwrap(),
            "This action requires maintainer authority over the bug."
        );

        let operator_token = crate::auth::create_token(
            "operator@example.org",
            SECRET,
            Some("session".to_string()),
            3600,
        )
        .unwrap();
        let close = reqwest::Client::new()
            .post(format!("http://{}/api/bug/action?id={}", addr, bug_id))
            .bearer_auth(operator_token)
            .json(&serde_json::json!({"action":"close", "reason":"Fixed upstream", "tool":"web"}))
            .send()
            .await
            .unwrap();
        assert_eq!(close.status(), 200);
        let stored = db.get_bug(bug_id).await.unwrap().unwrap();
        let comment = stored
            .enrichments
            .iter()
            .find(|e| e.content.as_deref() == Some("Fixed upstream"))
            .unwrap();
        assert_eq!(comment.author.as_deref(), Some("operator@example.org"));
        assert_eq!(comment.tool, "web");
        assert!(comment.model.is_none());
        let raw: serde_json::Value = get(format!("http://{}/api/bug/raw?id={}", addr, bug_id))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(raw["records"].as_array().unwrap().len(), 2);
        assert!(
            raw["records"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|b| b["enrichments"].as_array().unwrap())
                .any(|e| e["logs"].as_str().is_some_and(|s| s.contains("test")))
        );
        let missing = get(format!("http://{}/api/bug/raw?id=999999", addr))
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);

        // Test 6: get_config reports capabilities, and a remote unauthenticated
        // action is denied with a clear message. Bug authority is deliberately
        // absent from that report, since it is per bug rather than global.
        //
        // The security list holds no capability of its own, so an identity
        // alone reports none, however local the caller is.
        let cfg_identity: serde_json::Value = get(format!("http://{}/api/config", addr))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(cfg_identity["project"], "Linux");
        assert_eq!(cfg_identity["permissions"]["review"], false);
        assert!(cfg_identity["permissions"]["action"].is_null());

        // Presenting the token the server published is what grants them.
        let cfg_local: serde_json::Value = reqwest::Client::new()
            .get(format!("http://{}/api/config", addr))
            .bearer_auth(local_token.secret())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(cfg_local["permissions"]["review"], true);
        assert!(cfg_local["permissions"]["action"].is_null());

        let remote_denied = reqwest::Client::new()
            .post(format!("http://{}/api/bug/action?id={}", addr, bug_id))
            .header("x-forwarded-for", "203.0.113.1")
            .json(&serde_json::json!({
                "action": "comment",
                "content": "Attempt by remote guest"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(remote_denied.status(), 401);
        let err_msg = remote_denied.text().await.unwrap();
        assert_eq!(err_msg, "Please log in to access bugs repository.");
    }

    #[tokio::test]
    async fn test_acl_blocklist_authorization_and_auth_endpoints() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let mut base_settings = crate::settings::Settings::new().unwrap();
        base_settings.server.jwt_secret = Some("test_jwt_secret_12345678901234567890".to_string());
        base_settings.server.testing_mode = false;
        base_settings.server.acl.admins = vec!["admin@example.com".to_string()];
        base_settings.server.acl.review = vec!["reviewer@example.com".to_string()];
        base_settings.server.acl.blocklist = vec![
            "blocked@example.com".to_string(),
            "admin@example.com".to_string(),
        ];

        let settings = Arc::new(base_settings);
        let (event_tx, _event_rx) = mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = mpsc::channel(10);

        let app = build_router(
            settings.clone(),
            db.clone(),
            event_tx.clone(),
            fetch_tx.clone(),
            ServerOptions {
                dry_run: true,
                ..Default::default()
            },
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();

        // 1. request_link for blocklisted email (even though it's in admins) -> 200 OK (unconditional)
        let res_blocked_admin = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .json(&serde_json::json!({ "email": "admin@example.com" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res_blocked_admin.status(), 200);

        // 2. request_link for blocklisted email with case variations -> 200 OK (unconditional)
        let res_blocked_case = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .json(&serde_json::json!({ "email": "BLOCKED@example.com" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res_blocked_case.status(), 200);

        // 3. request_link for allowed email -> 200 OK
        let res_allowed = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .json(&serde_json::json!({ "email": "reviewer@example.com" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res_allowed.status(), 200);

        // 4. verify_link for blocklisted email -> 403 Forbidden
        let secret = "test_jwt_secret_12345678901234567890";
        let blocked_token = crate::auth::create_token(
            "blocked@example.com",
            secret,
            Some("sign_in_link".to_string()),
            1800,
        )
        .unwrap();

        let res_verify_blocked = client
            .get(format!(
                "http://{}/api/auth/verify?token={}",
                addr, blocked_token
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res_verify_blocked.status(), 403);

        // 5. verify_link for allowed email -> 200 OK and returns session token
        let allowed_token = crate::auth::create_token(
            "reviewer@example.com",
            secret,
            Some("sign_in_link".to_string()),
            1800,
        )
        .unwrap();

        let res_verify_allowed = client
            .get(format!(
                "http://{}/api/auth/verify?token={}",
                addr, allowed_token
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res_verify_allowed.status(), 200);
        let session_json: serde_json::Value = res_verify_allowed.json().await.unwrap();
        let session_token = session_json["token"].as_str().unwrap();
        let session_claims = crate::auth::verify_token(session_token, secret).unwrap();
        assert_eq!(
            session_claims.exp - session_claims.iat.unwrap(),
            SESSION_TOKEN_LIFETIME_SECONDS
        );

        // 6. refresh_token for blocklisted email session -> 403 Forbidden
        let blocked_session_token = crate::auth::create_token(
            "blocked@example.com",
            secret,
            Some("session".to_string()),
            SESSION_TOKEN_LIFETIME_SECONDS,
        )
        .unwrap();

        unsafe {
            std::env::set_var("JWT_SECRET", secret);
        }

        let res_refresh_blocked = client
            .post(format!("http://{}/api/auth/refresh", addr))
            .header("Authorization", format!("Bearer {}", blocked_session_token))
            .send()
            .await
            .unwrap();
        assert_eq!(res_refresh_blocked.status(), 403);

        // 7. refresh_token for allowed email session -> 200 OK
        let res_refresh_allowed = client
            .post(format!("http://{}/api/auth/refresh", addr))
            .header("Authorization", format!("Bearer {}", session_token))
            .send()
            .await
            .unwrap();
        assert_eq!(res_refresh_allowed.status(), 200);
        let refreshed_json: serde_json::Value = res_refresh_allowed.json().await.unwrap();
        let refreshed_claims =
            crate::auth::verify_token(refreshed_json["token"].as_str().unwrap(), secret).unwrap();
        assert!(
            refreshed_claims.exp >= session_claims.iat.unwrap() + SESSION_TOKEN_LIFETIME_SECONDS
        );

        // Sign-in link token cannot be used directly as a session API token
        let res_bearer_sign_in = client
            .post(format!("http://{}/api/auth/refresh", addr))
            .header("Authorization", format!("Bearer {}", allowed_token))
            .send()
            .await
            .unwrap();
        assert_eq!(res_bearer_sign_in.status(), 401);

        // Session token whose initial issue date is 31 days old (< 1 year) can still be refreshed
        let month_old_iat = u64::try_from(chrono::Utc::now().timestamp())
            .unwrap_or(0)
            .saturating_sub(31 * 86400);
        let month_old_session_token = crate::auth::create_token_with_session(
            "reviewer@example.com",
            secret,
            Some("session".to_string()),
            SESSION_TOKEN_LIFETIME_SECONDS,
            Some(month_old_iat),
            Some("month-old-session-id".to_string()),
        )
        .unwrap();
        let res_month_old_refresh = client
            .post(format!("http://{}/api/auth/refresh", addr))
            .header(
                "Authorization",
                format!("Bearer {}", month_old_session_token),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(res_month_old_refresh.status(), 200);

        // Session token whose initial issue date is older than 1 year cannot be refreshed
        let old_iat = u64::try_from(chrono::Utc::now().timestamp())
            .unwrap_or(0)
            .saturating_sub(366 * 86400);
        let expired_session_token = crate::auth::create_token_with_session(
            "reviewer@example.com",
            secret,
            Some("session".to_string()),
            SESSION_TOKEN_LIFETIME_SECONDS,
            Some(old_iat),
            Some("old-session-id".to_string()),
        )
        .unwrap();
        let res_expired_refresh = client
            .post(format!("http://{}/api/auth/refresh", addr))
            .header("Authorization", format!("Bearer {}", expired_session_token))
            .send()
            .await
            .unwrap();
        assert_eq!(res_expired_refresh.status(), 401);

        // 8. Test is_authorized directly:
        let mut proxy_headers = axum::http::HeaderMap::new();
        proxy_headers.insert("x-forwarded-for", "203.0.113.195".parse().unwrap());
        let state_arc = Arc::new(AppState {
            settings: settings.clone(),
            db: db.clone(),
            sender: event_tx,
            fetch_sender: fetch_tx,
            read_only: false,
            forge_registry: Arc::new(crate::forge::ForgeRegistry::new()),
            allow_all_submit: false,
            smtp_enabled: false,
            dry_run: true,
            local_token: None,
            email_worker_heartbeat: None,
            activity: Default::default(),
            pull_request_resolver: Arc::new(FakeResolver::default()),
            sign_in_link_rate_limiter: SignInLinkRateLimiter::new(),
            stats_timeline_cache: AsyncMapCache::new(Duration::from_secs(60)),
            stats_reviews_cache: AsyncCache::new(Duration::from_secs(60)),
            stats_tools_cache: AsyncCache::new(Duration::from_secs(60)),
            messages_count_cache: AsyncCache::new(Duration::from_secs(30)),
            patchsets_count_cache: AsyncCache::new(Duration::from_secs(30)),
            patchsets_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
            messages_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
            bug_subsystems_cache: AsyncMapCache::new(Duration::from_secs(5)),
        });

        let blocked_user = crate::auth::AuthUser::new("blocked@example.com");
        assert!(!is_authorized(
            &state_arc,
            &proxy_headers,
            Some(&blocked_user),
            crate::settings::Permission::Review,
        ));

        let empty_headers = axum::http::HeaderMap::new();
        assert!(!is_authorized(
            &state_arc,
            &empty_headers,
            Some(&blocked_user),
            crate::settings::Permission::Review,
        ));

        let allowed_user = crate::auth::AuthUser::new("reviewer@example.com");
        assert!(is_authorized(
            &state_arc,
            &proxy_headers,
            Some(&allowed_user),
            crate::settings::Permission::Review
        ));
    }

    #[tokio::test]
    async fn test_sign_in_link_email_delivery_and_logging() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let mut settings = crate::settings::Settings::new().unwrap();
        settings.server.jwt_secret = Some("test-jwt-secret-for-email-delivery-123".to_string());
        settings.server.testing_mode = false;
        settings.server.public_base_url = Some("https://sashiko.dev".to_string());
        settings.server.acl.admins = vec!["maintainer@example.org".to_string()];
        settings.smtp = Some(crate::settings::SmtpSettings {
            transport: crate::settings::MailTransport::Smtp,
            server: Some("smtp.example.org".to_string()),
            port: Some(587),
            username: None,
            password: None,
            sendmail_path: None,
            sender_address: "sashiko@sashiko.dev".to_string(),
            reply_to: None,
            dry_run: false,
        });

        let (event_tx, _event_rx) = mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = mpsc::channel(10);
        let app = build_router(
            Arc::new(settings),
            db.clone(),
            event_tx,
            fetch_tx,
            ServerOptions {
                smtp_enabled: true,
                ..Default::default()
            },
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let res = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .header("x-forwarded-for", "198.51.100.24, 10.0.0.1")
            .json(&serde_json::json!({ "email": "maintainer@example.org" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        let email = db
            .lock_pending_email()
            .await
            .unwrap()
            .expect("email was queued in outbox");
        assert_eq!(email.kind, crate::db::EmailKind::SignInLink);
        assert_eq!(email.status, "Sending");
        assert_eq!(email.to_addresses, r#"["maintainer@example.org"]"#);
        assert_eq!(email.subject, "[sashiko] Your sign-in link");
        assert!(
            email
                .body
                .contains("https://sashiko.dev/auth/verify?token=")
        );
        assert!(email.body.contains("Requested from 198.51.100.24."));
        assert!(email.body.contains("Open this link within 30 minutes"));
        assert!(
            email
                .body
                .contains("-- \nSashiko AI review · https://sashiko.dev")
        );
    }

    #[tokio::test]
    async fn test_bug_assignment_emails_only_a_new_assignee() {
        const SECRET: &str = "bug-assign-secret-123456789012345678901";
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();
        let bug_id = db
            .create_bug(&crate::db::NewBug {
                bugid: "linux-assign01".to_string(),
                title: "UAF in\nbtrfs".to_string(),
                lifecycle_status: crate::db::BugLifecycleStatus::New,
                pipeline_state: crate::db::BugPipelineState::Pending,
                assignee: None,
                reporter: "sashiko".to_string(),
                reported_at: 123456,
                discovered_in_patchset_id: None,
                discovered_in_patch_id: None,
                discovered_in_commit: None,
                source_ref: None,
                vector_json: None,
                duplicate_of_id: None,
                subsystems: vec![crate::db::AttributedSubsystem::from_maintainers(
                    "BTRFS FILE SYSTEM",
                )],
            })
            .await
            .unwrap();

        let mut settings = crate::settings::Settings::new().unwrap();
        settings.server.testing_mode = false;
        settings.server.read_only = false;
        settings.server.jwt_secret = Some(SECRET.to_string());
        settings.server.public_base_url = Some("https://sashiko.dev".to_string());
        settings.server.acl.admins = vec!["lead@example.org".to_string()];
        settings.server.acl.blocklist = vec!["revoked@example.org".to_string()];
        settings.smtp = Some(crate::settings::SmtpSettings {
            transport: crate::settings::MailTransport::Smtp,
            server: Some("smtp.example.org".to_string()),
            port: Some(587),
            username: None,
            password: None,
            sendmail_path: None,
            sender_address: "sashiko@sashiko.dev".to_string(),
            reply_to: None,
            dry_run: false,
        });

        let (event_tx, _event_rx) = mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = mpsc::channel(10);
        let app = build_router(
            Arc::new(settings),
            db.clone(),
            event_tx,
            fetch_tx,
            ServerOptions {
                smtp_enabled: true,
                ..Default::default()
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let token = crate::auth::create_token(
            "lead@example.org",
            SECRET,
            Some("session".to_string()),
            3600,
        )
        .unwrap();
        let assign = |assignee: Option<&str>, reason: Option<&str>| {
            client
                .post(format!("http://{}/api/bug/action?id={}", addr, bug_id))
                .header("Authorization", format!("Bearer {}", token))
                .json(&serde_json::json!({
                    "action": "assign",
                    "assignee": assignee,
                    "reason": reason,
                }))
                .send()
        };

        let res = assign(Some("dev@example.org"), Some("Could you take this?"))
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let email = db
            .lock_pending_email()
            .await
            .unwrap()
            .expect("assignment notice queued");
        assert_eq!(email.kind, crate::db::EmailKind::BugAssignment);
        assert_eq!(email.to_addresses, r#"["dev@example.org"]"#);
        assert_eq!(email.cc_addresses, r#"["lead@example.org"]"#);
        assert_eq!(
            email.subject,
            "[sashiko] lead@example.org assigned you a bug: UAF in btrfs"
        );
        assert!(
            email
                .body
                .starts_with("lead@example.org assigned you this Sashiko bug report:\n\n")
        );
        assert!(
            email
                .body
                .contains("  UAF in btrfs\n  https://sashiko.dev/bug/linux-assign01\n")
        );
        assert!(
            email
                .body
                .contains("Their note:\n\n  Could you take this?\n\n")
        );
        assert!(
            email
                .body
                .contains("Questions about the assignment go to lead@example.org")
        );
        db.mark_email_sent(email.id).await.unwrap();

        // The same assignee again, in either case, a list, Sashiko's own,
        // a blocklisted or a non-plain address, a self-assignment and an
        // unassignment queue no notice.
        for assignee in [
            Some("dev@example.org"),
            Some("Dev@example.org"),
            Some("kvmarm@lists.linux.dev"),
            Some("sashiko@sashiko.dev"),
            Some("revoked@example.org"),
            Some("x<y@example.com>@example.org"),
            Some("lead@example.org"),
            None,
        ] {
            let res = assign(assignee, None).await.unwrap();
            assert_eq!(res.status(), 200);
        }
        assert!(db.lock_pending_email().await.unwrap().is_none());
        assert_eq!(db.get_bug(bug_id).await.unwrap().unwrap().assignee, None);
    }

    #[test]
    fn test_bug_assignment_email_bounds_the_title_and_note() {
        let title = "é".repeat(100);
        let link = "https://sashiko.dev/bug/linux-x";
        let base = "https://sashiko.dev";
        let (subject, body) =
            build_bug_assignment_email("lead@example.org", &title, link, Some("   "), base);
        assert!(subject.ends_with(&format!("{}...", "é".repeat(40))));
        assert!(body.contains(&title));
        assert!(!body.contains("Their note"));

        let note = "x".repeat(ASSIGNMENT_NOTE_BYTES + 1);
        let (_, body) =
            build_bug_assignment_email("lead@example.org", "t", link, Some(&note), base);
        assert!(body.contains(&format!("  {}\n", "x".repeat(ASSIGNMENT_NOTE_BYTES))));
        assert!(!body.contains(&note));
        assert!(body.contains("  [...] The full note is on the bug page.\n"));
    }

    #[tokio::test]
    async fn test_request_link_unconditional_response_equivalence() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let mut settings = crate::settings::Settings::new().unwrap();
        settings.smtp = Some(crate::settings::SmtpSettings {
            transport: crate::settings::MailTransport::Smtp,
            server: Some("smtp.example.org".to_string()),
            port: Some(587),
            username: None,
            password: None,
            sendmail_path: None,
            sender_address: "sashiko@sashiko.dev".to_string(),
            reply_to: None,
            dry_run: false,
        });
        settings.server.jwt_secret = Some("test_jwt_secret_12345678901234567890".to_string());
        settings.server.testing_mode = false;
        settings.server.public_base_url = Some("https://sashiko.dev".to_string());
        settings
            .server
            .acl
            .bug_reporters
            .push("eligible@example.com".to_string());
        settings
            .server
            .acl
            .blocklist
            .push("blocklisted@example.com".to_string());

        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = tokio::sync::mpsc::channel(10);
        let app = build_router(
            Arc::new(settings),
            db.clone(),
            event_tx,
            fetch_tx,
            ServerOptions {
                smtp_enabled: true,
                ..Default::default()
            },
        );

        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();

        // 1. Eligible address
        let res_eligible = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .json(&serde_json::json!({ "email": "eligible@example.com" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res_eligible.status(), 200);
        let bytes_eligible = res_eligible.bytes().await.unwrap();

        // 2. Ineligible (unknown) address
        let res_ineligible = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .json(&serde_json::json!({ "email": "unregistered@example.com" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res_ineligible.status(), 200);
        let bytes_ineligible = res_ineligible.bytes().await.unwrap();

        // 3. Blocklisted address
        let res_blocklisted = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .json(&serde_json::json!({ "email": "blocklisted@example.com" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res_blocklisted.status(), 200);
        let bytes_blocklisted = res_blocklisted.bytes().await.unwrap();

        // 4. Malformed address
        let res_malformed_email = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .json(&serde_json::json!({ "email": "not-an-email" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res_malformed_email.status(), 200);
        let bytes_malformed_email = res_malformed_email.bytes().await.unwrap();

        // 5. Malformed payload
        let res_malformed_json = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .header("Content-Type", "application/json")
            .body("{not valid json")
            .send()
            .await
            .unwrap();
        assert_eq!(res_malformed_json.status(), 200);
        let bytes_malformed_json = res_malformed_json.bytes().await.unwrap();

        // All responses must be byte-identical
        assert_eq!(bytes_eligible, bytes_ineligible);
        assert_eq!(bytes_eligible, bytes_blocklisted);
        assert_eq!(bytes_eligible, bytes_malformed_email);
        assert_eq!(bytes_eligible, bytes_malformed_json);

        // Only the eligible address must have queued an email
        let queued = db
            .lock_pending_email()
            .await
            .unwrap()
            .expect("eligible email queued");
        assert_eq!(queued.to_addresses, r#"["eligible@example.com"]"#);
        assert!(db.lock_pending_email().await.unwrap().is_none());
    }

    #[test]
    fn test_sign_in_link_rate_limiter_buckets() {
        let limiter = SignInLinkRateLimiter::new();

        // 1. Per-address limit (3 per 15 mins)
        assert!(limiter.check_and_record("dev@example.org", "192.0.2.1"));
        assert!(limiter.check_and_record("dev@example.org", "192.0.2.2"));
        assert!(limiter.check_and_record("dev@example.org", "192.0.2.3"));
        // 4th attempt for the same address fails even from another IP
        assert!(!limiter.check_and_record("dev@example.org", "192.0.2.4"));

        // Another address from 192.0.2.1 still succeeds
        assert!(limiter.check_and_record("dev2@example.org", "192.0.2.1"));

        // 2. Per-IP limit (10 per 15 mins)
        // 192.0.2.1 already made 2 requests above (dev@example.org and dev2@example.org)
        for i in 3..=10 {
            assert!(limiter.check_and_record(&format!("user{}@example.org", i), "192.0.2.1"));
        }
        // 11th request from 192.0.2.1 fails regardless of address
        assert!(!limiter.check_and_record("user11@example.org", "192.0.2.1"));
        // But another IP can still request
        assert!(limiter.check_and_record("user11@example.org", "192.0.2.99"));

        // 3. Global limit (100 per hour)
        let global_limiter = SignInLinkRateLimiter::new();
        for i in 0..100 {
            assert!(global_limiter.check_and_record(
                &format!("batch{}@example.org", i),
                &format!("198.51.100.{}", i % 250),
            ));
        }
        // 101st request fails globally
        assert!(!global_limiter.check_and_record("overflow@example.org", "198.51.100.254"));
    }

    #[tokio::test]
    async fn test_request_link_rate_limiting_suppresses_mail_but_returns_200() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let mut settings = crate::settings::Settings::new().unwrap();
        settings.smtp = Some(crate::settings::SmtpSettings {
            transport: crate::settings::MailTransport::Smtp,
            server: Some("smtp.example.org".to_string()),
            port: Some(587),
            username: None,
            password: None,
            sendmail_path: None,
            sender_address: "sashiko@sashiko.dev".to_string(),
            reply_to: None,
            dry_run: false,
        });
        settings.server.jwt_secret = Some("test_jwt_secret_12345678901234567890".to_string());
        settings.server.testing_mode = false;
        settings.server.public_base_url = Some("https://sashiko.dev".to_string());
        settings
            .server
            .acl
            .bug_reporters
            .push("maintainer@example.org".to_string());

        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = tokio::sync::mpsc::channel(10);
        let app = build_router(
            Arc::new(settings),
            db.clone(),
            event_tx,
            fetch_tx,
            ServerOptions {
                smtp_enabled: true,
                ..Default::default()
            },
        );

        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();

        // 3 allowed requests within 15 minutes
        for _ in 0..3 {
            let res = client
                .post(format!("http://{}/api/auth/request-link", addr))
                .json(&serde_json::json!({ "email": "maintainer@example.org" }))
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), 200);
        }

        // 4th request exceeds rate limit: still returns 200 OK
        let res_4th = client
            .post(format!("http://{}/api/auth/request-link", addr))
            .json(&serde_json::json!({ "email": "maintainer@example.org" }))
            .send()
            .await
            .unwrap();
        assert_eq!(res_4th.status(), 200);

        // Verify exactly 3 emails were queued, not 4
        let mut queued_count = 0;
        while let Ok(Some(_email)) = db.lock_pending_email().await {
            queued_count += 1;
        }
        assert_eq!(queued_count, 3);
    }

    #[tokio::test]
    async fn test_blocklist_outranks_every_bypass() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let state_with = |testing_mode: bool, allow_all_submit: bool| {
            let mut settings = crate::settings::Settings::new().unwrap();
            settings.server.testing_mode = testing_mode;
            settings.server.acl.admins = vec!["operator@example.org".to_string()];
            // Spelled with stray padding and mixed case to pin the tolerant
            // comparison, since a revocation that a capital letter defeats is
            // not a revocation.
            settings.server.acl.blocklist = vec![" Blocked@Example.ORG ".to_string()];
            let (event_tx, _event_rx) = mpsc::channel(10);
            let (fetch_tx, _fetch_rx) = mpsc::channel(10);
            Arc::new(AppState {
                settings: Arc::new(settings),
                db: db.clone(),
                sender: event_tx,
                fetch_sender: fetch_tx,
                read_only: false,
                forge_registry: Arc::new(crate::forge::ForgeRegistry::new()),
                allow_all_submit,
                smtp_enabled: false,
                dry_run: true,
                local_token: None,
                email_worker_heartbeat: None,
                activity: Default::default(),
                pull_request_resolver: Arc::new(FakeResolver::default()),
                sign_in_link_rate_limiter: SignInLinkRateLimiter::new(),
                stats_timeline_cache: AsyncMapCache::new(Duration::from_secs(60)),
                stats_reviews_cache: AsyncCache::new(Duration::from_secs(60)),
                stats_tools_cache: AsyncCache::new(Duration::from_secs(60)),
                messages_count_cache: AsyncCache::new(Duration::from_secs(30)),
                patchsets_count_cache: AsyncCache::new(Duration::from_secs(30)),
                patchsets_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
                messages_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
                bug_subsystems_cache: AsyncMapCache::new(Duration::from_secs(5)),
            })
        };

        let blocked = crate::auth::AuthUser::new("blocked@example.org");
        let operator = crate::auth::AuthUser::new("operator@example.org");
        let headers = axum::http::HeaderMap::new();

        for (testing_mode, allow_all_submit) in [(true, false), (false, true), (true, true)] {
            let state = state_with(testing_mode, allow_all_submit);
            assert!(
                !is_authorized(
                    &state,
                    &headers,
                    Some(&blocked),
                    crate::settings::Permission::Review
                ),
                "blocklisted caller admitted with testing_mode={} allow_all_submit={}",
                testing_mode,
                allow_all_submit
            );
            assert!(
                is_authorized(
                    &state,
                    &headers,
                    Some(&operator),
                    crate::settings::Permission::Review
                ),
                "bypass stopped working for everyone else"
            );
        }
    }

    #[tokio::test]
    async fn test_loopback_grants_nothing_without_the_token() {
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let state_with = |local_token: Option<crate::auth::LocalToken>| {
            let mut settings = crate::settings::Settings::new().unwrap();
            settings.server.testing_mode = false;
            let (event_tx, _event_rx) = mpsc::channel(10);
            let (fetch_tx, _fetch_rx) = mpsc::channel(10);
            Arc::new(AppState {
                settings: Arc::new(settings),
                db: db.clone(),
                sender: event_tx,
                fetch_sender: fetch_tx,
                read_only: false,
                forge_registry: Arc::new(crate::forge::ForgeRegistry::new()),
                allow_all_submit: false,
                smtp_enabled: false,
                dry_run: true,
                local_token,
                email_worker_heartbeat: None,
                activity: Default::default(),
                pull_request_resolver: Arc::new(FakeResolver::default()),
                sign_in_link_rate_limiter: SignInLinkRateLimiter::new(),
                stats_timeline_cache: AsyncMapCache::new(Duration::from_secs(60)),
                stats_reviews_cache: AsyncCache::new(Duration::from_secs(60)),
                stats_tools_cache: AsyncCache::new(Duration::from_secs(60)),
                messages_count_cache: AsyncCache::new(Duration::from_secs(30)),
                patchsets_count_cache: AsyncCache::new(Duration::from_secs(30)),
                patchsets_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
                messages_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
                bug_subsystems_cache: AsyncMapCache::new(Duration::from_secs(5)),
            })
        };

        let token = crate::auth::LocalToken::generate().unwrap();
        let bare = axum::http::HeaderMap::new();

        let mut presented = axum::http::HeaderMap::new();
        presented.insert(
            "authorization",
            format!("Bearer {}", token.secret()).parse().unwrap(),
        );

        // The dangerous shape: a reverse proxy that forwards no markers makes
        // every remote request look exactly like a local one, so arriving on
        // loopback buys nothing at all.
        assert!(
            !is_authorized(
                &state_with(None),
                &bare,
                None,
                crate::settings::Permission::Review
            ),
            "a request was authorized for its source address"
        );

        // Holding the token is the whole of the claim, so failing to present
        // one the server did publish is no different.
        assert!(
            !is_authorized(
                &state_with(Some(token.clone())),
                &bare,
                None,
                crate::settings::Permission::Review
            ),
            "a caller that presented nothing was admitted"
        );

        assert!(
            is_authorized(
                &state_with(Some(token.clone())),
                &presented,
                None,
                crate::settings::Permission::Review
            ),
            "the token was refused"
        );

        // Proxy markers used to veto the old bypass. They are irrelevant now:
        // what the caller can present does not change with the route it took.
        let mut forwarded = presented.clone();
        forwarded.insert("x-forwarded-for", "203.0.113.195".parse().unwrap());
        assert!(
            is_authorized(
                &state_with(Some(token)),
                &forwarded,
                None,
                crate::settings::Permission::Review
            ),
            "a header a caller controls changed the answer"
        );
    }

    #[tokio::test]
    async fn bug_evidence_excludes_inaccessible_family_members() {
        let db = Arc::new(
            Database::new(&crate::settings::DatabaseSettings {
                url: ":memory:".into(),
                token: String::new(),
            })
            .await
            .unwrap(),
        );
        db.migrate().await.unwrap();
        let mut ids = Vec::new();
        for (name, section) in [
            ("canonical", "SECTION A"),
            ("hidden", "SECTION B"),
            ("sibling", "SECTION A"),
        ] {
            let bug: crate::db::NewBug = serde_json::from_value(serde_json::json!({
                "bugid": name, "title": format!("{name} title"), "reporter": format!("{name}@example.org"),
                "subsystems": [{"name": section, "source": "maintainers_section"}]
            })).unwrap();
            let id = db.create_bug(&bug).await.unwrap();
            for kind in ["report", "comment", "candidate"] {
                db.add_bug_enrichment(
                    id,
                    &crate::db::NewBugEnrichment {
                        kind: kind.into(),
                        content: Some(format!("{name} {kind} confidential content")),
                        model: Some(format!("{name}-model")),
                        tool: format!("{name}-tool"),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            }
            ids.push(id);
        }
        db.add_bug_enrichment(
            ids[0],
            &crate::db::NewBugEnrichment {
                kind: "deduplication".into(),
                content: Some("Deduplication Check completed".into()),
                data_json: Some(serde_json::json!({
                    "stage_id": "deduplication",
                    "stage_title": "Deduplication Check"
                })),
                tool: "sashiko:linux_bug".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        for &id in &ids[1..] {
            db.mark_bug_as_duplicate(crate::db::MarkDuplicateBugParams {
                ephemeral_id: id,
                canonical_id: ids[0],
                reasoning: "same defect as canonical",
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let settings = Arc::new(crate::settings::Settings::new().unwrap());
        let (sender, _) = mpsc::channel(1);
        let (fetch_sender, _) = mpsc::channel(1);
        let state = Arc::new(AppState {
            settings,
            db,
            sender,
            fetch_sender,
            read_only: false,
            allow_all_submit: false,
            smtp_enabled: false,
            dry_run: true,
            forge_registry: Arc::new(crate::forge::ForgeRegistry::new()),
            sign_in_link_rate_limiter: SignInLinkRateLimiter::new(),
            stats_timeline_cache: AsyncMapCache::new(Duration::from_secs(60)),
            stats_reviews_cache: AsyncCache::new(Duration::from_secs(60)),
            stats_tools_cache: AsyncCache::new(Duration::from_secs(60)),
            messages_count_cache: AsyncCache::new(Duration::from_secs(30)),
            patchsets_count_cache: AsyncCache::new(Duration::from_secs(30)),
            patchsets_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
            messages_homepage_cache: AsyncCache::new(Duration::from_secs(10)),
            bug_subsystems_cache: AsyncMapCache::new(Duration::from_secs(5)),
            local_token: None,
            email_worker_heartbeat: None,
            activity: Default::default(),
            pull_request_resolver: Arc::new(FakeResolver::default()),
        });
        let index = crate::maintainers::MaintainersIndex::from_reader(
            b"Maintainers List\n================\n\nSECTION A\nM:\tAlice <a@example.org>\nF:\ta/\n\nSECTION B\nM:\tBob <b@example.org>\nF:\tb/\n".as_slice()
        ).unwrap();
        let acl = crate::settings::AclSettings {
            admins: vec!["operator@example.org".into()],
            ..Default::default()
        };
        // Exercise the canonical, a sibling duplicate, and a duplicate whose
        // canonical bug is invisible. Resolve locally to avoid global test state.
        for (email, id, count, excluded) in [
            ("a@example.org", ids[0], 2, vec!["hidden"]),
            ("a@example.org", ids[2], 2, vec!["hidden"]),
            ("b@example.org", ids[1], 1, vec!["canonical", "sibling"]),
            ("operator@example.org", ids[0], 3, vec![]),
        ] {
            let principal = Principal::resolve(email, &acl, Some(&index));
            let Json(body) = get_bug(
                Ok(principal.clone()),
                State(state.clone()),
                Query(BugQuery {
                    id: Some(id),
                    bugid: None,
                    slug: None,
                }),
            )
            .await
            .unwrap();
            assert_eq!(body["evidence"]["count"], count);
            let evidence = body["evidence"].to_string();
            for name in &excluded {
                assert!(
                    !evidence.contains(name),
                    "{email} learned about {name}: {evidence}"
                );
            }
            if email == "a@example.org" && id == ids[0] {
                assert!(evidence.contains("Deduplication Check completed"));
                let Json(enrichments) = get_bug_enrichments(
                    principal.clone(),
                    State(state.clone()),
                    Query(BugQuery {
                        id: Some(id),
                        bugid: None,
                        slug: None,
                    }),
                )
                .await
                .unwrap();
                assert!(
                    enrichments
                        .to_string()
                        .contains("Deduplication Check completed")
                );
            }
            if email == "b@example.org" {
                assert!(body.get("duplicate_of_id").is_none());
                assert!(body.get("duplicate_of").is_none());
                assert!(!evidence.contains("hidden candidate confidential content"));

                let Json(enrichments) = get_bug_enrichments(
                    principal,
                    State(state.clone()),
                    Query(BugQuery {
                        id: Some(id),
                        bugid: None,
                        slug: None,
                    }),
                )
                .await
                .unwrap();
                let enrichments_str = enrichments.to_string();
                assert!(!enrichments_str.contains("canonical"));
                assert!(!enrichments_str.contains("hidden candidate confidential content"));
                let items = enrichments.as_array().unwrap();
                let audit_dup = items
                    .iter()
                    .find(|e| e["kind"] == "audit" && e["data_json"]["field"] == "duplicate_of_id")
                    .expect("expected duplicate_of_id audit enrichment");
                assert_eq!(audit_dup["content"], "Duplicate relationship updated");
                assert_eq!(
                    audit_dup["data_json"],
                    serde_json::json!({ "field": "duplicate_of_id" })
                );
                let dedup = items
                    .iter()
                    .find(|e| e["kind"] == "deduplication")
                    .expect("expected deduplication enrichment");
                assert_eq!(dedup["content"], "Matched an existing bug");
                assert!(dedup["data_json"].is_null());
            }
            if email == "operator@example.org" {
                assert!(evidence.contains("hidden comment confidential content"));
                assert!(evidence.contains("hidden-model"));
                assert!(evidence.contains("same defect as canonical"));
            }
        }

        // Anonymous capability read by bugid sees only that single bug's own
        // evidence and never learns about its canonical or duplicate siblings.
        for (bugid, excluded) in [
            ("canonical", vec!["hidden", "sibling"]),
            ("hidden", vec!["canonical", "sibling"]),
        ] {
            let Json(body) = get_bug(
                Err((StatusCode::UNAUTHORIZED, "no token")),
                State(state.clone()),
                Query(BugQuery {
                    id: None,
                    bugid: Some(bugid.into()),
                    slug: None,
                }),
            )
            .await
            .unwrap();
            assert_eq!(body["bugid"], bugid);
            assert_eq!(body["can_comment"], false);
            assert_eq!(body["can_manage"], false);
            assert_eq!(body["evidence"]["count"], 1);
            let evidence = body["evidence"].to_string();
            assert!(evidence.contains(&format!("{bugid} comment confidential content")));
            assert!(!evidence.contains(&format!("{bugid} candidate confidential content")));
            for name in excluded {
                assert!(
                    !evidence.contains(name),
                    "anonymous bugid lookup for {bugid} leaked {name}: {evidence}"
                );
            }
            assert!(body["duplicate_of_id"].is_null());
            assert!(body.get("duplicate_of").is_none());
            if let Some(dups) = body.get("duplicates").and_then(|d| d.as_array()) {
                assert!(dups.is_empty());
            }
        }

        // Re-parenting `hidden` (SECTION B) from `canonical` (SECTION A) to a
        // readable SECTION B bug must still redact the historical audit and
        // deduplication enrichments for `b@example.org`.
        let b_canon_spec: crate::db::NewBug = serde_json::from_value(serde_json::json!({
            "bugid": "b-canon", "title": "b-canon title", "reporter": "b@example.org",
            "subsystems": [{"name": "SECTION B", "source": "maintainers_section"}]
        }))
        .unwrap();
        let b_canon_id = state.db.create_bug(&b_canon_spec).await.unwrap();
        state
            .db
            .mark_bug_as_duplicate(crate::db::MarkDuplicateBugParams {
                ephemeral_id: ids[1],
                canonical_id: b_canon_id,
                reasoning: "re-parented to b-canon",
                ..Default::default()
            })
            .await
            .unwrap();
        let b_principal = Principal::resolve("b@example.org", &acl, Some(&index));
        let Json(reparented_body) = get_bug(
            Ok(b_principal.clone()),
            State(state.clone()),
            Query(BugQuery {
                id: Some(ids[1]),
                bugid: None,
                slug: None,
            }),
        )
        .await
        .unwrap();
        assert!(
            !reparented_body["evidence"]
                .to_string()
                .contains("canonical")
        );
        let Json(reparented_enrichments) = get_bug_enrichments(
            b_principal,
            State(state.clone()),
            Query(BugQuery {
                id: Some(ids[1]),
                bugid: None,
                slug: None,
            }),
        )
        .await
        .unwrap();
        assert!(!reparented_enrichments.to_string().contains("canonical"));
        for item in reparented_enrichments.as_array().unwrap() {
            if item["kind"] == "audit" && item["data_json"]["field"] == "duplicate_of_id" {
                assert_eq!(
                    item["data_json"],
                    serde_json::json!({ "field": "duplicate_of_id" })
                );
            }
        }
    }

    #[tokio::test]
    async fn test_bug_reads_require_an_authorized_principal() {
        const SECRET: &str = "bug-authz-secret-12345678901234567890";
        let db_settings = crate::settings::DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        };
        let db = Arc::new(Database::new(&db_settings).await.unwrap());
        db.migrate().await.unwrap();

        let bug_id = db
            .create_bug(&crate::db::NewBug {
                bugid: "linux-authz01".to_string(),
                title: "UAF in btrfs".to_string(),
                lifecycle_status: crate::db::BugLifecycleStatus::New,
                pipeline_state: crate::db::BugPipelineState::Pending,
                assignee: None,
                reporter: "sashiko".to_string(),
                reported_at: 123456,
                discovered_in_patchset_id: None,
                discovered_in_patch_id: None,
                discovered_in_commit: None,
                source_ref: None,
                vector_json: None,
                duplicate_of_id: None,
                subsystems: vec![crate::db::AttributedSubsystem::from_maintainers(
                    "BTRFS FILE SYSTEM",
                )],
            })
            .await
            .unwrap();

        let mut settings = crate::settings::Settings::new().unwrap();
        settings.server.testing_mode = false;
        settings.server.jwt_secret = Some(SECRET.to_string());
        settings.server.acl.admins = vec!["operator@example.org".to_string()];
        settings.server.acl.security = Vec::new();
        settings.server.acl.blocklist = Vec::new();
        settings.server.acl.bug_reporters = Vec::new();
        settings.server.read_only = false;
        let settings = Arc::new(settings);

        let (event_tx, _event_rx) = mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = mpsc::channel(10);
        let app = build_router(
            settings.clone(),
            db.clone(),
            event_tx,
            fetch_tx,
            ServerOptions {
                dry_run: true,
                ..Default::default()
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let token = |email: &str| {
            crate::auth::create_token(email, SECRET, Some("session".to_string()), 3600).unwrap()
        };
        let get = |path: String, bearer: Option<String>| {
            let mut req = client.get(format!("http://{}{}", addr, path));
            if let Some(bearer) = bearer {
                req = req.header("Authorization", format!("Bearer {}", bearer));
            }
            req.send()
        };

        // Sequential numeric ID lookups require authentication and subsystem
        // authority.
        let anonymous = get(format!("/api/bug?id={}", bug_id), None).await.unwrap();
        assert_eq!(anonymous.status(), 401);

        let garbage = get(format!("/api/bug?id={}", bug_id), Some("nonsense".into()))
            .await
            .unwrap();
        assert_eq!(garbage.status(), 401);

        let anon_enrich_id = get(format!("/api/bug/enrichments?id={}", bug_id), None)
            .await
            .unwrap();
        assert_eq!(anon_enrich_id.status(), 401);

        // Authenticated, but maintains nothing and holds no capability. Lookup
        // by sequential ID must be indistinguishable from an absent bug.
        let stranger = token("stranger@example.org");
        let denied = get(format!("/api/bug?id={}", bug_id), Some(stranger.clone()))
            .await
            .unwrap();
        assert_eq!(denied.status(), 404);
        let denied_enrich_id = get(
            format!("/api/bug/enrichments?id={}", bug_id),
            Some(stranger.clone()),
        )
        .await
        .unwrap();
        assert_eq!(denied_enrich_id.status(), 404);
        let absent = get("/api/bug?id=999999".to_string(), Some(stranger.clone()))
            .await
            .unwrap();
        assert_eq!(absent.status(), 404);

        // Knowing the bugid or slug grants read-only access on GET /api/bug
        // for both anonymous and unprivileged authenticated callers, while
        // GET /api/bug/enrichments remains gated by Principal authority.
        for bearer in [None, Some("nonsense".into()), Some(stranger.clone())] {
            for query in ["bugid=linux-authz01", "slug=linux-authz01"] {
                let res = get(format!("/api/bug?{}", query), bearer.clone())
                    .await
                    .unwrap();
                assert_eq!(res.status(), 200);
                let body: serde_json::Value = res.json().await.unwrap();
                assert_eq!(body["bugid"], "linux-authz01");
                assert_eq!(body["can_comment"], false);
                assert_eq!(body["can_manage"], false);
            }
            let enrich_res = get(
                "/api/bug/enrichments?bugid=linux-authz01".to_string(),
                bearer.clone(),
            )
            .await
            .unwrap();
            let expected_enrich_status = if bearer.as_deref() == Some(stranger.as_str()) {
                404
            } else {
                401
            };
            assert_eq!(enrich_res.status(), expected_enrich_status);

            let missing_bugid = get(
                "/api/bug?bugid=linux-does-not-exist".to_string(),
                bearer.clone(),
            )
            .await
            .unwrap();
            assert_eq!(missing_bugid.status(), 404);
        }

        // Raw transcript routes remain restricted even when bugid is known.
        for path in ["/api/bug/logs", "/api/bug/raw", "/api/bug/input"] {
            let res = get(format!("{}?id={}", path, bug_id), Some(stranger.clone()))
                .await
                .unwrap();
            assert_eq!(res.status(), 404, "{} leaked to a stranger by id", path);
            let res = get(format!("{}?id={}", path, bug_id), None).await.unwrap();
            assert_eq!(res.status(), 401, "{} served without a session", path);
            let res = get(
                format!("{}?bugid=linux-authz01", path),
                Some(stranger.clone()),
            )
            .await
            .unwrap();
            assert_eq!(
                res.status(),
                404,
                "{} leaked existence to stranger with bugid",
                path
            );
            let res = get(format!("{}?bugid=linux-authz01", path), None)
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                401,
                "{} served by bugid without a session",
                path
            );
        }

        let listing: serde_json::Value = get("/api/bugs".to_string(), Some(stranger.clone()))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listing["total"], 0);
        assert!(listing["items"].as_array().unwrap().is_empty());

        let facets: serde_json::Value =
            get("/api/bugs/subsystems".to_string(), Some(stranger.clone()))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        assert!(facets.as_array().unwrap().is_empty());

        // The operator sees everything and keeps full management rights by id
        // and by bugid.
        let operator = token("operator@example.org");
        for query in [format!("id={}", bug_id), "bugid=linux-authz01".to_string()] {
            let allowed = get(format!("/api/bug?{}", query), Some(operator.clone()))
                .await
                .unwrap();
            assert_eq!(allowed.status(), 200);
            let body: serde_json::Value = allowed.json().await.unwrap();
            assert_eq!(body["bugid"], "linux-authz01");
            assert_eq!(body["can_comment"], true);
            assert_eq!(body["can_manage"], true);
        }

        let listing: serde_json::Value = get("/api/bugs".to_string(), Some(operator))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(listing["total"], 1);

        // Mutations are not bypassable from loopback or by knowing bugid.
        let unauthenticated_action = client
            .post(format!("http://{}/api/bug/action?id={}", addr, bug_id))
            .json(&serde_json::json!({"action": "comment", "content": "hello"}))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthenticated_action.status(), 401);

        let unauthenticated_bugid_action = client
            .post(format!(
                "http://{}/api/bug/action?bugid=linux-authz01",
                addr
            ))
            .json(&serde_json::json!({"action": "comment", "content": "hello"}))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthenticated_bugid_action.status(), 401);

        let stranger_bugid_action = client
            .post(format!(
                "http://{}/api/bug/action?bugid=linux-authz01",
                addr
            ))
            .header("Authorization", format!("Bearer {}", stranger))
            .json(&serde_json::json!({"action": "comment", "content": "hello"}))
            .send()
            .await
            .unwrap();
        assert_eq!(stranger_bugid_action.status(), 404);

        // Filing a bug spends money, so it needs the create capability. The
        // principal is resolved before the body is, so an anonymous caller is
        // turned away without the payload being looked at.
        let analyze = |bearer: Option<String>| {
            let mut req = client
                .post(format!("http://{}/api/bug/analyze", addr))
                .json(&serde_json::json!({
                    "problem": "a use after free",
                    "reasoning": "the pointer outlives the allocation",
                    "source_files": [],
                }));
            if let Some(bearer) = bearer {
                req = req.header("Authorization", format!("Bearer {}", bearer));
            }
            req.send()
        };

        let unauthenticated_analyze = analyze(None).await.unwrap();
        assert_eq!(unauthenticated_analyze.status(), 401);

        // A session alone is not enough, and the refusal happens before any
        // provider is built.
        let denied_analyze = analyze(Some(stranger)).await.unwrap();
        assert_eq!(denied_analyze.status(), 403);
        assert_eq!(
            denied_analyze.text().await.unwrap(),
            "You don't have permissions to file bugs."
        );

        // When linux_bug.enabled is false (the default), /api/config reports
        // bugs_enabled: false and /api/bug/analyze refuses even an admin.
        let cfg: serde_json::Value = get("/api/config".into(), None)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(cfg["bugs_enabled"], false);

        let disabled_analyze = analyze(Some(token("operator@example.org"))).await.unwrap();
        assert_eq!(disabled_analyze.status(), 404);
        assert_eq!(
            disabled_analyze.text().await.unwrap(),
            "Linux bug tracking is disabled on this server."
        );
    }

    #[test]
    fn test_is_sign_in_eligible_checks_maintainers_and_blocklist() {
        let maintainers_text = r#"
Maintainers List
===================

BTRFS FILE SYSTEM
M:	Chris Mason <clm@fb.com>
S:	Maintained
F:	fs/btrfs/

NETWORKING [GENERAL]
M:	David S. Miller <davem@davemloft.net>
S:	Maintained
F:	net/
"#;
        let index =
            crate::maintainers::MaintainersIndex::from_reader(maintainers_text.as_bytes()).unwrap();
        let acl = crate::settings::AclSettings {
            admins: vec!["operator@example.org".to_string()],
            blocklist: vec!["clm@fb.com".to_string()],
            ..Default::default()
        };

        // Subsystem maintainer in MAINTAINERS (not in Settings.toml ACL) can sign in.
        assert!(is_sign_in_eligible(
            "davem@davemloft.net",
            &acl,
            Some(&index)
        ));
        assert!(is_sign_in_eligible(
            "  DAVEM@davemloft.net ",
            &acl,
            Some(&index)
        ));

        // Blocklist revokes sign-in even for a maintainer listed in MAINTAINERS.
        assert!(!is_sign_in_eligible("clm@fb.com", &acl, Some(&index)));

        // Configured operator can sign in even without a MAINTAINERS index.
        assert!(is_sign_in_eligible("operator@example.org", &acl, None));

        // Unknown address cannot sign in.
        assert!(!is_sign_in_eligible(
            "stranger@example.org",
            &acl,
            Some(&index)
        ));
    }

    #[tokio::test]
    async fn test_health_check_email_worker_heartbeat() {
        let mut settings = crate::settings::Settings::new().unwrap();
        settings.database.url = ":memory:".to_string();
        let db = Arc::new(Database::new(&settings.database).await.unwrap());
        db.migrate().await.unwrap();
        let settings = Arc::new(settings);

        let now = chrono::Utc::now().timestamp();
        let hb = Arc::new(std::sync::atomic::AtomicI64::new(now));

        let (event_tx, _event_rx) = mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = mpsc::channel(10);
        let app = build_router(
            settings,
            db,
            event_tx,
            fetch_tx,
            ServerOptions {
                smtp_enabled: true,
                email_worker_heartbeat: Some(hb.clone()),
                ..Default::default()
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let health_url = format!("http://{}/health", addr);

        // Fresh heartbeat -> 200 OK
        let resp = client.get(&health_url).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Stale heartbeat (> 300s old) -> 503 SERVICE_UNAVAILABLE
        hb.store(
            now - EMAIL_WORKER_STALE_SECONDS - 10,
            std::sync::atomic::Ordering::Relaxed,
        );
        let resp = client.get(&health_url).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_release_patchset_embargo_authorization_and_endpoint() {
        let mut settings = crate::settings::Settings::new().unwrap();
        settings.database.url = ":memory:".to_string();
        settings.server.testing_mode = false;
        settings.server.read_only = false;
        settings.server.acl.admins = vec!["operator@example.org".to_string()];
        settings.server.acl.review = vec!["reviewer@example.org".to_string()];
        settings.server.acl.blocklist = vec!["blocked@example.org".to_string()];
        let db = Arc::new(Database::new(&settings.database).await.unwrap());
        db.migrate().await.unwrap();
        let settings = Arc::new(settings);

        let now = chrono::Utc::now().timestamp();
        let thread_id = db
            .create_thread("<embargo-srv@example.com>", "[PATCH] btrfs: fix", now)
            .await
            .unwrap();
        let ps_id = db
            .create_patchset(
                thread_id,
                None,
                "<embargo-srv@example.com>",
                "[PATCH] btrfs: fix",
                "Author <author@example.com>",
                now,
                1,
                1,
                "",
                "",
                Some(1),
                1,
                None,
                false,
                None,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        db.create_message(
            "<embargo-srv@example.com>",
            thread_id,
            None,
            "Author <author@example.com>",
            "[PATCH] btrfs: fix",
            now,
            "",
            "",
            "",
            None,
            None,
        )
        .await
        .unwrap();
        let patch_id = db
            .create_patch(ps_id, "<embargo-srv@example.com>", 1, "diff")
            .await
            .unwrap();
        let review_id = db
            .create_review(
                ps_id,
                Some(patch_id),
                "gemini",
                "gemini-2.5",
                None,
                None,
                None,
            )
            .await
            .unwrap();
        db.complete_review(
            review_id,
            "Reviewed",
            "Found high severity issue",
            Some("Use-after-free in btrfs"),
            None,
            Some("inline review text"),
            Some("{\"step\":\"done\"}"),
        )
        .await
        .unwrap();
        db.create_finding(crate::db::Finding {
            review_id,
            severity: crate::db::Severity::High,
            severity_explanation: Some("Exploitable UAF".to_string()),
            problem: "Use-after-free in btrfs".to_string(),
            preexisting: Some(false),
            locations: None,
            prior: None,
        })
        .await
        .unwrap();
        db.update_patchset_status(ps_id, "Reviewed").await.unwrap();
        db.set_patchset_embargo_until(ps_id, now + 3600)
            .await
            .unwrap();
        db.add_patchset_maintainer_sections(
            ps_id,
            &[
                crate::db::AttributedSubsystem::from_maintainers("BTRFS FILE SYSTEM"),
                crate::db::AttributedSubsystem::from_path_prefix("fs/btrfs"),
            ],
        )
        .await
        .unwrap();

        let maintainers_text = r#"
Maintainers List
===================

BTRFS FILE SYSTEM
M:	Chris Mason <clm@fb.com>
R:	Josef Bacik <josef@toxicpanda.com>
M:	Blocked Maintainer <blocked@example.org>
S:	Maintained
F:	fs/btrfs/

NETWORKING [GENERAL]
M:	David S. Miller <davem@davemloft.net>
S:	Maintained
F:	net/

THE REST
M:	Linus Torvalds <torvalds@linux-foundation.org>
S:	Buried alive in reporters
F:	*
"#;
        let index =
            crate::maintainers::MaintainersIndex::from_reader(maintainers_text.as_bytes()).unwrap();
        let acl = &settings.server.acl;

        let (event_tx, _event_rx) = mpsc::channel(10);
        let (fetch_tx, _fetch_rx) = mpsc::channel(10);
        let state = Arc::new(AppState {
            db: db.clone(),
            sender: event_tx,
            fetch_sender: fetch_tx,
            settings: settings.clone(),
            read_only: false,
            allow_all_submit: false,
            smtp_enabled: false,
            dry_run: true,
            local_token: None,
            email_worker_heartbeat: None,
            activity: Default::default(),
            forge_registry: Arc::new(crate::forge::ForgeRegistry::new()),
            patchsets_homepage_cache: AsyncCache::new(Duration::from_secs(2)),
            messages_homepage_cache: AsyncCache::new(Duration::from_secs(2)),
            patchsets_count_cache: AsyncCache::new(Duration::from_secs(10)),
            messages_count_cache: AsyncCache::new(Duration::from_secs(10)),
            stats_timeline_cache: AsyncMapCache::new(Duration::from_secs(60)),
            stats_reviews_cache: AsyncCache::new(Duration::from_secs(60)),
            stats_tools_cache: AsyncCache::new(Duration::from_secs(60)),
            bug_subsystems_cache: AsyncMapCache::new(Duration::from_secs(60)),
            sign_in_link_rate_limiter: SignInLinkRateLimiter::new(),
            pull_request_resolver: Arc::new(crate::forge::NoResolver),
        });

        let headers = axum::http::HeaderMap::new();
        let auth_for = |email: &str| {
            crate::auth::OptionalAuthUser(Some(crate::auth::AuthUser {
                email: email.to_string(),
                typ: Some("session".to_string()),
                iat: Some(now as u64),
                sid: None,
                max_bug_access: None,
            }))
        };
        let query = || PatchQuery {
            id: ps_id.to_string(),
            page: None,
            per_page: None,
            limit: None,
        };
        let list_pagination = || Pagination {
            page: Some(1),
            per_page: Some(50),
            q: None,
            mailing_list: None,
        };

        // 1. Anonymous caller sees Embargoed with empty reviews, zeroed list counts,
        // 404 on /api/review, and 403 on release_patchset_embargo.
        let Json(anon_details) = get_patchset(
            crate::auth::OptionalAuthUser(None),
            OptionalPrincipal(Principal::anonymous()),
            headers.clone(),
            State(state.clone()),
            Query(query()),
        )
        .await
        .unwrap();
        assert_eq!(anon_details["status"], "Embargoed");
        assert_eq!(anon_details["can_view_embargoed"], false);
        assert_eq!(anon_details["can_release_embargo"], false);
        assert_eq!(anon_details["reviews"].as_array().unwrap().len(), 0);

        let Json(anon_list) = list_patchsets(
            crate::auth::OptionalAuthUser(None),
            OptionalPrincipal(Principal::anonymous()),
            headers.clone(),
            State(state.clone()),
            Query(list_pagination()),
        )
        .await
        .unwrap();
        assert_eq!(anon_list.items[0].status.as_deref(), Some("Embargoed"));
        assert_eq!(anon_list.items[0].findings_high, Some(0));

        let rev_err = get_review(
            crate::auth::OptionalAuthUser(None),
            OptionalPrincipal(Principal::anonymous()),
            headers.clone(),
            State(state.clone()),
            Query(ReviewQuery {
                id: Some(review_id),
                patchset_id: None,
                after_seq: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(rev_err, StatusCode::NOT_FOUND);

        let err = release_patchset_embargo(
            crate::auth::OptionalAuthUser(None),
            OptionalPrincipal(Principal::anonymous()),
            headers.clone(),
            State(state.clone()),
            Query(query()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);

        // 2. Unrelated subsystem maintainer (davem) and blocklisted maintainer are denied
        // both viewing embargoed results and releasing the embargo.
        for email in [
            "davem@davemloft.net",
            "stranger@example.org",
            "blocked@example.org",
        ] {
            let p = Principal::resolve(email, acl, Some(&index));
            let Json(d) = get_patchset(
                auth_for(email),
                OptionalPrincipal(p.clone()),
                headers.clone(),
                State(state.clone()),
                Query(query()),
            )
            .await
            .unwrap();
            assert_eq!(
                d["can_view_embargoed"], false,
                "{email} should not have can_view_embargoed"
            );
            assert_eq!(
                d["can_release_embargo"], false,
                "{email} should not have can_release_embargo"
            );
            assert_eq!(
                d["reviews"].as_array().unwrap().len(),
                0,
                "{email} should not see embargoed reviews"
            );
            let Json(list) = list_patchsets(
                auth_for(email),
                OptionalPrincipal(p.clone()),
                headers.clone(),
                State(state.clone()),
                Query(list_pagination()),
            )
            .await
            .unwrap();
            assert_eq!(
                list.items[0].findings_high,
                Some(0),
                "{email} should not see embargoed finding counts"
            );
            let err = release_patchset_embargo(
                auth_for(email),
                OptionalPrincipal(p),
                headers.clone(),
                State(state.clone()),
                Query(query()),
            )
            .await
            .unwrap_err();
            assert_eq!(err.0, StatusCode::FORBIDDEN);
        }

        // 3. Scoped API token with Read ceiling for the subsystem maintainer CAN view
        // embargoed results, but is denied releasing the embargo.
        let clm_read = Principal::resolve("clm@fb.com", acl, Some(&index))
            .with_max_access(Some(crate::access::BugAccess::Read));
        let clm_api_auth = || {
            crate::auth::OptionalAuthUser(Some(crate::auth::AuthUser {
                email: "clm@fb.com".to_string(),
                typ: Some("api_token".to_string()),
                iat: Some(now as u64),
                sid: None,
                max_bug_access: Some("read".to_string()),
            }))
        };
        let Json(clm_read_details) = get_patchset(
            clm_api_auth(),
            OptionalPrincipal(clm_read.clone()),
            headers.clone(),
            State(state.clone()),
            Query(query()),
        )
        .await
        .unwrap();
        assert_eq!(clm_read_details["status"], "Embargoed");
        assert_eq!(clm_read_details["can_view_embargoed"], true);
        assert_eq!(clm_read_details["can_release_embargo"], false);
        assert_eq!(clm_read_details["reviews"].as_array().unwrap().len(), 1);
        assert_eq!(
            clm_read_details["reviews"][0]["inline_review"],
            "inline review text"
        );
        assert_eq!(
            clm_read_details["reviews"][0]["logs"],
            "{\"step\":\"done\"}"
        );
        assert_eq!(clm_read_details["reviews"][0]["patch_id"], patch_id);

        let err = release_patchset_embargo(
            clm_api_auth(),
            OptionalPrincipal(clm_read),
            headers.clone(),
            State(state.clone()),
            Query(query()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);

        // 4. Corresponding maintainer (M:), reviewer (R:), global maintainer, operator,
        // and ACL Permission::Review holder all see embargoed results immediately AND
        // have can_release_embargo = true while status remains "Embargoed".
        for email in [
            "clm@fb.com",
            "josef@toxicpanda.com",
            "torvalds@linux-foundation.org",
            "operator@example.org",
            "reviewer@example.org",
        ] {
            let p = Principal::resolve(email, acl, Some(&index));
            let Json(d) = get_patchset_summary(
                auth_for(email),
                OptionalPrincipal(p.clone()),
                headers.clone(),
                State(state.clone()),
                Query(query()),
            )
            .await
            .unwrap();
            assert_eq!(d["status"], "Embargoed");
            assert_eq!(
                d["can_view_embargoed"], true,
                "{email} should have can_view_embargoed"
            );
            assert_eq!(
                d["can_release_embargo"], true,
                "{email} should have can_release_embargo"
            );
            assert_eq!(
                d["reviews"].as_array().unwrap().len(),
                1,
                "{email} should see embargoed reviews on summary"
            );
            assert!(
                d["reviews"][0].get("logs").is_none(),
                "summary endpoint must omit review logs"
            );

            let Json(list) = list_patchsets(
                auth_for(email),
                OptionalPrincipal(p.clone()),
                headers.clone(),
                State(state.clone()),
                Query(list_pagination()),
            )
            .await
            .unwrap();
            assert_eq!(list.items[0].status.as_deref(), Some("Embargoed"));
            assert_eq!(
                list.items[0].findings_high,
                Some(1),
                "{email} should see embargoed finding counts in list_patchsets"
            );

            let Json(rev) = get_review(
                auth_for(email),
                OptionalPrincipal(p),
                headers.clone(),
                State(state.clone()),
                Query(ReviewQuery {
                    id: Some(review_id),
                    patchset_id: None,
                    after_seq: None,
                }),
            )
            .await
            .unwrap();
            assert_eq!(rev["id"], review_id);
        }

        // Verify the shared homepage cache was not polluted by maintainer list_patchsets calls.
        let Json(anon_list_after) = list_patchsets(
            crate::auth::OptionalAuthUser(None),
            OptionalPrincipal(Principal::anonymous()),
            headers.clone(),
            State(state.clone()),
            Query(list_pagination()),
        )
        .await
        .unwrap();
        assert_eq!(anon_list_after.items[0].findings_high, Some(0));

        // 5. Corresponding maintainer releases the embargo; patchset immediately becomes Reviewed.
        let clm = Principal::resolve("clm@fb.com", acl, Some(&index));
        let Json(res) = release_patchset_embargo(
            auth_for("clm@fb.com"),
            OptionalPrincipal(clm.clone()),
            headers.clone(),
            State(state.clone()),
            Query(query()),
        )
        .await
        .unwrap();
        assert_eq!(res["status"], "released");
        assert_eq!(res["released"], true);

        // Re-releasing an already released patchset is idempotent.
        let Json(res_again) = release_patchset_embargo(
            auth_for("clm@fb.com"),
            OptionalPrincipal(clm.clone()),
            headers.clone(),
            State(state.clone()),
            Query(query()),
        )
        .await
        .unwrap();
        assert_eq!(res_again["status"], "released");
        assert_eq!(res_again["released"], false);

        let Json(after_details) = get_patchset(
            auth_for("clm@fb.com"),
            OptionalPrincipal(clm),
            headers,
            State(state),
            Query(query()),
        )
        .await
        .unwrap();
        assert_eq!(after_details["status"], "Reviewed");
        assert_eq!(after_details["can_release_embargo"], false);
        assert_eq!(after_details["reviews"].as_array().unwrap().len(), 1);
    }
}

/// Whether the caller may exercise a capability.
///
/// The request's source address is deliberately not an input. A reverse proxy
/// terminates in front of the service and forwards from loopback, so the
/// address distinguishes nothing: a caller is trusted for what it can present,
/// not for where it appears to come from.
pub fn is_authorized(
    state: &std::sync::Arc<AppState>,
    headers: &axum::http::HeaderMap,
    auth: Option<&crate::auth::AuthUser>,
    perm: crate::settings::Permission,
) -> bool {
    // The blocklist is a revocation, so it outranks every bypass below it. It
    // can only match an address the caller actually presented; a blocklisted
    // person who omits their credential and presents the local token instead
    // has proved only that they share the server's machine, which grants no
    // authority over a bug.
    if let Some(user) = auth {
        if state.settings.server.acl.is_blocklisted(&user.email) {
            return false;
        }
        if user
            .sid
            .as_deref()
            .is_some_and(|sid| state.settings.server.acl.is_blocklisted(sid))
        {
            return false;
        }
        if user.typ.as_deref() == Some("api_token") {
            return false;
        }
    }
    if state.settings.server.testing_mode {
        return true;
    }
    if state.allow_all_submit {
        return true;
    }

    // A caller holding this process's token has read a file only the server's
    // own user can read. Nothing else about where the request came from is
    // consulted: the source address cannot distinguish a local caller from an
    // internet one a reverse proxy forwarded, so it is not evidence of
    // anything. Bug routes resolve their own principal and never call here.
    if presents_local_token(headers, state) && perm.granted_by_local_token() {
        return true;
    }

    if let Some(user) = auth {
        return state.settings.server.acl.has_permission(&user.email, perm);
    }

    false
}

/// Whether the request carries the local operator token this process wrote.
///
/// The token shares the Authorization header with session JWTs, which is safe
/// because the two cannot be confused: a JWT never has the shape of a token,
/// and a token never carries the signature a JWT is accepted on.
pub(crate) fn presents_local_token(
    headers: &axum::http::HeaderMap,
    state: &std::sync::Arc<AppState>,
) -> bool {
    let Some(token) = state.local_token.as_ref() else {
        return false;
    };

    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| token.matches(presented.trim()))
}

fn resolve_jwt_secret(state: &AppState) -> Option<String> {
    state
        .settings
        .server
        .jwt_secret
        .clone()
        .or_else(|| std::env::var("JWT_SECRET").ok())
}

fn extract_client_ip(
    addr: &std::net::SocketAddr,
    headers: &axum::http::HeaderMap,
) -> Option<std::net::IpAddr> {
    for name in ["x-forwarded-for", "x-real-ip", "forwarded"] {
        if let Some(val) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let first = val.split(',').next().unwrap_or("").trim();
            if let Ok(ip) = first.parse::<std::net::IpAddr>() {
                return Some(ip);
            }
        }
    }
    Some(addr.ip())
}

fn build_sign_in_link_email(
    email: &str,
    link: &str,
    base_url: &str,
    lifetime_seconds: i64,
    client_ip: Option<std::net::IpAddr>,
) -> String {
    let minutes = lifetime_seconds / 60;
    let ip_line = match client_ip {
        Some(ip) => format!("\n\nRequested from {}.", ip),
        None => String::new(),
    };
    format!(
        "Someone asked to sign in to Sashiko as {email}.\n\n\
         Open this link within {minutes} minutes to continue:\n\n\
         {link}\
         {ip_line}\n\n\
         If you did not ask to sign in, ignore this message. Nothing has\n\
         changed and nobody has gained access.\n\n\
         -- \n\
         Sashiko AI review · {base_url}"
    )
}

fn is_sign_in_eligible(
    email: &str,
    acl: &crate::settings::AclSettings,
    maintainers: Option<&crate::maintainers::MaintainersIndex>,
) -> bool {
    !email.is_empty()
        && !acl.is_blocklisted(email)
        && (acl.is_known_identity(email)
            || maintainers.is_some_and(|m| m.subsystems_for_address(email).is_some()))
}

use crate::db::SIGN_IN_LINK_LIFETIME_SECONDS;

async fn request_link(
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    body: axum::body::Bytes,
) -> Result<StatusCode, StatusCode> {
    if let Some(secret) = resolve_jwt_secret(&state) {
        let payload: Option<RequestLinkRequest> = serde_json::from_slice(&body).ok();
        let email = payload.as_ref().map(|p| p.email.trim()).unwrap_or("");
        let lifetime = SIGN_IN_LINK_LIFETIME_SECONDS;

        let client_ip = extract_client_ip(&addr, &headers);
        let client_ip_str = client_ip
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| addr.ip().to_string());
        let rate_ok = state
            .sign_in_link_rate_limiter
            .check_and_record(email, &client_ip_str);

        let acl = &state.settings.server.acl;
        let maintainers = crate::maintainers::get_global_maintainers();
        let is_eligible = is_sign_in_eligible(email, acl, maintainers.as_deref());

        if is_eligible && rate_ok {
            let token = crate::auth::create_token(
                email,
                &secret,
                Some("sign_in_link".to_string()),
                lifetime.max(0) as u64,
            )
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let base_url = state.settings.server.sign_in_base_url();
            let link = format!("{}/auth/verify?token={}", base_url, token);

            if state.settings.smtp.is_some() {
                let body = build_sign_in_link_email(email, &link, &base_url, lifetime, client_ip);
                let status = match &state.settings.smtp {
                    Some(s) if s.dry_run => "Dry-Run",
                    _ => "Pending",
                };
                state
                    .db
                    .insert_transactional_email(
                        crate::db::EmailKind::SignInLink,
                        status,
                        email,
                        &[],
                        "[sashiko] Your sign-in link",
                        &body,
                    )
                    .await
                    .map_err(|e| {
                        tracing::error!("Failed to queue sign-in email: {}", e);
                        StatusCode::INTERNAL_SERVER_ERROR
                    })?;
                tracing::info!("Queued sign-in link email for {}", email);
            } else if state.settings.server.log_sign_in_links {
                tracing::info!("SIGN-IN LINK REQUESTED for {}: {}", email, link);
            } else {
                // The link is a bearer credential, so it is not written to the
                // log by default. Without a transport there is nothing else to
                // do with it, which is worth saying plainly rather than
                // leaving the request looking like it succeeded.
                tracing::warn!(
                    "Sign-in link requested for {} but no SMTP transport is configured, \
                     so it could not be delivered. Set server.log_sign_in_links to print \
                     it to the log instead.",
                    email
                );
            }
        } else {
            if !rate_ok {
                tracing::warn!(
                    "Sign-in link request rate limit exceeded for address '{}' from IP '{}'",
                    email,
                    client_ip_str
                );
            }
            // Timing equalization: simulate token creation and DB roundtrip
            let _ = crate::auth::create_token(
                "timing-equalizer@sashiko.internal",
                &secret,
                Some("sign_in_link".to_string()),
                lifetime.max(0) as u64,
            );
            if state.settings.smtp.is_some() {
                let _ = state.db.conn.query("SELECT 1", ()).await;
            }
            tracing::debug!(
                "Ignored sign-in link request for ineligible or rate-limited address: {}",
                email
            );
        }

        Ok(StatusCode::OK)
    } else {
        Err(StatusCode::NOT_IMPLEMENTED)
    }
}

const SESSION_TOKEN_LIFETIME_SECONDS: u64 = 30 * 24 * 3600;
const MAX_SESSION_LIFETIME_SECONDS: u64 = 365 * 24 * 3600;

async fn verify_link(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<VerifyLinkQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, &'static str)> {
    if let Some(secret) = resolve_jwt_secret(&state) {
        let claims = crate::auth::verify_token(&query.token, &secret)
            .map_err(|_| (StatusCode::UNAUTHORIZED, "Invalid sign-in link"))?;
        if claims.typ.as_deref() != Some("sign_in_link") {
            return Err((StatusCode::UNAUTHORIZED, "Invalid token type"));
        }
        if state.settings.server.acl.is_blocklisted(&claims.sub) {
            return Err((StatusCode::FORBIDDEN, "User is blocklisted"));
        }
        let session_token = crate::auth::create_token(
            &claims.sub,
            &secret,
            Some("session".to_string()),
            SESSION_TOKEN_LIFETIME_SECONDS,
        )
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create session",
            )
        })?;
        Ok(Json(serde_json::json!({ "token": session_token })))
    } else {
        Err((StatusCode::NOT_IMPLEMENTED, "JWT not configured"))
    }
}

async fn refresh_token(
    auth: crate::auth::OptionalAuthUser,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, (StatusCode, &'static str)> {
    if let Some(secret) = resolve_jwt_secret(&state) {
        if let Some(user) = auth.0 {
            if user.typ.as_deref() != Some("session") {
                return Err((
                    StatusCode::FORBIDDEN,
                    "Only interactive session tokens can be refreshed",
                ));
            }
            if state.settings.server.acl.is_blocklisted(&user.email)
                || user
                    .sid
                    .as_deref()
                    .is_some_and(|sid| state.settings.server.acl.is_blocklisted(sid))
            {
                return Err((StatusCode::FORBIDDEN, "User is blocklisted"));
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("Time went backwards")
                .as_secs();

            if let Some(iat) = user.iat
                && now.saturating_sub(iat) > MAX_SESSION_LIFETIME_SECONDS
            {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    "Session has reached maximum lifetime (1 year). Please log in again.",
                ));
            }

            let session_token = crate::auth::create_token_with_session(
                &user.email,
                &secret,
                Some("session".to_string()),
                SESSION_TOKEN_LIFETIME_SECONDS,
                user.iat,
                user.sid,
            )
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Failed to create session",
                )
            })?;
            Ok(Json(serde_json::json!({ "token": session_token })))
        } else {
            Err((StatusCode::UNAUTHORIZED, "Missing or invalid token"))
        }
    } else {
        Err((StatusCode::NOT_IMPLEMENTED, "JWT not configured"))
    }
}

async fn create_auth_token(
    headers: axum::http::HeaderMap,
    auth: crate::auth::OptionalAuthUser,
    OptionalPrincipal(principal): crate::access::OptionalPrincipal,
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    Json(req): Json<CreateApiTokenRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, &'static str)> {
    let is_local_operator = presents_local_token(&headers, &state);
    let (caller_email, is_operator) = match auth.0.as_ref() {
        Some(user) => {
            if user.typ.as_deref() != Some("session") {
                return Err((
                    StatusCode::FORBIDDEN,
                    "Only interactive sessions may mint API tokens",
                ));
            }
            if state.settings.server.acl.is_blocklisted(&user.email)
                || user
                    .sid
                    .as_deref()
                    .is_some_and(|sid| state.settings.server.acl.is_blocklisted(sid))
            {
                return Err((StatusCode::FORBIDDEN, "Caller is blocklisted"));
            }
            (
                user.email.as_str(),
                principal.is_operator() || is_local_operator,
            )
        }
        None if is_local_operator => ("", true),
        None => return Err((StatusCode::UNAUTHORIZED, "Authentication required")),
    };
    let secret =
        resolve_jwt_secret(&state).ok_or((StatusCode::NOT_IMPLEMENTED, "JWT not configured"))?;

    let target_email = match req.email.as_deref().map(str::trim) {
        Some(email) if !email.is_empty() && !email.eq_ignore_ascii_case(caller_email) => {
            if !is_operator {
                return Err((
                    StatusCode::FORBIDDEN,
                    "Only operators may mint tokens for another address",
                ));
            }
            email
        }
        _ => caller_email,
    };

    if target_email.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Target email address is required"));
    }
    if state.settings.server.acl.is_blocklisted(target_email) {
        return Err((StatusCode::FORBIDDEN, "Target user is blocklisted"));
    }

    let raw_access = req.max_bug_access.as_deref().unwrap_or("read");
    let level = crate::access::BugAccess::parse(raw_access).ok_or((
        StatusCode::BAD_REQUEST,
        "Invalid max_bug_access; expected none, read, comment, or manage",
    ))?;
    let canonical_access = match level {
        crate::access::BugAccess::Read => "read",
        crate::access::BugAccess::Comment => "comment",
        crate::access::BugAccess::Manage => "manage",
        crate::access::BugAccess::None => "none",
    };

    let expires_in_days = req.expires_in_days.unwrap_or(30);
    if !(1..=90).contains(&expires_in_days) {
        return Err((
            StatusCode::BAD_REQUEST,
            "expires_in_days must be between 1 and 90",
        ));
    }

    let token = crate::auth::create_api_token(
        target_email,
        &secret,
        expires_in_days * 86400,
        Some(canonical_access.to_string()),
    )
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to create API token",
        )
    })?;
    let token_id = crate::auth::verify_token(&token, &secret)
        .ok()
        .and_then(|c| c.sid);

    Ok(Json(serde_json::json!({
        "token": token,
        "token_id": token_id,
        "email": target_email,
        "max_bug_access": canonical_access,
        "expires_in_days": expires_in_days,
    })))
}
