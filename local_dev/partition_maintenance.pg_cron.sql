-- Optional in-database scheduling of transcript_sentences partition maintenance via
-- pg_cron. This is the alternative to the launchd job
-- (local_dev/com.hushai.partition-maintenance.plist) / system cron + the
-- local_dev/partition_maintenance.sh wrapper. Use whichever fits your deployment;
-- you do NOT need both.
--
-- Prerequisites (pg_cron is NOT installed by default on Homebrew Postgres):
--   1. Build/install pg_cron, then add to postgresql.conf:
--        shared_preload_libraries = 'pg_cron'
--        cron.database_name = 'hushai'      -- the DB pg_cron runs jobs against
--      and restart Postgres.
--   2. In the hushai database:  CREATE EXTENSION IF NOT EXISTS pg_cron;
--
-- The functions called here are defined in
-- hushai-backend/migrations/0003_scalability.sql.
--
-- This is intentionally NOT a sqlx migration: migrations run once and shouldn't depend
-- on an optional, environment-specific extension. Run it by hand where pg_cron exists:
--     psql "$DATABASE_URL" -f local_dev/partition_maintenance.pg_cron.sql

-- cron.schedule is idempotent on job NAME (re-running replaces the existing schedule).

-- 1. Pre-create the current + next 3 month partitions, monthly on the 1st at 00:05.
SELECT cron.schedule(
    'hushai-ensure-partitions',
    '5 0 1 * *',
    $$SELECT ensure_transcript_partitions(3)$$
);

-- 2. Retention: drop month partitions older than ~12 months, monthly on the 1st at 00:15.
--    *** IRREVERSIBLE — this deletes that month's transcripts/embeddings. ***
--    Change the interval to adjust the window, or unschedule this job to disable
--    retention (SELECT cron.unschedule('hushai-retention');). Rehearse first, e.g.:
--      BEGIN;
--      SELECT drop_transcript_partitions_before((date_trunc('month', now()) - interval '12 months')::date);
--      ROLLBACK;   -- confirm it targeted only the intended old partitions
SELECT cron.schedule(
    'hushai-retention',
    '15 0 1 * *',
    $$SELECT drop_transcript_partitions_before((date_trunc('month', now()) - interval '12 months')::date)$$
);

-- Inspect scheduled jobs:   SELECT jobid, schedule, jobname, command FROM cron.job;
-- Recent runs:              SELECT * FROM cron.job_run_details ORDER BY start_time DESC LIMIT 10;
