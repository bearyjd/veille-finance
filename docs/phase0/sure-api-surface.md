# Sure API surface — Phase 0 findings

Empirical documentation of the two HTTP surfaces on `ghcr.io/we-promise/sure:stable`
(`sha256:12361b7b…`, probed 2026-08-18) relevant to `SureSource`. Raw captured
responses live in `fixtures/phase0/`; the rig that produced them is in
`docs/phase0/rig/`. Topology conclusions are in ADR-001.

---

## 1. `/mcp` (JSON-RPC 2.0)

- `POST /mcp`, same port as the web UI (3000).
- Methods: `initialize`, `tools/list`, `tools/call`. Nothing else
  (`-32601` otherwise). Protocol version string: `2025-03-26`.
- Notifications (no `id`) get HTTP 204 and no body.

### Auth

| Path | Credential | Resolves to | Notes |
|---|---|---|---|
| Env token | `Authorization: Bearer $MCP_API_TOKEN` | the single `MCP_USER_EMAIL` user | constant-time compare; 401 on mismatch; 401 (not 5xx) when email matches no user |
| OAuth (Doorkeeper) | access token, scope `read_write` | token's resource owner | per-user, but write-capable by scope definition; dynamic client registration supported |

The authenticated user gates everything. Data scope = `Account.accessible_by(user)`:
accounts the user **owns or has been granted via `account_shares`** — not
automatically the whole family.

### Tools (default surface, from live `tools/list`)

`get_transactions`, `get_accounts`, `get_holdings`, `get_balance_sheet`,
`get_income_statement`, `get_budget`, `import_bank_statement`,
`search_family_files`, `create_goal`, `get_tags`, `create_tag`, `update_tag`,
`get_categories`, `create_category`, `update_category`, `update_transaction`,
`update_budget`.

With preview features enabled for the user, five more appear:
`upload_account_statement`, `list_account_statements`, `get_account_statement`,
`get_statement_coverage`, `record_valuation`. Calling a hidden tool by name
returns "Unknown tool".

**Note the write tools.** `import_bank_statement`, `update_transaction`,
`create_*`/`update_*`, `record_valuation` are all callable with the same token
that reads. There is no read-only MCP credential.

### Read-tool details (as probed)

**`get_accounts`** — no arguments.
Returns `{as_of_date, accounts: [...]}`; per account:
`name, balance, currency, balance_formatted, classification, type, start_date,
is_linked, provider, status, historical_balances`.
- **No account ID of any kind.** Names are the only key.
- `historical_balances`: monthly interval, up to 5 years, but values are
  locale-formatted strings (`"$1,234.56"`), not numbers.
- Historical balances therefore exist, but unusable without fragile parsing.

**`get_transactions`** — args: `order` (asc|desc, required), `page` (required),
`search`, `amount`+`amount_operator`, `start_date`, `end_date`, and
name-enum filters `accounts`, `categories`, `merchants`, `tags`.
- The enums in the tool schema are built from the *user's own data* — i.e.
  `tools/list` output differs per configured user.
- Page size fixed at 50. Response: `transactions[], total_results, page,
  page_size, total_pages, total_income, total_expenses`.
- Per transaction: `id` (UUID), `name`, `date`, `amount` (**absolute value,
  float**), `currency`, `formatted_amount`, `classification`
  ("income"/"expense" — this carries the sign), `account` (name), `notes`,
  `category`, `merchant`, `tags`, `is_transfer`.
- Filtering by a nonexistent/foreign account name silently returns
  `total_results: 0` — no error.

**`get_holdings`** — args: `page` (required), `accounts`, `securities`
(name/ticker enums). Investment + Crypto accounts only, latest row per
(account, security), zero-qty filtered out. Per holding: `ticker, name,
quantity, price, currency, amount, formatted_amount, weight, average_cost,
formatted_average_cost, account` (name), `date`. Floats throughout; no IDs.

**`get_balance_sheet`** — no args; current net worth breakdown, formatted
strings. No history.

Tool-call failures return `isError: true` with an
`{"error": …}` payload inside `content[0].text` — HTTP status stays 200.

### Gaps vs `SureSource` (§5)

- No sync/institution-health data anywhere → `UpstreamHealth` cannot be built.
- No account IDs → `account_snapshot.external_id` would be a display name.
- Money as floats or formatted strings → conflicts with integer-minor-units
  invariant.
