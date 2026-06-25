#!/usr/bin/env bash
#
# partition_maintenance.sh — keep transcript_sentences' monthly partitions ahead of
# incoming inserts and (optionally) enforce a retention window. Idempotent; safe to
# run on a schedule.
#
# Why this exists: hushai-worker calls `SELECT ensure_transcript_partitions(3)` once
# at startup, which only buys ~3 months of headroom. A long-running deployment that
# never restarts would eventually write into transcript_sentences_default once those
# months pass (still correct, but un-pruned and un-prunable). Run this monthly — via
# launchd (see local_dev/com.hushai.partition-maintenance.plist), system cron, or
# pg_cron (see local_dev/partition_maintenance.pg_cron.sql) — so a real month
# partition always exists *before* the month rolls over.
#
# The helper functions it calls (ensure_transcript_partitions /
# drop_transcript_partitions_before) are defined in
# hushai-backend/migrations/0003_scalability.sql.
#
# Environment:
#   DATABASE_URL   (required)  e.g. postgres://mf@localhost:5432/hushai
#   MONTHS_AHEAD   (default 3) number of future month partitions to pre-create.
#   RETAIN_MONTHS  (default 12) retention window. Month partitions older than the
#                  trailing RETAIN_MONTHS (plus the current month) are DROPPED.
#                  *** Dropping a partition is IRREVERSIBLE — it deletes that month's
#                  transcripts/embeddings. *** Set RETAIN_MONTHS=0 to DISABLE
#                  retention entirely (only ever create partitions, never drop).
#   DRY_RUN        (default 0) if 1, only PRINT which partitions retention would drop;
#                  do not drop anything. Always rehearse with DRY_RUN=1 first.
#
# Examples:
#   DATABASE_URL=postgres://mf@localhost:5432/hushai ./partition_maintenance.sh
#   DRY_RUN=1 RETAIN_MONTHS=6 DATABASE_URL=... ./partition_maintenance.sh   # preview
#   RETAIN_MONTHS=0 DATABASE_URL=... ./partition_maintenance.sh             # never drop
set -euo pipefail

: "${DATABASE_URL:?set DATABASE_URL (e.g. postgres://mf@localhost:5432/hushai)}"
MONTHS_AHEAD="${MONTHS_AHEAD:-3}"
RETAIN_MONTHS="${RETAIN_MONTHS:-12}"
DRY_RUN="${DRY_RUN:-0}"

psql_q() { psql "$DATABASE_URL" -X -q -t -A -v ON_ERROR_STOP=1 "$@"; }

ts() { date '+%Y-%m-%dT%H:%M:%S%z'; }
log() { echo "[$(ts)] partition-maintenance: $*"; }

# 1. Create the current + next MONTHS_AHEAD month partitions (idempotent).
log "ensuring current + ${MONTHS_AHEAD} future month partitions"
psql_q -c "SELECT ensure_transcript_partitions(${MONTHS_AHEAD});" >/dev/null
log "month partitions now: $(psql_q -c "
  SELECT string_agg(c.relname, ', ' ORDER BY c.relname)
  FROM pg_inherits i
  JOIN pg_class c ON c.oid = i.inhrelid
  JOIN pg_class p ON p.oid = i.inhparent
  WHERE p.relname = 'transcript_sentences'
    AND c.relname ~ '^transcript_sentences_[0-9]{4}_[0-9]{2}$';")"

# Surface any rows that slipped into the DEFAULT partition (should always be 0).
default_rows="$(psql_q -c "SELECT count(*) FROM transcript_sentences_default;")"
if [ "${default_rows}" != "0" ]; then
  log "WARNING: transcript_sentences_default holds ${default_rows} rows — a month partition was missing when they were inserted. Schedule this job more reliably."
fi

# 2. Retention (optional). RETAIN_MONTHS=0 disables it.
if [ "${RETAIN_MONTHS}" -le 0 ] 2>/dev/null; then
  log "retention disabled (RETAIN_MONTHS=${RETAIN_MONTHS}); not dropping any partitions"
  exit 0
fi

# cutoff = first day of (current month - RETAIN_MONTHS). drop_transcript_partitions_before
# drops month partitions strictly before date_trunc('month', cutoff), so this keeps the
# trailing RETAIN_MONTHS months plus the current month.
cutoff="$(psql_q -c "SELECT (date_trunc('month', now()) - make_interval(months => ${RETAIN_MONTHS}))::date;")"

targets="$(psql_q -c "
  SELECT c.relname
  FROM pg_inherits i
  JOIN pg_class c ON c.oid = i.inhrelid
  JOIN pg_class p ON p.oid = i.inhparent
  WHERE p.relname = 'transcript_sentences'
    AND c.relname ~ '^transcript_sentences_[0-9]{4}_[0-9]{2}\$'
    AND to_date(right(c.relname, 7), 'YYYY_MM') < date_trunc('month', '${cutoff}'::date)::date
  ORDER BY c.relname;")"

if [ -z "${targets}" ]; then
  log "retention (keep ${RETAIN_MONTHS} months, cutoff ${cutoff}): nothing older to drop"
  exit 0
fi

log "retention (keep ${RETAIN_MONTHS} months, cutoff ${cutoff}) targets: $(echo "${targets}" | tr '\n' ' ')"
if [ "${DRY_RUN}" = "1" ]; then
  log "DRY_RUN=1 — not dropping anything"
  exit 0
fi

psql_q -c "SELECT drop_transcript_partitions_before('${cutoff}'::date);" >/dev/null
log "dropped $(echo "${targets}" | wc -l | tr -d ' ') partition(s) older than ${cutoff}"
