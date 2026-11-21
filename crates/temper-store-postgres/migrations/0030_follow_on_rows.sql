-- Moves' follow-on rows: what a committed move still owes (its reactions,
-- children and integrations), sent again by a boot until it settles. The
-- same columns as scheduled_actions and apart from it: a release from before
-- follow-on rows reads only scheduled_actions, so a rollback never fires a
-- follow-on row as a scheduled action of the move's own action.
CREATE TABLE IF NOT EXISTS follow_on_rows (
    tenant      TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id   TEXT NOT NULL,
    arm_id      TEXT NOT NULL,
    action      TEXT NOT NULL,
    due_at      TEXT NOT NULL,
    armed_by    TEXT NOT NULL,
    PRIMARY KEY (tenant, entity_type, entity_id, arm_id)
);

CREATE INDEX IF NOT EXISTS idx_follow_on_rows_due ON follow_on_rows (tenant, due_at COLLATE "C");

ALTER TABLE follow_on_rows ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON follow_on_rows; -- additive-ok: replaced by the identical policy on the next line; this table is new
CREATE POLICY tenant_isolation ON follow_on_rows USING (tenant = current_setting('app.current_tenant', true));
