-- 0031_gotham.sql — the Gotham "Detective" agentic runtime (G3, Gotham.md Part 2 §2.6). The graph
-- (0028–0030) is the DERIVED fact layer; this migration adds the two pieces the AGENT needs on top of
-- the existing chat surface (chat_sessions/chat_messages, 0008):
--
--   1. chat_messages.tool_trace — a per-assistant-turn record of WHICH tools the agent ran (shape,
--      not payloads: seq/tool/args/ok/elapsed_ms/result_chars/outcome). The full tool RESULTS are
--      never persisted (they can be large + re-derivable); audit_log carries the args for forensics.
--      NULL on every non-Gotham assistant turn (and user turns), so existing chat rows are unchanged.
--
--   2. gotham_pending_actions — the two-phase confirmation outbox for MUTATING tools (watchlist /
--      alert-rule create-delete, Wave 3 of G3). A mutate tool call does NOT execute on first sight:
--      the runtime persists a pending row + speaks a deterministic confirmation summary, and only an
--      affirmative NEXT user turn (detected before condensation/routing) executes it. At most ONE
--      pending action per session at a time (the partial-unique index) — a new mutate proposal
--      supersedes an unconfirmed one only after it is cancelled/expired.
--
-- Read-only Phase 1 (tools 1–14 + ask_user) writes tool_trace but never a pending action.
-- See Gotham.md §2.4 (agent loop) / §2.5 (safety/audit) / §2.7 (surfaces).

-- 1. Tool trace on assistant chat turns. jsonb array; NULL for user turns and non-Gotham assistants.
ALTER TABLE chat_messages ADD COLUMN tool_trace jsonb;
COMMENT ON COLUMN chat_messages.tool_trace IS
    'Gotham agent tool trace (assistant turns only): [{seq,tool,args,ok,elapsed_ms,result_chars,outcome}]. Shape, not payloads; NULL elsewhere.';

-- 2. Pending mutating actions awaiting the user's yes/no confirmation.
CREATE TABLE gotham_pending_actions (
    action_id  uuid PRIMARY KEY,
    session_id uuid NOT NULL REFERENCES chat_sessions (session_id) ON DELETE CASCADE,
    tool       text NOT NULL,                          -- the mutating tool name (e.g. watchlist_add)
    args       jsonb NOT NULL,                          -- parsed, validated args the confirmation summarizes
    summary    text NOT NULL,                           -- deterministic NL summary spoken to the user
    status     text NOT NULL DEFAULT 'pending',         -- pending | confirmed | cancelled | expired
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL                     -- GOTHAM_CONFIRM_TTL_SECS from creation
);

-- At most one OUTSTANDING (pending) action per session — the confirm intercept keys on it.
-- Terminal rows (confirmed/cancelled/expired) are unconstrained (audit trail of proposals).
CREATE UNIQUE INDEX gotham_pending_one_per_session
    ON gotham_pending_actions (session_id) WHERE status = 'pending';
