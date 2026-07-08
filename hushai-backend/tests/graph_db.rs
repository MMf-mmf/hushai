//! Live-DB integration coverage for the Gotham entity-graph fold (migrations 0028–0030):
//! `graph_pass::rebuild` materializes the batch-local cross-subject edges (`arrived_with_vehicle`,
//! `co_present`) correctly when the WHOLE scenario is folded in one batch.
//!
//! This is the regression guard for the confirmed PR3 finding: the incremental worker fold
//! correlates person↔plate / co-presence edges only within a single pass's freshly-drained visits,
//! so subjects that drain in separate passes never correlate. The eval sidesteps that by triggering
//! one authoritative rebuild after injection; this test proves that rebuild path end-to-end at the
//! DB level (no media pipeline): seed subject-bearing `events`, rebuild, assert the edges + counts.
//!
//! Gated on `DATABASE_URL` (skips cleanly when unset), same idiom as tests/threading_db.rs:
//! namespaced to a unique device + fresh catalog ids, cleaned up at the end. `rebuild` TRUNCATEs
//! `entity_edges` globally (derived data), so this must be the only graph test in its binary.

use hushai_backend::graph::GraphCfg;
use hushai_backend::graph_pass::{rebuild, GraphOpts};
use sqlx::{PgPool, Row};
use uuid::Uuid;

const SEC: i64 = 1_000_000_000;

fn now_ns() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as i64
}

async fn connect() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let pool = sqlx::postgres::PgPoolOptions::new().max_connections(5).connect(&url).await.expect("connect");
    sqlx::migrate!("./migrations").run(&pool).await.expect("migrate");
    Some(pool)
}

/// Insert a subject-bearing event (the graph's fold input). `updated_at` is pinned a few seconds in
/// the past so a later-transaction rebuild with `grace_secs=0` treats it as eligible; capture times
/// are pinned ~10 days back so the drain's in-progress guard (`end <= now - slack`) passes.
async fn seed_event(pool: &PgPool, device: &str, stype: &str, sid: Uuid, start: i64, end: i64) {
    sqlx::query(
        "INSERT INTO events (event_id, event_type, subject_type, subject_id, device_id, \
             start_unix_nanos, end_unix_nanos, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, now() - interval '5 seconds')",
    )
    .bind(Uuid::now_v7())
    .bind(if stype == "plate" { "plate_seen" } else { "person_seen" })
    .bind(stype)
    .bind(sid)
    .bind(device)
    .bind(start)
    .bind(end)
    .execute(pool)
    .await
    .unwrap();
}

/// Seed a CLOSED conversation (a voice↔face binding-trial input) with one speaker on a device.
async fn seed_conversation(pool: &PgPool, device: &str, speaker: Uuid, t0: i64, t1: i64) {
    sqlx::query(
        "INSERT INTO conversations (conversation_id, status, primary_device_id, \
             started_at_unix_nanos, ended_at_unix_nanos, speaker_ids, updated_at) \
         VALUES ($1, 'closed', $2, $3, $4, ARRAY[$5]::uuid[], now() - interval '5 seconds')",
    )
    .bind(Uuid::now_v7())
    .bind(device)
    .bind(t0)
    .bind(t1)
    .bind(speaker)
    .execute(pool)
    .await
    .unwrap();
}

async fn edge_obs(pool: &PgPool, kind: &str, a_type: &str, a: &str, b_type: &str, b: &str) -> Option<i64> {
    // Match either endpoint order (undirected edges are producer-canonicalized).
    let row = sqlx::query(
        "SELECT observation_count FROM entity_edges WHERE edge_type = $1 \
           AND ((src_type=$2 AND src_id=$3 AND dst_type=$4 AND dst_id=$5) \
             OR (src_type=$4 AND src_id=$5 AND dst_type=$2 AND dst_id=$3))",
    )
    .bind(kind).bind(a_type).bind(a).bind(b_type).bind(b)
    .fetch_optional(pool)
    .await
    .unwrap();
    row.map(|r| r.get::<i64, _>("observation_count"))
}

/// The binding review-queue status of a `same_identity_candidate` edge (either endpoint order).
async fn binding_status(pool: &PgPool, a: &str, b: &str) -> Option<String> {
    let row = sqlx::query(
        "SELECT status FROM entity_edges WHERE edge_type = 'same_identity_candidate' \
           AND ((src_id=$1 AND dst_id=$2) OR (src_id=$2 AND dst_id=$1))",
    )
    .bind(a).bind(b)
    .fetch_optional(pool)
    .await
    .unwrap();
    row.and_then(|r| r.get::<Option<String>, _>("status"))
}

