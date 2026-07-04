//! Ingest hint-gate integration tests: device content hints in `attrs` decide whether a
//! lane's status row is born `pending` (worker work) or terminal `skipped` (migration 0022).
//! Gated on `DATABASE_URL` like the other integration tests; exercises the REAL router +
//! multipart ingest so decode → hints::parse → db::persist_segment is covered end to end.
//!
//! Determinism note: the audit lot is `segment_id.as_bytes()[15] % 100`, so tests pin the
//! UUID's last byte — 99 forces PAST the default 2% audit window (a pure skip), 0 forces
//! INTO it (an audit enqueue).

use std::collections::HashMap;

use hushai_backend::config::Config;
use hushai_backend::proto::{MediaType, SegmentManifest};
use hushai_backend::{build_state, routes};
use prost::Message;
use reqwest::Client;
use reqwest::multipart::{Form, Part};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

fn manifest_bytes(
    segment_id: [u8; 16],
    session_id: [u8; 16],
    device: &str,
    sequence: u64,
    body: &[u8],
    attrs: &[(&str, &str)],
) -> Vec<u8> {
    SegmentManifest {
        segment_id: segment_id.to_vec(),
        session_id: session_id.to_vec(),
        device_id: device.into(),
        stream_id: "hint-stream".into(),
        sequence,
        source_kind: "integration_test".into(),
        media_type: MediaType::Muxed as i32,
        codec: "h264+aac".into(),
        container: "fmp4".into(),
        content_sha256: Sha256::digest(body).to_vec(),
        byte_len: body.len() as u64,
        capture_start_unix_nanos: 1,
        monotonic_start_nanos: 1,
        duration_nanos: 2_000_000_000,
        attrs: attrs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<HashMap<_, _>>(),
        ..Default::default()
    }
    .encode_to_vec()
}

async fn post(client: &Client, url: &str, manifest: Vec<u8>, body: Vec<u8>) -> u16 {
    let form = Form::new()
        .part("manifest", Part::bytes(manifest).file_name("manifest"))
        .part("body", Part::bytes(body).file_name("body"));
    client
        .post(url)
        .bearer_auth("test-token")
        .multipart(form)
        .send()
        .await
        .expect("request")
        .status()
        .as_u16()
}

/// A fresh UUIDv7 with the last byte pinned (the deterministic audit roll).
fn seg_id(last_byte: u8) -> [u8; 16] {
    let mut b = *Uuid::now_v7().as_bytes();
    b[15] = last_byte;
    b
}

async fn lane_row(pool: &PgPool, table: &str, seg: [u8; 16]) -> (String, Option<String>, bool) {
    // Test-only: `table` is a hardcoded constant per call site.
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT status, skip_reason, hint_audit FROM {table} WHERE segment_id = $1"
    )))
    .bind(Uuid::from_bytes(seg))
    .fetch_one(pool)
    .await
    .expect("lane status row must exist")
}

