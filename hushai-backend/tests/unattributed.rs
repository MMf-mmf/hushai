//! Integration coverage for the unattributed-audio surfacing endpoints
//! (`GET /v1/speakers/unattributed` + `POST /v1/speakers/unattributed/name`).
//!
//! Gated on `DATABASE_URL` (skips cleanly when unset), like the other backend integration
//! tests. Calls the handlers directly (not over HTTP) so we can assert on the typed result.
//! Assertions are existence-based, not exact counts, because the dev DB may hold other
//! unattributed audio; everything is namespaced to a unique device and cleaned up at the end.

use axum::Json;
use axum::extract::State;
use hushai_backend::build_state;
use hushai_backend::config::Config;
use hushai_backend::error::IngestError;
use hushai_backend::speakers::{self, NameUnattributedReq};
use hushai_backend::state::AppState;
use pgvector::Vector;
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// A 192-d unit vector with a single hot dimension — orthogonal hot dims are cosine-distance
/// 1.0 apart (distinct clusters); identical hot dims are distance 0 (same cluster).
fn unit_vec(hot: usize) -> Vector {
    let mut v = vec![0f32; 192];
    v[hot] = 1.0;
    Vector::from(v)
}

async fn make_state() -> Option<AppState> {
    let database_url = std::env::var("DATABASE_URL").ok()?;
    let blob_dir = std::env::temp_dir().join(format!("hushai-unattr-{}", Uuid::now_v7()));
    let config = Config {
        database_url,
        blob_dir,
        device_token: "test-token".into(),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        max_body_bytes: 1024 * 1024,
        concurrency_cap: 64,
        disk_watermark_bytes: 0,
        db_max_connections: 5,
        db_acquire_timeout_secs: 5,
        request_timeout_secs: 30,
    };
    Some(build_state(config).await.expect("build state"))
}

/// Insert a NULL-speaker `speaker_segments` row (with a transcript sentence) for one segment,
/// using `hot` as the embedding direction. Returns the segment_id.
async fn insert_null_segment(
    pool: &PgPool,
    device_id: &str,
    session_id: Uuid,
    sequence: i64,
    hot: usize,
    text: &str,
) -> Uuid {
    let segment_id = Uuid::now_v7();
    let stream_id = "cam0-audio";

    sqlx::query(
        "INSERT INTO segments (segment_id, device_id, stream_id, session_id, sequence, \
            media_type, codec, container, capture_start_unix_nanos, monotonic_start_nanos, \
            duration_nanos, content_sha256, byte_len, blob_uri, storage_backend) \
         VALUES ($1,$2,$3,$4,$5,1,'aac','mp4',$5,0,2000000000,$6,100,$7,'file')",
    )
    .bind(segment_id)
    .bind(device_id)
    .bind(stream_id)
    .bind(session_id)
    .bind(sequence)
    .bind(vec![0u8; 32])
    .bind(format!("file:///nonexistent/{segment_id}"))
    .execute(pool)
    .await
    .unwrap();

    // A transcript sentence (NULL speaker), so the cluster has a sample utterance + we can
    // verify the repoint of transcript_sentences.speaker_id.
    sqlx::query(
        "INSERT INTO transcript_sentences \
            (segment_id, device_id, text, start_unix_nanos, end_unix_nanos, embedding, \
             embedding_model, embedding_dim, speaker_id) \
         VALUES ($1,$2,$3,$4,$5,$6,'test',1024,NULL)",
    )
    .bind(segment_id)
    .bind(device_id)
    .bind(text)
    .bind(sequence)
    .bind(sequence + 1)
    .bind(Vector::from(vec![0f32; 1024]))
    .execute(pool)
    .await
    .unwrap();

    // The unattributed voiceprint (speaker_id NULL, quality 'marginal') the surfacing reads.
    sqlx::query(
        "INSERT INTO speaker_segments \
            (segment_id, device_id, speaker_id, start_unix_nanos, end_unix_nanos, embedding, quality) \
         VALUES ($1,$2,NULL,$3,$4,$5,'marginal')",
    )
    .bind(segment_id)
    .bind(device_id)
    .bind(sequence)
    .bind(sequence + 1)
    .bind(unit_vec(hot))
    .execute(pool)
    .await
    .unwrap();

    segment_id
}

