//! Live-DB tests for the reflection analytics digest. Gated on `DATABASE_URL`.
//!
//! Exercises the novel pieces: the gap-grouped conversation derivation, talk-balance over
//! target-vs-others speaking time, the segment-grain sentiment collapse, the social graph,
//! and the graceful decline gate. A unique device + speaker uuids isolate from real data.

use pgvector::Vector;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use hushai_rag::analytics::{self, AnalysisWindow, DigestConfig};

const DAY_NS: i64 = 86_400 * 1_000_000_000;
const SEC_NS: i64 = 1_000_000_000;
const BASE: i64 = 1_700_000_000 * 1_000_000_000; // a fixed past instant (created_at is now()).

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .ok()
}

fn test_cfg() -> DigestConfig {
    DigestConfig {
        gap_threshold_nanos: 300 * SEC_NS,
        top_interlocutors: 5,
        tz_offset_secs: 0,
        min_segments: 1, // small so a modest fixture passes the gate
        max_weeks: 13,
        statement_timeout_ms: 10_000,
        excerpt_max_chars: 200,
    }
}

async fn setup_device(pool: &PgPool, device: &str) {
    let session_id = Uuid::now_v7();
    sqlx::query("INSERT INTO devices (device_id, source_kind) VALUES ($1,'test') ON CONFLICT (device_id) DO NOTHING")
        .bind(device).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO sessions (session_id, device_id) VALUES ($1,$2)")
        .bind(session_id)
        .bind(device)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO streams (session_id, stream_id, device_id, media_type, codec, container) VALUES ($1,'s0',$2,3,'h264+aac','fmp4')")
        .bind(session_id).bind(device).execute(pool).await.unwrap();
    // Stash the session id on the device row's attrs so seg inserts can reuse it.
    sqlx::query("UPDATE devices SET attrs = jsonb_build_object('sid',$2::text) WHERE device_id=$1")
        .bind(device)
        .bind(session_id.to_string())
        .execute(pool)
        .await
        .unwrap();
}

async fn session_of(pool: &PgPool, device: &str) -> Uuid {
    let row: (String,) = sqlx::query_as("SELECT attrs->>'sid' FROM devices WHERE device_id=$1")
        .bind(device)
        .fetch_one(pool)
        .await
        .unwrap();
    Uuid::parse_str(&row.0).unwrap()
}

#[allow(clippy::too_many_arguments)]
async fn insert_seg(
    pool: &PgPool,
    device: &str,
    seq: i64,
    speaker: Option<&str>,
    sentiment: Option<&str>,
    start: i64,
    dur: i64,
    text: &str,
) -> Uuid {
    let session_id = session_of(pool, device).await;
    let segment_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO segments (segment_id, device_id, stream_id, session_id, sequence, media_type, codec, container, capture_start_unix_nanos, monotonic_start_nanos, duration_nanos, content_sha256, byte_len, blob_uri, storage_backend) \
         VALUES ($1,$2,'s0',$3,$4,3,'h264+aac','fmp4',$5,0,$6,$7,100,$8,'file')",
    )
    .bind(segment_id).bind(device).bind(session_id).bind(seq).bind(start).bind(dur)
    .bind(vec![(seq & 0xff) as u8; 32]).bind(format!("file:///nonexistent/{segment_id}"))
    .execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO transcript_sentences (segment_id, device_id, text, start_unix_nanos, end_unix_nanos, sentiment, speaker_id, embedding, embedding_model, embedding_dim) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'test',1024)",
    )
    .bind(segment_id).bind(device).bind(text).bind(start).bind(start + dur)
    .bind(sentiment).bind(speaker)
    .bind(Vector::from(vec![0f32; 1024]))
    .execute(pool).await.unwrap();
    segment_id
}

async fn insert_speaker(pool: &PgPool, id: Uuid, name: &str) {
    sqlx::query(
        "INSERT INTO speakers (speaker_id, centroid, n_samples, display_name) VALUES ($1,$2,1,$3) \
         ON CONFLICT (speaker_id) DO UPDATE SET display_name = EXCLUDED.display_name",
    )
    .bind(id)
    .bind(Vector::from(vec![0f32; 192]))
    .bind(name)
    .execute(pool)
    .await
    .unwrap();
}

async fn cleanup(pool: &PgPool, device: &str, speakers: &[Uuid]) {
    for sql in [
        "DELETE FROM transcript_sentences WHERE device_id=$1",
        "DELETE FROM segment_transcription_status WHERE segment_id IN (SELECT segment_id FROM segments WHERE device_id=$1)",
        "DELETE FROM segments WHERE device_id=$1",
        "DELETE FROM streams WHERE device_id=$1",
        "DELETE FROM sessions WHERE device_id=$1",
        "DELETE FROM devices WHERE device_id=$1",
    ] {
        let _ = sqlx::query(sql).bind(device).execute(pool).await;
    }
    for s in speakers {
        let _ = sqlx::query("DELETE FROM speakers WHERE speaker_id=$1")
            .bind(s)
            .execute(pool)
            .await;
    }
}

