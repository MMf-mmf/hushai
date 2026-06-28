//! End-to-end HTTP integration tests against the real router + Postgres.
//!
//! Gated on `DATABASE_URL`: when unset (e.g. offline CI without a DB) the test
//! skips rather than fails. These back up — they don't replace — the real
//! `local_dev/feed_segments.py` run against IMG_7256.mp4.

use hushai_backend::config::Config;
use hushai_backend::proto::{MediaType, SegmentManifest};
use hushai_backend::{build_state, routes};
use prost::Message;
use reqwest::Client;
use reqwest::multipart::{Form, Part};
use sha2::{Digest, Sha256};
use uuid::Uuid;

fn manifest_bytes(
    segment_id: [u8; 16],
    session_id: [u8; 16],
    device: &str,
    stream: &str,
    sequence: u64,
    body: &[u8],
) -> Vec<u8> {
    SegmentManifest {
        segment_id: segment_id.to_vec(),
        session_id: session_id.to_vec(),
        device_id: device.into(),
        stream_id: stream.into(),
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
        ..Default::default()
    }
    .encode_to_vec()
}

async fn post(client: &Client, url: &str, token: &str, manifest: Vec<u8>, body: Vec<u8>) -> u16 {
    let form = Form::new()
        .part("manifest", Part::bytes(manifest).file_name("manifest"))
        .part("body", Part::bytes(body).file_name("body"));
    client
        .post(url)
        .bearer_auth(token)
        .multipart(form)
        .send()
        .await
        .expect("request")
        .status()
        .as_u16()
}

#[tokio::test]
async fn ingest_end_to_end() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        eprintln!("SKIP: DATABASE_URL not set");
        return;
    };

    let blob_dir = std::env::temp_dir().join(format!("hushai-it-{}", Uuid::now_v7()));
    let config = Config {
        database_url,
        blob_dir,
        device_token: "test-token".into(),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        max_body_bytes: 1024 * 1024,
        concurrency_cap: 64,
        disk_watermark_bytes: 0, // never shed in tests
        db_max_connections: 5,
        db_acquire_timeout_secs: 5,
        request_timeout_secs: 30,
    };

    let state = build_state(config).await.expect("build state");
    let app = routes::router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let url = format!("http://{addr}/v1/segments");
    let client = Client::new();

    // Unique ids so repeated runs never collide on UNIQUE(session, stream, sequence).
    let seg = *Uuid::now_v7().as_bytes();
    let sess = *Uuid::now_v7().as_bytes();
    let dev = "it-dev";
    let stream = "it-stream";
    let body = b"the-muxed-segment-bytes".to_vec();

    // Happy path -> 200.
    assert_eq!(
        post(
            &client,
            &url,
            "test-token",
            manifest_bytes(seg, sess, dev, stream, 0, &body),
            body.clone()
        )
        .await,
        200,
        "happy path should be 200"
    );

    // Idempotent retry (same segment_id, same bytes) -> 200.
    assert_eq!(
        post(
            &client,
            &url,
            "test-token",
            manifest_bytes(seg, sess, dev, stream, 0, &body),
            body.clone()
        )
        .await,
        200,
        "idempotent retry should be 200"
    );

    // Same segment_id, DIFFERENT bytes -> 422.
    let other = b"completely-different-bytes".to_vec();
    assert_eq!(
        post(
            &client,
            &url,
            "test-token",
            manifest_bytes(seg, sess, dev, stream, 0, &other),
            other.clone()
        )
        .await,
        422,
        "segment_id reused for different bytes should be 422"
    );

    // Integrity mismatch: manifest describes `body`, but we send corrupted bytes -> 422.
    let seg2 = *Uuid::now_v7().as_bytes();
    assert_eq!(
        post(
            &client,
            &url,
            "test-token",
            manifest_bytes(seg2, sess, dev, stream, 1, &body),
            b"corrupted".to_vec()
        )
        .await,
        422,
        "body not matching content_sha256 should be 422"
    );

    // Bad token -> 401.
    let seg3 = *Uuid::now_v7().as_bytes();
    assert_eq!(
        post(
            &client,
            &url,
            "wrong-token",
            manifest_bytes(seg3, sess, dev, stream, 2, &body),
            body.clone()
        )
        .await,
        401,
        "bad token should be 401"
    );

    // Missing `body` part -> 400.
    let seg4 = *Uuid::now_v7().as_bytes();
    let form = Form::new().part(
        "manifest",
        Part::bytes(manifest_bytes(seg4, sess, dev, stream, 3, &body)).file_name("manifest"),
    );
    let status = client
        .post(&url)
        .bearer_auth("test-token")
        .multipart(form)
        .send()
        .await
        .expect("request")
        .status()
        .as_u16();
    assert_eq!(status, 400, "missing body part should be 400");

    // NEW segment_id reusing the happy-path's (session, stream, sequence=0) -> 409.
    let seg5 = *Uuid::now_v7().as_bytes();
    let collide = b"a-new-segment-for-an-existing-sequence-slot".to_vec();
    assert_eq!(
        post(
            &client,
            &url,
            "test-token",
            manifest_bytes(seg5, sess, dev, stream, 0, &collide),
            collide.clone()
        )
        .await,
        409,
        "reusing (session, stream, sequence) with a new segment_id should be 409"
    );
}
