//! Live-DB tests for person (face) attribution retrieval. Gated on `DATABASE_URL`.
//! Seeds `persons` + `person_segments` under a unique device and verifies:
//!   * `list_by_person` returns one sighting per (person, segment) — deduped across frames — in
//!     time order ("when did I see Bob");
//!   * `list_co_occurring_persons` returns the co-present non-owner person and excludes the owner
//!     ("who was I with").

use pgvector::Vector;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use hushai_rag::retrieve;

fn unit_vec(hot: usize) -> Vector {
    let mut v = vec![0f32; 512];
    v[hot] = 1.0;
    Vector::from(v)
}

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .ok()
}

async fn insert_dss(pool: &PgPool, device_id: &str) -> Uuid {
    let session_id = Uuid::now_v7();
    sqlx::query("INSERT INTO devices (device_id, source_kind) VALUES ($1,'test') ON CONFLICT (device_id) DO NOTHING")
        .bind(device_id).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO sessions (session_id, device_id) VALUES ($1,$2)")
        .bind(session_id)
        .bind(device_id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO streams (session_id, stream_id, device_id, media_type, codec, container) VALUES ($1,'cam0-video',$2,2,'h264','fmp4')")
        .bind(session_id).bind(device_id).execute(pool).await.unwrap();
    session_id
}

async fn insert_segment(pool: &PgPool, device_id: &str, session_id: Uuid, sequence: i64) -> Uuid {
    let segment_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO segments (segment_id, device_id, stream_id, session_id, sequence, media_type, codec, container, capture_start_unix_nanos, monotonic_start_nanos, duration_nanos, content_sha256, byte_len, blob_uri, storage_backend) \
         VALUES ($1,$2,'cam0-video',$3,$4,2,'h264','fmp4',$4,0,2000000000,$5,100,$6,'file')",
    )
    .bind(segment_id).bind(device_id).bind(session_id).bind(sequence).bind(vec![0u8;32]).bind(format!("file:///nonexistent/{segment_id}"))
    .execute(pool).await.unwrap();
    segment_id
}

async fn insert_person(pool: &PgPool, device_id: &str, name: Option<&str>, hot: usize) -> Uuid {
    let person_id = Uuid::now_v7();
    sqlx::query("INSERT INTO persons (person_id, centroid, n_samples, display_name, first_seen_device_id) VALUES ($1,$2,1,$3,$4)")
        .bind(person_id).bind(unit_vec(hot)).bind(name).bind(device_id)
        .execute(pool).await.unwrap();
    person_id
}

async fn insert_person_segment(
    pool: &PgPool,
    segment_id: Uuid,
    device_id: &str,
    person_id: Uuid,
    start: i64,
    hot: usize,
) {
    sqlx::query(
        "INSERT INTO person_segments (segment_id, device_id, person_id, start_unix_nanos, end_unix_nanos, frame_offset_nanos, embedding, bbox, det_score, quality) \
         VALUES ($1,$2,$3,$4,$4,0,$5,$6::jsonb,0.9,'clean')",
    )
    .bind(segment_id).bind(device_id).bind(person_id).bind(start).bind(unit_vec(hot)).bind("[1,2,3,4]")
    .execute(pool).await.unwrap();
}

async fn cleanup(pool: &PgPool, device_id: &str) {
    for sql in [
        "DELETE FROM person_segments WHERE device_id=$1",
        "DELETE FROM persons WHERE first_seen_device_id=$1",
        "DELETE FROM segment_vision_status WHERE segment_id IN (SELECT segment_id FROM segments WHERE device_id=$1)",
        "DELETE FROM segments WHERE device_id=$1",
        "DELETE FROM streams WHERE device_id=$1",
        "DELETE FROM sessions WHERE device_id=$1",
        "DELETE FROM devices WHERE device_id=$1",
    ] {
        let _ = sqlx::query(sql).bind(device_id).execute(pool).await;
    }
}

#[tokio::test]
async fn list_by_person_dedups_per_segment_and_orders_by_time() {
    let Some(pool) = pool().await else {
        eprintln!(
            "skipping list_by_person_dedups_per_segment_and_orders_by_time: DATABASE_URL unset"
        );
        return;
    };
    let device = format!("test-pe-{}", Uuid::now_v7());
    let session = insert_dss(&pool, &device).await;
    let bob = insert_person(&pool, &device, Some("Bob"), 0).await;

    // Segment A: two frame rows (same ~2s appearance) -> ONE sighting after dedup.
    let seg_a = insert_segment(&pool, &device, session, 0).await;
    insert_person_segment(&pool, seg_a, &device, bob, 1_000, 0).await;
    insert_person_segment(&pool, seg_a, &device, bob, 1_500, 0).await;
    // Segment B: a later appearance -> a second sighting.
    let seg_b = insert_segment(&pool, &device, session, 1).await;
    insert_person_segment(&pool, seg_b, &device, bob, 9_000, 0).await;

    let sightings =
        retrieve::list_by_person(&pool, &[bob.to_string()], Some(&device), None, None, 100)
            .await
            .unwrap();
    assert_eq!(
        sightings.len(),
        2,
        "two frames in one segment collapse to one sighting"
    );
    assert!(
        sightings[0].start_unix_nanos <= sightings[1].start_unix_nanos,
        "time-ordered"
    );
    assert_eq!(
        sightings[0].speaker_id.as_deref(),
        Some(bob.to_string().as_str())
    );
    assert_eq!(sightings[0].text, "(seen on camera)");

    cleanup(&pool, &device).await;
}

