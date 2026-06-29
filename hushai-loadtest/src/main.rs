//! hushai-loadtest — camera fan-out capacity harness. See the crate-level docs in Cargo.toml and
//! `docs/hardware-sizing-30-cameras.md`. Replays one clip as N synthetic cameras (identity-only),
//! ramps 1..N, and reports the per-camera cost + saturation point.

mod camera;
mod config;
mod controller;
mod corpus;
mod report;
mod saturation;
mod scrape;
mod sysload;

use anyhow::{Context, Result};
use clap::Parser;
use config::Cli;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    if cli.cleanup {
        return cleanup(&cli).await;
    }
    controller::run(cli).await
}

/// Delete synthetic load-test devices via the backend API (HTTP-only; no DB coupling).
async fn cleanup(cli: &Cli) -> Result<()> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(cli.insecure)
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let base = cli.backend_base();
    let token = cli.token();

    let devices: serde_json::Value = client
        .get(format!("{base}/v1/devices"))
        .bearer_auth(&token)
        .send()
        .await
        .context("GET /v1/devices")?
        .error_for_status()
        .context("GET /v1/devices status")?
        .json()
        .await
        .context("parse /v1/devices")?;

    let list = devices.as_array().cloned().unwrap_or_default();
    let mut removed = 0usize;
    for d in list {
        let kind = d.get("source_kind").and_then(|v| v.as_str()).unwrap_or("");
        if kind != "loadtest_replica" {
            continue;
        }
        let Some(id) = d.get("device_id").and_then(|v| v.as_str()) else { continue };
        let resp = client
            .delete(format!("{base}/v1/devices/{id}"))
            .bearer_auth(&token)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                removed += 1;
                tracing::info!(device = id, "deleted synthetic device");
            }
            Ok(r) => tracing::warn!(device = id, status = %r.status(), "delete failed"),
            Err(e) => tracing::warn!(device = id, error = %e, "delete error"),
        }
    }
    tracing::info!("cleanup complete: removed {removed} loadtest_replica device(s)");
    Ok(())
}
