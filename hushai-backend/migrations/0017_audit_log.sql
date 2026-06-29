-- 0017_audit_log.sql — an append-only AUDIT LOG of admin/operator actions (roadmap B6). Enterprise
-- VSaaS + cloud-compliance table-stakes: "who did what, when, from where, with what outcome".
--
-- Written primarily at the GATEWAY (the viewer): it is the single authenticated admin entry point,
-- sees the real client IP, and reverse-proxies every mutating /v1/* call + owns login/logout/export.
-- The backend owns this schema (like every table) and exposes the READ API (`GET /v1/audit`, proxied
-- via the viewer like the other admin surfaces); the `hushai_backend::audit` helper is shared so the
-- viewer (and, later, backend-side hooks) write through one code path.
--
-- Plain table (not partitioned): admin actions are low-volume vs detections/segments. Retention is a
-- future concern (a simple time-based purge); kept simple for now. Honest scope note: this captures
-- mutations + auth that flow THROUGH the viewer; a direct call to the backend port (device token)
-- bypasses the gateway audit — the `audit::record` helper is ready for backend-side hooks to close
-- that, deferred. Actor is coarse ("admin") under the current single-password model — per-user
-- actors arrive with RBAC (roadmap C3).

CREATE TABLE audit_log (
    audit_id     uuid PRIMARY KEY,
    ts           timestamptz NOT NULL DEFAULT now(),
    actor        text NOT NULL,                 -- who: "admin" (single-admin), "local" (auth-disabled), "system"
    actor_ip     text,                          -- client IP as seen by the viewer (NULL if unknown)
    action       text NOT NULL,                 -- semantic verb, e.g. device.delete | alert_rule.create | auth.login
    target_type  text,                          -- device | speaker | person | plate | alert_rule | session | ...
    target_id    text,                          -- the affected id, when the path carries one
    method       text,                          -- HTTP method (for proxied actions)
    path         text,                          -- request path (for proxied actions)
    status       integer,                       -- upstream/result HTTP status (NULL for non-HTTP events)
    detail       jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at   timestamptz NOT NULL DEFAULT now()
);

-- Read API hot paths: newest-first feed, filter by action / actor / target.
CREATE INDEX audit_log_ts_idx        ON audit_log (ts DESC);
CREATE INDEX audit_log_action_ts_idx ON audit_log (action, ts DESC);
CREATE INDEX audit_log_actor_ts_idx  ON audit_log (actor, ts DESC);
CREATE INDEX audit_log_target_idx    ON audit_log (target_type, target_id);
