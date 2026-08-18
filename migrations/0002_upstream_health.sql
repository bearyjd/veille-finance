-- Per-institution upstream health, captured at sync time so `veille evaluate`
-- can run offline and deterministically from the store alone. Replaced per
-- (tenant, institution) on every sync — current state, not history.

CREATE TABLE upstream_health (
    id                      INTEGER PRIMARY KEY,
    tenant_id               INTEGER NOT NULL REFERENCES tenant(id),
    institution             TEXT NOT NULL,
    last_successful_sync_at TEXT,           -- RFC 3339 UTC; NULL = no successful sync observed
    fetched_at              TEXT NOT NULL,  -- RFC 3339 UTC, when this was observed upstream
    UNIQUE (tenant_id, institution)
);
