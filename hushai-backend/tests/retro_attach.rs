//! Integration coverage for the retro-attach passes: naming/owning a voice pulls in its
//! unattributed history on MULTI-VECTOR evidence, folds anonymous duplicates at the auto-heal
//! tightness, and never loosens anything (single-agreement candidates and other speakers'
//! segments are untouched; named↔named never auto-folds).
//!
//! Gated on `DATABASE_URL` (skips cleanly when unset), same idiom as tests/unattributed.rs:
//! handlers called directly, everything namespaced to a unique device, cleaned up at the end.

use axum::Json;
use axum::extract::{Path, State};
use hushai_backend::build_state;
use hushai_backend::config::Config;
use hushai_backend::speakers::{self, RenameReq};
use hushai_backend::state::AppState;
use pgvector::Vector;
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// A 192-d unit vector with a single hot dimension.
fn unit_vec(hot: usize) -> Vector {
    let mut v = vec![0f32; 192];
    v[hot] = 1.0;
    Vector::from(v)
}

/// A unit vector at a chosen cosine to `unit_vec(a)`, leaning into dimension `b`:
/// cos(angle to e_a) == `cos_a`, so cosine DISTANCE to e_a is `1 - cos_a`.
fn mixed_vec(a: usize, b: usize, cos_a: f32) -> Vector {
    let mut v = vec![0f32; 192];
    v[a] = cos_a;
    v[b] = (1.0 - cos_a * cos_a).sqrt();
    Vector::from(v)
}

async fn make_state() -> Option<AppState> {
    let database_url = std::env::var("DATABASE_URL").ok()?;
    let blob_dir = std::env::temp_dir().join(format!("hushai-retro-{}", Uuid::now_v7()));
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

/// One segment + one speaker_segments voiceprint (and a matching transcript sentence).
/// `speaker`: Some(id) = attributed (quality 'clean'); None = unattributed ('marginal').
async fn insert_voiceprint(
    pool: &PgPool,
    device: &str,
    session: Uuid,
    sequence: i64,
    emb: Vector,
    speaker: Option<Uuid>,
) -> Uuid {
    let segment_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO segments (segment_id, device_id, stream_id, session_id, sequence, \
            media_type, codec, container, capture_start_unix_nanos, monotonic_start_nanos, \
            duration_nanos, content_sha256, byte_len, blob_uri, storage_backend) \
         VALUES ($1,$2,'cam0-audio',$3,$4,1,'aac','mp4',$4,0,2000000000,$5,100,$6,'file')",
    )
    .bind(segment_id)
    .bind(device)
    .bind(session)
    .bind(sequence)
    .bind(vec![0u8; 32])
    .bind(format!("file:///nonexistent/{segment_id}"))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO transcript_sentences \
            (segment_id, device_id, text, start_unix_nanos, end_unix_nanos, embedding, \
             embedding_model, embedding_dim, speaker_id) \
         VALUES ($1,$2,$3,$4,$5,$6,'test',1024,$7)",
    )
    .bind(segment_id)
    .bind(device)
    .bind(format!("line {sequence}"))
    .bind(sequence)
    .bind(sequence + 1)
    .bind(Vector::from(vec![0f32; 1024]))
    .bind(speaker.map(|s| s.to_string()))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO speaker_segments \
            (segment_id, device_id, speaker_id, start_unix_nanos, end_unix_nanos, embedding, quality) \
         VALUES ($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(segment_id)
    .bind(device)
    .bind(speaker)
    .bind(sequence)
    .bind(sequence + 1)
    .bind(emb)
    .bind(if speaker.is_some() { "clean" } else { "marginal" })
    .execute(pool)
    .await
    .unwrap();
    segment_id
}

async fn insert_speaker(pool: &PgPool, device: &str, name: Option<&str>, centroid: Vector) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO speakers (speaker_id, centroid, n_samples, display_name, first_seen_device_id) \
         VALUES ($1,$2,0,$3,$4)",
    )
    .bind(id)
    .bind(centroid)
    .bind(name)
    .bind(device)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn speaker_of_segment(pool: &PgPool, segment_id: Uuid) -> Option<Uuid> {
    sqlx::query("SELECT speaker_id FROM speaker_segments WHERE segment_id = $1")
        .bind(segment_id)
        .fetch_one(pool)
        .await
        .unwrap()
        .get("speaker_id")
}

