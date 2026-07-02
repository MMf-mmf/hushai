//! `probe` — the human-in-the-loop labeling flow.
//!
//! Take ANY audio (or muxed) clip, run it through the LIVE pipeline, print everything the pipeline
//! "heard" in a reviewable form, and write a DRAFT fixture (meta.json + expected.json pre-filled
//! from the observed output) under `fixtures/staging/<case>/`. The human reviews the printout, says
//! what's right/wrong, and we correct the draft and promote it to a regression fixture.

use crate::ctx::{self, Ctx};
use crate::fixtures::Meta;
use crate::{inject, poll, query, reset};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Fixed capture base for probes (any constant; partitioning is by insertion time).
const PROBE_BASE_NS: i64 = 1781784000000000000;

pub struct ProbeOpts {
    pub audio: PathBuf,
    pub case: String,
    pub vision: bool,
}

pub async fn probe(opts: ProbeOpts) -> Result<()> {
    let root = ctx::repo_root();
    ctx::load_env_files(&root);
    let ctx = Ctx::connect(root).await?;

    let media = ensure_muxed(&ctx, &opts.audio, &opts.case)?;
    let device = format!("probe-{}", opts.case);

    let mut modalities: Vec<String> =
        ["transcript", "speakers", "sentiment", "events"].iter().map(|s| s.to_string()).collect();
    if opts.vision {
        modalities.extend(["persons", "objects", "plates"].iter().map(|s| s.to_string()));
    }
    let meta = Meta::synthetic(&device, &opts.case, "muxed", PROBE_BASE_NS, modalities.clone());

    eprintln!("[probe] reset → inject → process '{}' (device {device})", opts.audio.display());
    reset::reset_db(&ctx).await?;
    reset::upsert_device(&ctx, &device).await?;

    let inj = inject::inject(&ctx, &media, &device, &meta.seed(), PROBE_BASE_NS, 2, None, &format!("probe-{}", opts.case))?;
    let ids = inj.segment_uuids()?;
    let p = poll::wait_until_complete(&ctx, &meta, &device, &ids, PROBE_BASE_NS).await?;
    if !p.settled {
        eprintln!("[probe] ⚠ processing did not fully settle (timed_out={}, errors={:?}); showing partial results", p.timed_out, p.errors);
    }

    let obs = query::observe(&ctx, std::slice::from_ref(&device), PROBE_BASE_NS, inj.end_unix_nanos(), &modalities).await?;
    print_report(&opts, &obs, &p, ids.len());

    let staging = write_draft(&ctx, &opts, &media, &obs)?;
    eprintln!("\n[probe] draft fixture written to {}", staging.display());
    eprintln!("[probe] Review the output above and tell me what's right/wrong. I'll correct the");
    eprintln!("[probe] ground truth in expected.json and promote it to fixtures/train/.");
    Ok(())
}

/// If the input isn't already a muxed container, render the audio over a tiny black video so
/// feed_segments.py can emit h264+aac MUXED segments (the pipeline's expected shape).
fn ensure_muxed(ctx: &Ctx, audio: &Path, case: &str) -> Result<PathBuf> {
    if !audio.is_file() {
        bail!("audio file not found: {}", audio.display());
    }
    let ext = audio.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
    if matches!(ext.as_str(), "mp4" | "mov" | "m4v") {
        return Ok(audio.to_path_buf());
    }
    let out = ctx.scratch.join(format!("probe-{case}.mp4"));
    let status = std::process::Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-i"])
        .arg(audio)
        .args([
            "-f", "lavfi", "-i", "color=c=black:s=320x240:r=5",
            "-map", "1:v", "-map", "0:a", "-shortest",
            "-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p", "-c:a", "aac",
        ])
        .arg(&out)
        .status()
        .context("running ffmpeg to mux audio over black video (is ffmpeg installed?)")?;
    if !status.success() {
        bail!("ffmpeg failed to prepare {}", audio.display());
    }
    Ok(out)
}

