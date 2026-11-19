-- ADR-0187: a list page ordered by an event time reads its index in order and stops after its LIMIT.
--
-- Each key is the expression `query_page.rs` (`event_time_key_sql`) orders by: the event time as the fixed-width
-- `YYYY-MM-DDTHH:MM:SS.nnnnnnnnn`, or '9999-99-99T99:99:99.999999999' (after every time) when the row has none,
-- compared byte by byte. A page orders by the key and then `entity_id` ascending, in either direction of the key,
-- so each key has an index per direction. `status` comes last so a status filter is checked from the index.
--
-- Building an index reads every catalog row once; it holds writes to `entity_catalog` while it builds.
CREATE INDEX IF NOT EXISTS idx_entity_catalog_last_event_desc
    ON entity_catalog (tenant, entity_type,
        (COALESCE(substr((state ->> 'last_event_at'), 1, 19) || '.' || substr(rtrim(substr((state ->> 'last_event_at'), 21), 'Z') || '000000000', 1, 9), '9999-99-99T99:99:99.999999999') COLLATE "C") DESC,
        entity_id, status);

CREATE INDEX IF NOT EXISTS idx_entity_catalog_last_event_asc
    ON entity_catalog (tenant, entity_type,
        (COALESCE(substr((state ->> 'last_event_at'), 1, 19) || '.' || substr(rtrim(substr((state ->> 'last_event_at'), 21), 'Z') || '000000000', 1, 9), '9999-99-99T99:99:99.999999999') COLLATE "C"),
        entity_id, status);

CREATE INDEX IF NOT EXISTS idx_entity_catalog_first_event_desc
    ON entity_catalog (tenant, entity_type,
        (COALESCE(substr(first_event_at, 1, 19) || '.' || substr(rtrim(substr(first_event_at, 21), 'Z') || '000000000', 1, 9), '9999-99-99T99:99:99.999999999') COLLATE "C") DESC,
        entity_id, status);

CREATE INDEX IF NOT EXISTS idx_entity_catalog_first_event_asc
    ON entity_catalog (tenant, entity_type,
        (COALESCE(substr(first_event_at, 1, 19) || '.' || substr(rtrim(substr(first_event_at, 21), 'Z') || '000000000', 1, 9), '9999-99-99T99:99:99.999999999') COLLATE "C"),
        entity_id, status);
