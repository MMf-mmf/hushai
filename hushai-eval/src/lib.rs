//! hushai-eval — end-to-end regression harness.
//!
//! Per case: collect the env manifest → reset the DB → enroll reference identities → inject the
//! fixture media deterministically → wait for both lanes + event quiescence → query results →
//! score vs ground truth → classify vs the config-hash baseline. The suite verdict + exit code is
//! what an agent loop consumes (0 pass/improved, 1 regression/floor-breach, 2 inconclusive/infra).

pub mod baseline;
pub mod ctx;
pub mod enroll;
pub mod fixtures;
pub mod inject;
pub mod manifest;
pub mod poll;
pub mod probe;
pub mod query;
pub mod query_rag;
pub mod report;
pub mod reset;
pub mod score;

use anyhow::{Context, Result};
use ctx::Ctx;
use fixtures::Fixture;
use manifest::EnvManifest;
use report::{CaseResult, SuiteResult};

#[derive(Clone, Debug)]
pub struct RunOpts {
    pub tier: String,         // "fast" | "full"
    pub case: Option<String>, // single case_id filter
    pub splits: Vec<String>,  // ["train"] | ["holdout"] | ["train","holdout"]
    pub update_baseline: bool,
    pub force: bool,
    pub json: bool,
}

pub async fn run(opts: RunOpts) -> Result<SuiteResult> {
    let root = ctx::repo_root();
    ctx::load_env_files(&root);
    let ctx = Ctx::connect(root).await?;

    let split_refs: Vec<&str> = opts.splits.iter().map(|s| s.as_str()).collect();
    let mut fixtures = fixtures::discover(&ctx.fixtures_root, &split_refs)
        .with_context(|| format!("discovering fixtures under {}", ctx.fixtures_root.display()))?;
    fixtures.retain(|f| {
        (opts.tier == "full" || f.meta.tier == "fast")
            && opts.case.as_ref().is_none_or(|c| &f.meta.case_id == c)
    });
    if fixtures.is_empty() {
        anyhow::bail!(
            "no fixtures matched (tier={}, case={:?}, splits={:?}) under {}",
            opts.tier,
            opts.case,
            opts.splits,
            ctx.fixtures_root.display()
        );
    }

    let mut cases = Vec::new();
    let mut last_manifest: Option<EnvManifest> = None;
    for fx in &fixtures {
        let manifest = EnvManifest::collect(&ctx, &fx.meta.config).await?;
        let case = run_case(&ctx, fx, &manifest, opts.update_baseline, opts.force).await?;
        cases.push(case);
        last_manifest = Some(manifest);
    }

    let manifest = last_manifest.expect("at least one fixture");
    Ok(SuiteResult::finalize(&opts.tier, manifest, cases))
}

