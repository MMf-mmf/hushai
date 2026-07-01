//! Roadmap A4 — the notification DELIVERY loop: drain the `alert_deliveries` outbox for non-`feed`
//! channels and actually send them (today: `webhook` → outbound HTTP POST; `push` awaits A7's
//! transport and is left pending). The `feed` channel is delivered in-app (the viewer reads the
//! outbox) and is never touched here.
//!
//! DELIVERY SEMANTICS: AT-LEAST-ONCE. The send (a non-undoable POST) and the `status='sent'` write
//! are separate steps, so a crash/DB-blip between them re-delivers after the lease. Every request
//! therefore carries a stable idempotency key — the `x-hushai-delivery` header AND `delivery_id` in
//! the body — and consumers MUST dedupe on it. (This contract is documented for integrators.)
//!
//! CRASH-SAFE LEASE + BACKOFF (see migration 0016): a batch is claimed by atomically `attempts += 1`
//! and pushing `next_attempt_at` a LEASE into the future under `FOR UPDATE SKIP LOCKED`, so two
//! worker processes never double-send and a crashed in-flight send is re-claimed only after the
//! lease (never lost). On a 2xx → `status='sent'`; on a transient failure → `next_attempt_at =
//! now()+backoff(attempts)` (exponential, capped); past `max_attempts` → `status='failed'`.
//!
//! SSRF posture (local-first): Hushai is a no-egress LAN product, so webhooks to PRIVATE/LAN targets
//! (Home Assistant, Node-RED, …) are the PRIMARY use case and are allowed by default. The cloud
//! metadata IP (169.254.169.254) is ALWAYS blocked (never a legitimate webhook target). Set
//! `ALERT_WEBHOOK_ALLOW_PRIVATE=false` for a cloud/hardened deployment to also block private/loopback/
//! link-local. Rule creation is already auth-gated (viewer IP-allowlist + password), so the operator
//! is trusted; the check is defense-in-depth, not a hard boundary (it resolves-then-connects, so it
//! does not defeat DNS rebinding — out of scope for the threat model).

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;
use sqlx::{PgPool, Row};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;
const USER_AGENT: &str = concat!("hushai-alerts/", env!("CARGO_PKG_VERSION"));
/// Cloud metadata endpoints — never a legitimate webhook target; blocked ALWAYS (even when
/// `allow_private`). The v4 link-local IP is the classic SSRF target; `fd00:ec2::254` is AWS IMDS
/// over IPv6 (which `allow_private` would otherwise let through, since ULA isn't blocked by default).
const METADATA_V4: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(169, 254, 169, 254));
const METADATA_V6: IpAddr =
    IpAddr::V6(std::net::Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254));
/// Cap on the pre-flight DNS resolve so it can't outlast the claim lease (a black-holed resolver
/// would otherwise block inside the leased critical section and risk a re-claim/double-send).
const DNS_TIMEOUT: Duration = Duration::from_secs(5);

/// Tunables for the delivery loop (parsed from `ALERT_*` env in `config::WorkerConfig::from_env`).
#[derive(Debug, Clone)]
pub struct DeliveryConfig {
    pub enabled: bool,
    pub poll_secs: u64,
    pub batch: i64,
    pub max_attempts: i32,
    pub timeout_ms: u64,
    /// In-flight lease (secs): how long a claimed row is hidden before re-claim (crash recovery).
    pub lease_secs: f64,
    pub backoff_base_secs: f64,
    pub backoff_max_secs: f64,
    /// Optional HMAC-SHA256 secret → `X-Hushai-Signature: sha256=<hex>` over the body.
    pub signing_secret: Option<String>,
    /// Allow webhooks to private/LAN IPs (default true — local-first). The metadata IP is always blocked.
    pub allow_private: bool,
}

/// One claimed outbox row to deliver.
#[derive(Debug)]
struct Claimed {
    delivery_id: Uuid,
    target: Option<String>,
    event_id: Option<Uuid>,
    rule_id: Option<Uuid>,
    device_id: Option<String>,
    event_type: Option<String>,
    severity: Option<String>,
    subject_label: Option<String>,
    attempts: i32,
    created_unix_nanos: i64,
}

