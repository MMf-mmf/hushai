//! Reference-identity enrollment (Invariant: named tests need a target in the catalog).
//!
//! Speakers/persons: inject a short reference clip on a SEPARATE device, an hour BEFORE the case
//! window (so it's outside the scored window), wait for the lane, then name the single freshly-
//! minted identity (the catalog was emptied by reset, so it's unambiguous). Plates: direct seed.

use crate::ctx::Ctx;
use crate::fixtures::{EnrollSpec, Fixture};
use crate::{inject, reset};
use anyhow::{Context, Result, bail};
use std::time::{Duration, Instant};
use uuid::Uuid;

const ENROLL_OFFSET_NS: i64 = 3_600_000_000_000; // 1h before the case window

pub async fn enroll_all(ctx: &Ctx, fx: &Fixture, base_ns: i64) -> Result<()> {
    for (i, spec) in fx.meta.enroll.iter().enumerate() {
        match spec.modality.as_str() {
            "speaker" | "person" => enroll_identity(ctx, fx, spec, i, base_ns).await?,
            "plate" => enroll_plate(ctx, fx, spec).await?,
            other => bail!("unknown enroll modality '{other}' in {}", fx.meta.case_id),
        }
    }
    Ok(())
}

async fn enroll_identity(ctx: &Ctx, fx: &Fixture, spec: &EnrollSpec, i: usize, base_ns: i64) -> Result<()> {
    let rel = spec.r#ref.as_ref().context("enroll speaker/person requires a 'ref' clip path")?;
    let media = fx.dir.join(rel);
    let device = format!("{}-ref{i}", fx.meta.device_id);
    reset::upsert_device(ctx, &device).await?;

    let seed = format!("{}::enroll{i}", fx.meta.seed());
    let enroll_base = base_ns - ENROLL_OFFSET_NS;
    let out = inject::inject(
        ctx,
        &media,
        &device,
        &seed,
        enroll_base,
        fx.meta.seg_seconds,
        None,
        &format!("{}-enroll{i}", fx.meta.case_id),
    )?;
    let ids = out.segment_uuids()?;

    let (table, sel) = if spec.modality == "speaker" {
        ("segment_transcription_status", "SELECT DISTINCT speaker_id FROM speaker_segments WHERE device_id=$1 AND speaker_id IS NOT NULL")
    } else {
        ("segment_vision_status", "SELECT DISTINCT person_id FROM person_segments WHERE device_id=$1 AND person_id IS NOT NULL")
    };
    wait_lane(ctx, table, &ids).await?;

    let minted: Vec<Uuid> = sqlx::query_scalar(sel).bind(&device).fetch_all(&ctx.pool).await?;
    let id = minted
        .first()
        .copied()
        .with_context(|| format!("enroll '{}' minted no {} identity", spec.name, spec.modality))?;
    let upd = if spec.modality == "speaker" {
        "UPDATE speakers SET display_name=$1, updated_at=now() WHERE speaker_id=$2"
    } else {
        "UPDATE persons SET display_name=$1, updated_at=now() WHERE person_id=$2"
    };
    sqlx::query(upd).bind(&spec.name).bind(id).execute(&ctx.pool).await?;
    Ok(())
}

async fn enroll_plate(ctx: &Ctx, fx: &Fixture, spec: &EnrollSpec) -> Result<()> {
    let text = spec.plate_text.as_ref().context("enroll plate requires 'plate_text'")?;
    let norm: String = text.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_uppercase()).collect();
    let device = fx.meta.device_id.clone();
    let now_ns = fx.meta.base_capture_unix_nanos;
    sqlx::query(
        "INSERT INTO license_plates
           (plate_id, plate_text, plate_text_norm, display_name,
            first_seen_unix_nanos, last_seen_unix_nanos, first_seen_device_id)
         VALUES ($1, $2, $3, $4, $5, $5, $6)
         ON CONFLICT (plate_text_norm) DO UPDATE SET display_name = EXCLUDED.display_name",
    )
    .bind(Uuid::now_v7())
    .bind(text)
    .bind(&norm)
    .bind(&spec.name)
    .bind(now_ns)
    .bind(&device)
    .execute(&ctx.pool)
    .await
    .context("direct-seeding license_plates")?;
    Ok(())
}

async fn wait_lane(ctx: &Ctx, table: &str, ids: &[Uuid]) -> Result<()> {
    let max_attempts: i32 = std::env::var("MAX_ATTEMPTS").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
    let timeout = Duration::from_secs(180);
    let start = Instant::now();
    loop {
        // SAFE: `table` is one of two hardcoded status-table names, never user input.
        let rows: Vec<(String, i32)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT status, attempts FROM {table} WHERE segment_id = ANY($1)"
        )))
        .bind(ids)
        .fetch_all(&ctx.pool)
        .await?;
        let settled = rows
            .iter()
            .filter(|(s, a)| s == "done" || (s == "error" && *a >= max_attempts))
            .count();
        if !ids.is_empty() && settled == ids.len() {
            return Ok(());
        }
        if start.elapsed() > timeout {
            bail!("enrollment lane {table} did not complete ({settled}/{} settled)", ids.len());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