- `transactions(since:)` requires date-range paging by 50 with no cursor.

---

## 2. `/api/v1` (REST)

Same image, same port. JSON. Pagy pagination: `page`, `per_page` params;
`pagination: {page, per_page, total_count, total_pages}` in responses.

### Auth

- `X-Api-Key: <key>` header. API keys are **per-user**, carry exactly one
  scope — `read` or `read_write` — and are rate-limited (standard tier:
  100 requests/hour, hourly Redis window).
- Same `accessible_by(user)` data scoping as MCP; verified empirically
  (Family B's key sees only Family B's account).
- Write with a `read` key → HTTP 403 (verified). This is the only
  server-enforced read-only credential Sure offers.
- OAuth bearer tokens also accepted (scope `read` suffices for reads).

### Endpoints relevant to `SureSource`

| Endpoint | Feeds | Shape highlights (from captures) |
|---|---|---|
| `GET /accounts` | `accounts()` | `id` (UUID), `name`, `balance` + `balance_cents`, `cash_balance(_cents)`, `currency`, `classification`, `account_type`, `subtype`, `status`, `institution_name`, `institution_domain`, timestamps |
| `GET /transactions` | `transactions(since:)` | `id`, `external_id` (upstream/aggregator id), `date`, `amount_cents`, `signed_amount_cents`, `classification`, `account{}`, `category`, `merchant`, `tags`, `transfer`, `source`, `notes`, timestamps; filterable, paginated with `total_count` |
| `GET /balances?account_id=` | history backfill / `balance-band` | daily rows: `id`, `date`, `balance_cents`, `cash_balance_cents`, `start_*_cents`, flows, `currency` |
| `GET /holdings` | `holdings()` | **A dated series, not current positions**: one row per (account, security, date), `.chronological` (oldest first) — the demo capture has `total_count: 3342` across ~20 accounts. Unlike MCP's `get_holdings`, NO latest-per-position filtering happens server-side; the client must window (`start_date`) and reduce to the newest row per (account, security). Row fields: `id`, `date`, `qty`, `price`, `amount`, `avg_cost`, `cost_basis_source`, `currency`, `account{}`, `security{}` |
| `GET /syncs`, `GET /syncs/latest` | `health()` / `sync-stale` | per-syncable (`Account`, …) `status`, `in_progress`, `terminal`, `pending_at`, `syncing_at`, `completed_at`, `failed_at`, `error`, `window_*`, `children_count` |
| `GET /provider_connections` | `health()` per-institution | empty array in demo (no aggregator linked) — **shape must be validated against a real SimpleFIN link before Phase 1 relies on it** |

Money is duplicated as display string + `*_cents` integer. Use only the
`*_cents` fields.

### Caveats

- 100 req/h standard rate limit: daily sync is fine; the initial
  balance-history backfill must pace itself.
- `provider_connections` unvalidated against a live aggregator (demo has none).
- `GET /accounts` hides disabled accounts unless a flag is passed
  (`accounts_scope` applies `.visible` by default).
- The REST API is what Sure's mobile app uses, but treat version pinning
  seriously: pin `:stable` and re-run the Phase 0 probe script after image
  bumps (it doubles as a contract test).

---

## 3. Experiment log (scoping)

Single instance, two families seeded (`docs/phase0/rig/seed_two_families.rb`,
demo seed 42):

| # | Configuration | Probe | Result |
|---|---|---|---|
| 1 | `MCP_USER_EMAIL=alpha.owner` | `get_accounts` | 20 Family A accounts |
| 2 | 〃 | search `BETA-CANARY` | 0 results |
| 3 | 〃 | filter `accounts:["BETA-CANARY-CHECKING"]` | 0 results, no error |
| 4 | 〃 | canary grep over all captures | no hits |
| 5 | 〃 | bad bearer token | HTTP 401 |
| 6 | `MCP_USER_EMAIL=beta.owner` (same token) | `get_accounts` | only `BETA-CANARY-CHECKING` |
| 7 | 〃 | search `Chase` / filter Alpha account | 0 results |
| 8 | REST, Alpha `read` key | `POST /transactions` | HTTP 403 |
| 9 | REST, Beta `read` key | `GET /accounts` | only the canary account |

Conclusion recorded in ADR-001: `/mcp` (and `/api/v1` alike) serve exactly one
user's accessible accounts per credential; multi-tenancy requires one instance
per tenant.
