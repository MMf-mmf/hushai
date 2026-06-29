-- 0018_audit_log_immutable.sql — enforce that audit_log entries are TAMPER-EVIDENT at the DB level,
-- not merely by convention (B6 review finding). INSERT is the only mutation allowed; UPDATE of an
-- existing row is rejected outright, so the app's own DB role cannot silently rewrite history.
-- DELETE is deliberately left allowed for a future time-based retention/purge (a coarse, auditable
-- bulk op) — the goal is "no quiet edits", not "rows live forever".

CREATE OR REPLACE FUNCTION audit_log_no_update() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'audit_log is append-only: UPDATE is not permitted';
END;
$$;

CREATE TRIGGER audit_log_block_update
    BEFORE UPDATE ON audit_log
    FOR EACH ROW EXECUTE FUNCTION audit_log_no_update();
