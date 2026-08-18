# Deploying veille

One veille process watches one or more Sure instances — **one instance per
tenant** (ADR-001). veille publishes no ports, holds only `read`-scoped
credentials, and is invoked by a timer; remote access is a VPN/overlay
problem, not an exposed port.

## Per-tenant Sure instance

Run each tenant's Sure from the published image, unmodified, pinned to
`:stable`, with its own Postgres and Redis (see upstream
`docs/hosting/docker.md`). The `MCP_*` variables are not needed — veille
uses `/api/v1`.

## Onboarding checklist (per tenant)

1. Create (or pick) the Sure user veille connects as. **It must own, or have
   been granted account shares to, every account veille should watch** —
   visibility is per-user, not per-family (ADR-001).
2. In that user's session, create an API key with the **`read`** scope.
   Export it as the env var named in `config/veille.toml` (`api_key_env`).
3. Verify: `veille sync --tenant <slug>` then `veille digest --tenant <slug>`
   — the account list and count must match expectations.
4. Aggregator tokens (SimpleFIN/SnapTrade) are created by the account owners
   themselves wherever possible — the operator never handles someone else's
   bank credential.
5. Before trusting the `sync-stale` rule in production, validate
   `/api/v1/syncs` and `/api/v1/provider_connections` against a REAL
   SimpleFIN-linked institution (open Phase 0 risk: the demo instance had no
   aggregator, so those shapes are unverified against live providers).
   `docs/phase0/rig/probe_mcp.sh` doubles as a contract test after image
   bumps.
6. Tell every recipient — owners and watchers alike — what veille sends and
   to whom. §2.2 is the deal: watchers see exactly what owners see, always.

## Scheduling (systemd)

No scheduler exists in the binary. One service + timer per deployment:

```ini
# /etc/systemd/system/veille.service
[Unit]
Description=veille watch cycle
Wants=network-online.target
After=network-online.target

[Service]
Type=oneshot
EnvironmentFile=/etc/veille/env        # the *_API_KEY / SMTP / LLM vars, root:root 0600
ExecStart=/usr/local/bin/veille --config /etc/veille/veille.toml run --once
User=veille
```

```ini
# /etc/systemd/system/veille.timer
[Unit]
Description=daily veille cycle

[Timer]
# Late in the day: SimpleFIN refreshes ~daily at bank-dependent times
# (PRP appendix A) — do not poll aggressively.
OnCalendar=*-*-* 21:30
Persistent=true

[Install]
WantedBy=timers.target
```

systemd never overlaps starts of the same service unit, which is the
concurrency assumption the delivery idempotency relies on. Run exactly one
unit per store file.

For the Docker deployment, `ExecStart` becomes:

```
docker compose -f /opt/sure/compose.yml -f /opt/veille/compose.veille.yml run --rm veille run --once
```

## Backup

The whole store is one SQLite file (`store_path`), created `0600`. Snapshot
it nightly to the backup target; history must outlive any Sure instance
(PRP §2.7). Copy the `-wal`/`-shm` sidecars atomically or use
`sqlite3 store.sqlite3 ".backup ..."`.

## Operational invariants worth re-reading before changing anything

- `--dry-run` paths are inert; `digest`/`evaluate` on the real store are
  read-only.
- Push requires owner recipients; the digest is the channel that guarantees
  owners see every alert. Give the ntfy topic to every owner and watcher.
- The digest coverage window extends to the last delivered digest — do not
  "optimize" that query; it is what makes coverage gap-free.
- `migrations/0001_init.sql` is frozen. Schema changes are new migration
  files only.
