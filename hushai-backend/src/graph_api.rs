//! Gotham graph read/admin API (spec: Gotham.md §1.7). Backend serves STRUCTURE (schema owner,
//! same bearer plane as the rest of `/v1/*`, reached through the viewer gateway with server-side
//! token injection + audit). hushai-rag reads the tables directly in-process for chat enrichment.
//!
//! Every GET maps 1:1 to a G3 tool. Responses are built as `serde_json::Value` from rows (no
//! catalog-join structs yet — labels are joined lazily where cheap). Journeys/digests are Wave
//! 4/2 producers; their endpoints return the (empty) table contract now so clients can bind early.

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder, Row};
use uuid::Uuid;

use crate::error::IngestError;
use crate::graph::NodeType;
use crate::graph_pass;
use crate::state::AppState;

fn validate_node_type(s: &str) -> Result<&'static str, IngestError> {
    NodeType::parse(s)
        .map(|t| t.as_str())
        .ok_or(IngestError::BadRequest("node type must be person|speaker|plate|device".into()))
}

/// Serialize one `entity_edges` row to the API shape.
fn edge_json(r: &sqlx::postgres::PgRow) -> Value {
    json!({
        "edge_id": r.get::<Uuid, _>("edge_id").to_string(),
        "edge_type": r.get::<String, _>("edge_type"),
        "src": { "type": r.get::<String, _>("src_type"), "id": r.get::<String, _>("src_id") },
        "dst": { "type": r.get::<String, _>("dst_type"), "id": r.get::<String, _>("dst_id") },
        "observation_count": r.get::<i64, _>("observation_count"),
        "first_seen_unix_nanos": r.try_get::<Option<i64>, _>("first_seen_unix_nanos").ok().flatten(),
        "last_seen_unix_nanos": r.try_get::<Option<i64>, _>("last_seen_unix_nanos").ok().flatten(),
        "confidence": r.try_get::<Option<f32>, _>("confidence").ok().flatten(),
        "evidence": r.try_get::<Value, _>("evidence").unwrap_or(json!([])),
        "metadata": r.try_get::<Value, _>("metadata").unwrap_or(json!({})),
        "status": r.try_get::<Option<String>, _>("status").ok().flatten(),
    })
}

// sqlx 0.9 only accepts `&'static str` for `query()`; dynamic `format!` strings are gated. So the
// edge column list is inlined literally at each call site (kept identical — see `edge_json`).
// For the QueryBuilder path (`list_edges`) it's pushed as a fragment where dynamic is allowed.
const EDGE_COLS: &str = "edge_id, edge_type, src_type, src_id, dst_type, dst_id, observation_count, \
    first_seen_unix_nanos, last_seen_unix_nanos, confidence, evidence, metadata, status";

// ---------------------------------------------------------------------------------------------
// GET /v1/graph/entities/{type}/{id}
// ---------------------------------------------------------------------------------------------

