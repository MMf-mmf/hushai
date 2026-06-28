//! Live-DB integration tests for the worker's durable claim/lease and idempotent
//! write. Gated on `DATABASE_URL` (skips cleanly when unset), like the backend's
//! own integration tests. Fixtures use unique per-test device ids with tiny
//! `capture_start` values so they sort oldest-first and never disturb real data.

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use hushai_worker::chunk::Sentence;
use hushai_worker::speaker_match::{self, SpeakerMatchConfig, SpeakerWrite};
use hushai_worker::vad::SpeakerQuality;
use hushai_worker::{claim, process};

/// A no-op match config for write paths that pass `speaker: None` (assign_speaker is never
/// reached, so the values are placeholders). Mirrors the WorkerConfig defaults.
fn test_match_cfg() -> SpeakerMatchConfig {
    SpeakerMatchConfig {
        match_threshold: 0.5,
        mint_distance_floor: 0.72,
        knn_k: 15,
        knn_neighbor_ceiling: 0.55,
        knn_min_neighbors: 3,
        knn_ef_search: 200,
        knn_statement_timeout_ms: 5000,
        centroid_window: 50,
    }
}

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
        // speakers has NO FK to segments, so the segment delete below won't cascade it.
        // Drop any speakers first-seen on this test device so stale centroids don't leak
        // into the next test's match-or-mint. (speaker_segments DOES cascade from segments.)
        "DELETE FROM speakers WHERE first_seen_device_id=$1",
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
            speaker_id: None,
            sentiment: None,
            emotion: None,
        },
        Sentence {
            text: "second sentence.".into(),
            start_unix_nanos: 20,
            end_unix_nanos: 30,
            speaker_id: None,
            sentiment: None,
            emotion: None,
        },
    ];
    let embeddings = vec![vec![0.1f32; 1024], vec![0.2f32; 1024]];

    process::write_transcript(
        &pool,
        seg,
        &device,
        &sentences,
        &embeddings,
        "test-model",
        None,
        &test_match_cfg(),
    )
    .await
    .unwrap();
    let c1 = count_sentences(&pool, seg).await;

    // Re-run with identical input: delete-then-insert must NOT duplicate.
    process::write_transcript(
        &pool,
        seg,
        &device,
        &sentences,
        &embeddings,
        "test-model",
        None,
        &test_match_cfg(),
    )
    .await
    .unwrap();
    let c2 = count_sentences(&pool, seg).await;

    assert_eq!(c1, 2, "first write inserts 2 sentences");
    assert_eq!(c2, 2, "re-write keeps exactly 2 (idempotent)");

    let stored_device: String = sqlx::query_scalar(
        "SELECT device_id FROM transcript_sentences WHERE segment_id=$1 LIMIT 1",
    )
    .bind(seg)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        stored_device, device,
        "device_id is denormalized onto each sentence"
    );

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

    process::write_transcript(
        &pool,
        seg,
        &device,
        &[],
        &[],
        "test-model",
        None,
        &test_match_cfg(),
    )
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

/// A unit-L2 192-d vector that is `eps` away (in dim `axis`) from a one-hot direction, so we
/// can synthesize "same voice" (tiny eps) vs "different voice" (orthogonal axis) embeddings
/// far from any real dense TitaNet centroid in the shared dev DB.
fn synth_embedding(axis: usize, eps: f32) -> Vec<f32> {
    let mut v = vec![0.0f32; 192];
    v[axis] = 1.0;
    v[(axis + 1) % 192] = eps;
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    for x in &mut v {
        *x /= norm;
    }
    v
}

async fn assign(
    pool: &PgPool,
    seg: Uuid,
    device: &str,
    emb: Vec<f32>,
    quality: SpeakerQuality,
    cfg: &SpeakerMatchConfig,
) -> Option<Uuid> {
    let sp = SpeakerWrite {
        embedding: emb,
        start_unix_nanos: 0,
        end_unix_nanos: 1,
        quality,
    };
    let mut tx = pool.begin().await.unwrap();
    let id = speaker_match::assign_speaker(&mut tx, seg, device, &sp, cfg)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    id
}

