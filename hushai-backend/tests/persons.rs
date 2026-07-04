//! Integration coverage for the person (face) catalog endpoints
//! (`GET /v1/persons`, `PATCH /v1/persons/{id}`, `POST /v1/persons/{id}/merge`).
//!
//! Gated on `DATABASE_URL` (skips cleanly when unset), like the other backend integration tests.
//! Calls the handlers directly (not over HTTP) so we can assert on the typed result. Assertions are
//! existence-based (the global catalog may hold other faces), namespaced to a unique device and
//! cleaned up at the end. `sample_face` needs a real video blob + ffmpeg, so it's exercised by the
//! curl / Android end-to-end steps, not here.
//!
//! NB: `persons.person_id` and `person_segments.person_id` are native `uuid` (the 0009 type
//! contract), so the merge repoint binds uuid directly — no `::text[]` cast (unlike speakers).

use axum::Json;
use axum::extract::{Path, State};
use hushai_backend::build_state;
use hushai_backend::config::Config;
use hushai_backend::error::IngestError;
use hushai_backend::persons::{self, MergeReq, RenameReq};
use hushai_backend::state::AppState;
use pgvector::Vector;
use sqlx::PgPool;
use uuid::Uuid;

/// A 512-d unit vector (ArcFace dim) with one hot dimension — distinct hot dims are distinct faces.
fn unit_vec(hot: usize) -> Vector {
    let mut v = vec![0f32; 512];
    v[hot] = 1.0;
    Vector::from(v)
}

async fn make_state() -> Option<AppState> {
    let database_url = std::env::var("DATABASE_URL").ok()?;
    let blob_dir = std::env::temp_dir().join(format!("hushai-persons-{}", Uuid::now_v7()));
    let config = Config {
        database_url,
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
        hint_gate: Default::default(),
    };
    Some(build_state(config).await.expect("build state"))
}

