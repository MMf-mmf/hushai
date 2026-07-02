//! Live-DB tests for the deterministic presence-aggregation engine (flaw F1). Gated on
//! `DATABASE_URL`. Seeds `person_segments` under a UNIQUE device (safe alongside other data — no
//! TRUNCATE, scoped cleanup) and verifies:
//!   * `person_presence` counts EVERY sighting (deduped per segment) UNCAPPED — where the old
//!     `list_by_person` + LLM path caps at top_k and the model miscounts;
//!   * first/last timestamps + hour/day rhythm are exact.

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use hushai_rag::{presence, retrieve};

const DAY: i64 = 86_400_000_000_000;

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    PgPoolOptions::new().max_connections(5).connect(&url).await.ok()
}

async fn insert_dss(pool: &PgPool, device_id: &str) -> Uuid {
    let session_id = Uuid::now_v7();
    sqlx::query("INSERT INTO devices (device_id, source_kind) VALUES ($1,'test') ON CONFLICT (device_id) DO NOTHING")
        .bind(device_id).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO sessions (session_id, device_id) VALUES ($1,$2)")
        .bind(session_id).bind(device_id).execute(pool).await.unwrap();
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

async fn insert_person(pool: &PgPool, device_id: &str, name: Option<&str>) -> Uuid {
    let person_id = Uuid::now_v7();
    let mut v = vec![0f32; 512];
    v[0] = 1.0;
    sqlx::query("INSERT INTO persons (person_id, centroid, n_samples, display_name, first_seen_device_id) VALUES ($1,$2,1,$3,$4)")
        .bind(person_id).bind(pgvector::Vector::from(v)).bind(name).bind(device_id)
        .execute(pool).await.unwrap();
    person_id
}

async fn insert_person_segment(pool: &PgPool, segment_id: Uuid, device_id: &str, person_id: Uuid, start: i64) {
    let mut v = vec![0f32; 512];
    v[0] = 1.0;
    sqlx::query(
        "INSERT INTO person_segments (segment_id, device_id, person_id, start_unix_nanos, end_unix_nanos, frame_offset_nanos, embedding, bbox, det_score, quality) \
         VALUES ($1,$2,$3,$4,$4,0,$5,$6::jsonb,0.9,'clean')",
    )
    .bind(segment_id).bind(device_id).bind(person_id).bind(start).bind(pgvector::Vector::from(v)).bind("[1,2,3,4]")
    .execute(pool).await.unwrap();
}

async fn cleanup(pool: &PgPool, device_id: &str) {
    for sql in [
        "DELETE FROM person_segments WHERE device_id=$1",
        "DELETE FROM persons WHERE first_seen_device_id=$1",
        "DELETE FROM segments WHERE device_id=$1",
        "DELETE FROM streams WHERE device_id=$1",
        "DELETE FROM sessions WHERE device_id=$1",
        "DELETE FROM devices WHERE device_id=$1",
    ] {
        let _ = sqlx::query(sql).bind(device_id).execute(pool).await;
    }
}

#[tokio::test]
async fn presence_counts_first_last_and_rhythm_deduped() {
    let Some(pool) = pool().await else {
        eprintln!("skipping presence_counts_first_last_and_rhythm_deduped: DATABASE_URL unset");
        return;
    };
    let device = format!("test-presence-{}", Uuid::now_v7());
    let session = insert_dss(&pool, &device).await;
    let alice = insert_person(&pool, &device, Some("Alice")).await;
    // Base at some fixed instant; 3 visits at the SAME time-of-day across 3 consecutive days plus a
    // 4th an hour later on day 0. One visit has TWO frame rows in a single segment → still ONE sighting.
    let base = 1_781_784_000_000_000_000i64;
    let visits = [base, base + 3_600_000_000_000, base + DAY, base + 2 * DAY];
    for (i, &t) in visits.iter().enumerate() {
        let seg = insert_segment(&pool, &device, session, i as i64).await;
        insert_person_segment(&pool, seg, &device, alice, t).await;
        if i == 0 {
            // second frame, same segment, slightly later → must NOT inflate the count
            insert_person_segment(&pool, seg, &device, alice, t + 500_000_000).await;
        }
    }

    let s = presence::person_presence(&pool, &[alice.to_string()], Some(&device), None, None, 0)
        .await
        .unwrap();
    assert_eq!(s.count, 4, "4 distinct segments = 4 sightings (frame dedup)");
    assert_eq!(s.first_ns, Some(base), "first sighting timestamp");
    assert_eq!(s.last_ns, Some(base + 2 * DAY), "last sighting timestamp");
    assert_eq!(s.by_hour.iter().sum::<i64>(), 4);
    assert_eq!(s.by_dow.iter().sum::<i64>(), 4);
    // 3 of 4 sightings share one time-of-day → that hour bucket holds 3 and is the peak.
    let ph = s.peak_hours();
    assert!(!ph.is_empty(), "a peak hour exists");
    assert_eq!(s.by_hour[ph[0]], 3, "the busiest hour has 3 of the 4 sightings");

    let line = presence::render_presence(&s, "Alice", base + 3 * DAY, 0);
    assert!(line.contains("4 times"), "count narrated verbatim: {line}");

    cleanup(&pool, &device).await;
}

#[tokio::test]
async fn presence_is_uncapped_where_the_sighting_list_caps() {
    // The F1 crux: the old path lists at most top_k rows and lets the LLM count them, so a heavy
    // repeat visitor is UNDERCOUNTED. The presence engine counts them all.
    let Some(pool) = pool().await else {
        eprintln!("skipping presence_is_uncapped_where_the_sighting_list_caps: DATABASE_URL unset");
        return;
    };
    let device = format!("test-presence-{}", Uuid::now_v7());
    let session = insert_dss(&pool, &device).await;
    let bob = insert_person(&pool, &device, Some("Bob")).await;
    let n = 55i64;
    let base = 1_781_784_000_000_000_000i64;
    for i in 0..n {
        let seg = insert_segment(&pool, &device, session, i).await;
        insert_person_segment(&pool, seg, &device, bob, base + i * 3_600_000_000_000).await;
    }

    // Old path, capped at 50 → undercounts.
    let capped = retrieve::list_by_person(&pool, &[bob.to_string()], Some(&device), None, None, 50)
        .await
        .unwrap();
    assert_eq!(capped.len(), 50, "sighting list is capped at top_k");

    // Presence engine → the true count, uncapped.
    let s = presence::person_presence(&pool, &[bob.to_string()], Some(&device), None, None, 0)
        .await
        .unwrap();
    assert_eq!(s.count, n, "presence counts every sighting past the cap");

    cleanup(&pool, &device).await;
}