pub async fn entity_page(
    State(st): State<AppState>,
    Path((ntype, id)): Path<(String, String)>,
) -> Result<Json<Value>, IngestError> {
    let nt = validate_node_type(&ntype)?;
    let rows = sqlx::query(
        "SELECT edge_id, edge_type, src_type, src_id, dst_type, dst_id, observation_count, \
                first_seen_unix_nanos, last_seen_unix_nanos, confidence, evidence, metadata, status \
         FROM entity_edges \
         WHERE (src_type = $1 AND src_id = $2) OR (dst_type = $1 AND dst_id = $2) \
         ORDER BY edge_type, observation_count DESC, edge_id LIMIT 2000",
    )
    .bind(nt)
    .bind(&id)
    .fetch_all(&st.pool)
    .await?;

    // Group edges by type.
    let mut grouped: std::collections::BTreeMap<String, Vec<Value>> = Default::default();
    for r in &rows {
        grouped.entry(r.get::<String, _>("edge_type")).or_default().push(edge_json(r));
    }

    // Profile text (0024) + baseline (0029) join, catalog-only types.
    let (profile, baseline) = if nt != "device" {
        if let Ok(uid) = Uuid::parse_str(&id) {
            let p: Option<String> = sqlx::query_scalar(
                "SELECT profile_text FROM entity_profiles WHERE subject_type = $1 AND subject_id = $2",
            )
            .bind(nt).bind(uid)
            .fetch_optional(&st.pool).await.ok().flatten();
            let b: Option<Value> = sqlx::query_scalar(
                "SELECT to_jsonb(entity_baselines) FROM entity_baselines WHERE subject_type = $1 AND subject_id = $2",
            )
            .bind(nt).bind(uid)
            .fetch_optional(&st.pool).await.ok().flatten();
            (p, b)
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    Ok(Json(json!({
        "entity": { "type": nt, "id": id },
        "edges_by_type": grouped,
        "edge_count": rows.len(),
        "profile_text": profile,
        "baseline": baseline,
    })))
}

// ---------------------------------------------------------------------------------------------
// GET /v1/graph/entities/{type}/{id}/timeline
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct TimelineQuery {
    pub limit: Option<i64>,
    pub before_unix_nanos: Option<i64>,
}

pub async fn entity_timeline(
    State(st): State<AppState>,
    Path((ntype, id)): Path<(String, String)>,
    Query(q): Query<TimelineQuery>,
) -> Result<Json<Value>, IngestError> {
    let nt = validate_node_type(&ntype)?;
    let uid = Uuid::parse_str(&id).map_err(|_| IngestError::BadRequest("id must be a uuid".into()))?;
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let before = q.before_unix_nanos.unwrap_or(i64::MAX);

    let rows = sqlx::query(
        "SELECT event_id, device_id, event_type, subject_type, subject_label, segment_id, \
                start_unix_nanos, end_unix_nanos \
         FROM events \
         WHERE subject_type = $1 AND subject_id = $2 AND start_unix_nanos < $3 \
         ORDER BY start_unix_nanos DESC LIMIT $4",
    )
    .bind(nt).bind(uid).bind(before).bind(limit)
    .fetch_all(&st.pool)
    .await?;

    let items: Vec<Value> = rows.iter().map(|r| json!({
        "kind": "event",
        "event_id": r.get::<Uuid, _>("event_id").to_string(),
        "device_id": r.try_get::<Option<String>, _>("device_id").ok().flatten(),
        "event_type": r.get::<String, _>("event_type"),
        "label": r.try_get::<Option<String>, _>("subject_label").ok().flatten(),
        "segment_id": r.try_get::<Option<Uuid>, _>("segment_id").ok().flatten().map(|s| s.to_string()),
        "start_unix_nanos": r.get::<i64, _>("start_unix_nanos"),
        "end_unix_nanos": r.get::<i64, _>("end_unix_nanos"),
    })).collect();
    Ok(Json(json!({ "entity": {"type": nt, "id": id}, "items": items })))
}

// ---------------------------------------------------------------------------------------------
// GET /v1/graph/edges
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct EdgeQuery {
    #[serde(rename = "type")]
    pub edge_type: Option<String>,
    /// Endpoint filter, "type:id".
    pub node: Option<String>,
    pub min_confidence: Option<f32>,
    pub since: Option<i64>,
    pub limit: Option<i64>,
}

pub async fn list_edges(
    State(st): State<AppState>,
    Query(q): Query<EdgeQuery>,
) -> Result<Json<Vec<Value>>, IngestError> {
    let limit = q.limit.unwrap_or(200).clamp(1, 2000);
    let (node_type, node_id) = match &q.node {
        Some(n) => {
            let (t, i) = n.split_once(':').ok_or(IngestError::BadRequest("node must be type:id".into()))?;
            (Some(validate_node_type(t)?.to_string()), Some(i.to_string()))
        }
        None => (None, None),
    };

    let mut qb: QueryBuilder<Postgres> =
        QueryBuilder::new(format!("SELECT {EDGE_COLS} FROM entity_edges WHERE 1=1"));
    if let Some(t) = &q.edge_type {
        qb.push(" AND edge_type = ").push_bind(t.clone());
    }
    if let (Some(t), Some(i)) = (&node_type, &node_id) {
        qb.push(" AND ((src_type = ").push_bind(t.clone()).push(" AND src_id = ").push_bind(i.clone())
          .push(") OR (dst_type = ").push_bind(t.clone()).push(" AND dst_id = ").push_bind(i.clone()).push("))");
    }
    if let Some(c) = q.min_confidence {
        qb.push(" AND confidence >= ").push_bind(c);
    }
    if let Some(s) = q.since {
        qb.push(" AND last_seen_unix_nanos >= ").push_bind(s);
    }
    qb.push(" ORDER BY last_seen_unix_nanos DESC NULLS LAST, edge_id LIMIT ").push_bind(limit);
    let rows = qb.build().fetch_all(&st.pool).await?;
    Ok(Json(rows.iter().map(edge_json).collect()))
}

// ---------------------------------------------------------------------------------------------
// GET /v1/graph/neighbors/{type}/{id}
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct NeighborQuery {
    pub hops: Option<i32>,
    pub min_confidence: Option<f32>,
    pub edge_types: Option<String>, // reserved for filtering; unused in the bounded BFS below
}

pub async fn neighbors(
    State(st): State<AppState>,
    Path((ntype, id)): Path<(String, String)>,
    Query(q): Query<NeighborQuery>,
) -> Result<Json<Value>, IngestError> {
    let nt = validate_node_type(&ntype)?;
    let hops = q.hops.unwrap_or(2).clamp(1, 3); // hops ≤ 3 hard cap (§1.7)
    let min_conf = q.min_confidence;

    // Bounded node-frontier BFS (UNION dedups; depth-capped so it terminates). Undirected walk.
    let nodes = sqlx::query(
        "WITH RECURSIVE frontier(node_type, node_id, depth) AS ( \
            SELECT $1::text, $2::text, 0 \
            UNION \
            SELECT nb.node_type, nb.node_id, f.depth + 1 \
            FROM frontier f \
            JOIN LATERAL ( \
                SELECT e.dst_type AS node_type, e.dst_id AS node_id FROM entity_edges e \
                    WHERE e.src_type = f.node_type AND e.src_id = f.node_id \
                      AND ($4::real IS NULL OR e.confidence >= $4) \
                UNION \
                SELECT e.src_type, e.src_id FROM entity_edges e \
                    WHERE e.dst_type = f.node_type AND e.dst_id = f.node_id \
                      AND ($4::real IS NULL OR e.confidence >= $4) \
            ) nb ON true \
            WHERE f.depth < $3 \
        ) \
        SELECT node_type, node_id, min(depth) AS depth FROM frontier \
        WHERE NOT (node_type = $1 AND node_id = $2) \
        GROUP BY node_type, node_id ORDER BY depth, node_type, node_id LIMIT 500",
    )
    .bind(nt).bind(&id).bind(hops).bind(min_conf)
    .fetch_all(&st.pool)
    .await?;

    let neighbor_nodes: Vec<Value> = nodes.iter().map(|r| json!({
        "type": r.get::<String, _>("node_type"),
        "id": r.get::<String, _>("node_id"),
        "depth": r.get::<i32, _>("depth"),
    })).collect();

    // The origin's direct (depth-1) edges, for evidence.
    let direct = sqlx::query(
        "SELECT edge_id, edge_type, src_type, src_id, dst_type, dst_id, observation_count, \
                first_seen_unix_nanos, last_seen_unix_nanos, confidence, evidence, metadata, status \
         FROM entity_edges \
         WHERE ((src_type=$1 AND src_id=$2) OR (dst_type=$1 AND dst_id=$2)) \
           AND ($3::real IS NULL OR confidence >= $3) \
         ORDER BY observation_count DESC, edge_id LIMIT 500",
    )
    .bind(nt).bind(&id).bind(min_conf)
    .fetch_all(&st.pool)
    .await?;

    Ok(Json(json!({
        "origin": {"type": nt, "id": id},
        "hops": hops,
        "neighbors": neighbor_nodes,
        "edges": direct.iter().map(edge_json).collect::<Vec<_>>(),
    })))
}

// ---------------------------------------------------------------------------------------------
// GET /v1/graph/path?from=type:id&to=type:id&max_hops=
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PathQuery {
    pub from: String,
    pub to: String,
    pub max_hops: Option<i32>,
}

pub async fn shortest_path(
    State(st): State<AppState>,
    Query(q): Query<PathQuery>,
) -> Result<Json<Value>, IngestError> {
    let (ft, fi) = q.from.split_once(':').ok_or(IngestError::BadRequest("from must be type:id".into()))?;
    let (tt, ti) = q.to.split_once(':').ok_or(IngestError::BadRequest("to must be type:id".into()))?;
    let ft = validate_node_type(ft)?;
    let tt = validate_node_type(tt)?;
    let max_hops = q.max_hops.unwrap_or(4).clamp(1, 4); // ≤ 4 (§1.7)

    let row = sqlx::query(
        "WITH RECURSIVE p(node_type, node_id, path, depth) AS ( \
            SELECT $1::text, $2::text, ARRAY[$1 || ':' || $2], 0 \
            UNION ALL \
            SELECT nb.node_type, nb.node_id, p.path || (nb.node_type || ':' || nb.node_id), p.depth + 1 \
            FROM p \
            JOIN LATERAL ( \
                SELECT e.dst_type AS node_type, e.dst_id AS node_id FROM entity_edges e \
                    WHERE e.src_type = p.node_type AND e.src_id = p.node_id \
                UNION \
                SELECT e.src_type, e.src_id FROM entity_edges e \
                    WHERE e.dst_type = p.node_type AND e.dst_id = p.node_id \
            ) nb ON true \
            WHERE p.depth < $5 \
              AND NOT ((nb.node_type || ':' || nb.node_id) = ANY(p.path)) \
        ) \
        SELECT path, depth FROM p WHERE node_type = $3 AND node_id = $4 \
        ORDER BY depth ASC LIMIT 1",
    )
    .bind(ft).bind(fi).bind(tt).bind(ti).bind(max_hops)
    .fetch_optional(&st.pool)
    .await?;

    match row {
        Some(r) => {
            let path: Vec<String> = r.get("path");
            Ok(Json(json!({ "found": true, "hops": r.get::<i32, _>("depth"), "path": path })))
        }
        None => Ok(Json(json!({ "found": false, "hops": null, "path": [] }))),
    }
}

// ---------------------------------------------------------------------------------------------
// Binding review queue (§1.4)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct BindingQuery {
    pub status: Option<String>,
}

pub async fn list_bindings(
    State(st): State<AppState>,
    Query(q): Query<BindingQuery>,
) -> Result<Json<Vec<Value>>, IngestError> {
    let status = q.status.unwrap_or_else(|| "candidate".into());
    if !matches!(status.as_str(), "candidate" | "confirmed" | "rejected") {
        return Err(IngestError::BadRequest("status must be candidate|confirmed|rejected".into()));
    }
    let rows = sqlx::query(
        "SELECT edge_id, edge_type, src_type, src_id, dst_type, dst_id, observation_count, \
                first_seen_unix_nanos, last_seen_unix_nanos, confidence, evidence, metadata, status \
         FROM entity_edges \
         WHERE edge_type = 'same_identity_candidate' AND status = $1 \
         ORDER BY confidence DESC NULLS LAST, updated_at DESC, edge_id LIMIT 500",
    )
    .bind(&status)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(rows.iter().map(edge_json).collect()))
}

