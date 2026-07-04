//! Integration coverage for the owner-identity endpoints (0023, "This is me"):
//! `POST /v1/speakers/{id}/owner` + `/unowner` and the persons pair.
//!
//! Gated on `DATABASE_URL` (skips cleanly when unset), like the other backend integration
//! tests. Calls the handlers directly (not over HTTP) so we can assert on the typed result.
//! The single-owner invariant is asserted with a namespaced WHERE (the dev DB may hold a real
//! owner outside the test's rows); everything is cleaned up at the end.

use axum::Json;
use axum::extract::{Path, State};
use hushai_backend::build_state;
use hushai_backend::config::Config;
use hushai_backend::error::IngestError;
use hushai_backend::state::AppState;
use hushai_backend::{persons, speakers};
use pgvector::Vector;
use sqlx::PgPool;
use uuid::Uuid;

fn unit_vec(dim: usize, hot: usize) -> Vector {
    let mut v = vec![0f32; dim];
    v[hot] = 1.0;
    Vector::from(v)
}

async fn make_state() -> Option<AppState> {
    let database_url = std::env::var("DATABASE_URL").ok()?;
    let blob_dir = std::env::temp_dir().join(format!("hushai-owner-{}", Uuid::now_v7()));
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

async fn insert_device(pool: &PgPool, device_id: &str) {
    sqlx::query(
        "INSERT INTO devices (device_id, source_kind) VALUES ($1,'test') ON CONFLICT (device_id) DO NOTHING",
    )
    .bind(device_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_speaker(pool: &PgPool, device_id: &str, name: &str, hot: usize) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO speakers (speaker_id, centroid, n_samples, display_name, first_seen_device_id) \
         VALUES ($1,$2,1,$3,$4)",
    )
    .bind(id)
    .bind(unit_vec(192, hot))
    .bind(name)
    .bind(device_id)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn insert_person(pool: &PgPool, device_id: &str, name: &str, hot: usize) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO persons (person_id, centroid, n_samples, display_name, first_seen_device_id) \
         VALUES ($1,$2,1,$3,$4)",
    )
    .bind(id)
    .bind(unit_vec(512, hot))
    .bind(name)
    .bind(device_id)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn cleanup(pool: &PgPool, device_id: &str) {
    for sql in [
        "DELETE FROM speakers WHERE first_seen_device_id=$1",
        "DELETE FROM persons WHERE first_seen_device_id=$1",
        "DELETE FROM devices WHERE device_id=$1",
    ] {
        let _ = sqlx::query(sql).bind(device_id).execute(pool).await;
    }
}

#[tokio::test]
async fn speaker_owner_set_moves_and_clear_clears() {
    let Some(state) = make_state().await else {
        eprintln!("SKIP speaker_owner_set_moves_and_clear_clears: DATABASE_URL unset");
        return;
    };
    let pool = state.pool.clone();
    let device = format!("test-owner-{}", Uuid::now_v7());
    insert_device(&pool, &device).await;
    let a = insert_speaker(&pool, &device, "Voice A", 0).await;
    let b = insert_speaker(&pool, &device, "Voice B", 1).await;

    // Set A as owner.
    let Json(row) = speakers::set_speaker_owner(State(state.clone()), Path(a))
        .await
        .unwrap();
    assert!(row.is_owner, "A must be owner after set");

    // Setting B moves the mark (single owner): A loses it in the same transaction.
    let Json(row) = speakers::set_speaker_owner(State(state.clone()), Path(b))
        .await
        .unwrap();
    assert!(row.is_owner, "B must be owner after set");
    let owners: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM speakers WHERE is_owner AND first_seen_device_id = $1",
    )
    .bind(&device)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(owners, 1, "at most one owner among the test speakers");
    let a_owner: bool = sqlx::query_scalar("SELECT is_owner FROM speakers WHERE speaker_id = $1")
        .bind(a)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!a_owner, "A must have lost the owner mark");

    // Clear is idempotent and reflected in the returned row.
    let Json(row) = speakers::clear_speaker_owner(State(state.clone()), Path(b))
        .await
        .unwrap();
    assert!(!row.is_owner, "B must not be owner after unowner");

    // Unknown id -> 404.
    let err = speakers::set_speaker_owner(State(state.clone()), Path(Uuid::now_v7()))
        .await
        .err()
        .expect("unknown id must 404");
    assert!(matches!(err, IngestError::NotFound(_)));

    cleanup(&pool, &device).await;
}

#[tokio::test]
async fn speaker_owner_rejects_archived_voice() {
    let Some(state) = make_state().await else {
        eprintln!("SKIP speaker_owner_rejects_archived_voice: DATABASE_URL unset");
        return;
    };
    let pool = state.pool.clone();
    let device = format!("test-owner-{}", Uuid::now_v7());
    insert_device(&pool, &device).await;
    let a = insert_speaker(&pool, &device, "Archived Voice", 0).await;
    sqlx::query("UPDATE speakers SET archived_at = now() WHERE speaker_id = $1")
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();

    let err = speakers::set_speaker_owner(State(state.clone()), Path(a))
        .await
        .err()
        .expect("archived voice must not become owner");
    assert!(matches!(err, IngestError::NotFound(_)));

    cleanup(&pool, &device).await;
}

#[tokio::test]
async fn person_owner_set_moves_and_clear_clears() {
    let Some(state) = make_state().await else {
        eprintln!("SKIP person_owner_set_moves_and_clear_clears: DATABASE_URL unset");
        return;
    };
    let pool = state.pool.clone();
    let device = format!("test-owner-{}", Uuid::now_v7());
    insert_device(&pool, &device).await;
    let a = insert_person(&pool, &device, "Face A", 0).await;
    let b = insert_person(&pool, &device, "Face B", 1).await;

    let Json(row) = persons::set_person_owner(State(state.clone()), Path(a))
        .await
        .unwrap();
    assert!(row.is_owner);

    let Json(row) = persons::set_person_owner(State(state.clone()), Path(b))
        .await
        .unwrap();
    assert!(row.is_owner);
    let owners: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM persons WHERE is_owner AND first_seen_device_id = $1",
    )
    .bind(&device)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(owners, 1, "at most one owner among the test persons");

    let Json(row) = persons::clear_person_owner(State(state.clone()), Path(b))
        .await
        .unwrap();
    assert!(!row.is_owner);

    cleanup(&pool, &device).await;
}
