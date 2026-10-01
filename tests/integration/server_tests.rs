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