async fn set_binding_status(
    st: &AppState,
    edge_id: &str,
    new_status: &str,
) -> Result<(), IngestError> {
    let eid = Uuid::parse_str(edge_id).map_err(|_| IngestError::BadRequest("edge_id must be a uuid".into()))?;
    let affected = sqlx::query(
        "UPDATE entity_edges SET status = $2, updated_at = now() \
         WHERE edge_id = $1 AND edge_type = 'same_identity_candidate'",
    )
    .bind(eid)
    .bind(new_status)
    .execute(&st.pool)
    .await?
    .rows_affected();
    if affected == 0 {
        return Err(IngestError::NotFound("binding edge"));
    }
    crate::audit::record(
        &st.pool,
        crate::audit::AuditEntry::event("operator", None, format!("graph.binding.{new_status}"))
            .with_target("entity_edge", edge_id.to_string())
            .with_detail(json!({ "status": new_status })),
    )
    .await;
    Ok(())
}

pub async fn confirm_binding(
    State(st): State<AppState>,
    Path(edge_id): Path<String>,
) -> Result<Json<Value>, IngestError> {
    set_binding_status(&st, &edge_id, "confirmed").await?;
    Ok(Json(json!({ "ok": true, "edge_id": edge_id, "status": "confirmed" })))
}

