//! One virtual camera = one async task that replays the shared corpus at wall-clock realtime
//! cadence. Identity-only fan-out: each camera has a unique device_id/session_id and mints a fresh
//! segment_id per emit (so every replica is genuinely new work for the worker, never an idempotent
//! dedup). The POST is decoupled from the emit tick (spawned, bounded in-flight) so upload latency
//! can never throttle capture — exactly how a real camera behaves.

use crate::corpus::Corpus;
use hushai_backend::proto::{MediaType, SegmentManifest};
use prost::Message;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Max concurrent in-flight POSTs per camera. If exceeded, the segment is dropped and counted as an
/// overrun — a signal the BACKEND can't even accept fast enough (distinct from worker saturation).
const MAX_INFLIGHT: usize = 8;

/// Process-wide feeder counters; the controller reads deltas each sample.
#[derive(Default)]
pub struct FeederStats {
    pub posted: AtomicU64,    // POST attempts that acquired an in-flight slot
    pub ok: AtomicU64,        // 2xx
    pub http_4xx: AtomicU64,  // includes 401/422 contract failures
    pub http_5xx: AtomicU64,
    pub errors: AtomicU64,    // transport errors
    pub overruns: AtomicU64,  // dropped: in-flight bound hit (backend can't keep up)
    pub post_ms_sum: AtomicU64,
    pub post_ms_max: AtomicU64,
}

impl FeederStats {
    fn record(&self, ms: u64, outcome: PostOutcome) {
        self.post_ms_sum.fetch_add(ms, Ordering::Relaxed);
        self.post_ms_max.fetch_max(ms, Ordering::Relaxed);
        match outcome {
            PostOutcome::Ok => { self.ok.fetch_add(1, Ordering::Relaxed); }
            PostOutcome::Http4xx => { self.http_4xx.fetch_add(1, Ordering::Relaxed); }
            PostOutcome::Http5xx => { self.http_5xx.fetch_add(1, Ordering::Relaxed); }
            PostOutcome::Error => { self.errors.fetch_add(1, Ordering::Relaxed); }
        }
    }
}

enum PostOutcome {
    Ok,
    Http4xx,
    Http5xx,
    Error,
}

/// Run one virtual camera until `shutdown` is set. `stagger` offsets this camera's tick phase so the
/// fleet's POSTs spread evenly across the segment window (smooth arrival, not synchronized bursts).
pub async fn run_camera(
    idx: usize,
    corpus: Arc<Corpus>,
    client: reqwest::Client,
    url: Arc<String>,
    token: Arc<String>,
    stats: Arc<FeederStats>,
    stagger: Duration,
    shutdown: Arc<AtomicBool>,
) {
    let device_id = format!("loadtest-cam-{idx:03}");
    let stream_id = format!("{device_id}-muxed");
    let session_id = Uuid_bytes();
    let duration_nanos = corpus.duration_nanos();
    let seg_dur = Duration::from_secs(corpus.seg_seconds.max(1));
    let n = corpus.segments.len().max(1);
    let base_wall = unix_nanos();
    let base_mono = base_wall; // synthetic monotonic base (raw, uncorrected per contract §5)

    let inflight = Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT));
    let mut tick = tokio::time::Instant::now() + stagger;
    let mut seq: i64 = 0;

    while !shutdown.load(Ordering::SeqCst) {
        tokio::time::sleep_until(tick).await;
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        let body_idx = (seq as usize) % n;
        let seg = &corpus.segments[body_idx];
        let manifest = build_manifest(
            Uuid_bytes(),
            session_id.clone(),
            &device_id,
            &stream_id,
            seq,
            seg.content_sha256,
            seg.byte_len,
            corpus.init_bytes.as_ref().clone(),
            base_wall + seq * duration_nanos,
            base_mono + seq * duration_nanos,
            duration_nanos,
        );
        let body_bytes = seg.body.as_ref().clone(); // ~0.5MB memcpy: the only per-emit copy, trivial

        match inflight.clone().try_acquire_owned() {
            Ok(permit) => {
                let client = client.clone();
                let url = url.clone();
                let token = token.clone();
                let stats = stats.clone();
                stats.posted.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    let _permit = permit; // released on task end -> frees an in-flight slot
                    let t0 = Instant::now();
                    let form = build_form(manifest, body_bytes);
                    let outcome = match client.post(url.as_str()).bearer_auth(token.as_str()).multipart(form).send().await {
                        Ok(resp) => {
                            let s = resp.status().as_u16();
                            if (200..300).contains(&s) {
                                PostOutcome::Ok
                            } else if (400..500).contains(&s) {
                                PostOutcome::Http4xx
                            } else {
                                PostOutcome::Http5xx
                            }
                        }
                        Err(_) => PostOutcome::Error,
                    };
                    stats.record(t0.elapsed().as_millis() as u64, outcome);
                });
            }
            Err(_) => {
                // No free slot — drop this segment (a real camera would too) and flag the overrun.
                stats.overruns.fetch_add(1, Ordering::Relaxed);
            }
        }

        seq += 1;
        tick += seg_dur;
    }
}

fn build_form(manifest: Vec<u8>, body: Vec<u8>) -> reqwest::multipart::Form {
    let manifest_part = reqwest::multipart::Part::bytes(manifest)
        .file_name("manifest")
        .mime_str("application/x-protobuf")
        .expect("static mime");
    let body_part = reqwest::multipart::Part::bytes(body)
        .file_name("body")
        .mime_str("application/octet-stream")
        .expect("static mime");
    reqwest::multipart::Form::new()
        .part("manifest", manifest_part)
        .part("body", body_part)
}

#[allow(clippy::too_many_arguments)]
fn build_manifest(
    segment_id: Vec<u8>,
    session_id: Vec<u8>,
    device_id: &str,
    stream_id: &str,
    sequence: i64,
    sha: [u8; 32],
    byte_len: i64,
    init_bytes: Vec<u8>,
    capture_ns: i64,
    monotonic_ns: i64,
    duration_ns: i64,
) -> Vec<u8> {
    // The proto numeric fields are uint64; the loop computes them as i64 (all non-negative).
    let m = SegmentManifest {
        segment_id,
        device_id: device_id.to_string(),
        stream_id: stream_id.to_string(),
        session_id,
        sequence: sequence as u64,
        source_kind: "loadtest_replica".to_string(),
        media_type: MediaType::Muxed as i32,
        codec: "h264+aac".to_string(),
        container: "fmp4".to_string(),
        codec_init_data: init_bytes,
        capture_start_unix_nanos: capture_ns as u64,
        monotonic_start_nanos: monotonic_ns as u64,
        duration_nanos: duration_ns as u64,
        content_sha256: sha.to_vec(),
        byte_len: byte_len as u64,
        gap_before: false,
        ..Default::default()
    };
    m.encode_to_vec()
}

/// 16-byte UUIDv7 (matches the contract's id format).
#[allow(non_snake_case)]
fn Uuid_bytes() -> Vec<u8> {
    uuid::Uuid::now_v7().as_bytes().to_vec()
}

fn unix_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}
