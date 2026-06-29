//! Integration coverage for the device-management + footage-deletion surface
//! (`GET /v1/devices`, `GET /v1/devices/{id}/usage`, `PATCH /v1/devices/{id}`,
//! `PUT /v1/devices/{id}/retention`, `DELETE /v1/devices/{id}/footage`, `DELETE /v1/devices/{id}`,
//! the retention sweep, and `storage::reclaim_blobs`).
//!
//! Gated on `DATABASE_URL` (skips cleanly when unset), like the other backend integration tests.
//! Handlers are called directly so we can assert on typed results. Namespaced to a unique device,
//! cleaned up at the end. Asserts the adversarial-review invariants: cascade to child tables,
//! `first_seen_device_id` NULLing so a device delete can't FK-fail, local-day bucket boundaries,
//! retention "fully older" semantics + idempotency, and content-addressed blob ref-counting.

use std::path::Path as StdPath;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::{Path, Query, State};
use hushai_backend::build_state;
use hushai_backend::config::Config;
use hushai_backend::devices::{
    self, DayQuery, RenameReq, RetentionReq, UsageParams,
};
use hushai_backend::error::IngestError;
use hushai_backend::state::AppState;
use hushai_backend::storage;
use sqlx::PgPool;
use uuid::Uuid;

async fn make_state() -> Option<AppState> {
    let database_url = std::env::var("DATABASE_URL").ok()?;
    let blob_dir = std::env::temp_dir().join(format!("hushai-devices-{}", Uuid::now_v7()));
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
    };
    Some(build_state(config).await.expect("build state"))
}

fn hex64(b: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

fn now_ns() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as i64
}

async fn ensure_device(pool: &PgPool, device_id: &str, session: Uuid) {
    sqlx::query("INSERT INTO devices (device_id, source_kind) VALUES ($1,'test') ON CONFLICT (device_id) DO NOTHING")
        .bind(device_id).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO sessions (session_id, device_id) VALUES ($1,$2)")
        .bind(session).bind(device_id).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO streams (session_id, stream_id, device_id, media_type, codec, container) VALUES ($1,'cam0-video',$2,3,'h264','fmp4')")
        .bind(session).bind(device_id).execute(pool).await.unwrap();
}

/// Insert a segment row whose `content_sha256` is `sha` and `blob_uri` points at the
/// content-addressed shard path. If `write_blob` is true, a dummy file of `byte_len` bytes is
/// written there (reclaim_blobs never re-hashes it — it only checks references then unlinks).
async fn insert_segment(
    state: &AppState,
    device: &str,
    session: Uuid,
    seq: i64,
    capture_ns: i64,
    sha: &[u8; 32],
    byte_len: i64,
    write_blob: bool,
) -> Uuid {
    let hex = hex64(sha);
    let path = storage::shard_path(&state.blob_root, &hex);
    if write_blob {
        tokio::fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        tokio::fs::write(&path, vec![7u8; byte_len as usize]).await.unwrap();
    }
    let seg = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO segments (segment_id, device_id, stream_id, session_id, sequence, media_type, codec, container, capture_start_unix_nanos, monotonic_start_nanos, duration_nanos, content_sha256, byte_len, blob_uri, storage_backend) \
         VALUES ($1,$2,'cam0-video',$3,$4,3,'h264','fmp4',$5,0,2000000000,$6,$7,$8,'file')",
    )
    .bind(seg).bind(device).bind(session).bind(seq).bind(capture_ns)
    .bind(&sha[..]).bind(byte_len).bind(format!("file://{}", path.display()))
    .execute(&state.pool).await.unwrap();
    seg
}

async fn add_children(pool: &PgPool, seg: Uuid, device: &str) {
    sqlx::query("INSERT INTO segment_transcription_status (segment_id) VALUES ($1)")
        .bind(seg).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO transcript_sentences (segment_id, device_id, text, start_unix_nanos) VALUES ($1,$2,'hi',0)")
        .bind(seg).bind(device).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO segment_vision_status (segment_id) VALUES ($1)")
        .bind(seg).execute(pool).await.unwrap();
}

