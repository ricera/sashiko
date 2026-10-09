//! Integration tests that spin up an in-memory HTTP server on an ephemeral port
//! and exercise the REST API.

use std::net::SocketAddr;
use std::sync::Arc;

use sashiko::db::Database;
use sashiko::events::Event;
use sashiko::fetcher::FetchRequest;
use sashiko::server::{ServerOptions, build_router};
use sashiko::settings::{DatabaseSettings, Settings};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// Build a minimal [`Settings`] for integration tests.
///
/// The `read_only` flag on `server` is set from the parameter; all other
/// fields use harmless defaults that don't require external resources.
fn test_settings(read_only: bool) -> Settings {
    let toml = format!(
        r#"
[database]
url = ":memory:"
token = ""

[nntp]
server = "localhost"
port = 119

[mailing_lists]
track = []

[ai]
provider = "gemini"
model = "test"

[server]
host = "127.0.0.1"
port = 0
read_only = {read_only}

[git]
repository_path = "."

[review]
concurrency = 1
worktree_dir = "/tmp/sashiko-test-trees"
timeout_seconds = 60
"#
    );
    let cfg = config::Config::builder()
        .add_source(config::File::from_str(&toml, config::FileFormat::Toml))
        .build()
        .expect("test settings parse");
    cfg.try_deserialize::<Settings>()
        .expect("test settings deserialize")
}

/// A running test server instance with its base URL and background handles.
struct TestServer {
    /// Base URL including the OS-assigned port, e.g. `http://127.0.0.1:12345`.
    base_url: String,
    /// Shared database handle — tests can insert fixture data directly.
    db: Arc<Database>,
    /// Event receiver — tests can drain submitted events from the channel.
    event_rx: mpsc::Receiver<Event>,
    /// Shared activity registry — tests can publish live phases directly.
    activity: Arc<sashiko::activity::ActivityRegistry>,
}

