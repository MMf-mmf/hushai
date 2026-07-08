//! hushai-eval — end-to-end regression harness.
//!
//! Per case: collect the env manifest → reset the DB → enroll reference identities → inject the
//! fixture media deterministically → wait for both lanes + event quiescence → query results →
//! score vs ground truth → classify vs the config-hash baseline. The suite verdict + exit code is
//! what an agent loop consumes (0 pass/improved, 1 regression/floor-breach, 2 inconclusive/infra).
//! (`advisor` cases skip the media pipeline entirely — they script a live-service conversation
//! instead; see `run_advisor_case`.)

pub mod baseline;
pub mod ctx;
pub mod enroll;
pub mod fixtures;
pub mod inject;
pub mod manifest;
pub mod poll;
pub mod probe;
pub mod query;
pub mod query_advisor;
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

    // Advisor cases are SERVICE-level (the `advisor` modality): a scripted conversation against
    // the live hushai-advisor, grounded in the pre-ingested book corpus — no media, no lanes, so
    // the whole enroll/inject/poll/observe pipeline is skipped. Branch before the media reset;
    // everything infra-shaped (service down, empty corpus, transport error) goes INCONCLUSIVE
    // inside, so advisor fixtures can never turn a suite red when the optional service is absent.
    if fx.meta.needs_advisor() {
        return run_advisor_case(ctx, fx, manifest, update_baseline, force).await;
    }

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

    // Threading quiescence (0025): the conversation threader runs on its own interval
    // AFTER the transcript lanes finish; observing before it has assigned every sentence
    // would score phantom NULLs. Timeout = infrastructure (inconclusive), never a FAIL.
    if fx.meta.modality("conversations") {
        let threaded = poll::wait_threaded(
            ctx,
            &devices_vec,
            fx.meta.poll.timeout_secs.min(300),
            fx.meta.poll.interval_secs.max(1),
        )
        .await
        .context("waiting for conversation threading")?;
        if !threaded {
            return Ok(CaseResult::inconclusive(
                cid,
                split,
                tier,
                "conversation threader did not assign all sentences in time (is THREADER_ENABLED on and the worker running?)".to_string(),
            ));
        }
    }

    // Gotham G1 graph fold. `graph_pass` correlates cross-subject edges BATCH-LOCALLY, so the
    // worker's incremental fold can't materialize a person↔plate / co-presence edge whose subjects
    // were injected in separate (serially-polled) clips. So instead of observing the incremental
    // fold: (1) wait for the graph's INPUTS to settle (conversations sealed + events committed),
    // then (2) trigger ONE authoritative rebuild that folds the whole scenario in a single batch —
    // deterministic and correct for every edge type (see `poll::wait_graph_inputs_settled`). Both
    // steps fail CLOSED to inconclusive, never a scored FAIL.
    let mut digest_sections: Option<serde_json::Value> = None;
    if fx.meta.needs_graph() {
        let settled = poll::wait_graph_inputs_settled(
            ctx,
            &devices_vec,
            fx.meta.poll.timeout_secs.min(300),
            fx.meta.poll.interval_secs.max(1),
        )
        .await
        .context("waiting for graph inputs to settle")?;
        if !settled {
            return Ok(CaseResult::inconclusive(
                cid,
                split,
                tier,
                "graph inputs did not settle in time (are conversations closing? is the threader running?)".to_string(),
            ));
        }
        if !query::trigger_graph_rebuild(ctx).await.context("triggering graph rebuild")? {
            return Ok(CaseResult::inconclusive(
                cid,
                split,
                tier,
                "graph rebuild endpoint unreachable/unauthorized (the graph modality needs the live backend graph API up)".to_string(),
            ));
        }
        // Briefing (Gotham G2 / Phase E): force-materialize the pinned-date digest AFTER the
        // authoritative rebuild folded the whole scenario, then score its structured `sections`.
        if let Some(b) = fx.expected.graph.as_ref().and_then(|g| g.briefing.as_ref()) {
            match query::generate_digest(ctx, &b.date).await.context("generating daily digest")? {
                Some(sections) => digest_sections = Some(sections),
                None => {
                    return Ok(CaseResult::inconclusive(
                        cid,
                        split,
                        tier,
                        "digest generate endpoint unreachable/unauthorized (the briefing assertion needs the live backend digest producer)".to_string(),
                    ));
                }
            }
        }
    }

    let mut obs = query::observe(ctx, &devices_vec, win_lo, win_hi, &fx.meta.modalities)
        .await
        .context("querying observed results")?;
    obs.digest_sections = digest_sections;
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
            // Session threading: the first question carrying a `session` label opens the session;
            // later questions with the same label continue it via the returned session_id. A 404
            // on a threaded id is a transport error → INCONCLUSIVE (infra drift, not quality).
            let mut sessions: std::collections::HashMap<String, String> = std::collections::HashMap::new();
            for (qi, q) in chat_gt.questions.iter().enumerate() {
                let sid = q.session.as_ref().and_then(|label| sessions.get(label)).cloned();
                match query_rag::ask(ctx, q, base_ns, sid.as_deref()).await {
                    Ok(a) => {
                        if let Some(label) = &q.session {
                            if !a.session_id.is_empty() {
                                sessions.entry(label.clone()).or_insert_with(|| a.session_id.clone());
                            }
                        }
                        answers.push(a);
                    }
                    Err(e) => return Ok(CaseResult::inconclusive(cid, split, tier, format!("rag chat q{qi} transport error: {e:#}"))),
                }
            }
            metrics.extend(
                score::score_chat(
                    chat_gt,
                    &answers,
                    ctx,
                    fx.expected.conversations.as_ref(),
                    &obs,
                    base_ns,
                )
                .await,
            );
        }
    }

    let processed = audio_done_total.max(vision_done_total);
    finalize_case(ctx, manifest, cid, split, tier, injected_total, processed, metrics, update_baseline, force)
}

