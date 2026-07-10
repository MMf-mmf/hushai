//! Gotham Phase D — the anomaly→alert DELIVERY end-to-end path (Gotham.md §1.6 / Part 4 Phase D).
//!
//! The existing `delivery.rs` suite proves the send LOOP in isolation (it inserts `alert_deliveries`
//! rows directly). This suite proves the JOIN that Phase D actually asks for and that no automated
//! test covered: a real `pattern_anomaly` `events` row → `alerts::evaluate` matches an operator
//! `alert_rules` row with `event_types={pattern_anomaly}` → a `feed` AND a `webhook` outbox row are
//! created → `delivery::run_once` POSTs the webhook with a VALID `X-Hushai-Signature` HMAC while the
//! `feed` row is left in-app (never sent). Plus the negatives (a non-matching type and a below-floor
//! severity fire nothing). This is the worker-side of Gotham.md §1.6: the backend `graph_pass` emits
//! the anomaly and hands its event_id to `hushai-worker/src/lib.rs`, which calls exactly the
//! `alerts::evaluate` this test drives.
//!
//! Gated on `DATABASE_URL` (skips cleanly when unset); requires migrations 0014 (events/alerts) +
//! 0016 (`alert_deliveries.next_attempt_at`). Everything is scoped to a per-test device_id marker +
//! a per-test rule (fresh `rule_id`), so it never truncates and is robust to stray enabled rules in
//! the shared `hushai_test` DB (all assertions filter on the test's own `rule_id`).

use std::sync::{Arc, Mutex};

use hmac::{Hmac, Mac};
use hushai_worker::delivery::{self, DeliveryConfig};
use sha2::Sha256;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;
const SECRET: &str = "phase-d-hmac-secret";

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    PgPoolOptions::new().max_connections(4).connect(&url).await.ok()
}

fn cfg_signed(target_allow_private: bool) -> DeliveryConfig {
    DeliveryConfig {
        enabled: true,
        poll_secs: 1,
        batch: 50,
        max_attempts: 6,
        timeout_ms: 4000,
        lease_secs: 30.0,
        backoff_base_secs: 60.0,
        backoff_max_secs: 3600.0,
        signing_secret: Some(SECRET.to_string()),
        allow_private: target_allow_private,
    }
}

/// A Content-Length-aware HTTP sink: reads the FULL request (so the captured body byte-for-byte
/// matches what was signed), records each raw request, replies 200. Returns base URL + capture list.
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
                let mut buf: Vec<u8> = Vec::with_capacity(8192);
                let mut chunk = [0u8; 4096];
                // Read until headers complete AND the declared body has fully arrived.
                loop {
                    let n = match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(hdr_end) = find_subslice(&buf, b"\r\n\r\n") {
                        let head = &buf[..hdr_end];
                        let want = content_length(head).unwrap_or(0);
                        let have = buf.len() - (hdr_end + 4);
                        if have >= want {
                            break;
                        }
                    }
                }
                recv3.lock().unwrap().push(buf);
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                    .await;
            });
        }
    });
    (format!("http://{addr}/hook"), recv)
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Parse `Content-Length` (case-insensitive) from an ASCII header block.
fn content_length(head: &[u8]) -> Option<usize> {
    let s = String::from_utf8_lossy(head);
    for line in s.split("\r\n") {
        let mut it = line.splitn(2, ':');
        let (k, v) = (it.next()?, it.next()?);
        if k.trim().eq_ignore_ascii_case("content-length") {
            return v.trim().parse().ok();
        }
    }
    None
}

/// Split a captured raw request into (header-value-for `x-hushai-signature`, body bytes).
fn parse_request(raw: &[u8]) -> (Option<String>, Vec<u8>) {
    let hdr_end = find_subslice(raw, b"\r\n\r\n").unwrap_or(raw.len());
    let head = String::from_utf8_lossy(&raw[..hdr_end]);
    let mut sig = None;
    for line in head.split("\r\n") {
        let mut it = line.splitn(2, ':');
        if let (Some(k), Some(v)) = (it.next(), it.next())
            && k.trim().eq_ignore_ascii_case("x-hushai-signature")
        {
            sig = Some(v.trim().to_string());
        }
    }
    let body = if hdr_end + 4 <= raw.len() { raw[hdr_end + 4..].to_vec() } else { Vec::new() };
    (sig, body)
}

