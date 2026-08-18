-- veille schema v1 (PRP-veille-001 §6).
-- FROZEN at the v1 release: this file was edited freely while no deployed
-- store existed. From the first deployment on, schema changes are NEW
-- migration files only — sqlx verifies applied-migration checksums.
-- All money columns are integer minor units; positive amount = inflow.
-- Naming deviation from the PRP: the spec's `transaction` table is named
-- `transactions` here because TRANSACTION is an SQL keyword.

CREATE TABLE tenant (
    id           INTEGER PRIMARY KEY,
    slug         TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL
);

-- Append-only: one row per account per sync. Never UPDATE or DELETE.
CREATE TABLE account_snapshot (
    id            INTEGER PRIMARY KEY,
    tenant_id     INTEGER NOT NULL REFERENCES tenant(id),
    external_id   TEXT NOT NULL,
    name          TEXT NOT NULL,
    institution   TEXT,
    kind          TEXT NOT NULL,
    status        TEXT NOT NULL,
    balance_minor INTEGER NOT NULL,
    currency      TEXT NOT NULL,
    as_of         TEXT NOT NULL  -- RFC 3339 UTC, time of the sync that observed it
);
CREATE INDEX idx_account_snapshot_lookup
    ON account_snapshot (tenant_id, external_id, as_of);

CREATE TABLE transactions (
    id                  INTEGER PRIMARY KEY,
    tenant_id           INTEGER NOT NULL REFERENCES tenant(id),
    external_id         TEXT NOT NULL,  -- Sure's transaction UUID (stable; upstream aggregator id may be null)
    account_external_id TEXT NOT NULL,
    posted_at           TEXT NOT NULL,  -- YYYY-MM-DD
    amount_minor        INTEGER NOT NULL,  -- signed; positive = inflow
    currency            TEXT NOT NULL,
    description         TEXT NOT NULL,
    category            TEXT,
    counterparty_key    TEXT,
    is_transfer         INTEGER NOT NULL DEFAULT 0,
    first_seen_at       TEXT NOT NULL,  -- RFC 3339 UTC
    updated_at          TEXT NOT NULL,  -- RFC 3339 UTC
    UNIQUE (tenant_id, external_id)
);
CREATE INDEX idx_transactions_account
    ON transactions (tenant_id, account_external_id, posted_at);

-- Append-only: one row per holding per sync. Never UPDATE or DELETE.
CREATE TABLE holding_snapshot (
    id                  INTEGER PRIMARY KEY,
    tenant_id           INTEGER NOT NULL REFERENCES tenant(id),
    account_external_id TEXT NOT NULL,
    symbol              TEXT NOT NULL,
    quantity            TEXT NOT NULL,      -- exact decimal string (not money)
    market_value_minor  INTEGER,            -- NULL when the upstream formatted string could not be strictly parsed
    currency            TEXT NOT NULL,
    position_date       TEXT NOT NULL,      -- YYYY-MM-DD: upstream's valuation date (may lag as_of)
    as_of               TEXT NOT NULL       -- RFC 3339 UTC, time of the sync that observed it
);
CREATE INDEX idx_holding_snapshot_lookup
    ON holding_snapshot (tenant_id, account_external_id, as_of);

-- Rolling statistics the rules compare against. Recomputed in place, not appended.
CREATE TABLE baseline (
    id          INTEGER PRIMARY KEY,
    tenant_id   INTEGER NOT NULL REFERENCES tenant(id),
    scope       TEXT NOT NULL,
    metric      TEXT NOT NULL,
    window_days INTEGER NOT NULL,
    value       TEXT NOT NULL,
    computed_at TEXT NOT NULL,
    UNIQUE (tenant_id, scope, metric, window_days)
);

CREATE TABLE finding (
    id              INTEGER PRIMARY KEY,
    tenant_id       INTEGER NOT NULL REFERENCES tenant(id),
    rule_id         TEXT NOT NULL,
    severity        TEXT NOT NULL CHECK (severity IN ('info', 'warn', 'alert')),
    subject         TEXT NOT NULL,
    summary         TEXT NOT NULL,  -- one human-readable sentence, written by the rule
    evidence        TEXT NOT NULL,  -- JSON
    detected_at     TEXT NOT NULL,
    last_seen_at    TEXT NOT NULL,  -- refreshed when the same condition is re-detected
    pushed_at       TEXT,           -- set when an Alert was successfully pushed; NULL = still owed a push
    dedupe_key      TEXT NOT NULL,
    acknowledged_at TEXT,
    UNIQUE (tenant_id, dedupe_key)
);

-- Audit trail: every send is recorded.
CREATE TABLE delivery (
    id          INTEGER PRIMARY KEY,
    tenant_id   INTEGER NOT NULL REFERENCES tenant(id),
    channel     TEXT NOT NULL,
    recipient   TEXT NOT NULL,
    finding_ids TEXT NOT NULL,  -- JSON array
    sent_at     TEXT NOT NULL
);