async fn cleanup(pool: &PgPool, device: &str) {
    for sql in [
        "DELETE FROM entity_profiles WHERE subject_id IN (SELECT speaker_id FROM speakers WHERE first_seen_device_id=$1)",
        "DELETE FROM speakers WHERE first_seen_device_id=$1",
        "DELETE FROM transcript_sentences WHERE device_id=$1",
        "DELETE FROM speaker_segments WHERE device_id=$1",
        "DELETE FROM segment_transcription_status WHERE segment_id IN (SELECT segment_id FROM segments WHERE device_id=$1)",
        "DELETE FROM segments WHERE device_id=$1",
        "DELETE FROM streams WHERE device_id=$1",
        "DELETE FROM sessions WHERE device_id=$1",
        "DELETE FROM devices WHERE device_id=$1",
    ] {
        let _ = sqlx::query(sql).bind(device).execute(pool).await;
    }
}

/// Naming a voice attaches its multi-agree NULL history (quality stays marginal, transcript
/// rows repointed, centroid recomputed), leaves single-agree and far candidates alone, and a
/// second pass is a no-op.
#[tokio::test]
async fn rename_retro_attaches_multi_agree_null_history() {
    let Some(state) = make_state().await else {
        eprintln!("SKIP rename_retro_attaches_multi_agree_null_history: DATABASE_URL unset");
        return;
    };
    let pool = state.pool.clone();
    let device = format!("test-retro-{}", Uuid::now_v7());
    let session = Uuid::now_v7();
    seed_device(&pool, &device, session).await;

    // Target voice: two raw directions — e0 (two clean segments) and e1 (one clean segment).
    let casey = insert_speaker(&pool, &device, None, unit_vec(0)).await;
    insert_voiceprint(&pool, &device, session, 1, unit_vec(0), Some(casey)).await;
    insert_voiceprint(&pool, &device, session, 2, unit_vec(0), Some(casey)).await;
    insert_voiceprint(&pool, &device, session, 3, unit_vec(1), Some(casey)).await;

    // Multi-agree NULL: close to BOTH e0 raws (d = 0.1 to each) → attach.
    let multi = insert_voiceprint(&pool, &device, session, 10, mixed_vec(0, 5, 0.9), None).await;
    // Single-agree NULL: close only to the lone e1 raw (d = 0.1), far from the e0 pair → stays.
    let single = insert_voiceprint(&pool, &device, session, 11, mixed_vec(1, 6, 0.9), None).await;
    // Far NULL: orthogonal to everything → stays.
    let far = insert_voiceprint(&pool, &device, session, 12, unit_vec(9), None).await;

    // Naming is the trigger.
    let resp = speakers::rename_speaker(
        State(state.clone()),
        Path(casey),
        Json(RenameReq { display_name: "Casey".into() }),
    )
    .await
    .expect("rename ok");
    assert_eq!(resp.0.display_name.as_deref(), Some("Casey"));

    assert_eq!(speaker_of_segment(&pool, multi).await, Some(casey), "multi-agree attached");
    assert_eq!(speaker_of_segment(&pool, single).await, None, "single-agree NOT attached");
    assert_eq!(speaker_of_segment(&pool, far).await, None, "far candidate NOT attached");

    // Quality untouched (marginal) so the clean-only centroid window never sees it.
    let q: String = sqlx::query("SELECT quality FROM speaker_segments WHERE segment_id = $1")
        .bind(multi)
        .fetch_one(&pool)
        .await
        .unwrap()
        .get("quality");
    assert_eq!(q, "marginal");
    // Transcript attribution repointed too.
    let t: Option<String> =
        sqlx::query("SELECT speaker_id FROM transcript_sentences WHERE segment_id = $1")
            .bind(multi)
            .fetch_one(&pool)
            .await
            .unwrap()
            .get("speaker_id");
    assert_eq!(t.as_deref(), Some(casey.to_string().as_str()));

    // Second pass is a no-op.
    let Json(stats) = speakers::retro_attach_speaker(State(state.clone()), Path(casey))
        .await
        .expect("retro route ok");
    assert_eq!(stats.segments_attached, 0, "idempotent");

    // The running-memory profile recorded the identification moment.
    let profile: Option<String> = sqlx::query(
        "SELECT profile_text FROM entity_profiles WHERE subject_type='speaker' AND subject_id=$1",
    )
    .bind(casey)
    .fetch_optional(&pool)
    .await
    .unwrap()
    .map(|r| r.get("profile_text"));
    assert!(
        profile.unwrap_or_default().contains("identified as Casey"),
        "identification line recorded"
    );

    cleanup(&pool, &device).await;
}