#[tokio::test]
async fn digest_computes_talk_balance_sentiment_and_social_graph() {
    let Some(pool) = pool().await else {
        eprintln!("skipping digest test: DATABASE_URL unset");
        return;
    };
    let device = format!("test-refl-{}", Uuid::now_v7());
    let me = Uuid::now_v7();
    let alice = Uuid::now_v7();
    let me_s = me.to_string();
    let alice_s = alice.to_string();

    setup_device(&pool, &device).await;
    insert_speaker(&pool, me, "Me").await;
    insert_speaker(&pool, alice, "Alice").await;

    // Day 1 conversation: Me(4s,pos), Alice(2s,neutral), Me(3s,neg) — 1s apart -> one convo.
    insert_seg(
        &pool,
        &device,
        0,
        Some(&me_s),
        Some("positive"),
        BASE,
        4 * SEC_NS,
        "good morning everyone",
    )
    .await;
    insert_seg(
        &pool,
        &device,
        1,
        Some(&alice_s),
        Some("neutral"),
        BASE + 5 * SEC_NS,
        2 * SEC_NS,
        "okay sounds fine",
    )
    .await;
    insert_seg(
        &pool,
        &device,
        2,
        Some(&me_s),
        Some("negative"),
        BASE + 8 * SEC_NS,
        3 * SEC_NS,
        "this is frustrating honestly",
    )
    .await;
    // Day 2: Me alone (2s, positive) — a solo conversation (no interlocutor).
    insert_seg(
        &pool,
        &device,
        3,
        Some(&me_s),
        Some("positive"),
        BASE + DAY_NS,
        2 * SEC_NS,
        "feeling good today",
    )
    .await;
    // Day 3: Me(5s, no sentiment), Alice(3s, no sentiment) — one convo with Alice again.
    insert_seg(
        &pool,
        &device,
        4,
        Some(&me_s),
        None,
        BASE + 2 * DAY_NS,
        5 * SEC_NS,
        "let me explain the plan in detail",
    )
    .await;
    insert_seg(
        &pool,
        &device,
        5,
        Some(&alice_s),
        None,
        BASE + 2 * DAY_NS + 6 * SEC_NS,
        3 * SEC_NS,
        "got it thanks",
    )
    .await;

    let window = AnalysisWindow {
        after_unix_nanos: BASE - DAY_NS,
        before_unix_nanos: BASE + 4 * DAY_NS,
    };
    let target = vec![me_s.clone()];
    let digest = analytics::compute_digest(&pool, &target, window, &test_cfg(), None)
        .await
        .expect("compute_digest");

    assert!(digest.enough_data, "fixture should pass the data gate");
    assert_eq!(digest.target_label, "Me");
    assert_eq!(digest.coverage.active_days, 3, "3 distinct UTC days");
    assert_eq!(digest.coverage.segments, 4, "Me speaks in 4 segments");

    // Three gap-separated conversations (one per day).
    assert_eq!(digest.talk.conversations, 3);
    assert!(digest.talk.talk_ratio > 0.0 && digest.talk.talk_ratio <= 1.0);

    // Own sentiment at segment grain: 2 positive, 1 negative, 1 unclassified.
    assert_eq!(digest.mood.own.pos, 2);
    assert_eq!(digest.mood.own.neg, 1);
    assert_eq!(digest.mood.own.neu, 0);
    assert_eq!(digest.mood.own.n_null, 1);

    // Others' sentiment within Me's conversations: Alice neutral once (day 3 is unclassified).
    assert_eq!(digest.mood.others_in_your_convos.neu, 1);

    // Social graph: Alice shares the day-1 and day-3 conversations (not the solo day-2).
    let alice_entry = digest
        .interlocutors
        .iter()
        .find(|i| i.name.as_deref() == Some("Alice"));
    let alice_entry = alice_entry.expect("Alice should be a top interlocutor");
    assert_eq!(alice_entry.shared_convos, 2);

    // render_digest should narrate the headline facts without panicking.
    let text = analytics::render_digest(&digest);
    assert!(text.contains("LIFE DIGEST for Me"));
    assert!(text.contains("TALK BALANCE"));
    assert!(text.contains("Alice"));
    assert!(text.contains("productivity")); // the LIMITS block

    cleanup(&pool, &device, &[me, alice]).await;
}

#[tokio::test]
async fn digest_declines_when_sparse() {
    let Some(pool) = pool().await else {
        eprintln!("skipping sparse test: DATABASE_URL unset");
        return;
    };
    let device = format!("test-refl-sparse-{}", Uuid::now_v7());
    let me = Uuid::now_v7();
    let me_s = me.to_string();
    setup_device(&pool, &device).await;
    insert_speaker(&pool, me, "Me").await;

    // Only one segment on one day -> active_days < 3 -> decline.
    insert_seg(
        &pool,
        &device,
        0,
        Some(&me_s),
        Some("positive"),
        BASE,
        2 * SEC_NS,
        "hello there",
    )
    .await;

    let window = AnalysisWindow {
        after_unix_nanos: BASE - DAY_NS,
        before_unix_nanos: BASE + 4 * DAY_NS,
    };
    let digest = analytics::compute_digest(&pool, &[me_s.clone()], window, &test_cfg(), None)
        .await
        .expect("compute_digest");

    assert!(!digest.enough_data);
    let text = analytics::render_digest(&digest);
    assert!(text.contains("NOT ENOUGH DATA"));

    cleanup(&pool, &device, &[me]).await;
}

#[tokio::test]
async fn empty_target_declines_gracefully() {
    let Some(pool) = pool().await else {
        eprintln!("skipping empty-target test: DATABASE_URL unset");
        return;
    };
    // No speaker ids -> matches nothing -> decline (never analyzes a stranger).
    let window = AnalysisWindow {
        after_unix_nanos: BASE - DAY_NS,
        before_unix_nanos: BASE + 4 * DAY_NS,
    };
    let digest = analytics::compute_digest(&pool, &[], window, &test_cfg(), None)
        .await
        .expect("compute_digest");
    assert!(!digest.enough_data);
    assert_eq!(digest.coverage.segments, 0);
}