#[tokio::test]
async fn rebuild_correlates_cross_subject_edges_in_one_batch() {
    let Some(pool) = connect().await else {
        eprintln!("DATABASE_URL unset — skipping graph_db integration test");
        return;
    };

    let device = format!("graphtest-{}", Uuid::now_v7());
    let alice = Uuid::now_v7();
    let bob = Uuid::now_v7();
    let carol = Uuid::now_v7();
    let plate = Uuid::now_v7();
    let dave = Uuid::now_v7(); // person for the voice↔face binding scenario
    let dave_spk = Uuid::now_v7(); // his speaker
    let base = now_ns() - 10 * 86_400 * SEC; // ~10 days ago: safely past the drain's slack guard.

    sqlx::query("INSERT INTO devices (device_id, source_kind) VALUES ($1,'test') ON CONFLICT DO NOTHING")
        .bind(&device).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO persons (person_id) VALUES ($1),($2),($3),($4)")
        .bind(alice).bind(bob).bind(carol).bind(dave).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO speakers (speaker_id) VALUES ($1)").bind(dave_spk).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO license_plates (plate_id, plate_text, plate_text_norm) VALUES ($1,'EMD 774','EMD774')")
        .bind(plate).execute(&pool).await.unwrap();

    // Timeline (one device): Alice arrives with the plate TWICE (two visits, >slack apart), Bob once,
    // Carol overlaps Alice's first visit. Plate co-sighted with each within the 180s vehicle window.
    seed_event(&pool, &device, "person", alice, base, base + 2 * SEC).await; // Alice v1
    seed_event(&pool, &device, "person", carol, base + SEC, base + 3 * SEC).await; // overlaps Alice v1
    seed_event(&pool, &device, "plate", plate, base + 30 * SEC, base + 32 * SEC).await; // with v1 (30s<180)
    seed_event(&pool, &device, "person", alice, base + 3600 * SEC, base + 3602 * SEC).await; // Alice v2
    seed_event(&pool, &device, "plate", plate, base + 3630 * SEC, base + 3632 * SEC).await; // with v2 (30s<180)
    seed_event(&pool, &device, "person", bob, base + 7200 * SEC, base + 7202 * SEC).await; // Bob
    seed_event(&pool, &device, "plate", plate, base + 7230 * SEC, base + 7232 * SEC).await; // with Bob (30s<180)

    // Binding scenario (isolated in time from the vehicle scenario so only Dave is "present"): Dave's
    // face appears during 3 CLOSED conversations where his voice speaks → 3 binding trials → the
    // same_identity_candidate edge surfaces to 'candidate' (>= GRAPH_BIND_MIN_SESSIONS=3). This guards
    // the upsert_edge status-persist fix: without it, status is frozen NULL at the first trial.
    for k in 0..3i64 {
        let t = base + (10_000 + k * 3600) * SEC;
        seed_event(&pool, &device, "person", dave, t, t + 12 * SEC).await; // Dave's face present
        seed_conversation(&pool, &device, dave_spk, t, t + 12 * SEC).await; // Dave's voice speaks
    }

    // One authoritative fold of the WHOLE scenario: grace 0 (fresh events eligible) + a huge budget
    // so everything drains in ONE batch (so cross-subject visits co-occur — the fix under test).
    let opts = GraphOpts {
        cfg: GraphCfg { grace_secs: 0, ..GraphCfg::default() },
        max_events_per_pass: 1_000_000,
        tz_offset_secs: 0,
    };
    rebuild(&pool, &opts).await.expect("rebuild");

    let (a, b, c, p) = (alice.to_string(), bob.to_string(), carol.to_string(), plate.to_string());

    // THE fix: cross-subject person→plate correlation only forms in a single-batch fold.
    assert_eq!(
        edge_obs(&pool, "arrived_with_vehicle", "person", &a, "plate", &p).await,
        Some(2),
        "Alice arrived with the plate on 2 distinct visits → obs 2"
    );
    assert_eq!(
        edge_obs(&pool, "arrived_with_vehicle", "person", &b, "plate", &p).await,
        Some(1),
        "Bob's single co-sighting → obs 1 (below a >=2 evidence bar)"
    );
    // co_present (the other batch-local edge type): Carol overlaps Alice's first visit.
    assert!(
        edge_obs(&pool, "co_present", "person", &a, "person", &c).await.is_some(),
        "Alice and Carol overlap on the same device → co_present edge"
    );
    // visits_place sanity: Alice's two visits → 2 observations to the device.
    assert_eq!(
        edge_obs(&pool, "visits_place", "person", &a, "device", &device).await,
        Some(2),
        "Alice made 2 visits to the device"
    );

    // Binding surfaced: the status-persist fix (upsert_edge ON CONFLICT ... SET status).
    let (d, ds) = (dave.to_string(), dave_spk.to_string());
    assert_eq!(
        edge_obs(&pool, "same_identity_candidate", "person", &d, "speaker", &ds).await,
        Some(3),
        "3 conversations with Dave present → binding obs 3"
    );
    assert_eq!(
        binding_status(&pool, &d, &ds).await.as_deref(),
        Some("candidate"),
        "binding must SURFACE to 'candidate' (guards the upsert_edge status-persist fix)"
    );

    // cleanup (device-namespaced + our catalog ids; entity_edges is derived/global — drop ours).
    sqlx::query("DELETE FROM entity_edges WHERE src_id = ANY($1) OR dst_id = ANY($1)")
        .bind(vec![a, b, c, p, d, ds, device.clone()]).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM conversations WHERE primary_device_id = $1").bind(&device).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM events WHERE device_id = $1").bind(&device).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM persons WHERE person_id = ANY($1)")
        .bind(vec![alice, bob, carol, dave]).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM speakers WHERE speaker_id = $1").bind(dave_spk).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM license_plates WHERE plate_id = $1").bind(plate).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM devices WHERE device_id = $1").bind(&device).execute(&pool).await.unwrap();
}
