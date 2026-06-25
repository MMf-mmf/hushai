//! Live-DB test for pgvector nearest-neighbour retrieval. Gated on `DATABASE_URL`.
//! Inserts three sentences with known embeddings under a unique device and verifies
//! cosine-distance ordering. The device filter isolates the test from real data.

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

use pgvector::Vector;

use hushai_rag::retrieve::{self, Filters, Tuning};

async fn pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .ok()
}

async fn insert_fixture_segment(pool: &PgPool, device_id: &str) -> Uuid {
    let session_id = Uuid::now_v7();
    let segment_id = Uuid::now_v7();
    sqlx::query("INSERT INTO devices (device_id, source_kind) VALUES ($1,'test') ON CONFLICT (device_id) DO NOTHING")
        .bind(device_id).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO sessions (session_id, device_id) VALUES ($1,$2)")
        .bind(session_id).bind(device_id).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO streams (session_id, stream_id, device_id, media_type, codec, container) VALUES ($1,'s0',$2,3,'h264+aac','fmp4')")
        .bind(session_id).bind(device_id).execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO segments (segment_id, device_id, stream_id, session_id, sequence, media_type, codec, container, capture_start_unix_nanos, monotonic_start_nanos, duration_nanos, content_sha256, byte_len, blob_uri, storage_backend) \
         VALUES ($1,$2,'s0',$3,0,3,'h264+aac','fmp4',1,0,2000000000,$4,100,$5,'file')",
    )
    .bind(segment_id).bind(device_id).bind(session_id).bind(vec![0u8;32]).bind(format!("file:///nonexistent/{segment_id}"))
    .execute(pool).await.unwrap();
    segment_id
}

async fn insert_sentence(pool: &PgPool, segment_id: Uuid, device_id: &str, text: &str, emb: Vec<f32>) {
    sqlx::query(
        "INSERT INTO transcript_sentences (segment_id, device_id, text, start_unix_nanos, end_unix_nanos, embedding, embedding_model, embedding_dim) \
         VALUES ($1,$2,$3,0,0,$4,'test',1024)",
    )
    .bind(segment_id)
    .bind(device_id)
    .bind(text)
    .bind(Vector::from(emb))
    .execute(pool)
    .await
    .unwrap();
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

#[tokio::test]
async fn nearest_orders_by_cosine_distance() {
    let Some(pool) = pool().await else {
        eprintln!("skipping nearest_orders_by_cosine_distance: DATABASE_URL unset");
        return;
    };
    let device = format!("test-retr-{}", Uuid::now_v7());
    let seg = insert_fixture_segment(&pool, &device).await;

    // Known 1024-dim embeddings: A == query, C partially aligned, B orthogonal.
    let mut a = vec![0f32; 1024];
    a[0] = 1.0;
    let mut c = vec![0f32; 1024];
    c[0] = 0.6;
    c[1] = 0.8;
    let mut b = vec![0f32; 1024];
    b[1] = 1.0;

    insert_sentence(&pool, seg, &device, "A exact match", a.clone()).await;
    insert_sentence(&pool, seg, &device, "C partial match", c).await;
    insert_sentence(&pool, seg, &device, "B orthogonal", b).await;

    let filters = Filters {
        device_id: Some(device.clone()),
        ..Default::default()
    };
    let results = retrieve::nearest(&pool, &a, 10, &Tuning::default(), &filters)
        .await
        .unwrap();

    assert_eq!(results.len(), 3, "should retrieve exactly our three fixtures");
    assert_eq!(results[0].text, "A exact match");
    assert_eq!(results[1].text, "C partial match");
    assert_eq!(results[2].text, "B orthogonal");
    assert!(results[0].distance <= results[1].distance);
    assert!(results[1].distance <= results[2].distance);
    assert!(results[0].distance < 1e-4, "exact match distance ~0");

    cleanup(&pool, &device).await;
}