/// Anonymous speakers are never retro-attach targets, and NULL segments near them stay put.
#[tokio::test]
async fn anonymous_speaker_is_not_a_retro_target() {
    let Some(state) = make_state().await else {
        eprintln!("SKIP anonymous_speaker_is_not_a_retro_target: DATABASE_URL unset");
        return;
    };
    let pool = state.pool.clone();
    let device = format!("test-retro-{}", Uuid::now_v7());
    let session = Uuid::now_v7();
    seed_device(&pool, &device, session).await;

    let anon = insert_speaker(&pool, &device, None, unit_vec(2)).await;
    insert_voiceprint(&pool, &device, session, 1, unit_vec(2), Some(anon)).await;
    insert_voiceprint(&pool, &device, session, 2, unit_vec(2), Some(anon)).await;
    let null_near = insert_voiceprint(&pool, &device, session, 10, mixed_vec(2, 5, 0.9), None).await;

    let Json(stats) = speakers::retro_attach_speaker(State(state.clone()), Path(anon))
        .await
        .expect("route ok");
    assert_eq!(stats.segments_attached, 0, "anonymous target refused");
    assert_eq!(speaker_of_segment(&pool, null_near).await, None);

    cleanup(&pool, &device).await;
}

/// Attributed segments are never stolen — retro-attach claims speaker_id IS NULL rows only.
#[tokio::test]
async fn retro_attach_never_steals_attributed_segments() {
    let Some(state) = make_state().await else {
        eprintln!("SKIP retro_attach_never_steals_attributed_segments: DATABASE_URL unset");
        return;
    };
    let pool = state.pool.clone();
    let device = format!("test-retro-{}", Uuid::now_v7());
    let session = Uuid::now_v7();
    seed_device(&pool, &device, session).await;

    let casey = insert_speaker(&pool, &device, Some("Casey"), unit_vec(3)).await;
    insert_voiceprint(&pool, &device, session, 1, unit_vec(3), Some(casey)).await;
    insert_voiceprint(&pool, &device, session, 2, unit_vec(3), Some(casey)).await;
    // Bob's segment sits acoustically near Casey's raws, but it is ATTRIBUTED — untouchable.
    // (Bob is FAR from Casey by centroid/raws elsewhere, so the anonymous-duplicate fold
    // can't claim him either — and he's named, which the fold refuses anyway.)
    let bob = insert_speaker(&pool, &device, Some("Bob"), unit_vec(8)).await;
    let bobs = insert_voiceprint(&pool, &device, session, 3, mixed_vec(3, 8, 0.9), Some(bob)).await;

    let Json(_) = speakers::retro_attach_speaker(State(state.clone()), Path(casey))
        .await
        .expect("route ok");
    assert_eq!(speaker_of_segment(&pool, bobs).await, Some(bob), "attributed row untouched");

    cleanup(&pool, &device).await;
}

/// The fold half: an ANONYMOUS near-duplicate id (auto-heal tightness) folds into the named
/// target on rename; a NAMED near-duplicate never does.
#[tokio::test]
async fn fold_merges_anonymous_duplicate_but_never_named() {
    let Some(state) = make_state().await else {
        eprintln!("SKIP fold_merges_anonymous_duplicate_but_never_named: DATABASE_URL unset");
        return;
    };
    let pool = state.pool.clone();
    let device = format!("test-retro-{}", Uuid::now_v7());
    let session = Uuid::now_v7();
    seed_device(&pool, &device, session).await;

    // Target + an anonymous duplicate whose raws are ~0.05 away (inside the 0.15 fold bar),
    // + a NAMED duplicate at the same distance (must survive).
    let casey = insert_speaker(&pool, &device, None, unit_vec(4)).await;
    insert_voiceprint(&pool, &device, session, 1, unit_vec(4), Some(casey)).await;
    insert_voiceprint(&pool, &device, session, 2, unit_vec(4), Some(casey)).await;
    let anon_dup = insert_speaker(&pool, &device, None, mixed_vec(4, 5, 0.95)).await;
    let anon_seg1 = insert_voiceprint(&pool, &device, session, 3, mixed_vec(4, 5, 0.95), Some(anon_dup)).await;
    insert_voiceprint(&pool, &device, session, 4, mixed_vec(4, 5, 0.95), Some(anon_dup)).await;
    let named_dup = insert_speaker(&pool, &device, Some("Dana"), mixed_vec(4, 6, 0.95)).await;
    insert_voiceprint(&pool, &device, session, 5, mixed_vec(4, 6, 0.95), Some(named_dup)).await;
    insert_voiceprint(&pool, &device, session, 6, mixed_vec(4, 6, 0.95), Some(named_dup)).await;

    let _ = speakers::rename_speaker(
        State(state.clone()),
        Path(casey),
        Json(RenameReq { display_name: "Casey".into() }),
    )
    .await
    .expect("rename ok");

    let anon_alive: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM speakers WHERE speaker_id=$1)")
            .bind(anon_dup)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!anon_alive, "anonymous duplicate folded into Casey");
    assert_eq!(speaker_of_segment(&pool, anon_seg1).await, Some(casey), "dup segments repointed");
    let named_alive: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM speakers WHERE speaker_id=$1)")
            .bind(named_dup)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(named_alive, "NAMED duplicate never auto-folds");

    cleanup(&pool, &device).await;
}
