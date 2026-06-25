//! Live-DB integration tests for the worker's durable claim/lease and idempotent
//! write. Gated on `DATABASE_URL` (skips cleanly when unset), like the backend's
//! own integration tests. Fixtures use unique per-test device ids with tiny
//! `capture_start` values so they sort oldest-first and never disturb real data.

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use hushai_worker::chunk::Sentence;
use hushai_worker::{claim, process};

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .ok()
}

/// Insert a minimal device/session/stream/segment + pending status row.
/// `capture_start` is the explicit oldest-first sort key — pass a tiny/sentinel value
/// so the fixture sorts ahead of real data (and, for claim-ordering tests, ahead of
/// other concurrently-running tests' fixtures).
async fn insert_fixture_segment(
    pool: &PgPool,
    device_id: &str,
    sequence: i64,
    capture_start: i64,
) -> Uuid {
    let session_id = Uuid::now_v7();
    let segment_id = Uuid::now_v7();
    let stream_id = "s0";

    sqlx::query(
        "INSERT INTO devices (device_id, source_kind) VALUES ($1,'test') \
         ON CONFLICT (device_id) DO NOTHING",
    )
    .bind(device_id)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query("INSERT INTO sessions (session_id, device_id) VALUES ($1,$2)")
        .bind(session_id)
        .bind(device_id)
        .execute(pool)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO streams (session_id, stream_id, device_id, media_type, codec, container) \
         VALUES ($1,$2,$3,3,'h264+aac','fmp4')",
    )
    .bind(session_id)
    .bind(stream_id)
    .bind(device_id)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO segments (segment_id, device_id, stream_id, session_id, sequence, \
            media_type, codec, container, capture_start_unix_nanos, monotonic_start_nanos, \
            duration_nanos, content_sha256, byte_len, blob_uri, storage_backend) \
         VALUES ($1,$2,$3,$4,$5,3,'h264+aac','fmp4',$6,0,2000000000,$7,100,$8,'file')",
    )
    .bind(segment_id)
    .bind(device_id)
    .bind(stream_id)
    .bind(session_id)
    .bind(sequence)
    .bind(capture_start)
    .bind(vec![0u8; 32])
    .bind(format!("file:///nonexistent/{segment_id}"))
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO segment_transcription_status (segment_id) VALUES ($1) ON CONFLICT DO NOTHING",
    )
    .bind(segment_id)
    .execute(pool)
    .await
    .unwrap();

    segment_id
}

async fn cleanup(pool: &PgPool, device_id: &str) {
    for sql in [
        "DELETE FROM transcript_sentences WHERE segment_id IN (SELECT segment_id FROM segments WHERE device_id=$1)",
        "DELETE FROM segment_transcription_status WHERE segment_id IN (SELECT segment_id FROM segments WHERE device_id=$1)",
        "DELETE FROM segments WHERE device_id=$1",
        "DELETE FROM streams WHERE device_id=$1",
        "DELETE FROM sessions WHERE device_id=$1",
        "DELETE FROM devices WHERE device_id=$1",
    ] {
        let _ = sqlx::query(sql).bind(device_id).execute(pool).await;
    }
}

async fn count_sentences(pool: &PgPool, seg: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM transcript_sentences WHERE segment_id=$1")
        .bind(seg)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn write_transcript_is_idempotent() {
    let Some(pool) = pool().await else {
        eprintln!("skipping write_transcript_is_idempotent: DATABASE_URL unset");
        return;
    };
    let device = format!("test-idem-{}", Uuid::now_v7());
    let seg = insert_fixture_segment(&pool, &device, 0, 1).await;

    let sentences = vec![
        Sentence {
            text: "first sentence.".into(),
            start_unix_nanos: 10,
            end_unix_nanos: 20,
        },
        Sentence {
            text: "second sentence.".into(),
            start_unix_nanos: 20,
            end_unix_nanos: 30,
        },
    ];
    let embeddings = vec![vec![0.1f32; 1024], vec![0.2f32; 1024]];

    process::write_transcript(&pool, seg, &device, &sentences, &embeddings, "test-model")
        .await
        .unwrap();
    let c1 = count_sentences(&pool, seg).await;

    // Re-run with identical input: delete-then-insert must NOT duplicate.
    process::write_transcript(&pool, seg, &device, &sentences, &embeddings, "test-model")
        .await
        .unwrap();
    let c2 = count_sentences(&pool, seg).await;

    assert_eq!(c1, 2, "first write inserts 2 sentences");
    assert_eq!(c2, 2, "re-write keeps exactly 2 (idempotent)");

    let stored_device: String =
        sqlx::query_scalar("SELECT device_id FROM transcript_sentences WHERE segment_id=$1 LIMIT 1")
            .bind(seg)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored_device, device, "device_id is denormalized onto each sentence");

    let status: String =
        sqlx::query_scalar("SELECT status FROM segment_transcription_status WHERE segment_id=$1")
            .bind(seg)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "done");

    cleanup(&pool, &device).await;
}

#[tokio::test]
async fn no_speech_writes_zero_and_marks_done() {
    let Some(pool) = pool().await else {
        eprintln!("skipping no_speech_writes_zero_and_marks_done: DATABASE_URL unset");
        return;
    };
    let device = format!("test-nospeech-{}", Uuid::now_v7());
    let seg = insert_fixture_segment(&pool, &device, 0, 1).await;

    process::write_transcript(&pool, seg, &device, &[], &[], "test-model")
        .await
        .unwrap();

    assert_eq!(count_sentences(&pool, seg).await, 0);
    let status: String =
        sqlx::query_scalar("SELECT status FROM segment_transcription_status WHERE segment_id=$1")
            .bind(seg)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "done", "a no-speech segment is still marked done");

    cleanup(&pool, &device).await;
}

#[tokio::test]
async fn claim_one_never_double_claims() {
    let Some(pool) = pool().await else {
        eprintln!("skipping claim_one_never_double_claims: DATABASE_URL unset");
        return;
    };
    let device = format!("test-claim-{}", Uuid::now_v7());
    // Sentinel capture_start (the oldest possible i64) guarantees these two are the
    // globally-oldest claimable rows, so the claim is deterministic even when other
    // tests' fixtures (or stale data) are pending concurrently.
    let s1 = insert_fixture_segment(&pool, &device, 0, i64::MIN + 1).await;
    let s2 = insert_fixture_segment(&pool, &device, 1, i64::MIN + 2).await;

    // Two concurrent claimers must pick two DISTINCT rows (FOR UPDATE SKIP LOCKED).
    let (a, b) = tokio::join!(
        claim::claim_one(&pool, 5, 300.0),
        claim::claim_one(&pool, 5, 300.0),
    );
    let a = a.unwrap();
    let b = b.unwrap();

    assert!(
        a.is_some() && b.is_some(),
        "both claims should succeed (2 oldest pending rows are our fixtures)"
    );
    assert_ne!(a, b, "the same segment must never be claimed twice");

    // Our fixtures have the smallest capture_start, so they're claimed first.
    let mut got = [a.unwrap(), b.unwrap()];
    got.sort();
    let mut want = [s1, s2];
    want.sort();
    assert_eq!(got, want, "the two oldest claimable rows are our fixtures");

    cleanup(&pool, &device).await;
}