/// Shared scoring tail: classify the metric vector against the config-hash baseline, assemble the
/// `CaseResult`, and (on request) persist a new baseline. Used by the media pipeline AND the
/// service-level advisor path so verdict/baseline semantics can't drift between them.
#[allow(clippy::too_many_arguments)]
fn finalize_case(
    ctx: &Ctx,
    manifest: &EnvManifest,
    cid: &str,
    split: &str,
    tier: &str,
    injected: usize,
    processed: i64,
    metrics: Vec<score::Metric>,
    update_baseline: bool,
    force: bool,
) -> Result<CaseResult> {
    let baseline = baseline::load(ctx, &manifest.config_hash, cid);
    let bmap = baseline.as_ref().map(|b| b.metrics.clone());
    let case = CaseResult::from_metrics(cid, split, tier, injected, processed, metrics.clone(), |m| {
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

/// The `advisor` modality: reset the advisor's per-run state, run the fixture's scripted
/// conversation against the live service (threading ONE session across turns), and score the
/// captured turn shapes. Preconditions fail CLOSED to INCONCLUSIVE — a missing service, an
/// un-ingested corpus, or a mid-conversation transport error is infrastructure (exit 2), never a
/// false regression; only assertion failures on successful turns produce a FAIL.
async fn run_advisor_case(
    ctx: &Ctx,
    fx: &Fixture,
    manifest: &EnvManifest,
    update_baseline: bool,
    force: bool,
) -> Result<CaseResult> {
    let (cid, split, tier) = (fx.meta.case_id.as_str(), fx.split.as_str(), fx.meta.tier.as_str());
    let Some(gt) = &fx.expected.advisor else {
        return Ok(CaseResult::inconclusive(
            cid,
            split,
            tier,
            "advisor modality listed but expected.json has no `advisor` block".to_string(),
        ));
    };

    // Precondition 1: the ingested book corpus. Every advisor answer is grounded in `book_chunks`;
    // an empty corpus can only produce garbage, and a failed probe means the advisor migrations
    // aren't applied. Both are infrastructure.
    match query_advisor::book_chunk_count(ctx).await {
        Ok(0) => {
            return Ok(CaseResult::inconclusive(
                cid,
                split,
                tier,
                "book corpus empty (book_chunks=0) — run ingest-book".to_string(),
            ));
        }
        Ok(_) => {}
        Err(e) => {
            return Ok(CaseResult::inconclusive(
                cid,
                split,
                tier,
                format!("book corpus probe failed (advisor migrations applied?): {e:#} — run ingest-book"),
            ));
        }
    }

    // Precondition 2: the advisor service is OPTIONAL — absence must never redden the suite.
    if !query_advisor::advisor_up(ctx).await {
        return Ok(CaseResult::inconclusive(
            cid,
            split,
            tier,
            "advisor service unreachable (the advisor modality needs the live :8095 service)".to_string(),
        ));
    }

    // Invariant 1 for the advisor surface: pinned starting state. Sessions/messages/memories are
    // per-run artifacts (a prior run's MEMORIZED facts would bleed into this run's answers); the
    // corpus is reference data and survives (see reset::reset_advisor).
    if let Err(e) = reset::reset_advisor(ctx).await {
        return Ok(CaseResult::inconclusive(cid, split, tier, format!("advisor state reset failed: {e:#}")));
    }

    // The scripted conversation: turn 1's `session` event mints the session; every later turn
    // continues it, so follow-up rounds and memory are exercised for real.
    let mut turns = Vec::with_capacity(gt.turns.len());
    let mut session_id: Option<String> = None;
    for (ti, t) in gt.turns.iter().enumerate() {
        match query_advisor::ask(ctx, &t.message, session_id.as_deref()).await {
            Ok(r) => {
                if session_id.is_none() && !r.session_id.is_empty() {
                    session_id = Some(r.session_id.clone());
                }
                turns.push(r);
            }
            Err(e) => {
                return Ok(CaseResult::inconclusive(cid, split, tier, format!("advisor turn t{ti} transport error: {e:#}")));
            }
        }
    }

    let metrics = score::score_advisor(gt, &turns);
    // injected/processed = scripted/completed turns (the advisor analog of segment counts).
    finalize_case(ctx, manifest, cid, split, tier, gt.turns.len(), turns.len() as i64, metrics, update_baseline, force)
}