#[tokio::test]
async fn co_occurring_persons_finds_co_present_excludes_owner() {
    let Some(pool) = pool().await else {
        eprintln!(
            "skipping co_occurring_persons_finds_co_present_excludes_owner: DATABASE_URL unset"
        );
        return;
    };
    let device = format!("test-pe-{}", Uuid::now_v7());
    let session = insert_dss(&pool, &device).await;
    let owner = insert_person(&pool, &device, Some("Me"), 0).await;
    let friend = insert_person(&pool, &device, Some("Friend"), 1).await;
    let stranger = insert_person(&pool, &device, Some("Stranger"), 2).await;

    // Shared segment S: owner + friend co-present.
    let seg_s = insert_segment(&pool, &device, session, 0).await;
    insert_person_segment(&pool, seg_s, &device, owner, 1_000, 0).await;
    insert_person_segment(&pool, seg_s, &device, friend, 1_000, 1).await;
    // Different segment T: only stranger (owner NOT present) -> must not appear.
    let seg_t = insert_segment(&pool, &device, session, 1).await;
    insert_person_segment(&pool, seg_t, &device, stranger, 5_000, 2).await;

    let with = retrieve::list_co_occurring_persons(
        &pool,
        &[owner.to_string()],
        Some(&device),
        None,
        None,
        100,
    )
    .await
    .unwrap();
    let ids: std::collections::HashSet<String> =
        with.iter().filter_map(|s| s.speaker_id.clone()).collect();
    assert!(
        ids.contains(&friend.to_string()),
        "the co-present friend must be returned"
    );
    assert!(
        !ids.contains(&owner.to_string()),
        "the owner must be excluded"
    );
    assert!(
        !ids.contains(&stranger.to_string()),
        "a non-co-present person must be excluded"
    );

    cleanup(&pool, &device).await;
}

#[tokio::test]
async fn recent_persons_lists_everyone_seen_once_most_recent_first() {
    // The "who have you seen so far" roster: every distinct person, deduped to one row, ordered by
    // most-recent sighting — and crucially needing NO owner (the bug was declining without one).
    let Some(pool) = pool().await else {
        eprintln!("skipping recent_persons_lists_everyone_seen_once_most_recent_first: DATABASE_URL unset");
        return;
    };
    let device = format!("test-pe-{}", Uuid::now_v7());
    let session = insert_dss(&pool, &device).await;
    let alice = insert_person(&pool, &device, Some("Alice"), 0).await;
    let bob = insert_person(&pool, &device, Some("Bob"), 1).await;
    let carol = insert_person(&pool, &device, None, 2).await; // an unnamed face still counts

    // Alice: two sightings across two segments (last_seen = 2_000) → must appear ONCE.
    let seg0 = insert_segment(&pool, &device, session, 0).await;
    insert_person_segment(&pool, seg0, &device, alice, 1_000, 0).await;
    let seg1 = insert_segment(&pool, &device, session, 1).await;
    insert_person_segment(&pool, seg1, &device, alice, 2_000, 0).await;
    // Bob: most recent overall (5_000). Carol: in between (3_000).
    let seg2 = insert_segment(&pool, &device, session, 2).await;
    insert_person_segment(&pool, seg2, &device, bob, 5_000, 1).await;
    let seg3 = insert_segment(&pool, &device, session, 3).await;
    insert_person_segment(&pool, seg3, &device, carol, 3_000, 2).await;

    let roster = retrieve::list_recent_persons(&pool, Some(&device), None, None, 100)
        .await
        .unwrap();

    let order: Vec<String> = roster.iter().filter_map(|s| s.speaker_id.clone()).collect();
    assert_eq!(
        order,
        vec![bob.to_string(), carol.to_string(), alice.to_string()],
        "one row per person, most-recently-seen first"
    );
    // Alice's row points at her LATEST sighting (so a citation deep-links to where she was last seen).
    let alice_row = roster.iter().find(|s| s.speaker_id.as_deref() == Some(alice.to_string().as_str())).unwrap();
    assert_eq!(alice_row.start_unix_nanos, 2_000, "roster row carries the most recent sighting");
    assert_eq!(alice_row.text, "(seen on camera)");

    cleanup(&pool, &device).await;
}