/// Build the shared HTTP client (one per loop): a hard per-request timeout + a UA. Redirects are
/// DISABLED so a webhook can't 30x-redirect past our pre-send SSRF check to the metadata/internal
/// host (the `target_allowed` resolve happens on the initial URL only). A delivery is a fire-and-
/// forget POST; following redirects is neither needed nor safe here.
pub fn build_client(cfg: &DeliveryConfig) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_millis(cfg.timeout_ms.max(1)))
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

/// Spawn the background delivery loop (fire-and-forget, like the heartbeat). No-op when disabled.
pub fn spawn_delivery_loop(pool: PgPool, cfg: DeliveryConfig, shutdown: Arc<AtomicBool>) {
    if !cfg.enabled {
        tracing::info!("alert delivery loop disabled (ALERT_DELIVERY_ENABLED=false)");
        return;
    }
    let client = match build_client(&cfg) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "alert delivery: HTTP client build failed; loop disabled");
            return;
        }
    };
    let poll = Duration::from_secs(cfg.poll_secs.max(1));
    tokio::spawn(async move {
        tracing::info!(allow_private = cfg.allow_private, "alert delivery loop started");
        while !shutdown.load(Ordering::SeqCst) {
            match run_once(&pool, &client, &cfg).await {
                Ok(n) if n > 0 => tracing::debug!(processed = n, "alert delivery batch"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "alert delivery batch failed"),
            }
            tokio::time::sleep(poll).await;
        }
        tracing::info!("alert delivery loop stopped");
    });
}

/// Claim and deliver one due batch. Returns the number of rows attempted (for logging/tests).
/// Rows are delivered CONCURRENTLY (bounded by `cfg.batch`) so one slow/dead webhook can't block the
/// peers behind it — each row owns its outbox row and does its own terminal `mark_*`.
pub async fn run_once(
    pool: &PgPool,
    client: &reqwest::Client,
    cfg: &DeliveryConfig,
) -> Result<u64, sqlx::Error> {
    let batch = claim_due(pool, cfg).await?;
    let n = batch.len() as u64;
    let mut set = tokio::task::JoinSet::new();
    for c in batch {
        let (pool, client, cfg) = (pool.clone(), client.clone(), cfg.clone());
        set.spawn(async move {
            // A per-row error (or a task panic, surfaced by join_next) must not abort the batch.
            if let Err(e) = deliver_one(&pool, &client, &cfg, &c).await {
                tracing::warn!(error = %e, delivery_id = %c.delivery_id, "alert delivery: row failed unexpectedly");
            }
        });
    }
    while set.join_next().await.is_some() {}
    Ok(n)
}

/// Atomically claim up to `batch` DUE webhook rows: bump attempts + push next_attempt_at a lease out.
async fn claim_due(pool: &PgPool, cfg: &DeliveryConfig) -> Result<Vec<Claimed>, sqlx::Error> {
    let rows = sqlx::query(
        r#"
        UPDATE alert_deliveries
           SET attempts = attempts + 1,
               next_attempt_at = now() + make_interval(secs => $2)
         WHERE delivery_id IN (
                SELECT delivery_id FROM alert_deliveries
                 WHERE channel = 'webhook' AND status = 'pending'
                   AND (next_attempt_at IS NULL OR next_attempt_at <= now())
                 ORDER BY next_attempt_at NULLS FIRST, created_at
                 FOR UPDATE SKIP LOCKED
                 LIMIT $1
              )
        RETURNING delivery_id, target, event_id, rule_id, device_id, event_type, severity,
                  subject_label, attempts,
                  (extract(epoch FROM created_at) * 1e9)::bigint AS created_unix_nanos
        "#,
    )
    .bind(cfg.batch.max(1))
    .bind(cfg.lease_secs.max(1.0))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| Claimed {
            delivery_id: r.get("delivery_id"),
            target: r.get("target"),
            event_id: r.get("event_id"),
            rule_id: r.get("rule_id"),
            device_id: r.get("device_id"),
            event_type: r.get("event_type"),
            severity: r.get("severity"),
            subject_label: r.get("subject_label"),
            attempts: r.get("attempts"),
            created_unix_nanos: r.get("created_unix_nanos"),
        })
        .collect())
}