async fn count(pool: &PgPool, sql: &'static str, device: &str) -> i64 {
    sqlx::query_scalar(sql).bind(device).fetch_one(pool).await.unwrap()
}

async fn cleanup(pool: &PgPool, device: &str) {
    for sql in [
        "DELETE FROM transcript_sentences WHERE device_id=$1",
        "DELETE FROM person_segments WHERE device_id=$1",
        "DELETE FROM speaker_segments WHERE device_id=$1",
        "UPDATE speakers SET first_seen_device_id=NULL WHERE first_seen_device_id=$1",
        "UPDATE persons SET first_seen_device_id=NULL WHERE first_seen_device_id=$1",
        "DELETE FROM segment_transcription_status WHERE segment_id IN (SELECT segment_id FROM segments WHERE device_id=$1)",
        "DELETE FROM segment_vision_status WHERE segment_id IN (SELECT segment_id FROM segments WHERE device_id=$1)",
        "DELETE FROM segments WHERE device_id=$1",
        "DELETE FROM streams WHERE device_id=$1",
        "DELETE FROM sessions WHERE device_id=$1",
        "DELETE FROM devices WHERE device_id=$1",
    ] {
        let _ = sqlx::query(sql).bind(device).execute(pool).await;
    }
}

#[tokio::test]
async fn list_rename_and_retention() {
    let Some(state) = make_state().await else {
        eprintln!("SKIP list_rename_and_retention: DATABASE_URL unset");
        return;
    };
    let pool = state.pool.clone();
    let device = format!("test-dev-{}", Uuid::now_v7());
    let session = Uuid::now_v7();
    ensure_device(&pool, &device, session).await;
    insert_segment(&state, &device, session, 0, now_ns(), &[1u8; 32], 100, false).await;

    // rename
    let Json(row) = devices::rename_device(
        State(state.clone()),
        Path(device.clone()),
        Json(RenameReq { display_name: "  Front Door  ".into() }),
    )
    .await
    .unwrap();
    assert_eq!(row.display_name.as_deref(), Some("Front Door"), "name trimmed");

    // empty rename -> 400; unknown device -> 404
    assert!(matches!(
        devices::rename_device(State(state.clone()), Path(device.clone()), Json(RenameReq { display_name: "  ".into() })).await,
        Err(IngestError::BadRequest(_))
    ));
    assert!(matches!(
        devices::rename_device(State(state.clone()), Path("nope".into()), Json(RenameReq { display_name: "X".into() })).await,
        Err(IngestError::NotFound(_))
    ));

    // retention: set 7, then clear (null), reject 0, 404 on unknown
    let Json(r) = devices::set_retention(State(state.clone()), Path(device.clone()), Json(RetentionReq { retention_days: Some(7) })).await.unwrap();
    assert_eq!(r.retention_days, Some(7));
    let Json(r) = devices::set_retention(State(state.clone()), Path(device.clone()), Json(RetentionReq { retention_days: None })).await.unwrap();
    assert_eq!(r.retention_days, None, "null clears the policy");
    assert!(matches!(
        devices::set_retention(State(state.clone()), Path(device.clone()), Json(RetentionReq { retention_days: Some(0) })).await,
        Err(IngestError::BadRequest(_))
    ));
    assert!(matches!(
        devices::set_retention(State(state.clone()), Path("nope".into()), Json(RetentionReq { retention_days: Some(3) })).await,
        Err(IngestError::NotFound(_))
    ));

    // list reflects the rename + counts
    let Json(list) = devices::list_devices(State(state.clone())).await.unwrap();
    let ours = list.iter().find(|d| d.device_id == device).expect("device listed");
    assert_eq!(ours.display_name.as_deref(), Some("Front Door"));
    assert_eq!(ours.segment_count, 1);
    assert_eq!(ours.logical_bytes, 100);
    assert!(ours.has_muxed, "media_type 3 → muxed");

    cleanup(&pool, &device).await;
}

