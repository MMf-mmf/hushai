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
use hushai_backend::graph_pass::{generate_digest_for_date, rebuild, GraphOpts};
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

/// True when a `pattern_anomaly` event of `kind` exists for the subject (Gotham G2).
async fn anomaly_present(pool: &PgPool, subject: Uuid, kind: &str) -> bool {
    let row = sqlx::query(
        "SELECT 1 AS ok FROM events \
         WHERE event_type = 'pattern_anomaly' AND subject_id = $1 AND metadata->>'kind' = $2 LIMIT 1",
    )
    .bind(subject).bind(kind)
    .fetch_optional(pool)
    .await
    .unwrap();
    row.is_some()
}

/// True when a device-keyed `pattern_anomaly` of `kind` exists (Gotham G2 unknown_person_cluster:
/// no catalog subject, matched by `device_id`).
async fn device_anomaly_present(pool: &PgPool, device: &str, kind: &str) -> bool {
    let row = sqlx::query(
        "SELECT 1 AS ok FROM events \
         WHERE event_type = 'pattern_anomaly' AND subject_type = 'device' AND subject_id IS NULL \
           AND device_id = $1 AND metadata->>'kind' = $2 LIMIT 1",
    )
    .bind(device).bind(kind)
    .fetch_optional(pool)
    .await
    .unwrap();
    row.is_some()
}

/// A subject's recomputed `entity_baselines.visits_in_window` (Gotham G2), if the row exists.
async fn baseline_visits(pool: &PgPool, subject: Uuid) -> Option<i64> {
    let row = sqlx::query("SELECT visits_in_window FROM entity_baselines WHERE subject_id = $1")
        .bind(subject)
        .fetch_optional(pool)
        .await
        .unwrap();
    row.map(|r| r.get::<i32, _>("visits_in_window") as i64)
}

