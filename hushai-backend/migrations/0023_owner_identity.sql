-- Owner identity: mark ONE speaker (voice) and/or ONE person (face) as the device owner,
-- settable from the naming UIs ("This is me") instead of only via env config.
--
-- Why a DB flag and not just OWNER_SPEAKER_ID/OWNER_PERSON_ID env vars: the env config
-- requires the operator to find a UUID and restart the service — the standing setup gotcha
-- that leaves reflection ("how have I been"), co-occurrence ("who was I with"), and the new
-- caller-identity path ("what's my name") dead on a fresh install. The flag is consulted by
-- hushai-rag's owner resolution BETWEEN request filters and the env fallback, so existing
-- env-configured deployments keep working unchanged.
--
-- Single-owner invariant: enforced by a partial UNIQUE index (at most one row WHERE
-- is_owner). The set-owner endpoint clears the previous owner in the same transaction, so
-- the invariant never trips in normal operation; it exists to make a racing double-set fail
-- loudly rather than silently produce two owners (the chat_messages seq idiom).

ALTER TABLE speakers ADD COLUMN is_owner boolean NOT NULL DEFAULT false;
ALTER TABLE persons  ADD COLUMN is_owner boolean NOT NULL DEFAULT false;

CREATE UNIQUE INDEX speakers_single_owner_idx ON speakers ((true)) WHERE is_owner;
CREATE UNIQUE INDEX persons_single_owner_idx  ON persons  ((true)) WHERE is_owner;