/// Spawn a real axum server on a random port with an in-memory database.
///
/// The server runs in a background tokio task and is dropped when the
/// [`TestServer`] goes out of scope (the task is detached, so cleanup is
/// automatic when the tokio runtime shuts down).
async fn spawn_test_server(read_only: bool) -> TestServer {
    let db_settings = DatabaseSettings {
        url: ":memory:".to_string(),
        token: String::new(),
    };
    let db = Arc::new(Database::new(&db_settings).await.unwrap());
    db.migrate().await.unwrap();

    let (event_tx, event_rx) = mpsc::channel::<Event>(100);
    let (fetch_tx, _fetch_rx) = mpsc::channel::<FetchRequest>(100);

    let settings = Arc::new(test_settings(read_only));
    let activity = sashiko::activity::ActivityRegistry::new();
    let app = build_router(
        settings,
        Arc::clone(&db),
        event_tx,
        fetch_tx,
        ServerOptions {
            allow_all_submit: true,
            dry_run: true,
            activity: Arc::clone(&activity),
            ..Default::default()
        },
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");

    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    TestServer {
        base_url,
        db,
        event_rx,
        activity,
    }
}

// ── Smoke Tests ─────────────────────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn test_activity_endpoint_reports_live_work() {
    use sashiko::activity::{ActivityKey, Phase, StageWait};

    let server = spawn_test_server(false).await;

    // Nothing running yet.
    let resp = reqwest::get(format!("{}/api/activity", server.base_url))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["count"], 0);

    // Publish what a mid-review patchset with two concurrent stages and an
    // in-flight fetch look like.
    server.activity.update(
        ActivityKey::Patchset(42),
        Phase::Reviewing {
            attempt: 1,
            max_attempts: 1,
        },
    );
    for (stage, turn) in [("locking", 9), ("security", 2)] {
        server.activity.update(
            ActivityKey::PatchsetStage {
                patchset_id: 42,
                patch_id: 1,
                stage: stage.to_string(),
            },
            Phase::Stage {
                stage: stage.to_string(),
                turn,
                max_turns: 100,
                waiting: StageWait::Model,
            },
        );
    }
    server.activity.update(
        ActivityKey::Commit("deadbeef".to_string()),
        Phase::Fetching {
            remote: "https://example.com/linux.git".to_string(),
            commits: 2,
        },
    );

    let body: serde_json::Value = reqwest::get(format!("{}/api/activity", server.base_url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["count"], 4);
    let entries = body["entries"].as_array().unwrap();

    let locking_stage = entries
        .iter()
        .find(|e| e["key"] == "patchset:42/patch:1/stage:locking")
        .expect("per-stage activity should be reported");
    assert_eq!(locking_stage["phase"]["kind"], "stage");
    assert_eq!(locking_stage["phase"]["turn"], 9);
    assert_eq!(locking_stage["stage"], "locking");
    assert_eq!(
        locking_stage["description"],
        "stage locking, turn 9/100 (awaiting model)"
    );

    // Both clocks must reach the client — idle is what distinguishes a slow
    // stage from a wedged one.
    assert!(locking_stage["age_seconds"].is_number());
    assert!(locking_stage["idle_seconds"].is_number());

    // Concurrent stages are tracked separately rather than overwriting each other.
    let security_stage = entries
        .iter()
        .find(|e| e["key"] == "patchset:42/patch:1/stage:security")
        .expect("a second concurrent stage should have its own entry");
    assert_eq!(security_stage["phase"]["turn"], 2);

    let fetch = entries
        .iter()
        .find(|e| e["key"] == "commit:deadbeef")
        .expect("fetch activity should be reported");
    assert_eq!(fetch["phase"]["kind"], "fetching");

    // Finishing one stage leaves its siblings alone.
    server.activity.clear(&ActivityKey::PatchsetStage {
        patchset_id: 42,
        patch_id: 1,
        stage: "locking".to_string(),
    });
    let body: serde_json::Value = reqwest::get(format!("{}/api/activity", server.base_url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["count"], 3);

    // Ending the review clears the patchset entry and every stage under it.
    server.activity.clear_patchset(42);
    let body: serde_json::Value = reqwest::get(format!("{}/api/activity", server.base_url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["count"], 1, "only the unrelated fetch should remain");
}

/// A fetch is keyed by the commit it fetches, not by patchset, so the
/// patchset's activity has to be bridged to it through the placeholder it was
/// created with. Without the bridge a fetch running right now was reported as
/// the last state of a daemon that had stopped.
#[tokio::test]
#[ignore]
async fn test_patchset_activity_reports_live_fetch_as_live() {
    use sashiko::activity::{ActivityKey, Phase};

    let server = spawn_test_server(false).await;
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let id = server
        .db
        .create_fetching_patchset(
            &format!("{sha}@sashiko.local"),
            "Fetching...",
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    server.activity.update(
        ActivityKey::Commit(sha.to_string()),
        Phase::GitOp {
            op: "git rev-parse (checking commit presence)".to_string(),
        },
    );

    let body: serde_json::Value =
        reqwest::get(format!("{}/api/patchset/activity?id={id}", server.base_url))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(body["live"], true, "{body}");
    assert_eq!(body["entries"][0]["key"], format!("commit:{sha}"));
    assert_eq!(body["entries"][0]["phase"]["kind"], "git_op");
}

#[tokio::test]
async fn test_empty_db_smoke_endpoints() {
    let server = spawn_test_server(false).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{}/api/stats", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert!(body["version"].is_string());

    for endpoint in ["patchsets", "messages"] {
        let resp = client
            .get(format!("{}/api/{endpoint}", server.base_url))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["total"], 0);
        assert!(body["items"].as_array().unwrap().is_empty());
    }

    let resp = client
        .get(format!("{}/api/lists", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body.as_array().unwrap().is_empty());
}

// ── Submit / Inject Tests ───────────────────────────────────────────────

/// A minimal mbox-formatted kernel patch for testing ingestion.
const SAMPLE_MBOX: &str = "\
From dummy@example.com Thu May 14 12:00:00 2026
From: Test Author <test@example.com>
Date: Thu, 14 May 2026 12:00:00 +0000
Subject: [PATCH] mm/slub: fix object count in partial slab
Message-Id: <test-integration-1@example.com>

Fix an off-by-one in the partial slab object count that could lead
to an incorrect freelist walk under memory pressure.

---
 mm/slub.c | 2 +-
 1 file changed, 1 insertion(+), 1 deletion(-)

diff --git a/mm/slub.c b/mm/slub.c
index 1a2b3c4d5e6f..7a8b9c0d1e2f 100644
--- a/mm/slub.c
+++ b/mm/slub.c
@@ -100,7 +100,7 @@ static int count_partial_objects(struct kmem_cache_node *n)
 \tstruct slab *slab;
 \tint count = 0;
 
-\tlist_for_each_entry(slab, &n->partial, slab_list)
+\tlist_for_each_entry(slab, &n->partial, slab_list) {
 \t\tcount += slab->objects - slab->inuse;
 \t}
 
-- 
2.40.0
";

#[tokio::test]
async fn test_submit_inject_validation_and_acceptance() {
    let mut server = spawn_test_server(false).await;
    let client = reqwest::Client::new();

    // 1. Reject payloads without a valid mbox header.
    let resp = client
        .post(format!("{}/api/submit", server.base_url))
        .json(&serde_json::json!({
            "type": "inject",
            "raw": "this is not an mbox",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // 2. Reject baselines that are not 40-hex-digit commit IDs.
    for baseline in [
        "v6.7-rc2",
        "origin/master",
        "1234567",
        "--upload-pack=sh",
        "1234567890ABCDEF1234567890ABCDEF12345678",
        "1234567890abcdef1234567890abcdef12345678\nbase-commit: x",
    ] {
        let resp = client
            .post(format!("{}/api/submit", server.base_url))
            .json(&serde_json::json!({
                "type": "inject",
                "raw": SAMPLE_MBOX,
                "base_commit": baseline,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "baseline {baseline:?} was accepted");
    }

    // 3. Accept a valid inject request and enqueue Event::RawMboxSubmitted.
    let resp = client
        .post(format!("{}/api/submit", server.base_url))
        .json(&serde_json::json!({
            "type": "inject",
            "raw": SAMPLE_MBOX,
            "base_commit": "1234567890abcdef1234567890abcdef12345678",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "accepted");

    let event = server
        .event_rx
        .try_recv()
        .expect("expected an event on the channel");
    match event {
        Event::RawMboxSubmitted { raw, baseline, .. } => {
            assert!(raw.contains("[PATCH] mm/slub"));
            assert_eq!(
                baseline.as_deref(),
                Some("1234567890abcdef1234567890abcdef12345678")
            );
        }
        other => panic!("expected RawMboxSubmitted, got {other:?}"),
    }
}

#[tokio::test]
async fn test_submit_rejected_in_read_only_mode() {
    let server = spawn_test_server(/* read_only */ true).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/api/submit", server.base_url))
        .json(&serde_json::json!({
            "type": "inject",
            "raw": SAMPLE_MBOX,
        }))
        .send()
        .await
        .unwrap();

    // read_only mode should reject POST requests.
    assert_eq!(resp.status(), 403);
}

// ── Database-Backed Query Tests ─────────────────────────────────────────

#[tokio::test]
async fn test_patchset_message_and_review_query_endpoints() {
    let server = spawn_test_server(false).await;
    let client = reqwest::Client::new();

    server
        .db
        .conn
        .execute(
            "INSERT INTO patchsets (id, status, subject, author, date) \
             VALUES (1, 'Pending', '[PATCH] test patch', 'Author <a@b.com>', 1234567890)",
            (),
        )
        .await
        .unwrap();

    server
        .db
        .conn
        .execute(
            "INSERT INTO messages (message_id, subject, author, date, body) \
             VALUES ('<integ-1@example.com>', 'Test Subject', 'Author <a@b.com>', 1234567890, 'Test body')",
            (),
        )
        .await
        .unwrap();

    server
        .db
        .conn
        .execute(
            "INSERT INTO patches (id, patchset_id, message_id, part_index) \
             VALUES (1, 1, '<integ-1@example.com>', 1)",
            (),
        )
        .await
        .unwrap();

    let logs_json = "[{\"role\":\"user\",\"content\":\"test prompt\"}]";
    server
        .db
        .conn
        .execute(
            &format!(
                "INSERT INTO reviews (id, patchset_id, status, logs, created_at) \
                 VALUES (1, 1, 'Reviewed', '{logs_json}', 1234567890)"
            ),
            (),
        )
        .await
        .unwrap();

    let resp = client
        .get(format!("{}/api/patchsets", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"].as_array().unwrap().len(), 1);

    let resp = client
        .get(format!(
            "{}/api/message?id=<integ-1@example.com>",
            server.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["subject"], "Test Subject");
    assert_eq!(body["body"], "Test body");

    let resp = client
        .get(format!("{}/api/review?id=1", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["logs"].as_str().unwrap(), logs_json);
}

#[tokio::test]
async fn test_stats_reviews_and_tools_endpoints() {
    let server = spawn_test_server(false).await;
    let client = reqwest::Client::new();

    server
        .db
        .conn
        .execute(
            "INSERT INTO patchsets (id, status, subject, author, date) \
             VALUES (1, 'Pending', '[PATCH] test patch', 'Author <a@b.com>', 1234567890)",
            (),
        )
        .await
        .unwrap();

    // Insert 1005 reviews (5 Failed first, then 1000 Reviewed) and tool usages in bulk.
    let tx = server.db.begin_immediate_transaction().await.unwrap();
    tx.execute(
        "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 5)
         INSERT INTO reviews (id, patchset_id, status, created_at)
         SELECT x, 1, 'Failed', 1234567890 + x FROM cnt",
        (),
    )
    .await
    .unwrap();
    tx.execute(
        "WITH RECURSIVE cnt(x) AS (SELECT 6 UNION ALL SELECT x + 1 FROM cnt WHERE x < 1005)
         INSERT INTO ai_interactions (id, tokens_in, tokens_out, tokens_cached)
         SELECT 'int-' || x, 10, 20, 5 FROM cnt",
        (),
    )
    .await
    .unwrap();
    tx.execute(
        "WITH RECURSIVE cnt(x) AS (SELECT 6 UNION ALL SELECT x + 1 FROM cnt WHERE x < 1005)
         INSERT INTO reviews (id, patchset_id, status, interaction_id, created_at)
         SELECT x, 1, 'Reviewed', 'int-' || x, 1234567890 + x FROM cnt",
        (),
    )
    .await
    .unwrap();
    tx.execute(
        "INSERT INTO tool_usages (review_id, tool_name, output_length)
         SELECT id, CASE WHEN id <= 5 THEN 'old_tool' ELSE 'new_tool' END,
                CASE WHEN id <= 5 THEN 100 ELSE 200 END
         FROM reviews WHERE id <= 10",
        (),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let resp = client
        .get(format!("{}/api/stats/reviews", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["total_reviews"], 1005);
    assert_eq!(body["total_failures"], 5);
    assert_eq!(body["total_partial"], 0);

    let reviews = body["reviews"].as_array().unwrap();
    assert_eq!(reviews.len(), 1);
    let group = &reviews[0];
    assert_eq!(group["status"], "Reviewed");
    assert_eq!(group["count"], 1000);
    assert_eq!(group["tokens_in"], 10000);
    assert_eq!(group["tokens_out"], 20000);
    assert_eq!(group["tokens_cached"], 5000);

    let resp = client
        .get(format!("{}/api/stats/tools", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    let tools = body.as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["tool"], "new_tool");
    assert_eq!(tools[0]["count"], 5);
    assert_eq!(tools[0]["avg_output_length"], 200.0);
}

// ── Redirect Tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn test_redirect_www_to_non_www() {
    let server = spawn_test_server(false).await;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    for (path, expected_location) in [
        ("", "https://sashiko.dev/"),
        (
            "/api/stats?foo=bar",
            "https://sashiko.dev/api/stats?foo=bar",
        ),
    ] {
        let resp = client
            .get(format!("{}{path}", server.base_url))
            .header("Host", "www.sashiko.dev")
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 308);
        assert_eq!(
            resp.headers().get("Location").unwrap().to_str().unwrap(),
            expected_location
        );
    }
}

/// Each review on the patchset pages and the single-review endpoint carries
/// the reference repositories it was recorded with, or null for a review that
/// has none.
#[tokio::test]
#[ignore]
async fn test_review_endpoints_include_reference_repos() {
    let server = spawn_test_server(false).await;
    let record = serde_json::json!({
        "attached": [{
            "name": "kernel",
            "kind": "kernel",
            "revision": "v6.18",
            "commit": "1a2b3c4d5e6f1a2b3c4d5e6f1a2b3c4d5e6f1a2b",
            "why": "always",
            "reads": 34,
        }],
        "dormant": [{"name": "rdma-core", "applies_to": ["rdma/drv/**"]}],
    });

    for sql in [
        "INSERT INTO patchsets (id, status, subject, author, date) \
         VALUES (1, 'Reviewed', '[PATCH] test patch', 'Author <a@b.com>', 1234567890)"
            .to_string(),
        "INSERT INTO messages (message_id, subject, author, date) \
         VALUES ('<refs-1@example.com>', '[PATCH] test patch', 'Author <a@b.com>', 1234567890)"
            .to_string(),
        "INSERT INTO patches (id, patchset_id, message_id, part_index) \
         VALUES (1, 1, '<refs-1@example.com>', 1)"
            .to_string(),
        format!(
            "INSERT INTO reviews (id, patchset_id, patch_id, status, created_at, reference_repos) \
             VALUES (1, 1, 1, 'Reviewed', 1234567890, '{record}')"
        ),
        "INSERT INTO reviews (id, patchset_id, patch_id, status, created_at) \
         VALUES (2, 1, 1, 'Reviewed', 1234567891)"
            .to_string(),
    ] {
        server.db.conn.execute(&sql, ()).await.unwrap();
    }

    for endpoint in ["patch", "patchset"] {
        let resp = reqwest::get(format!("{}/api/{endpoint}?id=1", server.base_url))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{endpoint}");
        let body: serde_json::Value = resp.json().await.unwrap();
        let reviews = body["reviews"].as_array().unwrap();
        let by_id = |id: i64| {
            reviews
                .iter()
                .find(|r| r["id"] == id)
                .unwrap_or_else(|| panic!("{endpoint}: review {id} missing"))
        };
        assert_eq!(by_id(1)["reference_repos"], record, "{endpoint}");
        assert!(by_id(2)["reference_repos"].is_null(), "{endpoint}");
    }

    let resp = reqwest::get(format!("{}/api/review?id=1", server.base_url))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["reference_repos"], record);
}

/// Inserts patchset 1, patch 1 and review 1 with the given review status and
/// logs (NULL for none).
async fn insert_review(server: &TestServer, status: &str, logs: Option<&str>) {
    let logs = logs.map_or("NULL".to_string(), |l| format!("'{l}'"));
    for sql in [
        "INSERT INTO patchsets (id, status, subject, author, date) \
         VALUES (1, 'In Review', '[PATCH] test patch', 'Author <a@b.com>', 1234567890)"
            .to_string(),
        "INSERT INTO messages (message_id, subject, author, date) \
         VALUES ('<live-1@example.com>', '[PATCH] test patch', 'Author <a@b.com>', 1234567890)"
            .to_string(),
        "INSERT INTO patches (id, patchset_id, message_id, part_index) \
         VALUES (1, 1, '<live-1@example.com>', 1)"
            .to_string(),
        format!(
            "INSERT INTO reviews (id, patchset_id, patch_id, status, logs, created_at) \
             VALUES (1, 1, 1, '{status}', {logs}, 1234567890)"
        ),
    ] {
        server.db.conn.execute(&sql, ()).await.unwrap();
    }
}

fn live_message(role: &str, content: &str) -> String {
    serde_json::json!({"role": role, "content": content}).to_string()
}

async fn get_json(server: &TestServer, path: &str) -> serde_json::Value {
    let resp = reqwest::get(format!("{}{path}", server.base_url))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "{path}");
    resp.json().await.unwrap()
}

/// A running review's conversation is served as `live_log` from both review
/// routes, as messages in the finished log's shape with their position added.
#[tokio::test]
#[ignore]
async fn test_review_endpoint_serves_live_log_while_running() {
    let server = spawn_test_server(false).await;
    insert_review(&server, "In Review", None).await;
    server
        .db
        .append_review_log_entry(1, 0, None, "system", &live_message("system", "sys"))
        .await
        .unwrap();
    server
        .db
        .append_review_log_entry(1, 1, Some("goal"), "user", &live_message("user", "go"))
        .await
        .unwrap();

    let body = get_json(&server, "/api/review?id=1").await;
    let live = body["live_log"].as_array().expect("live_log");
    assert_eq!(live.len(), 2);
    assert_eq!(live[1]["role"], "user");
    assert_eq!(live[1]["content"], "go");
    assert_eq!(live[1]["seq"], 1);
    assert_eq!(live[1]["stage"], "goal");
    assert!(body["logs"].is_null());

    let tail = get_json(&server, "/api/review?id=1&after_seq=0").await;
    assert_eq!(tail["live_log"].as_array().unwrap().len(), 1);

    let via_log = get_json(&server, "/api/review_log?id=1").await;
    assert_eq!(via_log["live_log"], body["live_log"]);
}

/// Once the final log lands, or the review ends without one, there is no
/// live log to serve.
#[tokio::test]
#[ignore]
async fn test_live_log_is_not_served_for_a_finished_review() {
    let server = spawn_test_server(false).await;
    insert_review(&server, "Reviewed", Some("[]")).await;
    server
        .db
        .append_review_log_entry(1, 0, None, "user", &live_message("user", "x"))
        .await
        .unwrap();
    let body = get_json(&server, "/api/review?id=1").await;
    assert!(body.get("live_log").is_none(), "{body}");

    // Failed, with rows left behind by a review that never completed.
    server
        .db
        .conn
        .execute(
            "UPDATE reviews SET status = 'Failed', logs = NULL WHERE id = 1",
            (),
        )
        .await
        .unwrap();
    for path in ["/api/review?id=1", "/api/review_log?id=1"] {
        let body = get_json(&server, path).await;
        assert!(body.get("live_log").is_none(), "{path}: {body}");
    }
}

/// The activity routes are public, embargoed patchsets included, so the
/// conversation must never reach them.
#[tokio::test]
#[ignore]
async fn test_patchset_activity_carries_no_conversation() {
    use sashiko::activity::{ActivityKey, Phase, StageWait};

    let server = spawn_test_server(false).await;
    insert_review(&server, "In Review", None).await;
    server.activity.update(
        ActivityKey::PatchsetStage {
            patchset_id: 1,
            patch_id: 1,
            stage: "goal".to_string(),
        },
        Phase::Stage {
            stage: "goal".to_string(),
            turn: 1,
            max_turns: 10,
            waiting: StageWait::Model,
        },
    );
    const SECRET: &str = "a-distinctive-line-of-model-conversation";
    server
        .db
        .append_review_log_entry(
            1,
            0,
            Some("goal"),
            "assistant",
            &live_message("assistant", SECRET),
        )
        .await
        .unwrap();

    for path in [
        "/api/activity",
        "/api/patchset/activity?id=1",
        "/api/patchset?id=1",
        "/api/patch?id=1",
    ] {
        let body = reqwest::get(format!("{}{path}", server.base_url))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        if path.contains("activity") {
            assert!(body.contains("goal"), "{path}: {body}");
        }
        assert!(!body.contains(SECRET), "{path}: {body}");
        assert!(!body.contains("live_log"), "{path}: {body}");
    }
    let activity = get_json(&server, "/api/patchset/activity?id=1").await;
    assert_eq!(activity["finished"], false);
}

/// A finished review's recorded stage rows carry the same order and label as
/// the live entries, so the page sorts both on the server's order.
#[tokio::test]
#[ignore]
async fn test_recorded_stage_rows_carry_order_and_label() {
    let server = spawn_test_server(false).await;
    insert_review(&server, "Reviewed", Some("[]")).await;
    server
        .db
        .set_review_stage_durations(
            1,
            r#"[{"stage":"report","seconds":4,"turns":1},{"stage":"goal","seconds":9,"turns":2},{"stage":"mystery","seconds":1,"turns":1}]"#,
        )
        .await
        .unwrap();

    let single = get_json(&server, "/api/review?id=1").await;
    let patchset = get_json(&server, "/api/patchset?id=1").await;
    for (label, review) in [("review", &single), ("patchset", &patchset["reviews"][0])] {
        let rows = review["stage_durations"].as_array().expect(label);
        let find = |name: &str| rows.iter().find(|r| r["stage"] == name).unwrap().clone();
        assert!(
            find("goal")["order"].as_u64().unwrap() < find("report")["order"].as_u64().unwrap(),
            "{label}: {review}"
        );
        assert_eq!(find("goal")["stage_label"], "Goal Analysis", "{label}");
        assert!(find("mystery")["order"].is_null(), "{label}");
        assert!(find("mystery")["stage_label"].is_null(), "{label}");
    }
}

// ── Review a Pull Request by Number ─────────────────────────────────────

/// The base every pull request [`FakeGh`] answers for is based on.
const PR_BASE: &str = "1111111111111111111111111111111111111111";

/// Stands in for gh, so that no test runs it or reaches the network: pull
/// request N of any repository has base [`PR_BASE`] and head N in hex.
struct FakeGh;

#[async_trait::async_trait]
impl sashiko::forge::PullRequestResolver for FakeGh {
    async fn resolve(
        &self,
        repo: &sashiko::settings::ForgeRepo,
        number: i64,
    ) -> Result<sashiko::forge::ResolvedPullRequest, sashiko::forge::GhError> {
        let commit = |hex: &str| sashiko::baseline::CommitId::parse(hex).expect("a commit id");
        Ok(sashiko::forge::ResolvedPullRequest {
            repo: repo.clone(),
            number,
            url: format!(
                "https://{}/{}/{}/pull/{number}",
                repo.host, repo.owner, repo.name
            ),
            title: Some("Fix the widget".to_string()),
            base: commit(PR_BASE),
            head: commit(&format!("{number:040x}")),
        })
    }
}

/// A running server that admits only the local operator token, resolves
/// pull request numbers with [`FakeGh`], and keeps its fetch requests for
/// the test to count.
struct PrServer {
    base_url: String,
    db: Arc<Database>,
    fetch_rx: mpsc::Receiver<FetchRequest>,
    token: String,
}

impl PrServer {
    /// Posts `request` to /api/pr/review, with the local token when
    /// `with_token` is set, and returns the status and the body text.
    async fn ask(&self, request: serde_json::Value, with_token: bool) -> (u16, String) {
        let mut post = reqwest::Client::new()
            .post(format!("{}/api/pr/review", self.base_url))
            .json(&request);
        if with_token {
            post = post.bearer_auth(&self.token);
        }
        let resp = post.send().await.unwrap();
        (resp.status().as_u16(), resp.text().await.unwrap())
    }

    /// Asks with the token for pull request 7 in `mode`, expecting success.
    async fn review_seven(&self, mode: &str) -> serde_json::Value {
        let (status, body) = self
            .ask(serde_json::json!({ "number": 7, "mode": mode }), true)
            .await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    /// How many fetch requests are waiting, taking them off the channel.
    fn fetches_sent(&mut self) -> usize {
        std::iter::from_fn(|| self.fetch_rx.try_recv().ok()).count()
    }

    async fn set_status(&self, id: i64, status: &str) {
        self.db
            .conn
            .execute(
                "UPDATE patchsets SET status = ? WHERE id = ?",
                libsql::params![status, id],
            )
            .await
            .unwrap();
    }
}

async fn spawn_pr_server(read_only: bool, github_repository: Option<&str>) -> PrServer {
    let db = Arc::new(
        Database::new(&DatabaseSettings {
            url: ":memory:".to_string(),
            token: String::new(),
        })
        .await
        .unwrap(),
    );
    db.migrate().await.unwrap();
    let (event_tx, _event_rx) = mpsc::channel::<Event>(100);
    let (fetch_tx, fetch_rx) = mpsc::channel::<FetchRequest>(100);

    let mut settings = test_settings(read_only);
    settings.forge.github_repository = github_repository.map(|repo| repo.parse().unwrap());
    let local_token = sashiko::auth::LocalToken::generate().unwrap();
    let token = local_token.secret().to_string();
    let app = build_router(
        Arc::new(settings),
        Arc::clone(&db),
        event_tx,
        fetch_tx,
        ServerOptions {
            local_token: Some(local_token),
            pull_request_resolver: Arc::new(FakeGh),
            ..Default::default()
        },
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    PrServer {
        base_url,
        db,
        fetch_rx,
        token,
    }
}

/// The route is a mutation: it refuses a caller without the token, and a
/// read-only server refuses even the token.
#[tokio::test]
#[ignore]
async fn test_pr_review_refuses_without_the_token_or_on_a_read_only_server() {
    let request = serde_json::json!({ "number": 7 });
    let mut server = spawn_pr_server(false, Some("acme/widget")).await;
    assert_eq!(server.ask(request.clone(), false).await.0, 403);
    assert_eq!(server.fetches_sent(), 0);

    let mut read_only = spawn_pr_server(true, Some("acme/widget")).await;
    assert_eq!(read_only.ask(request, true).await.0, 403);
    assert_eq!(read_only.fetches_sent(), 0);
}

/// Without forge.github_repository the route cannot say which repository a
/// number belongs to, and refuses with 422 and a reason naming the setting.
#[tokio::test]
#[ignore]
async fn test_pr_review_needs_the_github_repository_setting() {
    let mut server = spawn_pr_server(false, None).await;
    let (status, body) = server.ask(serde_json::json!({ "number": 7 }), true).await;
    assert_eq!(status, 422);
    assert!(body.contains("forge.github_repository"), "{body}");
    assert_eq!(server.fetches_sent(), 0);
}

/// One head revision asked for again and again as its row moves through
/// the statuses: queued with its id when new, in progress while still
/// Fetching (the fetch is sent again, to recover a lost request) and while
/// Pending (none is), finished once Reviewed, rerun under rerun_head, and
/// queued again after a failure.
#[tokio::test]
#[ignore]
async fn test_pr_review_by_number_follows_the_head_revision() {
    let mut server = spawn_pr_server(false, Some("acme/widget")).await;

    let queued = server.review_seven("review_head").await;
    assert_eq!(queued["outcome"], "queued");
    assert_eq!(queued["number"], 7);
    assert_eq!(queued["head"], format!("{:040x}", 7));
    assert!(queued["previous_status"].is_null(), "{queued}");
    let id = queued["patchset_id"].as_i64().unwrap();
    assert_eq!(server.fetches_sent(), 1);

    let fetching = server.review_seven("review_head").await;
    assert_eq!(fetching["outcome"], "already_in_progress");
    assert_eq!(fetching["previous_status"], "Fetching");
    assert_eq!(server.fetches_sent(), 1);

    for (status, mode, outcome, fetches) in [
        ("Pending", "review_head", "already_in_progress", 0),
        ("Reviewed", "review_head", "already_finished", 0),
        ("Reviewed", "rerun_head", "rerun_queued", 0),
        ("Failed", "review_head", "queued", 1),
    ] {
        server.set_status(id, status).await;
        let reply = server.review_seven(mode).await;
        assert_eq!(reply["patchset_id"], id, "one head has one row");
        assert_eq!(reply["outcome"], outcome, "{status} under {mode}: {reply}");
        assert_eq!(server.fetches_sent(), fetches, "{status} under {mode}");
    }
}

// ── Open a Pull Request Revision by Its Head Commit ─────────────────────

/// After later pushes take the bare slug, the first revision is still
/// opened by its head prefix on both routes, the bare slug opens the
/// newest and the rotated -v1 slug the first. Each miss is a 404 whose
/// body is its reason: an unknown pull request, a prefix no head starts
/// with, and a prefix two heads share.
#[tokio::test]
#[ignore]
async fn test_a_pull_request_revision_opens_by_its_head_after_rotation() {
    let server = spawn_test_server(false).await;
    let mut revisions = Vec::new();
    let shares_b_prefix = format!("bbbbbbb{}", "e".repeat(33));
    for head in ["b".repeat(40), shares_b_prefix, "c".repeat(40)] {
        let id = server
            .db
            .create_fetching_patchset(
                &format!("mr-7-{PR_BASE}..{head}@sashiko.local"),
                "Fetching PR #7",
                None,
                None,
                Some("https://github.com/acme/widget/pull/7"),
                Some("Fix the widget"),
                Some(7),
                Some("widget-7"),
            )
            .await
            .unwrap();
        revisions.push(id);
    }

    for route in ["/api/patch", "/api/patchset"] {
        for (id, expected) in [
            ("widget-7-bbbbbbbbbbbb", revisions[0]),
            ("widget-7", revisions[2]),
            ("widget-7-v1", revisions[0]),
        ] {
            let found = get_json(&server, &format!("{route}?id={id}")).await;
            assert_eq!(found["id"], expected, "{route} {id}");
        }

        for (id, reason) in [
            (
                "widget-9-bbbbbbb",
                "no pull request revision has this address",
            ),
            (
                "widget-7-ddddddd",
                "no revision of this pull request has a head commit starting ddddddd",
            ),
            (
                "widget-7-bbbbbbb",
                "bbbbbbb matches 2 revisions of this pull request; give more of the commit",
            ),
        ] {
            let resp = reqwest::get(format!("{}{route}?id={id}", server.base_url))
                .await
                .unwrap();
            assert_eq!(resp.status().as_u16(), 404, "{route} {id}");
            assert_eq!(resp.text().await.unwrap(), reason, "{route} {id}");
        }
    }
}