async fn insert_device_session_stream(pool: &PgPool, device_id: &str) -> Uuid {
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

async fn insert_person(
    pool: &PgPool,
    device_id: &str,
    name: Option<&str>,
    hot: usize,
    n_samples: i64,
) -> Uuid {
    let person_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO persons (person_id, centroid, n_samples, display_name, first_seen_device_id) VALUES ($1,$2,$3,$4,$5)",
    )
    .bind(person_id).bind(unit_vec(hot)).bind(n_samples).bind(name).bind(device_id)
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
async fn list_persons_returns_catalog_with_recent_sightings() {
    let Some(state) = make_state().await else {
        eprintln!("SKIP list_persons_returns_catalog_with_recent_sightings: DATABASE_URL unset");
        return;
    };
    let pool = state.pool.clone();
    let device = format!("test-persons-{}", Uuid::now_v7());
    let session = insert_device_session_stream(&pool, &device).await;

    let person = insert_person(&pool, &device, Some("Test Face"), 0, 4).await;
    // Four sightings at increasing times; list_persons returns the 3 most recent, DESC.
    for i in 0..4i64 {
        let seg = insert_segment(&pool, &device, session, i).await;
        insert_person_segment(&pool, seg, &device, person, 1_000 + i * 1_000, 0).await;
    }

    let Json(list) = persons::list_persons(State(state.clone())).await.unwrap();
    let ours = list
        .iter()
        .find(|p| p.person_id == person)
        .expect("our person must appear in the global catalog");
    assert_eq!(ours.display_name.as_deref(), Some("Test Face"));
    assert_eq!(ours.n_samples, 4);
    // Four near-simultaneous detections (1µs apart) are one continuous appearance → one sighting,
    // even though they are four raw face templates (n_samples).
    assert_eq!(
        ours.n_sightings, 1,
        "four detections within the gap window collapse to a single sighting"
    );
    assert!(
        ours.sample_sighting_unix_nanos.len() <= 3,
        "at most 3 recent sightings"
    );
    assert_eq!(ours.sample_sighting_unix_nanos.len(), 3);
    // Most-recent-first.
    let mut sorted = ours.sample_sighting_unix_nanos.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(
        ours.sample_sighting_unix_nanos, sorted,
        "sightings must be DESC"
    );
    assert_eq!(
        ours.sample_sighting_unix_nanos[0], 4_000,
        "newest sighting first"
    );

    cleanup(&pool, &device).await;
}

#[tokio::test]
async fn list_persons_clusters_sightings_by_time_gap() {
    let Some(state) = make_state().await else {
        eprintln!("SKIP list_persons_clusters_sightings_by_time_gap: DATABASE_URL unset");
        return;
    };
    let pool = state.pool.clone();
    let device = format!("test-persons-{}", Uuid::now_v7());
    let session = insert_device_session_stream(&pool, &device).await;

    // Default gap is 60s. Two real appearances, two hours apart, each made of several closely
    // spaced per-frame detections: the kind of data the old per-template count inflated.
    let person = insert_person(&pool, &device, Some("Cluster Face"), 0, 6).await;
    let base = 10_000_000_000i64; // 10s, comfortably positive
    let two_hours = 7_200_000_000_000i64;
    let detections = [
        base,                       // appearance #1 ...
        base + 1_000,
        base + 2_000,
        base + two_hours,           // appearance #2 (well beyond the 60s gap) ...
        base + two_hours + 1_000,
        base + two_hours + 2_000,
    ];
    // One segment per appearance; a segment legitimately holds many per-frame face rows.
    let seg1 = insert_segment(&pool, &device, session, 0).await;
    let seg2 = insert_segment(&pool, &device, session, 1).await;
    for (i, &t) in detections.iter().enumerate() {
        let seg = if i < 3 { seg1 } else { seg2 };
        insert_person_segment(&pool, seg, &device, person, t, 0).await;
    }

    let Json(list) = persons::list_persons(State(state.clone())).await.unwrap();
    let ours = list
        .iter()
        .find(|p| p.person_id == person)
        .expect("our person must appear in the global catalog");
    assert_eq!(
        ours.n_sightings, 2,
        "six detections in two time-separated bursts = two sightings (not six)"
    );

    cleanup(&pool, &device).await;
}

#[tokio::test]
async fn rename_person_sets_name_and_validates() {
    let Some(state) = make_state().await else {
        eprintln!("SKIP rename_person_sets_name_and_validates: DATABASE_URL unset");
        return;
    };
    let pool = state.pool.clone();
    let device = format!("test-persons-{}", Uuid::now_v7());
    insert_device_session_stream(&pool, &device).await;
    let person = insert_person(&pool, &device, None, 0, 1).await;

    // Names a face (trimmed).
    let Json(row) = persons::rename_person(
        State(state.clone()),
        Path(person),
        Json(RenameReq {
            display_name: "  Alice  ".into(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        row.display_name.as_deref(),
        Some("Alice"),
        "name is trimmed"
    );

    // Empty name -> 400.
    let bad = persons::rename_person(
        State(state.clone()),
        Path(person),
        Json(RenameReq {
            display_name: "   ".into(),
        }),
    )
    .await;
    assert!(
        matches!(bad, Err(IngestError::BadRequest(_))),
        "empty name must be rejected: {bad:?}"
    );

    // Unknown id -> 404.
    let missing = persons::rename_person(
        State(state.clone()),
        Path(Uuid::now_v7()),
        Json(RenameReq {
            display_name: "Nobody".into(),
        }),
    )
    .await;
    assert!(
        matches!(missing, Err(IngestError::NotFound(_))),
        "unknown id must 404: {missing:?}"
    );

    cleanup(&pool, &device).await;
}

#[tokio::test]
async fn merge_person_folds_loser_into_survivor() {
    let Some(state) = make_state().await else {
        eprintln!("SKIP merge_person_folds_loser_into_survivor: DATABASE_URL unset");
        return;
    };
    let pool = state.pool.clone();
    let device = format!("test-persons-{}", Uuid::now_v7());
    let session = insert_device_session_stream(&pool, &device).await;

    // Survivor P1 (2 sightings) + loser P2 (3 sightings); same real face over-split by the matcher.
    let survivor = insert_person(&pool, &device, Some("Bob"), 0, 2).await;
    let loser = insert_person(&pool, &device, None, 1, 3).await;
    for i in 0..2i64 {
        let seg = insert_segment(&pool, &device, session, i).await;
        insert_person_segment(&pool, seg, &device, survivor, 1_000 + i, 0).await;
    }
    for i in 0..3i64 {
        let seg = insert_segment(&pool, &device, session, 100 + i).await;
        insert_person_segment(&pool, seg, &device, loser, 5_000 + i, 1).await;
    }

    // Self-merge rejected.
    let self_merge = persons::merge_person(
        State(state.clone()),
        Path(survivor),
        Json(MergeReq { into: survivor }),
    )
    .await;
    assert!(
        matches!(self_merge, Err(IngestError::BadRequest(_))),
        "self-merge must be rejected: {self_merge:?}"
    );

    // Unknown survivor rejected (both rows must exist).
    let missing = persons::merge_person(
        State(state.clone()),
        Path(loser),
        Json(MergeReq {
            into: Uuid::now_v7(),
        }),
    )
    .await;
    assert!(
        matches!(missing, Err(IngestError::NotFound(_))),
        "missing survivor must 404: {missing:?}"
    );

    // Fold loser -> survivor.
    let ok = persons::merge_person(
        State(state.clone()),
        Path(loser),
        Json(MergeReq { into: survivor }),
    )
    .await;
    assert!(ok.is_ok(), "merge should succeed: {ok:?}");

    // Loser gone; survivor n_samples summed; all person_segments repointed; centroid preserved.
    let loser_exists: i64 = sqlx::query_scalar("SELECT count(*) FROM persons WHERE person_id=$1")
        .bind(loser)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(loser_exists, 0, "loser person row deleted");
    let survivor_n: i64 = sqlx::query_scalar("SELECT n_samples FROM persons WHERE person_id=$1")
        .bind(survivor)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(survivor_n, 5, "survivor n_samples = 2 + 3");
    let repointed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM person_segments WHERE person_id=$1")
            .bind(survivor)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        repointed, 5,
        "all 5 person_segments now point at the survivor"
    );
    let has_centroid: bool =
        sqlx::query_scalar("SELECT centroid IS NOT NULL FROM persons WHERE person_id=$1")
            .bind(survivor)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(has_centroid, "merged survivor keeps a centroid");

    cleanup(&pool, &device).await;
}