/// The core anti-duplicate behavior: clean near-duplicate embeddings collapse to ONE
/// identity; a clean far embedding mints a second; marginal (noisy) audio ATTACHES to a known
/// voice but a marginal FAR embedding is refused (NULL) rather than minting a duplicate.
/// Requires migration 0007 (the `quality` column + HNSW index). Gated on `DATABASE_URL`.
#[tokio::test]
async fn mint_guard_collapses_dupes_and_refuses_noise() {
    let Some(pool) = pool().await else {
        eprintln!("skipping mint_guard_collapses_dupes_and_refuses_noise: DATABASE_URL unset");
        return;
    };
    let device = format!("test-mintguard-{}", Uuid::now_v7());
    let cfg = test_match_cfg();
    let segs: Vec<Uuid> = {
        let mut v = Vec::new();
        for i in 0..5 {
            v.push(insert_fixture_segment(&pool, &device, i, 1 + i).await);
        }
        v
    };

    // 1. Clean, empty catalog -> mint S1.
    let s1 = assign(
        &pool,
        segs[0],
        &device,
        synth_embedding(0, 0.0),
        SpeakerQuality::Mint,
        &cfg,
    )
    .await
    .expect("clean first embedding mints");
    // 2. Clean near-duplicate of S1 (tiny perturbation) -> MATCH S1 (no new identity).
    let s2 = assign(
        &pool,
        segs[1],
        &device,
        synth_embedding(0, 0.02),
        SpeakerQuality::Mint,
        &cfg,
    )
    .await
    .expect("clean near-dup matches");
    assert_eq!(s1, s2, "a clean near-duplicate must NOT mint a new voice");
    // 3. Clean, orthogonal (far) -> mint a DISTINCT S3.
    let s3 = assign(
        &pool,
        segs[2],
        &device,
        synth_embedding(50, 0.0),
        SpeakerQuality::Mint,
        &cfg,
    )
    .await
    .expect("clean far embedding mints a second voice");
    assert_ne!(s1, s3, "a genuinely different clean voice mints its own id");
    // 4. MARGINAL near S3 -> ATTACH to S3 (no mint, no centroid update).
    let s4 = assign(
        &pool,
        segs[3],
        &device,
        synth_embedding(50, 0.02),
        SpeakerQuality::AttachOnly,
        &cfg,
    )
    .await
    .expect("marginal near-dup attaches");
    assert_eq!(s3, s4, "marginal audio attaches to the nearest known voice");
    // 5. MARGINAL far from everyone -> NULL (refuse), never a duplicate mint.
    let s5 = assign(
        &pool,
        segs[4],
        &device,
        synth_embedding(120, 0.0),
        SpeakerQuality::AttachOnly,
        &cfg,
    )
    .await;
    assert!(
        s5.is_none(),
        "marginal audio far from all voices must be refused (NULL), not minted"
    );

    // Exactly TWO identities were minted for this device (S1 and S3), not five.
    let minted: i64 =
        sqlx::query_scalar("SELECT count(*) FROM speakers WHERE first_seen_device_id = $1")
            .bind(&device)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        minted, 2,
        "5 segments of 2 real voices (+ noise) must yield exactly 2 ids"
    );

    cleanup(&pool, &device).await;
}

/// A no-voice segment gets a `quality='reject'` tombstone so the startup reconcile does NOT
/// re-queue it forever (convergence), yet a later, better run can still upgrade it: the
/// tombstone is ignored by `assign_speaker`'s idempotency early-return.
#[tokio::test]
async fn tombstone_prevents_requeue_but_is_upgradable() {
    let Some(pool) = pool().await else {
        eprintln!("skipping tombstone_prevents_requeue_but_is_upgradable: DATABASE_URL unset");
        return;
    };
    let device = format!("test-tomb-{}", Uuid::now_v7());
    let seg = insert_fixture_segment(&pool, &device, 0, i64::MIN + 10).await;

    // 1. A no-voice write (speaker = None) marks done AND writes a tombstone.
    let sentences = vec![Sentence {
        text: "barely audible.".into(),
        start_unix_nanos: 10,
        end_unix_nanos: 20,
        speaker_id: None,
        sentiment: None,
        emotion: None,
    }];
    let embeddings = vec![vec![0.1f32; 1024]];
    process::write_transcript(
        &pool,
        seg,
        &device,
        &sentences,
        &embeddings,
        "m",
        None,
        &test_match_cfg(),
    )
    .await
    .unwrap();

    let (spk, quality, emb_null): (Option<Uuid>, Option<String>, bool) = sqlx::query_as(
        "SELECT speaker_id, quality, embedding IS NULL FROM speaker_segments WHERE segment_id=$1",
    )
    .bind(seg)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        spk.is_none() && quality.as_deref() == Some("reject") && emb_null,
        "tombstone written"
    );

    // 2. The reconcile must NOT re-queue it (it has a speaker_segments row now): stays 'done'.
    claim::reconcile_missing_speaker_segments(&pool)
        .await
        .unwrap();
    let status: String =
        sqlx::query_scalar("SELECT status FROM segment_transcription_status WHERE segment_id=$1")
            .bind(seg)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        status, "done",
        "a tombstoned segment must not be re-queued by the reconcile"
    );

    // 3. A later run with a real (clean, far) embedding upgrades the tombstone -> mints.
    let id = assign(
        &pool,
        seg,
        &device,
        synth_embedding(77, 0.0),
        SpeakerQuality::Mint,
        &test_match_cfg(),
    )
    .await
    .expect("tombstone is ignored by the idempotency early-return; a real voiceprint mints");
    let (spk2, quality2): (Option<Uuid>, Option<String>) =
        sqlx::query_as("SELECT speaker_id, quality FROM speaker_segments WHERE segment_id=$1")
            .bind(seg)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(spk2, Some(id), "the row is upgraded to the minted speaker");
    assert_eq!(
        quality2.as_deref(),
        Some("clean"),
        "upgraded row is clean (feeds the centroid)"
    );

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
