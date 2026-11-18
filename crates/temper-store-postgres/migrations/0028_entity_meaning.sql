-- ADR-0186: one meaning row per record, written with its query-plane row.
--
-- `text` is the record's text and `text_hash` its hash. The vector is filled after
-- the save by the kernel's model: empty (and `model_tag` empty) until then, and
-- cleared again when the text changes. `vector` is packed little-endian f32.
-- `entity_vector_index` (0012) stays, unused, so the previous release can roll back.
CREATE TABLE IF NOT EXISTS entity_meaning (
    tenant      TEXT  NOT NULL,
    entity_type TEXT  NOT NULL,
    entity_id   TEXT  NOT NULL,
    text        TEXT  NOT NULL,
    text_hash   TEXT  NOT NULL,
    model_tag   TEXT  NOT NULL DEFAULT '',
    vector      BYTEA NOT NULL DEFAULT ''::bytea,
    PRIMARY KEY (tenant, entity_type, entity_id)
);

-- Rows that wait for the model: a tenant's rows whose model tag is not the current one.
CREATE INDEX IF NOT EXISTS idx_entity_meaning_pending
    ON entity_meaning (tenant, model_tag, entity_type, entity_id);

-- Which types had their earlier records given a meaning row, by which text rule.
CREATE TABLE IF NOT EXISTS entity_meaning_backfill (
    tenant       TEXT        NOT NULL,
    entity_type  TEXT        NOT NULL,
    rule         TEXT        NOT NULL,
    completed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, entity_type)
);

-- A meaning row holds the record's text: the same tenant isolation as its catalog row.
ALTER TABLE entity_meaning ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON entity_meaning;
CREATE POLICY tenant_isolation ON entity_meaning USING (tenant = current_setting('app.current_tenant', true));
ALTER TABLE entity_meaning_backfill ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON entity_meaning_backfill;
CREATE POLICY tenant_isolation ON entity_meaning_backfill USING (tenant = current_setting('app.current_tenant', true));