pub async fn reject_binding(
    State(st): State<AppState>,
    Path(edge_id): Path<String>,
) -> Result<Json<Value>, IngestError> {
    // Sticky negative: the pass never auto-flips a rejected edge back (§1.4).
    set_binding_status(&st, &edge_id, "rejected").await?;
    Ok(Json(json!({ "ok": true, "edge_id": edge_id, "status": "rejected" })))
}

// ---------------------------------------------------------------------------------------------
// Journeys (Wave 4 producer) / digests (Wave 2 producer) — table contract now
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct JourneyQuery {
    pub subject: Option<String>,
    pub since: Option<i64>,
    pub limit: Option<i64>,
}

pub async fn list_journeys(
    State(st): State<AppState>,
    Query(q): Query<JourneyQuery>,
) -> Result<Json<Vec<Value>>, IngestError> {
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let (stype, sid) = match &q.subject {
        Some(s) => {
            let (t, i) = s.split_once(':').ok_or(IngestError::BadRequest("subject must be type:id".into()))?;
            (Some(t.to_string()), Uuid::parse_str(i).ok())
        }
        None => (None, None),
    };
    let rows = sqlx::query(
        "SELECT journey_id, subject_type, subject_id, started_at_unix_nanos, ended_at_unix_nanos, \
                hop_count, hops, status \
         FROM entity_journeys \
         WHERE ($1::text IS NULL OR subject_type = $1) AND ($2::uuid IS NULL OR subject_id = $2) \
           AND ($3::bigint IS NULL OR started_at_unix_nanos >= $3) \
         ORDER BY started_at_unix_nanos DESC LIMIT $4",
    )
    .bind(stype).bind(sid).bind(q.since).bind(limit)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(rows.iter().map(|r| json!({
        "journey_id": r.get::<Uuid, _>("journey_id").to_string(),
        "subject": { "type": r.get::<String, _>("subject_type"), "id": r.get::<Uuid, _>("subject_id").to_string() },
        "started_at_unix_nanos": r.get::<i64, _>("started_at_unix_nanos"),
        "ended_at_unix_nanos": r.get::<i64, _>("ended_at_unix_nanos"),
        "hop_count": r.get::<i32, _>("hop_count"),
        "hops": r.try_get::<Value, _>("hops").unwrap_or(json!([])),
        "status": r.get::<String, _>("status"),
    })).collect()))
}