async fn deliver_one(
    pool: &PgPool,
    client: &reqwest::Client,
    cfg: &DeliveryConfig,
    c: &Claimed,
) -> Result<(), sqlx::Error> {
    let target = match c.target.as_deref().filter(|t| !t.is_empty()) {
        Some(t) => t,
        None => return mark_failed(pool, c.delivery_id, "no target url").await,
    };
    let url = match reqwest::Url::parse(target) {
        Ok(u) => u,
        Err(e) => return mark_failed(pool, c.delivery_id, &format!("invalid url: {e}")).await,
    };
    if let Err(v) = vet_target(&url, cfg.allow_private).await {
        // A policy BLOCK is permanent (config error) → fail; a resolve failure/timeout is TRANSIENT
        // (DNS hiccup) → retry with backoff rather than burning the delivery.
        return if v.transient {
            decide_retry(pool, cfg, c, &format!("resolve: {}", v.reason)).await
        } else {
            mark_failed(pool, c.delivery_id, &format!("blocked target: {}", v.reason)).await
        };
    }

    let body = payload(c);
    let body_bytes = serde_json::to_vec(&body).unwrap_or_default();
    let mut req = client
        .post(url)
        .header("content-type", "application/json")
        .header("x-hushai-delivery", c.delivery_id.to_string());
    if let Some(secret) = cfg.signing_secret.as_deref().filter(|s| !s.is_empty()) {
        req = req.header("x-hushai-signature", format!("sha256={}", sign(secret, &body_bytes)));
    }
    let req = req.body(body_bytes);

    // Host (not full URL) for logs: enough to identify the endpoint without leaking query/token
    // material a target might carry in its path.
    let host = req
        .try_clone()
        .and_then(|r| r.build().ok())
        .and_then(|r| r.url().host_str().map(str::to_string))
        .unwrap_or_else(|| "?".to_string());
    let sent_at = std::time::Instant::now();
    let result = req.send().await;
    let latency_ms = sent_at.elapsed().as_millis() as u64;

    match result {
        Ok(resp) if resp.status().is_success() => {
            tracing::debug!(
                delivery_id = %c.delivery_id, host, status = resp.status().as_u16(),
                attempt = c.attempts, latency_ms, "alert delivery: sent"
            );
            mark_sent(pool, c.delivery_id).await
        }
        Ok(resp) => {
            tracing::warn!(
                delivery_id = %c.delivery_id, host, status = resp.status().as_u16(),
                attempt = c.attempts, latency_ms, "alert delivery: non-2xx response"
            );
            decide_retry(pool, cfg, c, &format!("http status {}", resp.status())).await
        }
        Err(e) => {
            let kind = if e.is_timeout() { "timeout" } else { "send error" };
            tracing::warn!(
                delivery_id = %c.delivery_id, host, attempt = c.attempts, latency_ms,
                error = %e, "alert delivery: {kind}"
            );
            decide_retry(pool, cfg, c, &format!("{kind}: {e}")).await
        }
    }
}

/// Transient failure: retry with capped exponential backoff, or give up past max_attempts.
async fn decide_retry(
    pool: &PgPool,
    cfg: &DeliveryConfig,
    c: &Claimed,
    err: &str,
) -> Result<(), sqlx::Error> {
    if c.attempts >= cfg.max_attempts {
        // Permanent give-up: previously only bumped a counter. A developer asking "why didn't my
        // webhook fire?" needs this at warn, with the rule/event that produced it.
        tracing::warn!(
            delivery_id = %c.delivery_id, rule_id = ?c.rule_id, event_id = ?c.event_id,
            attempts = c.attempts, %err, "alert delivery: gave up (max attempts) — marking failed"
        );
        mark_failed(
            pool,
            c.delivery_id,
            &format!("{err} (gave up after {} attempts)", c.attempts),
        )
        .await
    } else {
        hushai_backend::observe::counter("hushai_deliveries_total", &[("result", "retry")]);
        let backoff = backoff_secs(cfg, c.attempts);
        tracing::debug!(delivery_id = %c.delivery_id, attempt = c.attempts, backoff_secs = backoff, %err, "alert delivery: scheduling retry");
        sqlx::query(
            "UPDATE alert_deliveries SET next_attempt_at = now() + make_interval(secs => $2), last_error = $3 \
             WHERE delivery_id = $1",
        )
        .bind(c.delivery_id)
        .bind(backoff)
        .bind(err)
        .execute(pool)
        .await
        .map(|_| ())
    }
}

