//! Live-DB integration coverage for the conversation threader (migration 0025):
//! thread_pass end-to-end over a two-group interleaved timeline (the concurrent-groups
//! requirement), stable-id rerun, close lifecycle + `conversation` event emission, and
//! append-only late-attach into a closed conversation.
//!
//! Gated on `DATABASE_URL` (skips cleanly when unset), same idiom as tests/retro_attach.rs:
//! everything namespaced to a unique device, cleaned up at the end. NB: thread_pass is a
//! GLOBAL watermark pass — on a shared test DB it may also (correctly) thread other
//! devices' rows; assertions here only inspect this test's device.

use hushai_backend::conversations::{thread_pass, ThreaderOpts};
use pgvector::Vector;
use sqlx::{PgPool, Row};
use uuid::Uuid;

const SEC: i64 = 1_000_000_000;

fn unit_1024(hot: usize) -> Vector {
    let mut v = vec![0f32; 1024];
    v[hot] = 1.0;
    Vector::from(v)
}

fn now_ns() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as i64
}

async fn connect() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("connect");
    sqlx::migrate!("./migrations").run(&pool).await.expect("migrate");
    Some(pool)
}

async fn seed_device(pool: &PgPool, device: &str, session: Uuid) {
    sqlx::query("INSERT INTO devices (device_id, source_kind) VALUES ($1,'test') ON CONFLICT DO NOTHING")
        .bind(device)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sessions (session_id, device_id) VALUES ($1,$2)")
        .bind(session)
        .bind(device)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO streams (session_id, stream_id, device_id, media_type, codec, container) VALUES ($1,'cam0-audio',$2,1,'aac','mp4')")
        .bind(session).bind(device).execute(pool).await.unwrap();
}

/// One segment + transcript sentence at absolute capture nanos with a topic embedding.
#[allow(clippy::too_many_arguments)]
async fn insert_sentence(
    pool: &PgPool,
    device: &str,
    session: Uuid,
    sequence: i64,
    start_ns: i64,
    end_ns: i64,
    speaker: Option<Uuid>,
    topic_hot: usize,
) -> i64 {
    let segment_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO segments (segment_id, device_id, stream_id, session_id, sequence, \
            media_type, codec, container, capture_start_unix_nanos, monotonic_start_nanos, \
            duration_nanos, content_sha256, byte_len, blob_uri, storage_backend) \
         VALUES ($1,$2,'cam0-audio',$3,$4,1,'aac','mp4',$5,0,2000000000,$6,100,$7,'file')",
    )
    .bind(segment_id)
    .bind(device)
    .bind(session)
    .bind(sequence)
    .bind(start_ns)
    .bind(vec![0u8; 32])
    .bind(format!("file:///nonexistent/{segment_id}"))
    .execute(pool)
    .await
    .unwrap();
    let row = sqlx::query(
        "INSERT INTO transcript_sentences \
            (segment_id, device_id, text, start_unix_nanos, end_unix_nanos, embedding, \
             embedding_model, embedding_dim, speaker_id) \
         VALUES ($1,$2,$3,$4,$5,$6,'test',1024,$7) RETURNING id",
    )
    .bind(segment_id)
    .bind(device)
    .bind(format!("utterance {sequence}"))
    .bind(start_ns)
    .bind(end_ns)
    .bind(unit_1024(topic_hot))
    .bind(speaker.map(|s| s.to_string()))
    .fetch_one(pool)
    .await
    .unwrap();
    row.get::<i64, _>("id")
}

async fn cleanup(pool: &PgPool, device: &str) {
    sqlx::query("DELETE FROM conversations WHERE primary_device_id = $1")
        .bind(device)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM events WHERE device_id = $1")
        .bind(device)
        .execute(pool)
        .await
        .unwrap();
    // segments cascade transcript_sentences.
    sqlx::query("DELETE FROM segments WHERE device_id = $1")
        .bind(device)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM streams WHERE device_id = $1")
        .bind(device)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM sessions WHERE device_id = $1")
        .bind(device)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM devices WHERE device_id = $1")
        .bind(device)
        .execute(pool)
        .await
        .unwrap();
}

fn test_opts() -> ThreaderOpts {
    ThreaderOpts {
        min_age_secs: 0, // rows inserted milliseconds ago must be threadable in-test
        ..ThreaderOpts::default()
    }
}