#[tokio::test]
async fn hint_gate_decides_lanes_independently_and_fails_open() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("SKIP: DATABASE_URL not set");
        return;
    };

    let blob_dir = std::env::temp_dir().join(format!("hushai-hint-{}", Uuid::now_v7()));
    let config = Config {
        database_url: database_url.clone(),
        blob_dir,
        device_token: "test-token".into(),
        device_tokens: None,
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        tls: None,
        max_body_bytes: 1024 * 1024,
        concurrency_cap: 64,
        disk_watermark_bytes: 0,
        db_max_connections: 5,
        db_acquire_timeout_secs: 5,
        request_timeout_secs: 30,
        hint_gate: Default::default(), // enabled, floors 0.005/4.0, audit 2%
    };
    let state = build_state(config).await.expect("build state");
    let pool = state.pool.clone();
    let app = routes::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let url = format!("http://{addr}/v1/segments");
    let client = Client::new();

    let sess = *Uuid::now_v7().as_bytes();
    let device = format!("it-hint-{}", Uuid::now_v7());
    let body = b"hint-gate-muxed-bytes".to_vec();

    // 1) Silent audio hint only (roll 99 = past the audit lot): transcription lane born
    //    skipped, vision lane (no motion hint) fails open to pending. MUXED lanes decide
    //    INDEPENDENTLY.
    let s1 = seg_id(99);
    let m = manifest_bytes(
        s1, sess, &device, 0, &body,
        &[("hint.v", "1"), ("hint.audio_peak_rms", "0.0001"), ("hint.audio_rms", "0.0001")],
    );
    assert_eq!(post(&client, &url, m, body.clone()).await, 200);
    let (status, reason, audit) = lane_row(&pool, "segment_transcription_status", s1).await;
    assert_eq!((status.as_str(), reason.as_deref(), audit), ("skipped", Some("silent_hint"), false));
    let (status, _, _) = lane_row(&pool, "segment_vision_status", s1).await;
    // A live worker may already have claimed the row (pg_notify), so assert the invariant
    // that matters: fail-open means NOT pre-terminated as skipped.
    assert_ne!(status, "skipped", "no motion hint => vision fails open");

    // 2) Both hints below the floors: both lanes born skipped.
    let s2 = seg_id(99);
    let m = manifest_bytes(
        s2, sess, &device, 1, &body,
        &[("hint.v", "1"), ("hint.audio_peak_rms", "0.0001"), ("hint.motion_score", "0.2")],
    );
    assert_eq!(post(&client, &url, m, body.clone()).await, 200);
    let (status, reason, _) = lane_row(&pool, "segment_transcription_status", s2).await;
    assert_eq!((status.as_str(), reason.as_deref()), ("skipped", Some("silent_hint")));
    let (status, reason, _) = lane_row(&pool, "segment_vision_status", s2).await;
    assert_eq!((status.as_str(), reason.as_deref()), ("skipped", Some("static_hint")));

    // 3) Loud/moving content: both pending.
    let s3 = seg_id(99);
    let m = manifest_bytes(
        s3, sess, &device, 2, &body,
        &[("hint.v", "1"), ("hint.audio_peak_rms", "0.4"), ("hint.motion_score", "55")],
    );
    assert_eq!(post(&client, &url, m, body.clone()).await, 200);
    assert_ne!(lane_row(&pool, "segment_transcription_status", s3).await.0, "skipped");
    assert_ne!(lane_row(&pool, "segment_vision_status", s3).await.0, "skipped");

    // 4) Malformed + unversioned hints fail OPEN to pending.
    let s4 = seg_id(99);
    let m = manifest_bytes(
        s4, sess, &device, 3, &body,
        &[("hint.v", "1"), ("hint.audio_peak_rms", "NaN"), ("hint.motion_score", "-3")],
    );
    assert_eq!(post(&client, &url, m, body.clone()).await, 200);
    assert_ne!(lane_row(&pool, "segment_transcription_status", s4).await.0, "skipped");
    assert_ne!(lane_row(&pool, "segment_vision_status", s4).await.0, "skipped");
    let s5 = seg_id(99);
    let m = manifest_bytes(
        s5, sess, &device, 4, &body,
        &[("hint.audio_peak_rms", "0.0001")], // no hint.v
    );
    assert_eq!(post(&client, &url, m, body.clone()).await, 200);
    assert_ne!(lane_row(&pool, "segment_transcription_status", s5).await.0, "skipped");

    // 5) Audit lot (roll 0 < 2%): a would-be skip is enqueued pending with hint_audit=true,
    //    so the worker can grade the hint.
    let s6 = seg_id(0);
    let m = manifest_bytes(
        s6, sess, &device, 5, &body,
        &[("hint.v", "1"), ("hint.audio_peak_rms", "0.0001"), ("hint.motion_score", "0.2")],
    );
    assert_eq!(post(&client, &url, m, body.clone()).await, 200);
    let (status, reason, audit) = lane_row(&pool, "segment_transcription_status", s6).await;
    assert_ne!(status, "skipped", "audit sample must be enqueued, not skipped");
    assert_eq!((reason.as_deref(), audit), (None, true));
    let (status, _, audit) = lane_row(&pool, "segment_vision_status", s6).await;
    assert_ne!(status, "skipped");
    assert!(audit);

    // Cleanup (FK-ordered).
    for sql in [
        "DELETE FROM segment_transcription_status WHERE segment_id IN (SELECT segment_id FROM segments WHERE device_id=$1)",
        "DELETE FROM segment_vision_status WHERE segment_id IN (SELECT segment_id FROM segments WHERE device_id=$1)",
        "DELETE FROM segments WHERE device_id=$1",
        "DELETE FROM streams WHERE device_id=$1",
        "DELETE FROM sessions WHERE device_id=$1",
        "DELETE FROM devices WHERE device_id=$1",
    ] {
        let _ = sqlx::query(sql).bind(&device).execute(&pool).await;
    }
}
