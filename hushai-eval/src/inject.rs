//! Injection: shell out to the (extended) `local_dev/feed_segments.py` with deterministic flags,
//! then read the emitted id manifest so the poller knows exactly which segments to wait on.

use crate::ctx::Ctx;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::Path;
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize)]
pub struct InjectedSegment {
    pub seq: i64,
    pub segment_id: String, // 32-hex (no dashes), as emitted by feed_segments.py
}

#[derive(Debug, Clone, Deserialize)]
pub struct InjectOutput {
    pub session_id: String,
    pub device_id: String,
    pub stream_id: String,
    pub capture_start_unix_nanos: i64,
    pub duration_nanos: i64,
    pub segments: Vec<InjectedSegment>,
}

impl InjectOutput {
    pub fn segment_uuids(&self) -> Result<Vec<Uuid>> {
        self.segments
            .iter()
            .map(|s| {
                let bytes = hex::decode(&s.segment_id)
                    .with_context(|| format!("decoding segment_id hex {}", s.segment_id))?;
                let arr: [u8; 16] = bytes
                    .as_slice()
                    .try_into()
                    .with_context(|| format!("segment_id is not 16 bytes: {}", s.segment_id))?;
                Ok(Uuid::from_bytes(arr))
            })
            .collect()
    }
    /// Upper bound of the captured timeline (for the query window).
    pub fn end_unix_nanos(&self) -> i64 {
        self.capture_start_unix_nanos + self.segments.len() as i64 * self.duration_nanos
    }
}

/// Inject `media` deterministically. `seed` drives reproducible segment ids; `base_ns` pins
/// capture timestamps; `label` names the emit-ids scratch file.
#[allow(clippy::too_many_arguments)]
pub fn inject(
    ctx: &Ctx,
    media: &Path,
    device_id: &str,
    seed: &str,
    base_ns: i64,
    seg_seconds: u32,
    limit: Option<u32>,
    label: &str,
) -> Result<InjectOutput> {
    if !media.is_file() {
        bail!("media file not found: {}", media.display());
    }
    let ids_path = ctx.scratch.join(format!("{label}-ids.json"));
    let work_dir = ctx.scratch.join(format!("{label}-segments"));
    let url = format!("{}/v1/segments", ctx.backend_url.trim_end_matches('/'));

    let mut cmd = std::process::Command::new("python3");
    cmd.current_dir(ctx.feed_script.parent().unwrap())
        .arg(&ctx.feed_script)
        .args(["--url", &url])
        .args(["--token", &ctx.device_token])
        .args(["--device", device_id])
        .args(["--video", &media.to_string_lossy()])
        .args(["--seg-seconds", &seg_seconds.to_string()])
        .args(["--segment-id-seed", seed])
        .args(["--capture-start-ns", &base_ns.to_string()])
        .args(["--work-dir", &work_dir.to_string_lossy()])
        .args(["--emit-ids", &ids_path.to_string_lossy()]);
    if let Some(n) = limit {
        cmd.args(["--limit", &n.to_string()]);
    }

    let out = cmd.output().context("spawning feed_segments.py (is python3 + requests installed?)")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        bail!(
            "feed_segments.py failed (exit {:?}). Not all segments were accepted (200).\n--- stdout ---\n{}\n--- stderr ---\n{}",
            out.status.code(),
            tail(&stdout, 1500),
            tail(&stderr, 1500),
        );
    }

    let text = std::fs::read_to_string(&ids_path)
        .with_context(|| format!("reading emitted ids {}", ids_path.display()))?;
    let parsed: InjectOutput = serde_json::from_str(&text)
        .with_context(|| format!("parsing emitted ids {}", ids_path.display()))?;
    if parsed.segments.is_empty() {
        bail!("injection produced 0 segments for {}", media.display());
    }
    Ok(parsed)
}

fn tail(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_string() } else { s[s.len() - n..].to_string() }
}