async fn mark_sent(pool: &PgPool, id: Uuid) -> Result<(), sqlx::Error> {
    hushai_backend::observe::counter("hushai_deliveries_total", &[("result", "sent")]);
    sqlx::query(
        "UPDATE alert_deliveries SET status = 'sent', sent_at = now(), last_error = NULL, \
         next_attempt_at = NULL WHERE delivery_id = $1",
    )
    .bind(id)
    .execute(pool)
    .await
    .map(|_| ())
}

async fn mark_failed(pool: &PgPool, id: Uuid, err: &str) -> Result<(), sqlx::Error> {
    hushai_backend::observe::counter("hushai_deliveries_total", &[("result", "failed")]);
    sqlx::query(
        "UPDATE alert_deliveries SET status = 'failed', last_error = $2, next_attempt_at = NULL \
         WHERE delivery_id = $1",
    )
    .bind(id)
    .bind(err)
    .execute(pool)
    .await
    .map(|_| ())
}

/// `base * 2^(attempts-1)`, capped at `backoff_max_secs`.
fn backoff_secs(cfg: &DeliveryConfig, attempts: i32) -> f64 {
    let exp = (attempts - 1).clamp(0, 20);
    let secs = cfg.backoff_base_secs * 2f64.powi(exp);
    secs.min(cfg.backoff_max_secs)
}

/// The webhook JSON body. Stable, documented shape so integrators can parse it.
fn payload(c: &Claimed) -> serde_json::Value {
    json!({
        "schema": "hushai.alert.v1",
        "delivery_id": c.delivery_id,
        "event_id": c.event_id,
        "rule_id": c.rule_id,
        "device_id": c.device_id,
        "event_type": c.event_type,
        "severity": c.severity,
        "subject_label": c.subject_label,
        "fired_at_unix_nanos": c.created_unix_nanos,
    })
}

fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// Why a target was rejected. `transient` (DNS hiccup/timeout) → retry with backoff; otherwise a
/// permanent policy BLOCK (bad scheme / metadata / internal) → fail the delivery.
struct VetErr {
    transient: bool,
    reason: String,
}

/// SSRF guard: bounded-resolve the target host and reject the cloud-metadata IPs (ALWAYS) +
/// private/loopback/link-local/CGNAT (unless `allow_private`, the local-first default). IPv4-mapped
/// IPv6 (`::ffff:a.b.c.d`) is normalized to v4 first so it can't smuggle a v4 metadata/internal IP
/// past the v4 checks. Resolve is capped by `DNS_TIMEOUT` so it can't outlast the claim lease.
async fn vet_target(url: &reqwest::Url, allow_private: bool) -> Result<(), VetErr> {
    let block = |reason: String| VetErr { transient: false, reason };
    let transient = |reason: String| VetErr { transient: true, reason };

    match url.scheme() {
        "http" | "https" => {}
        s => return Err(block(format!("scheme {s:?} not allowed"))),
    }
    let host = url
        .host_str()
        .ok_or_else(|| block("no host in url".to_string()))?;
    let port = url.port_or_known_default().unwrap_or(80);

    let resolved = tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((host, port))).await;
    let addrs: Vec<_> = match resolved {
        Err(_) => return Err(transient("dns resolve timed out".into())),
        Ok(Err(e)) => return Err(transient(format!("dns resolve failed: {e}"))),
        Ok(Ok(it)) => it.collect(),
    };
    if addrs.is_empty() {
        return Err(transient("host did not resolve".into()));
    }
    for sa in addrs {
        let ip = normalize_ip(sa.ip());
        if ip == METADATA_V4 || ip == METADATA_V6 {
            return Err(block(format!("cloud metadata IP {ip}")));
        }
        if !allow_private && is_internal(ip) {
            return Err(block(format!(
                "internal IP {ip} (set ALERT_WEBHOOK_ALLOW_PRIVATE=true for LAN webhook targets)"
            )));
        }
    }
    Ok(())
}

/// Collapse an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) to its v4 form so the v4 policy checks
/// can't be bypassed by expressing an internal/metadata v4 IP as v6.
fn normalize_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v) => v.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v)),
        other => other,
    }
}

fn is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_unspecified()
                || v.is_broadcast()
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10 CGNAT (shared address space)
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // 198.18.0.0/15 benchmarking
        }
        IpAddr::V6(v) => {
            let s = v.segments();
            v.is_loopback()
                || v.is_unspecified()
                || (s[0] & 0xfe00) == 0xfc00 // ULA fc00::/7
                || (s[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        }
    }
}