fn print_report(opts: &ProbeOpts, obs: &query::Observed, p: &poll::PollOutcome, injected: usize) {
    let distinct: std::collections::HashSet<Uuid> =
        obs.sentences.iter().filter_map(|s| s.speaker_id).collect();
    println!("\n=== PROBE: {} ===", opts.case);
    println!(
        "segments {}/{} processed  |  distinct speakers: {}  |  sentences: {}  |  events: {}",
        p.audio_done.max(p.vision_done),
        injected,
        distinct.len(),
        obs.sentences.len(),
        obs.events.len()
    );

    println!("\n--- transcript (what the pipeline heard) ---");
    if obs.sentences.is_empty() {
        println!("  (no speech transcribed)");
    }
    for s in &obs.sentences {
        let t = (s.start_ns - PROBE_BASE_NS) as f64 / 1e9;
        let spk = match s.speaker_id {
            Some(id) => obs
                .speaker_names
                .get(&id)
                .cloned()
                .flatten()
                .unwrap_or_else(|| format!("spk:{}", &id.to_string()[..8])),
            None => "unattributed".into(),
        };
        let sent = s.sentiment.clone().unwrap_or_else(|| "—".into());
        println!("  t={t:>5.1}s  [{spk:<14}] ({sent:<8})  {}", s.text.trim());
    }

    println!("\n--- speakers (distinct voices minted) ---");
    if distinct.is_empty() {
        println!("  (none minted)  ⚠ known speaker-lane issue — confirm the rest, then we fix this");
    } else {
        for id in &distinct {
            let name = obs.speaker_names.get(id).cloned().flatten().unwrap_or_else(|| "<unnamed>".into());
            let n = obs.sentences.iter().filter(|s| s.speaker_id.as_ref() == Some(id)).count();
            println!("  {}  ({} utterances)  name={}", &id.to_string()[..8], n, name);
        }
    }

    if opts.vision {
        println!("\n--- vision ---");
        println!("  persons (distinct): {}", obs.distinct_persons);
        let mut obj: BTreeMap<String, usize> = BTreeMap::new();
        for o in &obs.objects {
            *obj.entry(o.label.clone()).or_default() += 1;
        }
        println!("  objects: {}", if obj.is_empty() { "(none)".into() } else { obj.iter().map(|(k, v)| format!("{k}×{v}")).collect::<Vec<_>>().join(", ") });
        println!("  plates: {}", if obs.plates.is_empty() { "(none)".into() } else { obs.plates.iter().map(|p| p.plate_text.clone()).collect::<Vec<_>>().join(", ") });
    }

    println!("\n--- events ---");
    if obs.events.is_empty() {
        println!("  (none)");
    } else {
        let mut ev: BTreeMap<(String, String), usize> = BTreeMap::new();
        for e in &obs.events {
            *ev.entry((e.event_type.clone(), e.severity.clone())).or_default() += 1;
        }
        for ((ty, sev), n) in ev {
            println!("  {ty:<16} {sev:<9} ×{n}");
        }
    }
}

/// Write a draft meta.json + expected.json + copy the media into fixtures/staging/<case>/.
/// expected.json is PRE-FILLED from the observed output so the human only has to CORRECT it.
fn write_draft(ctx: &Ctx, opts: &ProbeOpts, media: &Path, obs: &query::Observed) -> Result<PathBuf> {
    let dir = ctx.fixtures_root.join("staging").join(&opts.case);
    std::fs::create_dir_all(&dir)?;
    std::fs::copy(media, dir.join("media.mp4")).context("copying probe media into staging")?;

    let full_text = obs.sentences.iter().map(|s| s.text.trim()).collect::<Vec<_>>().join(" ");
    let distinct: std::collections::HashSet<Uuid> = obs.sentences.iter().filter_map(|s| s.speaker_id).collect();
    let mut ev: BTreeMap<String, i64> = BTreeMap::new();
    for e in &obs.events {
        *ev.entry(e.event_type.clone()).or_default() += 1;
    }

    let meta = serde_json::json!({
        "case_id": opts.case,
        "description": format!("probed from {}", opts.audio.display()),
        "device_id": format!("eval-{}", opts.case),
        "media_file": "media.mp4",
        "media_kind": "muxed",
        "seg_seconds": 2,
        "base_capture_unix_nanos": PROBE_BASE_NS,
        // NOTE: 'speakers' omitted by default pending the speaker-lane fix; add it once correct.
        "modalities": ["transcript", "events"],
        "tier": "full",
        "poll": {"timeout_secs": 180, "interval_secs": 2}
    });
    let expected = serde_json::json!({
        "_draft_note": "Pre-filled from the pipeline's OWN output. CORRECT anything wrong, then promote.",
        "transcript": {"full_text": full_text, "max_wer": 0.20, "min_similarity": 0.80},
        "speakers": {"distinct_count": distinct.len(), "count_tolerance": 0, "min_purity": 0.75},
        "events": {"expected": ev.into_iter().map(|(ty, n)| serde_json::json!({"event_type": ty, "min_count": n.max(1)})).collect::<Vec<_>>()}
    });
    std::fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
    std::fs::write(dir.join("expected.json"), serde_json::to_string_pretty(&expected)?)?;
    Ok(dir)
}