async fn run_case(
    ctx: &Ctx,
    fx: &Fixture,
    manifest: &EnvManifest,
    update_baseline: bool,
    force: bool,
) -> Result<CaseResult> {
    let base_ns = fx.meta.base_capture_unix_nanos;
    let (cid, split, tier) = (fx.meta.case_id.as_str(), fx.split.as_str(), fx.meta.tier.as_str());

    reset::reset_db(ctx).await.context("reset db")?;

    if let Err(e) = enroll::enroll_all(ctx, fx, base_ns).await {
        return Ok(CaseResult::inconclusive(cid, split, tier, format!("enroll failed: {e:#}")));
    }

    // Inject the whole timeline (one clip for legacy fixtures, several for a scenario). Each clip is
    // injected then polled to terminal BEFORE the next — serialized processing keeps mint-vs-match
    // ordering deterministic under WORKER_CONCURRENCY=1. We aggregate the poll counters, union the
    // touched devices, and grow the observe window to span the whole scenario.
    let plan = fx.meta.effective_injections();
    let mut all_ids: Vec<uuid::Uuid> = Vec::new();
    let mut devices: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut injected_total = 0usize;
    let mut audio_done_total = 0i64;
    let mut vision_done_total = 0i64;
    let mut win_lo = i64::MAX;
    let mut win_hi = i64::MIN;

    for ri in &plan {
        reset::upsert_device(ctx, &ri.device_id).await?;
        let inj = match inject::inject(
            ctx,
            &fx.dir.join(&ri.media_file),
            &ri.device_id,
            &ri.seed,
            ri.base_ns,
            ri.seg_seconds,
            ri.limit,
            &ri.label,
        ) {
            Ok(o) => o,
            Err(e) => return Ok(CaseResult::inconclusive(cid, split, tier, format!("inject failed ({}): {e:#}", ri.label))),
        };
        let ids = inj.segment_uuids()?;

        let poll = poll::wait_until_complete(ctx, &fx.meta, &ri.device_id, &ids, ri.base_ns).await?;
        if !poll.settled {
            return Ok(CaseResult::inconclusive(
                cid,
                split,
                tier,
                format!(
                    "processing incomplete ({}): audio_done={} vision_done={} injected={} timed_out={} errors={:?}",
                    ri.label, poll.audio_done, poll.vision_done, poll.injected, poll.timed_out, poll.errors
                ),
            ));
        }
        // `settled` only means "terminal for polling" — a segment that exhausted its retries counts
        // as settled but NOT done, and lands in `poll.errors`. Never SCORE a run with permanently-
        // errored segments (or a completion count below injected): surviving segments might happen to
        // cover the ground-truth windows and mask real pipeline breakage — exactly what inconclusive/
        // exit-2 exists to surface. Fail closed to inconclusive.
        //
        // `skipped` (migration 0022: silent-audio / static-video content gates) counts as SUCCESSFUL
        // completion here: the pipeline finished and decided there was nothing to infer — for a silent
        // fixture that IS the correct outcome (e.g. `silence_no_speech` must skip everything and mint 0
        // speakers). If a skip is ever wrong, the fixture's own metric assertions catch it (a skipped
        // segment produces no transcript/detections), which is a scoreable FAIL, not infrastructure.
        let audio_complete = poll.audio_done + poll.audio_skipped;
        let vision_complete = poll.vision_done + poll.vision_skipped;
        let fully_done = audio_complete.max(vision_complete) >= poll.injected as i64;
        if !fully_done || !poll.errors.is_empty() {
            return Ok(CaseResult::inconclusive(
                cid,
                split,
                tier,
                format!(
                    "processing not fully successful ({}) (would mask real breakage if scored): \
                     audio_done={} (+{} skipped) vision_done={} (+{} skipped) injected={} errors={:?}",
                    ri.label, poll.audio_done, poll.audio_skipped, poll.vision_done,
                    poll.vision_skipped, poll.injected, poll.errors
                ),
            ));
        }

        injected_total += poll.injected;
        // Completion totals include content-gate skips (successful "nothing to infer" verdicts),
        // so the report's processed/injected reads complete for silent/static fixtures.
        audio_done_total += poll.audio_done + poll.audio_skipped;
        vision_done_total += poll.vision_done + poll.vision_skipped;
        all_ids.extend(ids);
        devices.insert(ri.device_id.clone());
        win_lo = win_lo.min(ri.base_ns);
        win_hi = win_hi.max(inj.end_unix_nanos());
    }
    let _ = &all_ids; // ids are per-injection polled above; kept for potential future cross-checks.

    let devices_vec: Vec<String> = devices.into_iter().collect();
    let obs = query::observe(ctx, &devices_vec, win_lo, win_hi, &fx.meta.modalities)
        .await
        .context("querying observed results")?;
    let mut metrics = score::score_all(&fx.expected, &obs, base_ns, &fx.meta.modalities);

    // RAG step: score LIVE chat answers. Unlike `observe` (DB-direct), this needs the running RAG
    // service — a down service / transport error is INFRASTRUCTURE (INCONCLUSIVE / exit 2), never a
    // false regression. Only assertion failures on a SUCCESSFUL answer produce a FAIL.
    if fx.meta.needs_rag() {
        if let Some(chat_gt) = &fx.expected.chat {
            if !query_rag::rag_up(ctx).await {
                return Ok(CaseResult::inconclusive(
                    cid,
                    split,
                    tier,
                    "RAG service unreachable (the chat/rag modality needs the live :8090 stack)".to_string(),
                ));
            }
            let mut answers = Vec::with_capacity(chat_gt.questions.len());
            for (qi, q) in chat_gt.questions.iter().enumerate() {
                match query_rag::ask(ctx, q, base_ns).await {
                    Ok(a) => answers.push(a),
                    Err(e) => return Ok(CaseResult::inconclusive(cid, split, tier, format!("rag chat q{qi} transport error: {e:#}"))),
                }
            }
            metrics.extend(score::score_chat(chat_gt, &answers, ctx).await);
        }
    }

    let baseline = baseline::load(ctx, &manifest.config_hash, cid);
    let bmap = baseline.as_ref().map(|b| b.metrics.clone());
    let processed = audio_done_total.max(vision_done_total);
    let case = CaseResult::from_metrics(cid, split, tier, injected_total, processed, metrics.clone(), |m| {
        let bv = bmap.as_ref().and_then(|mm| mm.get(&m.key).copied());
        let (cls, delta) = baseline::classify(m, bv);
        (cls, bv, delta)
    });

    if update_baseline && (case.passed() || force) {
        let p = baseline::save(ctx, &manifest.config_hash, &manifest.git_sha, cid, &metrics)?;
        eprintln!("[baseline] wrote {}", p.display());
    } else if update_baseline {
        eprintln!("[baseline] NOT updating {cid}: verdict {:?} (pass --force to override)", case.verdict);
    }

    Ok(case)
}