pub async fn list_digests(
    State(st): State<AppState>,
    Query(q): Query<TimelineQuery>,
) -> Result<Json<Vec<Value>>, IngestError> {
    let limit = q.limit.unwrap_or(30).clamp(1, 365);
    let rows = sqlx::query(
        "SELECT digest_date::text AS d, sections, rendered_text FROM daily_digests \
         ORDER BY digest_date DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(rows.iter().map(|r| json!({
        "date": r.get::<String, _>("d"),
        "sections": r.try_get::<Value, _>("sections").unwrap_or(json!({})),
        "rendered_text": r.get::<String, _>("rendered_text"),
    })).collect()))
}

pub async fn digest_by_date(
    State(st): State<AppState>,
    Path(date): Path<String>,
) -> Result<Json<Value>, IngestError> {
    let row = sqlx::query(
        "SELECT digest_date::text AS d, sections, rendered_text FROM daily_digests WHERE digest_date = $1::date",
    )
    .bind(&date)
    .fetch_optional(&st.pool)
    .await?
    .ok_or(IngestError::NotFound("digest"))?;
    Ok(Json(json!({
        "date": row.get::<String, _>("d"),
        "sections": row.try_get::<Value, _>("sections").unwrap_or(json!({})),
        "rendered_text": row.get::<String, _>("rendered_text"),
    })))
}

