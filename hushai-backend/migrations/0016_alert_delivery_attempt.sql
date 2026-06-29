-- 0016_alert_delivery_attempt.sql — give the alert-delivery outbox a retry clock so the worker's
-- delivery loop (roadmap A4) can claim → POST → retry-with-backoff crash-safely.
--
-- `next_attempt_at` is the lease/backoff timestamp: NULL or <= now() means "due". The worker claims a
-- batch by atomically bumping `attempts` + pushing `next_attempt_at` a lease into the future (so a
-- crashed in-flight send is re-claimed only after the lease, never lost, never double-claimed by
-- FOR UPDATE SKIP LOCKED peers). On a transient failure it's set to now()+backoff(attempts); on
-- success the row goes status='sent'; past max attempts it goes status='failed'. The `feed` channel
-- is delivered in-app (the viewer reads it) and is never touched by this loop.

ALTER TABLE alert_deliveries ADD COLUMN next_attempt_at timestamptz;

-- The claim hot path: due, pending, non-feed deliveries, oldest first. Partial so it stays tiny
-- (almost everything is 'sent'/'feed'). next_attempt_at NULLS FIRST so brand-new rows are due.
CREATE INDEX alert_deliveries_due_idx
    ON alert_deliveries (next_attempt_at NULLS FIRST)
    WHERE status = 'pending' AND channel <> 'feed';
