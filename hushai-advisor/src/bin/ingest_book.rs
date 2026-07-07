//! One-shot book ingest CLI.
//!
//! Usage:
//!   cargo run -p hushai-advisor --bin ingest-book -- \
//!     --dir "Agent Ahithophel/books/chapters_text" \
//!     --slug yes-50-ways \
//!     --title "Yes!, 50 Scientifically Proven Ways to Be Persuasive" \
//!     [--author "Goldstein, Martin, Cialdini"] [--no-llm-clean] [--chunk-chars 1500]
//!
//! NB: the running-head strip patterns derive from the title's comma/colon parts
//! (ingest.rs) — keep the comma so a lone "Yes!" page-top head line is stripped.

use std::sync::Arc;

use anyhow::{Context, anyhow};

use hushai_advisor::config::AdvisorConfig;
use hushai_advisor::embed::Embedder;
use hushai_advisor::ingest::{IngestOpts, ingest_book};
use hushai_advisor::llm::Llm;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    dotenvy::from_path("hushai-backend/.env").ok();
    hushai_advisor::init_tracing();

    let mut dir = None;
    let mut slug = None;
    let mut title = None;
    let mut author = None;
    let mut llm_clean = true;
    let mut chunk_chars: usize = 1500;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dir" => dir = args.next(),
            "--slug" => slug = args.next(),
            "--title" => title = args.next(),
            "--author" => author = args.next(),
            "--no-llm-clean" => llm_clean = false,
            "--chunk-chars" => {
                chunk_chars = args
                    .next()
                    .ok_or_else(|| anyhow!("--chunk-chars needs a value"))?
                    .parse()
                    .context("--chunk-chars must be a number")?;
            }
            other => return Err(anyhow!("unknown argument: {other}")),
        }
    }
    let dir = dir.ok_or_else(|| anyhow!("--dir is required"))?;
    let slug = slug.ok_or_else(|| anyhow!("--slug is required"))?;
    let title = title.ok_or_else(|| anyhow!("--title is required"))?;

    let cfg = AdvisorConfig::from_env()?;
    let backend_cfg = hushai_backend::config::Config::from_env()
        .context("loading shared backend config (DATABASE_URL)")?;
    let pool = hushai_backend::db::connect(&backend_cfg)
        .await
        .context("connecting to Postgres")?;
    sqlx::migrate!("../hushai-backend/migrations")
        .run(&pool)
        .await
        .context("running migrations")?;

    let embedder = Arc::new(Embedder::new(&cfg.embed_ollama_base_url, &cfg.embed_model)?);
    let llm = Arc::new(Llm::new(
        &cfg.llm_ollama_base_url,
        &cfg.llm_model,
        cfg.judge_model.as_deref(),
        cfg.llm_temperature,
        cfg.llm_seed,
        cfg.llm_num_ctx,
    )?);

    let opts = IngestOpts {
        dir,
        slug,
        title,
        author,
        llm_clean,
        chunk_target_chars: chunk_chars,
        embed_model: cfg.embed_model.clone(),
    };
    let report = ingest_book(&pool, &embedder, &llm, &opts).await?;
    println!(
        "ingest complete: {} chapters seen, {} updated, {} skipped (unchanged), {} chunks embedded, {} LLM-clean fallbacks",
        report.chapters_seen,
        report.chapters_updated,
        report.chapters_skipped,
        report.chunks_embedded,
        report.llm_clean_fallbacks
    );
    Ok(())
}