#[tokio::test]
async fn rebuild_correlates_cross_subject_edges_in_one_batch() {
    let Some(pool) = connect().await else {
        eprintln!("DATABASE_URL unset — skipping graph_db integration test");
        return;
    };

    // Idempotent pre-clean: end-cleanup runs only on success, so a prior ABORTED run can leave the
    // fixed-norm plate (unique `plate_text_norm='EMD774'`) or `graphtest-*` rows behind and collide.
    // `entity_edges`/`entity_baselines`/`entity_journeys` are TRUNCATEd by `rebuild` below. Order
    // respects the events→devices FK.
    sqlx::query("DELETE FROM events WHERE device_id LIKE 'graphtest-%'").execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM conversations WHERE primary_device_id LIKE 'graphtest-%'").execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM license_plates WHERE plate_text_norm IN ('EMD774','GTAAA','GTBBB')").execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM devices WHERE device_id LIKE 'graphtest-%'").execute(&pool).await.unwrap();

    let device = format!("graphtest-{}", Uuid::now_v7());
    let alice = Uuid::now_v7();
    let bob = Uuid::now_v7();
    let carol = Uuid::now_v7();
    let plate = Uuid::now_v7();
    let dave = Uuid::now_v7(); // person for the voice↔face binding scenario
    let dave_spk = Uuid::now_v7(); // his speaker
    let erin = Uuid::now_v7(); // person for the G2 baseline / off_schedule scenario
    // Wave-2 edge-anomaly scenarios (named ⇒ NOT "unknown"; time-isolated on the same device).
    let frank = Uuid::now_v7(); // first_time_pairing: mature regular A
    let gwen = Uuid::now_v7(); // first_time_pairing: mature regular B
    let heidi = Uuid::now_v7(); // new_vehicle_for_person: a person with two plates
    let plate_a = Uuid::now_v7(); // heidi's established vehicle
    let plate_b = Uuid::now_v7(); // heidi's NEW vehicle
    let unk = [Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7()]; // unknown_person_cluster: 3 anon faces
    let base = now_ns() - 10 * 86_400 * SEC; // ~10 days ago: safely past the drain's slack guard.

    sqlx::query("INSERT INTO devices (device_id, source_kind) VALUES ($1,'test') ON CONFLICT DO NOTHING")
        .bind(&device).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO persons (person_id) VALUES ($1),($2),($3),($4),($5)")
        .bind(alice).bind(bob).bind(carol).bind(dave).bind(erin).execute(&pool).await.unwrap();
    // Named persons for the pairing / vehicle scenarios (display_name set ⇒ excluded from the unknown
    // cluster). The three `unk` persons stay display_name NULL ⇒ they ARE the unknown cluster.
    sqlx::query("INSERT INTO persons (person_id, display_name) VALUES ($1,'Frank'),($2,'Gwen'),($3,'Heidi')")
        .bind(frank).bind(gwen).bind(heidi).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO persons (person_id) VALUES ($1),($2),($3)")
        .bind(unk[0]).bind(unk[1]).bind(unk[2]).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO speakers (speaker_id) VALUES ($1)").bind(dave_spk).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO license_plates (plate_id, plate_text, plate_text_norm) VALUES ($1,'EMD 774','EMD774')")
        .bind(plate).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO license_plates (plate_id, plate_text, plate_text_norm) VALUES ($1,'GT AAA','GTAAA'),($2,'GT BBB','GTBBB')")
        .bind(plate_a).bind(plate_b).execute(&pool).await.unwrap();

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

    // G2 baseline / off_schedule scenario (Erin), temporally isolated ~60 days back so it never
    // co-occurs with the others. Five visits one week apart at the SAME time-of-day establish a
    // mature rhythm in ONE hour-of-week bucket; a LATER sixth visit (+29 days → a different weekday
    // bucket, after the rhythm is set) holds 0/5 of her prior mass → exactly one off_schedule
    // anomaly under the AS-OF model. Alice, with only 2 visits, has an IMMATURE baseline → never
    // flagged. The outlier must come AFTER the regulars (as-of judges vs strictly-earlier visits).
    let base_e = now_ns() - 90 * 86_400 * SEC;
    for k in 0..5i64 {
        let t = base_e + k * 7 * 86_400 * SEC; // weekly → same hour-of-week bucket
        seed_event(&pool, &device, "person", erin, t, t + 12 * SEC).await;
    }
    let outlier = base_e + 29 * 86_400 * SEC; // a later, different-weekday bucket (rhythm established)
    seed_event(&pool, &device, "person", erin, outlier, outlier + 12 * SEC).await;

    // --- Wave-2 EDGE anomalies (all time-isolated on `device`, ~150-200 days back) ---------------
    // first_time_pairing: Frank + Gwen each visit 5× ALONE (mature, ≥ GRAPH_ANOMALY_MIN_VISITS=5),
    // NON-overlapping (6h apart) so they don't co-occur until BOTH are established; then ONE
    // overlapping co-visit forms the first co_present edge (0→1) between two mature regulars → a
    // first_time_pairing anomaly fires for EACH endpoint. (off_schedule may also co-fire on the
    // novel-weekday co-day; it is not asserted here — only the pairing is.)
    let base_pair = now_ns() - 200 * 86_400 * SEC;
    for k in 0..5i64 {
        let tf = base_pair + k * 86_400 * SEC; // Frank, day k @ 00:00
        seed_event(&pool, &device, "person", frank, tf, tf + 2 * SEC).await;
        let tg = base_pair + k * 86_400 * SEC + 6 * 3600 * SEC; // Gwen, day k @ 06:00 (no overlap)
        seed_event(&pool, &device, "person", gwen, tg, tg + 2 * SEC).await;
    }
    let co = base_pair + 5 * 86_400 * SEC; // day 5: Frank + Gwen overlap → first co_present
    seed_event(&pool, &device, "person", frank, co, co + 2 * SEC).await;
    seed_event(&pool, &device, "person", gwen, co, co + 2 * SEC).await;

    // new_vehicle_for_person: Heidi arrives with plate_a (t0), then LATER with plate_b (t1). At t1 she
    // already has an established DIFFERENT-plate edge (plate_a, first_seen < t1) → new_vehicle fires
    // for the plate_b arrival. (No person-maturity requirement for this predicate.)
    let base_veh = now_ns() - 150 * 86_400 * SEC;
    seed_event(&pool, &device, "person", heidi, base_veh, base_veh + 2 * SEC).await;
    seed_event(&pool, &device, "plate", plate_a, base_veh + 30 * SEC, base_veh + 32 * SEC).await;
    seed_event(&pool, &device, "person", heidi, base_veh + 3600 * SEC, base_veh + 3602 * SEC).await;
    seed_event(&pool, &device, "plate", plate_b, base_veh + 3630 * SEC, base_veh + 3632 * SEC).await;

    // unknown_person_cluster: 3 distinct UNKNOWN persons (display_name NULL) overlap in one window on
    // one device → a device-keyed unknown_person_cluster anomaly (≥ GRAPH_ANOMALY_UNKNOWN_CLUSTER_MIN=3).
    let base_clu = now_ns() - 180 * 86_400 * SEC;
    for u in &unk {
        seed_event(&pool, &device, "person", *u, base_clu, base_clu + 4 * SEC).await;
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

    // G2: Erin's baseline recomputed (6 visits in the trailing window) and her lone off-hours visit
    // flags exactly one off_schedule_presence anomaly.
    assert!(
        baseline_visits(&pool, erin).await.is_some_and(|v| v >= 5),
        "Erin's entity_baselines row must be recomputed with >= 5 visits (got {:?})",
        baseline_visits(&pool, erin).await
    );
    assert!(
        anomaly_present(&pool, erin, "off_schedule_presence").await,
        "Erin's +15h off-hours visit must fire an off_schedule_presence anomaly (leave-one-out vs 5 mature regulars)"
    );
    // Negative: Alice (2 visits, immature baseline) must NOT be flagged — the anomaly-storm guard.
    assert!(
        !anomaly_present(&pool, alice, "off_schedule_presence").await,
        "Alice's immature 2-visit baseline must NOT fire off_schedule (visits < GRAPH_ANOMALY_MIN_VISITS)"
    );

    // G2 EDGE anomalies (the three Wave-2 predicates beyond off_schedule).
    // first_time_pairing: the first co_present between two MATURE regulars fires for BOTH endpoints.
    assert!(
        anomaly_present(&pool, frank, "first_time_pairing").await,
        "Frank (mature) meeting Gwen (mature) for the first time fires first_time_pairing"
    );
    assert!(
        anomaly_present(&pool, gwen, "first_time_pairing").await,
        "first_time_pairing is emitted per-endpoint — Gwen gets her own row too"
    );
    // Negative: a mature-vs-immature or single co-sighting must not fire it. Alice+Carol overlap once
    // and are immature (2 and 1 visits) → no first_time_pairing.
    assert!(
        !anomaly_present(&pool, alice, "first_time_pairing").await,
        "Alice's co-presence with Carol is between IMMATURE subjects → no first_time_pairing"
    );
    // new_vehicle_for_person: Heidi's SECOND, different plate fires exactly one anomaly for her.
    assert!(
        anomaly_present(&pool, heidi, "new_vehicle_for_person").await,
        "Heidi arriving with plate_b after an established plate_a fires new_vehicle_for_person"
    );
    // Negative: a person with a SINGLE vehicle never fires it (Alice → one plate only).
    assert!(
        !anomaly_present(&pool, alice, "new_vehicle_for_person").await,
        "Alice has only one vehicle → no new_vehicle_for_person"
    );
    // unknown_person_cluster: 3 anonymous faces co-present on one device → one device-keyed anomaly.
    assert!(
        device_anomaly_present(&pool, &device, "unknown_person_cluster").await,
        "3 distinct unknown persons co-present on one device fire unknown_person_cluster (device-keyed)"
    );

    // G2 Phase E: the daily digest for Erin's OUTLIER civil day surfaces exactly that anomaly and
    // nothing spurious. The outlier is temporally isolated (base_e + 29d = 1 day after her last
    // regular, 61d back), so that day holds ONLY her lone off-hours visit: 1 anomaly, 0 new entities
    // (Erin was first seen 29 days earlier), and the kind appears in the structured `sections`.
    let outlier_day = outlier.div_euclid(SEC).div_euclid(86_400); // tz offset 0
    let digest_date: String = sqlx::query_scalar("SELECT (DATE '1970-01-01' + ($1::int))::text")
        .bind(outlier_day as i32)
        .fetch_one(&pool)
        .await
        .unwrap();
    let sections = generate_digest_for_date(&pool, &opts, &digest_date).await.expect("generate digest");
    assert_eq!(
        sections["counts"]["anomalies"].as_i64(),
        Some(1),
        "outlier-day digest must carry Erin's single off_schedule anomaly (sections={sections})"
    );
    assert_eq!(
        sections["counts"]["new_entities"].as_i64(),
        Some(0),
        "Erin is not a NEW entity on the outlier day (first seen 29 days earlier)"
    );
    assert!(
        serde_json::to_string(&sections).unwrap().contains("off_schedule_presence"),
        "the digest's anomalies section must name the anomaly kind"
    );

    // cleanup (device-namespaced + our catalog ids; entity_edges is derived/global — drop ours).
    let all_persons = vec![alice, bob, carol, dave, erin, frank, gwen, heidi, unk[0], unk[1], unk[2]];
    let edge_ids: Vec<String> = [
        alice, bob, carol, dave, erin, frank, gwen, heidi, plate, plate_a, plate_b, unk[0], unk[1],
        unk[2], dave_spk,
    ]
    .iter()
    .map(|u| u.to_string())
    .chain(std::iter::once(device.clone()))
    .collect();
    sqlx::query("DELETE FROM daily_digests WHERE digest_date = $1::date").bind(&digest_date).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM entity_edges WHERE src_id = ANY($1) OR dst_id = ANY($1)")
        .bind(&edge_ids).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM conversations WHERE primary_device_id = $1").bind(&device).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM events WHERE device_id = $1").bind(&device).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM entity_baselines WHERE subject_id = ANY($1)")
        .bind(&all_persons).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM persons WHERE person_id = ANY($1)")
        .bind(&all_persons).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM speakers WHERE speaker_id = $1").bind(dave_spk).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM license_plates WHERE plate_id = ANY($1)")
        .bind(vec![plate, plate_a, plate_b]).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM devices WHERE device_id = $1").bind(&device).execute(&pool).await.unwrap();
}
