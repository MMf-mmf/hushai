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
pub mod query;
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
    reset::upsert_device(ctx, &fx.meta.device_id).await?;

    if let Err(e) = enroll::enroll_all(ctx, fx, base_ns).await {
        return Ok(CaseResult::inconclusive(cid, split, tier, format!("enroll failed: {e:#}")));
    }

    let inj = match inject::inject(
        ctx,
        &fx.media_path(),
        &fx.meta.device_id,
        &fx.meta.seed(),
        base_ns,
        fx.meta.seg_seconds,
        fx.meta.limit,
        cid,
    ) {
        Ok(o) => o,
        Err(e) => return Ok(CaseResult::inconclusive(cid, split, tier, format!("inject failed: {e:#}"))),
    };
    let ids = inj.segment_uuids()?;

    let poll = poll::wait_until_complete(ctx, &fx.meta, &ids, base_ns).await?;
    if !poll.settled {
        return Ok(CaseResult::inconclusive(
            cid,
            split,
            tier,
            format!(
                "processing incomplete: audio_done={} vision_done={} injected={} timed_out={} errors={:?}",
                poll.audio_done, poll.vision_done, poll.injected, poll.timed_out, poll.errors
            ),
        ));
    }

    let obs = query::observe(ctx, &fx.meta.device_id, base_ns, inj.end_unix_nanos(), &fx.meta.modalities)
        .await
        .context("querying observed results")?;
    let metrics = score::score_all(&fx.expected, &obs, base_ns, &fx.meta.modalities);

    let baseline = baseline::load(ctx, &manifest.config_hash, cid);
    let bmap = baseline.as_ref().map(|b| b.metrics.clone());
    let processed = poll.audio_done.max(poll.vision_done);
    let case = CaseResult::from_metrics(cid, split, tier, poll.injected, processed, metrics.clone(), |m| {
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