/// POST /v1/graph/digests/{date} — admin (Gotham.md §1.6 / Phase E): force-materialize the digest
/// for a pinned ISO civil date (`YYYY-MM-DD`) and return it. The GET on this path reads; this WRITE
/// path is what the eval + operator use to generate a digest OFF the worker's wall-clock schedule
/// (the wall-clock trigger can't be used deterministically — eval invariant 3). Audit-logged.
pub async fn generate_digest(
    State(st): State<AppState>,
    Path(date): Path<String>,
) -> Result<Json<Value>, IngestError> {
    // Cheap format guard so a malformed date is a clean 400, not a SQL cast 500.
    if !is_iso_date(&date) {
        return Err(IngestError::BadRequest("date must be YYYY-MM-DD".into()));
    }
    let opts = graph_pass::GraphOpts::from_env();
    let sections = graph_pass::generate_digest_for_date(&st.pool, &opts, &date)
        .await
        .map_err(IngestError::Internal)?;
    crate::audit::record(
        &st.pool,
        crate::audit::AuditEntry::event("operator", None, "graph.digest.generate")
            .with_detail(json!({ "date": date })),
    )
    .await;
    Ok(Json(json!({ "date": date, "sections": sections })))
}

/// Strict `YYYY-MM-DD` validation: shape (digits + hyphens at 4/7) AND a real calendar date
/// (month 1–12, day 1–days-in-month, leap-year-aware). A shape-valid but out-of-range date like
/// `2026-13-45` is client error → a clean 400 here, rather than a Postgres `$1::date` cast error
/// surfacing as a 500 downstream.
fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    if !b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit()) {
        return false;
    }
    // Safe: the positions are verified all-ASCII-digit above.
    let num = |lo: usize, hi: usize| s[lo..hi].parse::<u32>().unwrap_or(0);
    let (y, m, d) = (num(0, 4), num(5, 7), num(8, 10));
    if !(1..=12).contains(&m) || d < 1 {
        return false;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days_in_month =
        [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][(m - 1) as usize];
    d <= days_in_month
}

// ---------------------------------------------------------------------------------------------
// POST /v1/graph/rebuild — admin, audit-logged (§1.7)
// ---------------------------------------------------------------------------------------------

pub async fn rebuild(State(st): State<AppState>) -> Result<Json<Value>, IngestError> {
    // Honor the operator's configured GRAPH_* knobs (not hard-coded defaults) — a tuned graph must
    // rebuild under its own config, and the eval's GRAPH_GRACE_SECS=0 lets a rebuild fold freshly
    // injected events (the 90s default grace would exclude events younger than 90s wall-clock).
    let opts = graph_pass::GraphOpts::from_env();
    let stats = graph_pass::rebuild(&st.pool, &opts)
        .await
        .map_err(IngestError::Internal)?;
    crate::audit::record(
        &st.pool,
        crate::audit::AuditEntry::event("operator", None, "graph.rebuild")
            .with_detail(json!({
                "events_consumed": stats.events_consumed,
                "conversations_consumed": stats.conversations_consumed,
                "edges_upserted": stats.edges_upserted,
            })),
    )
    .await;
    Ok(Json(json!({
        "ok": true,
        "events_consumed": stats.events_consumed,
        "conversations_consumed": stats.conversations_consumed,
        "edges_upserted": stats.edges_upserted,
        "bindings_surfaced": stats.bindings_surfaced,
    })))
}