#[tokio::test]
async fn thread_pass_end_to_end() {
    let Some(pool) = connect().await else {
        eprintln!("DATABASE_URL unset; skipping");
        return;
    };
    let device = format!("threading-test-{}", &Uuid::now_v7().to_string()[..8]);
    let session = Uuid::now_v7();
    seed_device(&pool, &device, session).await;

    // Two concurrent group conversations, interleaved on ONE device inside one gap
    // block: group A = speakers 1,2 on topic 0; group B = speakers 3,4 on topic 7.
    // Timeline ends ~350s in the past so a zero-grace close pass can close it later.
    let (s1, s2, s3, s4) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let base = now_ns() - 450 * SEC;
    let mut ids_a: Vec<i64> = Vec::new();
    let mut ids_b: Vec<i64> = Vec::new();
    let mut seq = 0i64;
    for round in 0..4 {
        let t0 = base + round * 24 * SEC;
        for (off, spk, topic, bucket) in [
            (0, s1, 0usize, 'a'),
            (5, s2, 0, 'a'),
            (12, s3, 7, 'b'),
            (17, s4, 7, 'b'),
        ] {
            let start = t0 + off * SEC;
            let id = insert_sentence(
                &pool, &device, session, seq, start, start + 4 * SEC, Some(spk), topic,
            )
            .await;
            if bucket == 'a' {
                ids_a.push(id);
            } else {
                ids_b.push(id);
            }
            seq += 1;
        }
    }

    let opts = test_opts();
    // NB: no assertion on stats.rows_assigned — tests run in parallel and share the
    // GLOBAL watermark, so a concurrent test's pass may have already threaded some of
    // these rows (diff-only updates then skip them). State assertions below are the
    // contract.
    thread_pass(&pool, &opts).await.expect("thread_pass");

    let fetch_cids = |ids: Vec<i64>| {
        let pool = pool.clone();
        async move {
            let rows = sqlx::query(
                "SELECT DISTINCT conversation_id FROM transcript_sentences WHERE id = ANY($1)",
            )
            .bind(&ids)
            .fetch_all(&pool)
            .await
            .unwrap();
            rows.iter()
                .map(|r| r.get::<Option<Uuid>, _>("conversation_id"))
                .collect::<Vec<_>>()
        }
    };
    let cids_a = fetch_cids(ids_a.clone()).await;
    let cids_b = fetch_cids(ids_b.clone()).await;
    assert_eq!(cids_a.len(), 1, "group A must be one conversation: {cids_a:?}");
    assert_eq!(cids_b.len(), 1, "group B must be one conversation: {cids_b:?}");
    let (ca, cb) = (cids_a[0].expect("assigned"), cids_b[0].expect("assigned"));
    assert_ne!(ca, cb, "concurrent groups must not merge");

    // Catalog rows exist, open, correct participants.
    let convo = sqlx::query(
        "SELECT status, speaker_ids, sentence_count FROM conversations WHERE conversation_id = $1",
    )
    .bind(ca)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(convo.get::<String, _>("status"), "open");
    let mut sp: Vec<Uuid> = convo.get("speaker_ids");
    sp.sort();
    let mut expect = vec![s1, s2];
    expect.sort();
    assert_eq!(sp, expect);
    assert_eq!(convo.get::<i32, _>("sentence_count"), 8);

    // Rerun: stable ids, zero churn.
    let stats2 = thread_pass(&pool, &opts).await.expect("rerun");
    assert_eq!(stats2.rows_assigned, 0, "identical rerun must be a no-op diff");
    assert_eq!(stats2.convos_minted, 0);
    let cids_a2 = fetch_cids(ids_a.clone()).await;
    assert_eq!(cids_a2[0], Some(ca), "rerun must not churn ids");

    // Close: zero grace → cutoff now-300s; the timeline ended ~350s ago → both close,
    // each emitting ONE `conversation` event (idempotent dedup_key).
    let close_opts = ThreaderOpts {
        close_grace_secs: 0,
        ..test_opts()
    };
    let stats3 = thread_pass(&pool, &close_opts).await.expect("close pass");
    assert!(stats3.convos_closed >= 2, "closed {}", stats3.convos_closed);
    let status: String =
        sqlx::query("SELECT status FROM conversations WHERE conversation_id = $1")
            .bind(ca)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("status");
    assert_eq!(status, "closed");
    let ev_count: i64 = sqlx::query(
        "SELECT count(*) AS n FROM events WHERE event_type = 'conversation' AND dedup_key = $1",
    )
    .bind(format!("convo:{ca}"))
    .fetch_one(&pool)
    .await
    .unwrap()
    .get("n");
    assert_eq!(ev_count, 1, "exactly one conversation event per close");

    // Late-attach: a reprocessed sentence whose capture time falls inside the CLOSED
    // span (and outside the lookback window) appends without reopening.
    let late_opts = ThreaderOpts {
        lookback_secs: 60, // base (~450s ago) is far outside → late-attach path
        ..test_opts()
    };
    let late_id = insert_sentence(
        &pool,
        &device,
        session,
        999,
        base + 6 * SEC,
        base + 8 * SEC,
        Some(s1),
        0,
    )
    .await;
    let stats4 = thread_pass(&pool, &late_opts).await.expect("late attach pass");
    assert!(stats4.late_attached >= 1, "late_attached {}", stats4.late_attached);
    let got: Option<Uuid> =
        sqlx::query("SELECT conversation_id FROM transcript_sentences WHERE id = $1")
            .bind(late_id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("conversation_id");
    assert_eq!(got, Some(ca), "late row must join the closed conversation");
    let (status2, count2): (String, i32) = {
        let r = sqlx::query(
            "SELECT status, sentence_count FROM conversations WHERE conversation_id = $1",
        )
        .bind(ca)
        .fetch_one(&pool)
        .await
        .unwrap();
        (r.get("status"), r.get("sentence_count"))
    };
    assert_eq!(status2, "closed", "late-attach must not reopen");
    assert_eq!(count2, 9);

    cleanup(&pool, &device).await;
}

/// Regression: a backlog upload arriving across MULTIPLE threader passes (capture times
/// pinned in the past) must stay ONE conversation. Before the close-pass `updated_at`
/// guard, the first pass's conversation closed instantly (capture-age rule) and every
/// later batch minted a new fragment — the probe's "conversation ×4" bug.
#[tokio::test]
async fn multi_pass_backlog_stays_one_conversation() {
    let Some(pool) = connect().await else {
        eprintln!("DATABASE_URL unset; skipping");
        return;
    };
    let device = format!("threading-frag-{}", &Uuid::now_v7().to_string()[..8]);
    let session = Uuid::now_v7();
    seed_device(&pool, &device, session).await;

    let spk = Uuid::now_v7();
    let base = now_ns() - 30 * 86_400 * SEC; // capture a month ago (eval-fixture reality)
    let opts = test_opts(); // default close grace 120s wall — far longer than this test

    // Batch 1: first half of a continuous monologue.
    let mut ids: Vec<i64> = Vec::new();
    for i in 0..4 {
        ids.push(
            insert_sentence(
                &pool, &device, session, i, base + i * 4 * SEC, base + (i * 4 + 3) * SEC,
                Some(spk), 0,
            )
            .await,
        );
    }
    thread_pass(&pool, &opts).await.expect("pass 1");

    // Batch 2: the SAME conversation continues (4s after batch 1 ends).
    for i in 4..8 {
        ids.push(
            insert_sentence(
                &pool, &device, session, i, base + i * 4 * SEC, base + (i * 4 + 3) * SEC,
                Some(spk), 0,
            )
            .await,
        );
    }
    let stats2 = thread_pass(&pool, &opts).await.expect("pass 2");
    assert_eq!(stats2.convos_minted, 0, "batch 2 must extend, not mint a fragment");

    let distinct: Vec<Uuid> = sqlx::query(
        "SELECT DISTINCT conversation_id FROM transcript_sentences WHERE id = ANY($1) AND conversation_id IS NOT NULL",
    )
    .bind(&ids)
    .fetch_all(&pool)
    .await
    .unwrap()
    .iter()
    .map(|r| r.get::<Uuid, _>("conversation_id"))
    .collect();
    assert_eq!(distinct.len(), 1, "one continuous backlog = one conversation: {distinct:?}");
    let status: String =
        sqlx::query("SELECT status FROM conversations WHERE conversation_id = $1")
            .bind(distinct[0])
            .fetch_one(&pool)
            .await
            .expect("missing convo row")
            .get("status");
    assert_eq!(status, "open", "still growing → must not close inside the wall grace");

    cleanup(&pool, &device).await;
}