fn hmac_hex(secret: &str, body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// Insert a device (FK target for events/deliveries) — idempotent.
async fn ensure_device(pool: &PgPool, device_id: &str) {
    sqlx::query("INSERT INTO devices (device_id, source_kind) VALUES ($1, 'test') ON CONFLICT (device_id) DO NOTHING")
        .bind(device_id)
        .execute(pool)
        .await
        .unwrap();
}

/// Insert one `events` row; returns its event_id. Mirrors `patterns::emit_anomaly`'s row shape.
#[allow(clippy::too_many_arguments)]
async fn insert_event(
    pool: &PgPool,
    device_id: &str,
    event_type: &str,
    severity: &str,
    subject_type: Option<&str>,
    subject_id: Option<Uuid>,
    subject_label: Option<&str>,
) -> Uuid {
    let id = Uuid::now_v7();
    // A recent, always-on-window-safe timestamp (nanos). now()-ish so tz gates never trip.
    let now_ns: i64 = sqlx::query_scalar("SELECT (extract(epoch FROM now()) * 1e9)::bigint")
        .fetch_one(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO events \
           (event_id, device_id, event_type, severity, subject_type, subject_id, subject_label, \
            start_unix_nanos, end_unix_nanos, dedup_key) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(id)
    .bind(device_id)
    .bind(event_type)
    .bind(severity)
    .bind(subject_type)
    .bind(subject_id)
    .bind(subject_label)
    .bind(now_ns)
    .bind(now_ns + 2_000_000_000)
    .bind(format!("phase-d:{id}"))
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Insert an always-on alert rule; returns rule_id. `channels` is a jsonb array (feed + webhook).
async fn insert_rule(
    pool: &PgPool,
    name: &str,
    event_types: &[&str],
    min_severity: &str,
    channels: serde_json::Value,
) -> Uuid {
    let id = Uuid::now_v7();
    let types: Vec<String> = event_types.iter().map(|s| s.to_string()).collect();
    sqlx::query(
        "INSERT INTO alert_rules (rule_id, name, enabled, event_types, min_severity, tz, cooldown_secs, channels) \
         VALUES ($1, $2, true, $3, $4, 'UTC', 0, $5)",
    )
    .bind(id)
    .bind(name)
    .bind(&types)
    .bind(min_severity)
    .bind(channels)
    .execute(pool)
    .await
    .unwrap();
    id
}

/// Delivery rows created for a specific rule (channel, status, target).
async fn deliveries_for_rule(pool: &PgPool, rule_id: Uuid) -> Vec<(String, String, Option<String>)> {
    sqlx::query(
        "SELECT channel, status, target FROM alert_deliveries WHERE rule_id = $1 ORDER BY channel",
    )
    .bind(rule_id)
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|r| (r.get("channel"), r.get("status"), r.get("target")))
    .collect()
}

async fn cleanup(pool: &PgPool, device_id: &str, rule_names: &[&str]) {
    // deliveries first (also cascade via event/rule FK, but be explicit), then events, rules, device.
    sqlx::query("DELETE FROM alert_deliveries WHERE device_id = $1").bind(device_id).execute(pool).await.ok();
    sqlx::query("DELETE FROM events WHERE device_id = $1").bind(device_id).execute(pool).await.ok();
    for n in rule_names {
        sqlx::query("DELETE FROM alert_rules WHERE name = $1").bind(n).execute(pool).await.ok();
    }
    sqlx::query("DELETE FROM devices WHERE device_id = $1").bind(device_id).execute(pool).await.ok();
}

/// Phase D happy path: pattern_anomaly event → rule match → feed + webhook rows → webhook delivered
/// with a valid HMAC; the feed row stays in-app (pending, never sent).
#[tokio::test]
async fn anomaly_fires_feed_and_signed_webhook() {
    let Some(pool) = pool().await else {
        eprintln!("skipping anomaly_fires_feed_and_signed_webhook: DATABASE_URL unset");
        return;
    };
    let device = "phase-d-deliver";
    let rule_name = "phase-d-anomaly-rule";
    cleanup(&pool, device, &[rule_name]).await;

    let (url, recv) = spawn_sink().await;
    ensure_device(&pool, device).await;
    let subj = Uuid::now_v7();
    let event_id =
        insert_event(&pool, device, "pattern_anomaly", "warning", Some("person"), Some(subj), Some("Alice")).await;
    let rule_id = insert_rule(
        &pool,
        rule_name,
        &["pattern_anomaly"],
        "info",
        serde_json::json!([{"type": "feed"}, {"type": "webhook", "url": url}]),
    )
    .await;

    // 1. Evaluate: the anomaly matches the rule and fans out onto both channels.
    let created = hushai_worker::alerts::evaluate(&pool, event_id).await.unwrap();
    assert!(created >= 2, "evaluate created at least the feed + webhook rows (got {created})");

    let rows = deliveries_for_rule(&pool, rule_id).await;
    assert_eq!(rows.len(), 2, "exactly one feed + one webhook row for this rule: {rows:?}");
    let feed = rows.iter().find(|(c, _, _)| c == "feed").expect("feed row present");
    let hook = rows.iter().find(|(c, _, _)| c == "webhook").expect("webhook row present");
    assert_eq!(feed.1, "pending", "feed starts pending (in-app)");
    assert!(feed.2.is_none(), "feed has no target url");
    assert_eq!(hook.1, "pending", "webhook starts pending");
    assert_eq!(hook.2.as_deref(), Some(url.as_str()), "webhook target is the rule's url");

    // 2. Deliver: the webhook is sent (with signature); the feed row is untouched.
    let c = cfg_signed(true);
    let client = delivery::build_client(&c).unwrap();
    delivery::run_once(&pool, &client, &c).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let rows = deliveries_for_rule(&pool, rule_id).await;
    let feed = rows.iter().find(|(c, _, _)| c == "feed").unwrap();
    let hook = rows.iter().find(|(c, _, _)| c == "webhook").unwrap();
    assert_eq!(hook.1, "sent", "webhook delivered → sent");
    assert_eq!(feed.1, "pending", "feed channel is NEVER touched by the delivery loop");

    // 3. The sink got a signed POST whose HMAC is valid over the exact wire body + the v1 schema.
    let captured = {
        let bodies = recv.lock().unwrap();
        bodies
            .iter()
            .map(|b| parse_request(b))
            .find(|(_, body)| {
                let s = String::from_utf8_lossy(body);
                s.contains(&event_id.to_string())
            })
            .expect("sink received the POST for our event")
    };
    let (sig, body) = captured;
    let sig = sig.expect("x-hushai-signature header present");
    assert_eq!(sig, format!("sha256={}", hmac_hex(SECRET, &body)), "HMAC valid over the wire body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("body is JSON");
    assert_eq!(json["schema"], "hushai.alert.v1");
    assert_eq!(json["event_type"], "pattern_anomaly");
    assert_eq!(json["severity"], "warning");
    assert_eq!(json["subject_label"], "Alice");
    assert_eq!(json["event_id"], event_id.to_string());

    cleanup(&pool, device, &[rule_name]).await;
}

/// Phase D negatives: a rule fires NOTHING for an event whose type doesn't match, and nothing for an
/// event below the rule's severity floor. (The "conforming day fires nothing" negative is the
/// anomaly-EMISSION negative, covered by the `anomaly_negatives` fixture / graph_db guard — here we
/// prove the DELIVERY-side gates.)
#[tokio::test]
async fn non_matching_event_and_below_floor_fire_nothing() {
    let Some(pool) = pool().await else {
        eprintln!("skipping non_matching_event_and_below_floor_fire_nothing: DATABASE_URL unset");
        return;
    };
    let device = "phase-d-negative";
    let type_rule = "phase-d-type-rule";
    let floor_rule = "phase-d-floor-rule";
    cleanup(&pool, device, &[type_rule, floor_rule]).await;
    ensure_device(&pool, device).await;

    // (a) Wrong type: a rule scoped to pattern_anomaly must ignore a person_seen event.
    let type_rid = insert_rule(&pool, type_rule, &["pattern_anomaly"], "info", serde_json::json!([{"type": "feed"}])).await;
    let ps_event = insert_event(&pool, device, "person_seen", "info", Some("person"), Some(Uuid::now_v7()), Some("Bob")).await;
    hushai_worker::alerts::evaluate(&pool, ps_event).await.unwrap();
    assert!(deliveries_for_rule(&pool, type_rid).await.is_empty(), "wrong event_type → no delivery");

    // (b) Below floor: a critical-only rule must ignore a warning anomaly.
    let floor_rid = insert_rule(&pool, floor_rule, &["pattern_anomaly"], "critical", serde_json::json!([{"type": "feed"}])).await;
    let warn_event = insert_event(&pool, device, "pattern_anomaly", "warning", Some("person"), Some(Uuid::now_v7()), Some("Carol")).await;
    hushai_worker::alerts::evaluate(&pool, warn_event).await.unwrap();
    assert!(deliveries_for_rule(&pool, floor_rid).await.is_empty(), "severity below floor → no delivery");

    cleanup(&pool, device, &[type_rule, floor_rule]).await;
}
