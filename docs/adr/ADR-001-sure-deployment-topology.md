# ADR-001: Sure deployment topology

**Status:** Accepted — signed off by JD, 2026-08-18
**Date:** 2026-08-18
**Decides:** PRP-veille-001 §3 (topology) · raises one transport recommendation for §5

---

## Question

Sure's `/mcp` endpoint is configured with a single global `MCP_API_TOKEN` +
`MCP_USER_EMAIL` pair. Sure has a family/tenancy model. Can one Sure instance's
`/mcp` serve data for more than one family? The answer determines whether
`veille` watches one shared instance or one instance per tenant.

## Method

Live experiment against the published image, unmodified, plus a source read for
mechanism. Everything is reproducible from `docs/phase0/rig/`.

- Image: `ghcr.io/we-promise/sure:stable` — resolved at test time to
  `sha256:12361b7b309f867002b8a1f54200607ca1a321e1b57cf21890caaf83602c3cd0`
- Source read at commit `e5750a6` (upstream `main`; mechanism only, empirical
  claims below were all verified against the running `:stable` image)
- Rig: podman 5.8.4 + compose, Postgres 16, Redis 7, web + sidekiq worker
- Seed: two families in one instance —
  - **Family A** "Demo Family": full realistic demo dataset
    (`Demo::Generator`, seed 42), admin `alpha.owner@phase0.test`, 20 accounts
  - **Family B** "Beta Family": admin `beta.owner@phase0.test`, one account
    `BETA-CANARY-CHECKING` and one transaction `BETA-CANARY-DEPOSIT` as canaries
- Probes: JSON-RPC calls to `/mcp` (`initialize`, `tools/list`, `tools/call`),
  captured under `fixtures/phase0/`, with the instance scoped first to the
  Family A user, then re-created scoped to the Family B user

## Evidence

**`/mcp` is single-user-scoped, therefore single-family-scoped.** Every MCP
request authenticates and then resolves exactly one `User` — from
`MCP_USER_EMAIL` (env-token path) or the OAuth token's resource owner — and
every tool instance is constructed with that user. There is no family or user
parameter anywhere in the protocol surface.

Empirically, with the instance scoped to Family A:

| Probe | Result |
|---|---|
| `get_accounts` | 20 Family A accounts, no Beta data |
| `get_transactions` search `"BETA-CANARY"` | `total_results: 0` |
| `get_transactions` filter `accounts: ["BETA-CANARY-CHECKING"]` | `total_results: 0` (no error, no data) |
| grep all captured responses for canary strings | zero hits |
| wrong bearer token | HTTP 401 |

Scoped to Family B (only `MCP_USER_EMAIL` changed): the canary account and
transaction appear; searching Family A's data (`"Chase"`, account filter
`"Chase Premier Checking"`) returns `total_results: 0`. Isolation holds in both
directions. One instance cannot serve two families through the env-token path,
which is the only path that fits an unattended service.

**Scoping is stricter than "family": it is per-user account access.** Account
visibility goes through `Account.accessible_by(user)` — accounts the user
*owns* or that are *shared* with them via `account_shares`. A family member
without full account shares sees a subset. Deployment consequence: the identity
`veille` connects as must own, or have been granted shares to, every account it
is supposed to watch. Verify this at onboarding, not after the first missed
finding.

**A second finding changes the adapter recommendation.** Sure also ships a
versioned REST API at `/api/v1/` (same image, same port), authenticated by
per-user API keys that carry exactly one scope, `read` or `read_write`.
Comparison against what `SureSource` (§5) needs:

| Requirement | `/mcp` | `/api/v1` |
|---|---|---|
| Stable account IDs | none — accounts keyed by display name only | UUID `id` on every record |
| Money as integers | formatted strings (`"$18,000.00"`), floats elsewhere | `*_cents` integer fields alongside display strings |
| Transaction identity | `id` present | `id` + upstream `external_id` |
| Balance history | monthly series, formatted strings, inside `get_accounts` | `/balances?account_id=` — daily rows, `balance_cents`, paginated |
| Per-institution sync state (feeds `sync-stale`) | **not exposed at all** | `/syncs`, `/syncs/latest` — status, `completed_at`, `failed_at`, `error`; `/provider_connections` |
| Read-only credential | **impossible** — the token exposes write tools (`update_transaction`, `import_bank_statement`, `create_*`, …); OAuth alternative is `read_write` | `read`-scoped key; write attempt returns 403, enforced server-side |
| Pagination | fixed page size 50, `total_pages` | `per_page` + `pagination.total_count` |
| Tenant isolation | verified (this experiment) | verified — Beta's key sees only the canary account |

The MCP surface fails §5 on two points: `UpstreamHealth` per-institution
last-successful-sync data does not exist there, and money arrives as
locale-formatted strings. Per the PRP §3 stop rule ("if `/mcp` does not expose
enough to satisfy `SureSource`, stop and report back"), this ADR is that
report — with the observation that the fallback is not Postgres: the REST API
is a supported, versioned HTTP surface of the unmodified image.

Rate limit note: standard API keys are limited to 100 requests/hour (Redis
sliding window). A daily sync fits trivially; the one-time balance-history
backfill must paginate politely or spread across hours.

## Decision

1. **One Sure instance per tenant** — per the PRP §3 decision rule, confirmed
   empirically. Each tenant gets its own Sure web + worker + Postgres + Redis
   and its own credentials. `veille` config holds a list of upstreams. This
   also preserves hard data isolation and lets any tenant take their instance
   with them intact.

2. **Recommended (requires sign-off, departs from PRP §5's assumed transport):**
   implement `SureSource` over `/api/v1` with a per-tenant `read`-scoped API
   key, not over `/mcp`. The trait is transport-agnostic, so nothing above the
   adapter changes; the implementation becomes `ApiSureSource` instead of
   `McpSureSource`. This choice makes invariant §2.1 ("never holds a credential
   that could write") true *at the server*, not merely in our code, and it is
   the only surface that can feed the `sync-stale` rule — the rule the PRP says
   to build first.

## Consequences

- Compose overlay and config gain per-tenant upstream entries (base URL +
  API key); `MCP_API_TOKEN` / `MCP_USER_EMAIL` need not be set on tenant
  instances at all, shrinking the attack surface.
- The `rmcp` dependency (§10) is unnecessary if the recommendation is
  accepted — plain HTTP + serde against `/api/v1`. The Phase 0 stack-override
  question ("is `rmcp` mature enough?") dissolves.
- Onboarding checklist gains: create the veille API user, grant it shares to
  every watched account, create a `read` API key, verify
  `GET /api/v1/accounts` count matches expectation.
- Holdings caveat from PRP Appendix A stands regardless of transport: validate
  `holding_snapshot` against real institutions before trusting it.
- If upstream ever removes or breaks `/api/v1`, fall back to `/mcp` behind the
  same trait (accepting degraded health data) — Postgres remains the last
  resort and still requires a human decision.

## Sign-off

- [x] Topology: one instance per tenant — accepted 2026-08-18
- [x] Transport: `SureSource` over `/api/v1` + read-scoped API key — accepted
      2026-08-18 (`/mcp` remains the documented fallback behind the same trait)
