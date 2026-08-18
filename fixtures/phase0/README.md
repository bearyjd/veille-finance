# Phase 0 golden captures

Live responses from `ghcr.io/we-promise/sure:stable`
(`sha256:12361b7b309f867002b8a1f54200607ca1a321e1b57cf21890caaf83602c3cd0`),
captured 2026-08-18 by `docs/phase0/rig/probe_mcp.sh` against the two-family
rig seeded by `docs/phase0/rig/seed_two_families.rb`.

**Provenance: synthetic only.** All data derives from Sure's `Demo::Generator`
(seed 42) plus hand-made `BETA-CANARY-*` records. No real accounts (PRP §13).
Tokens appearing in captures (`phase0-mcp-token-family-a`, `phase0_rest_key_*`)
are throwaway rig values, already invalid anywhere but a rebuilt rig.

- `mcp-alpha/` — `/mcp` scoped to the Family A admin (20-account demo family)
- `mcp-beta/` — same instance, `MCP_USER_EMAIL` flipped to the Family B admin
- `rest/` — `/api/v1` with a `read`-scoped API key (Family A user)

These are evidence for ADR-001 and seed material for `FixtureSureSource`.
Interpretation: `docs/phase0/sure-api-surface.md`.