#[tokio::test]
async fn usage_buckets_by_local_day_and_validates_tz() {
    let Some(state) = make_state().await else {
        eprintln!("SKIP usage_buckets_by_local_day_and_validates_tz: DATABASE_URL unset");
        return;
    };
    let pool = state.pool.clone();
    let device = format!("test-dev-{}", Uuid::now_v7());
    let session = Uuid::now_v7();
    ensure_device(&pool, &device, session).await;

    // 2021-01-10T12:00:00Z and 2021-01-11T12:00:00Z (distinct UTC days).
    let day_a_ns = 1_610_280_000i64 * 1_000_000_000;
    let day_b_ns = 1_610_366_400i64 * 1_000_000_000;
    insert_segment(&state, &device, session, 0, day_a_ns, &[10u8; 32], 100, false).await;
    insert_segment(&state, &device, session, 1, day_a_ns + 1, &[11u8; 32], 100, false).await;
    insert_segment(&state, &device, session, 2, day_b_ns, &[12u8; 32], 100, false).await;

    let Json(days) = devices::device_usage(
        State(state.clone()),
        Path(device.clone()),
        Query(UsageParams { tz: Some("UTC".into()) }),
    )
    .await
    .unwrap();
    assert_eq!(days.len(), 2, "two UTC days");
    let a = days.iter().find(|d| d.day == "2021-01-10").expect("day A bucket");
    assert_eq!(a.segment_count, 2);
    assert_eq!(a.logical_bytes, 200);
    let b = days.iter().find(|d| d.day == "2021-01-11").expect("day B bucket");
    assert_eq!(b.segment_count, 1);

    // Invalid tz → 400, not 500.
    assert!(matches!(
        devices::device_usage(State(state.clone()), Path(device.clone()), Query(UsageParams { tz: Some("Mars/Olympus".into()) })).await,
        Err(IngestError::BadRequest(_))
    ));

    cleanup(&pool, &device).await;
}

#[tokio::test]
async fn delete_day_cascades_children_and_spares_other_days() {
    let Some(state) = make_state().await else {
        eprintln!("SKIP delete_day_cascades_children_and_spares_other_days: DATABASE_URL unset");
        return;
    };
    let pool = state.pool.clone();
    let device = format!("test-dev-{}", Uuid::now_v7());
    let session = Uuid::now_v7();
    ensure_device(&pool, &device, session).await;

    let day_a_ns = 1_610_280_000i64 * 1_000_000_000; // 2021-01-10
    let day_b_ns = 1_610_366_400i64 * 1_000_000_000; // 2021-01-11
    let seg_a = insert_segment(&state, &device, session, 0, day_a_ns, &[20u8; 32], 100, false).await;
    let seg_b = insert_segment(&state, &device, session, 1, day_b_ns, &[21u8; 32], 100, false).await;
    add_children(&pool, seg_a, &device).await;
    add_children(&pool, seg_b, &device).await;

    let Json(impact) = devices::delete_footage(
        State(state.clone()),
        Path(device.clone()),
        Query(DayQuery { tz: Some("UTC".into()), day: "2021-01-10".into() }),
    )
    .await
    .unwrap();
    assert_eq!(impact.segments_deleted, 1, "only day A's one segment");
    assert_eq!(impact.logical_bytes, 100);

    // Day A's segment + ALL its derived child rows are gone (ON DELETE CASCADE).
    let seg_a_left: i64 = sqlx::query_scalar("SELECT count(*) FROM segments WHERE segment_id=$1").bind(seg_a).fetch_one(&pool).await.unwrap();
    assert_eq!(seg_a_left, 0);
    for tbl in ["transcript_sentences", "segment_transcription_status", "segment_vision_status"] {
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {tbl} WHERE segment_id=$1"
        )))
        .bind(seg_a)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 0, "{tbl} cascade-deleted for day A");
    }
    // Day B untouched (segment + children remain); streams/sessions NOT GC'd on a range delete.
    let seg_b_left: i64 = sqlx::query_scalar("SELECT count(*) FROM segments WHERE segment_id=$1").bind(seg_b).fetch_one(&pool).await.unwrap();
    assert_eq!(seg_b_left, 1, "day B segment spared");
    let sess_left = count(&pool, "SELECT count(*) FROM sessions WHERE device_id=$1", &device).await;
    assert_eq!(sess_left, 1, "session kept on a range delete");

    cleanup(&pool, &device).await;
}