async fn cleanup(pool: &PgPool, device_id: &str) {
    for sql in [
        "DELETE FROM speakers WHERE first_seen_device_id=$1",
        "DELETE FROM transcript_sentences WHERE device_id=$1",
        "DELETE FROM speaker_segments WHERE device_id=$1",
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
async fn unattributed_cluster_name_and_idempotency() {
    let Some(state) = make_state().await else {
        eprintln!("SKIP unattributed_cluster_name_and_idempotency: DATABASE_URL unset");
        return;
    };
    let pool = state.pool.clone();
    let device = format!("test-unattr-{}", Uuid::now_v7());

    // Two distinct voices among unattributed audio: cluster A (hot=0) ×3, cluster B (hot=1) ×3.
    let session = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO devices (device_id, source_kind) VALUES ($1,'test') ON CONFLICT DO NOTHING",
    )
    .bind(&device)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO sessions (session_id, device_id) VALUES ($1,$2)")
        .bind(session)
        .bind(&device)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO streams (session_id, stream_id, device_id, media_type, codec, container) VALUES ($1,'cam0-audio',$2,1,'aac','mp4')")
        .bind(session).bind(&device).execute(&pool).await.unwrap();

    let mut a = Vec::new();
    for i in 0..3 {
        a.push(
            insert_null_segment(&pool, &device, session, i, 0, &format!("voice A line {i}")).await,
        );
    }
    let mut b = Vec::new();
    for i in 0..3 {
        b.push(
            insert_null_segment(
                &pool,
                &device,
                session,
                100 + i,
                1,
                &format!("voice B line {i}"),
            )
            .await,
        );
    }

    // 1. The surfacing groups our A segments together and our B segments together — find the
    //    cluster that contains a[0] and assert it is exactly our A set (none of B).
    let Json(clusters) = speakers::list_unattributed(State(state.clone()))
        .await
        .unwrap();
    let a_set: std::collections::HashSet<Uuid> = a.iter().copied().collect();
    let b_set: std::collections::HashSet<Uuid> = b.iter().copied().collect();
    let a_cluster = clusters
        .iter()
        .find(|c| c.segment_ids.contains(&a[0]))
        .expect("a cluster containing our voice-A segments");
    let got: std::collections::HashSet<Uuid> = a_cluster.segment_ids.iter().copied().collect();
    assert_eq!(got, a_set, "cluster A should be exactly our 3 A segments");
    assert!(got.is_disjoint(&b_set), "cluster A must not absorb voice B");
    assert!(
        !a_cluster.sample_utterances.is_empty(),
        "cluster carries sample utterances"
    );

    // 2. Name cluster A -> mints a speaker and repoints its segments.
    let req = NameUnattributedReq {
        display_name: "  Voice A  ".into(),
        segment_ids: a.clone(),
    };
    let Json(row) = speakers::name_unattributed(State(state.clone()), Json(req))
        .await
        .unwrap();
    assert_eq!(
        row.display_name.as_deref(),
        Some("Voice A"),
        "name is trimmed"
    );
    assert_eq!(row.n_samples, 3);
    let new_id = row.speaker_id;

    // speaker_segments + transcript_sentences for A now point at the new speaker; B stays NULL.
    let a_attr: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM speaker_segments WHERE segment_id = ANY($1) AND speaker_id = $2",
    )
    .bind(a.clone())
    .bind(new_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(a_attr, 3, "all A speaker_segments repointed");
    let a_sent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM transcript_sentences WHERE segment_id = ANY($1) AND speaker_id = $2",
    )
    .bind(a.clone())
    .bind(new_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(a_sent, 3, "all A transcript_sentences repointed");
    let b_null: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM speaker_segments WHERE segment_id = ANY($1) AND speaker_id IS NULL",
    )
    .bind(b.clone())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(b_null, 3, "voice B untouched");
    // The minted speaker has a centroid (so it can match future audio).
    let has_centroid: bool =
        sqlx::query("SELECT centroid IS NOT NULL AS c FROM speakers WHERE speaker_id=$1")
            .bind(new_id)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("c");
    assert!(has_centroid, "minted speaker has a centroid");

    // 3. Idempotency / race guard: re-naming the same (now-attributed) segments is refused.
    let again = NameUnattributedReq {
        display_name: "Voice A again".into(),
        segment_ids: a.clone(),
    };
    let res = speakers::name_unattributed(State(state.clone()), Json(again)).await;
    assert!(
        matches!(res, Err(IngestError::BadRequest(_))),
        "re-naming already-attributed segments must be rejected, got {res:?}"
    );

    cleanup(&pool, &device).await;
}
