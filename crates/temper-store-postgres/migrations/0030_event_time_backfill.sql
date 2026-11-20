-- ADR-0187 "Backfill": the durable cursor of the event-time fill, which runs only when
-- TEMPER_EVENT_TIME_BACKFILL=fill. Counting writes nothing and does not use this table.
CREATE TABLE IF NOT EXISTS event_time_backfill (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    tenant TEXT NOT NULL DEFAULT '', entity_type TEXT NOT NULL DEFAULT '', entity_id TEXT NOT NULL DEFAULT '',
    records BIGINT NOT NULL DEFAULT 0, first_filled BIGINT NOT NULL DEFAULT 0,
    last_filled BIGINT NOT NULL DEFAULT 0, rows_written BIGINT NOT NULL DEFAULT 0,
    complete BOOLEAN NOT NULL DEFAULT false
);