#[tokio::test]
async fn delete_device_nulls_first_seen_and_tears_down() {
    let Some(state) = make_state().await else {
        eprintln!("SKIP delete_device_nulls_first_seen_and_tears_down: DATABASE_URL unset");
        return;
    };
    let pool = state.pool.clone();
    let device = format!("test-dev-{}", Uuid::now_v7());
    let session = Uuid::now_v7();
    ensure_device(&pool, &device, session).await;
    let seg = insert_segment(&state, &device, session, 0, now_ns(), &[30u8; 32], 100, false).await;
    add_children(&pool, seg, &device).await;

    // A speaker AND a person whose first_seen_device_id points at this device — these NO-ACTION FKs
    // would block the delete with 23503 if not NULLed first (the bug the review caught).
    let speaker = Uuid::now_v7();
    sqlx::query("INSERT INTO speakers (speaker_id, n_samples, first_seen_device_id) VALUES ($1,1,$2)")
        .bind(speaker).bind(&device).execute(&pool).await.unwrap();
    let person = Uuid::now_v7();
    sqlx::query("INSERT INTO persons (person_id, n_samples, first_seen_device_id) VALUES ($1,1,$2)")
        .bind(person).bind(&device).execute(&pool).await.unwrap();

    let Json(impact) = devices::delete_device(State(state.clone()), Path(device.clone())).await.unwrap();
    assert_eq!(impact.segments_deleted, 1);

    // Everything structural is gone.
    for (tbl, sql) in [
        ("segments", "SELECT count(*) FROM segments WHERE device_id=$1"),
        ("streams", "SELECT count(*) FROM streams WHERE device_id=$1"),
        ("sessions", "SELECT count(*) FROM sessions WHERE device_id=$1"),
        ("devices", "SELECT count(*) FROM devices WHERE device_id=$1"),
    ] {
        assert_eq!(count(&pool, sql, &device).await, 0, "{tbl} torn down");
    }
    // The global speaker/person survive, with first_seen NULLed (metadata only).
    let sp_fs: Option<String> = sqlx::query_scalar("SELECT first_seen_device_id FROM speakers WHERE speaker_id=$1").bind(speaker).fetch_one(&pool).await.unwrap();
    assert_eq!(sp_fs, None, "speaker.first_seen_device_id NULLed");
    let pe_fs: Option<String> = sqlx::query_scalar("SELECT first_seen_device_id FROM persons WHERE person_id=$1").bind(person).fetch_one(&pool).await.unwrap();
    assert_eq!(pe_fs, None, "person.first_seen_device_id NULLed");

    // Unknown device → 404.
    assert!(matches!(
        devices::delete_device(State(state.clone()), Path("nope".into())).await,
        Err(IngestError::NotFound(_))
    ));

    // cleanup the catalog rows we minted
    let _ = sqlx::query("DELETE FROM speakers WHERE speaker_id=$1").bind(speaker).execute(&pool).await;
    let _ = sqlx::query("DELETE FROM persons WHERE person_id=$1").bind(person).execute(&pool).await;
    cleanup(&pool, &device).await;
}

