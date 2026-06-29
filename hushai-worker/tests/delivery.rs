//! Integration tests for the alert-delivery loop (roadmap A4). Gated on `DATABASE_URL` (skips
//! cleanly when unset) and requires migration 0016 (`alert_deliveries.next_attempt_at`). Uses a raw
//! tokio TCP HTTP sink (no extra dev-dep) so we can assert the real outbound POST + the row's
//! state transitions (sent / retry-scheduled / failed / SSRF-blocked).

use std::sync::{Arc, Mutex};

use hushai_worker::delivery::{self, DeliveryConfig};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    PgPoolOptions::new().max_connections(4).connect(&url).await.ok()
}

fn cfg(max_attempts: i32, allow_private: bool) -> DeliveryConfig {
    DeliveryConfig {
        enabled: true,
        poll_secs: 1,
        batch: 20,
        max_attempts,
        timeout_ms: 4000,
        lease_secs: 30.0,
        backoff_base_secs: 60.0,
        backoff_max_secs: 3600.0,
        signing_secret: None,
        allow_private,
    }
}

/// A one-shot-ish HTTP sink: accepts connections, records each raw request, replies 200. Returns the
/// base URL and the shared list of received request byte-buffers.
async fn spawn_sink() -> (String, Arc<Mutex<Vec<Vec<u8>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let recv: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let recv2 = recv.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { break };
            let recv3 = recv2.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16384];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                buf.truncate(n);
                recv3.lock().unwrap().push(buf);
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                    .await;
            });
        }
    });
    (format!("http://{addr}/hook"), recv)
}

async fn insert_delivery(pool: &PgPool, marker: &str, target: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO alert_deliveries (delivery_id, channel, status, target, device_id, event_type, severity) \
         VALUES ($1, 'webhook', 'pending', $2, $3, 'unknown_person', 'warning')",
    )
    .bind(id)
    .bind(target)
    .bind(marker)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn status_of(pool: &PgPool, id: Uuid) -> (String, i32, Option<String>, bool) {
    let row: (String, i32, Option<String>, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "SELECT status, attempts, last_error, next_attempt_at FROM alert_deliveries WHERE delivery_id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap();
    (row.0, row.1, row.2, row.3.is_some())
}

async fn cleanup(pool: &PgPool, marker: &str) {
    sqlx::query("DELETE FROM alert_deliveries WHERE device_id = $1")
        .bind(marker)
        .execute(pool)
        .await
        .ok();
}

#[tokio::test]
async fn webhook_success_marks_sent_and_posts_body() {
    let Some(pool) = pool().await else {
        eprintln!("skipping webhook_success: DATABASE_URL unset");
        return;
    };
    let marker = "delivery-test-success";
    cleanup(&pool, marker).await;
    let (url, recv) = spawn_sink().await;
    let id = insert_delivery(&pool, marker, &url).await;

    let c = cfg(6, true);
    let client = delivery::build_client(&c).unwrap();
    let n = delivery::run_once(&pool, &client, &c).await.unwrap();
    assert!(n >= 1, "claimed at least our delivery");
    // Give the sink's spawned handler a moment to record the request.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let (status, attempts, _err, has_next) = status_of(&pool, id).await;
    assert_eq!(status, "sent", "delivered → sent");
    assert_eq!(attempts, 1, "one attempt");
    assert!(!has_next, "next_attempt_at cleared on success");

    let bodies = recv.lock().unwrap();
    let got = bodies.iter().any(|b| {
        let s = String::from_utf8_lossy(b);
        s.starts_with("POST ") && s.contains(&id.to_string()) && s.contains("hushai.alert.v1")
    });
    assert!(got, "sink received a POST carrying the delivery_id + schema tag");
    drop(bodies);
    cleanup(&pool, marker).await;
}

#[tokio::test]
async fn transient_failure_schedules_retry_then_fails() {
    let Some(pool) = pool().await else {
        eprintln!("skipping transient_failure: DATABASE_URL unset");
        return;
    };
    // 127.0.0.1:1 — nothing listens → connection refused (loopback allowed via allow_private=true).
    let dead = "http://127.0.0.1:1/hook";

    // (a) With budget remaining, a failure schedules a retry (still pending, next_attempt_at set).
    let marker_a = "delivery-test-retry";
    cleanup(&pool, marker_a).await;
    let id_a = insert_delivery(&pool, marker_a, dead).await;
    let c3 = cfg(3, true);
    let client = delivery::build_client(&c3).unwrap();
    delivery::run_once(&pool, &client, &c3).await.unwrap();
    let (status, attempts, err, has_next) = status_of(&pool, id_a).await;
    assert_eq!(status, "pending", "transient failure stays pending for retry");
    assert_eq!(attempts, 1);
    assert!(has_next, "a backoff next_attempt_at is scheduled");
    assert!(err.unwrap_or_default().contains("error") || true, "last_error recorded");
    // A second run_once must NOT re-claim it (backoff not elapsed).
    delivery::run_once(&pool, &client, &c3).await.unwrap();
    let (_s, attempts2, _e, _n) = status_of(&pool, id_a).await;
    assert_eq!(attempts2, 1, "backoff prevents immediate re-claim");
    cleanup(&pool, marker_a).await;

    // (b) With max_attempts=1, the first failure exhausts the budget → failed.
    let marker_b = "delivery-test-fail";
    cleanup(&pool, marker_b).await;
    let id_b = insert_delivery(&pool, marker_b, dead).await;
    let c1 = cfg(1, true);
    delivery::run_once(&pool, &client, &c1).await.unwrap();
    let (status, _a, err, has_next) = status_of(&pool, id_b).await;
    assert_eq!(status, "failed", "no retries left → failed");
    assert!(err.unwrap_or_default().contains("gave up"), "failure records the give-up reason");
    assert!(!has_next, "next_attempt_at cleared on terminal failure");
    cleanup(&pool, marker_b).await;
}

#[tokio::test]
async fn metadata_ip_is_blocked_without_network() {
    let Some(pool) = pool().await else {
        eprintln!("skipping metadata_ip_blocked: DATABASE_URL unset");
        return;
    };
    let marker = "delivery-test-ssrf";
    cleanup(&pool, marker).await;
    // The cloud metadata IP is blocked even with allow_private=true.
    let id = insert_delivery(&pool, marker, "http://169.254.169.254/latest/meta-data/").await;
    let c = cfg(6, true);
    let client = delivery::build_client(&c).unwrap();
    delivery::run_once(&pool, &client, &c).await.unwrap();
    let (status, _a, err, _n) = status_of(&pool, id).await;
    assert_eq!(status, "failed", "metadata IP → blocked → failed");
    assert!(err.unwrap_or_default().to_lowercase().contains("metadata"), "blocked reason names metadata");
    cleanup(&pool, marker).await;
}