#[tokio::test]
async fn retention_sweep_purges_old_and_is_idempotent() {
    let Some(state) = make_state().await else {
        eprintln!("SKIP retention_sweep_purges_old_and_is_idempotent: DATABASE_URL unset");
        return;
    };
    let pool = state.pool.clone();
    let device = format!("test-dev-{}", Uuid::now_v7());
    let session = Uuid::now_v7();
    ensure_device(&pool, &device, session).await;

    let now = now_ns();
    let old = now - 100 * 86_400 * 1_000_000_000; // 100 days ago
    let recent = now - 60 * 1_000_000_000; // 1 min ago
    let seg_old = insert_segment(&state, &device, session, 0, old, &[40u8; 32], 100, false).await;
    let seg_recent = insert_segment(&state, &device, session, 1, recent, &[41u8; 32], 100, false).await;

    // Keep last 1 day → the 100-day-old segment is fully past the window; the recent one stays.
    let _ = devices::set_retention(State(state.clone()), Path(device.clone()), Json(RetentionReq { retention_days: Some(1) })).await.unwrap();
    devices::run_retention_sweep(&state).await;

    let old_left: i64 = sqlx::query_scalar("SELECT count(*) FROM segments WHERE segment_id=$1").bind(seg_old).fetch_one(&pool).await.unwrap();
    let recent_left: i64 = sqlx::query_scalar("SELECT count(*) FROM segments WHERE segment_id=$1").bind(seg_recent).fetch_one(&pool).await.unwrap();
    assert_eq!(old_left, 0, "old footage purged by retention");
    assert_eq!(recent_left, 1, "recent footage kept");

    // Idempotent: a second pass changes nothing.
    devices::run_retention_sweep(&state).await;
    let recent_left2: i64 = sqlx::query_scalar("SELECT count(*) FROM segments WHERE segment_id=$1").bind(seg_recent).fetch_one(&pool).await.unwrap();
    assert_eq!(recent_left2, 1, "second pass is a no-op");

    cleanup(&pool, &device).await;
}

#[tokio::test]
async fn reclaim_blobs_refcounts_shared_content() {
    let Some(state) = make_state().await else {
        eprintln!("SKIP reclaim_blobs_refcounts_shared_content: DATABASE_URL unset");
        return;
    };
    let pool = state.pool.clone();
    let device = format!("test-dev-{}", Uuid::now_v7());
    let session = Uuid::now_v7();
    ensure_device(&pool, &device, session).await;

    let sha_x = [50u8; 32]; // shared by two segments
    let sha_y = [51u8; 32]; // referenced by one segment
    let seg_x1 = insert_segment(&state, &device, session, 0, now_ns(), &sha_x, 100, true).await;
    let _seg_x2 = insert_segment(&state, &device, session, 1, now_ns(), &sha_x, 100, true).await;
    let seg_y = insert_segment(&state, &device, session, 2, now_ns(), &sha_y, 200, true).await;

    let root: &StdPath = &state.blob_root;
    let path_x = storage::shard_path(root, &hex64(&sha_x));
    let path_y = storage::shard_path(root, &hex64(&sha_y));
    assert!(path_x.exists() && path_y.exists(), "both blobs written");

    // Delete seg_x1 (sha_x still referenced by seg_x2) and seg_y (sha_y now unreferenced).
    sqlx::query("DELETE FROM segments WHERE segment_id IN ($1,$2)").bind(seg_x1).bind(seg_y).execute(&pool).await.unwrap();
    let freed = storage::reclaim_blobs(&pool, root, &[sha_x, sha_y], Duration::ZERO).await;
    assert_eq!(freed, 200, "only sha_y's 200 bytes reclaimed");
    assert!(path_x.exists(), "shared blob kept (still referenced)");
    assert!(!path_y.exists(), "unreferenced blob unlinked");

    // Now delete the last sha_x sharer → sha_x becomes reclaimable.
    sqlx::query("DELETE FROM segments WHERE device_id=$1").bind(&device).execute(&pool).await.unwrap();
    let freed2 = storage::reclaim_blobs(&pool, root, &[sha_x], Duration::ZERO).await;
    assert_eq!(freed2, 100, "sha_x reclaimed once unreferenced");
    assert!(!path_x.exists(), "shared blob finally unlinked");

    cleanup(&pool, &device).await;
}
