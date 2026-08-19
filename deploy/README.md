# deploy/ — all-compose two-tenant deployment

One compose project holds both tenant Sure stacks (web + worker + Postgres +
Redis each) and the veille container, which joins both tenant networks and
publishes no ports. Scheduling stays on the host: a systemd timer invokes
`compose run` (see `veille.service` / `veille.timer`), because systemd's
same-unit non-overlap is the concurrency guarantee delivery idempotency
relies on. Full context: `docs/deployment.md`.

## Host layout

```
/opt/veille/repo          this repository (compose builds veille from it)
/etc/veille/env           secrets, root:root 0600 — from env.example
/etc/veille/veille.toml   veille config — from config/example.toml
```

## Bring-up

1. Copy `env.example` to `/etc/veille/env`, fill it, `chmod 0600`.
2. Copy `config/example.toml` to `/etc/veille/veille.toml`; one `[[tenants]]`
   block per tenant with `base_url = "http://alpha-web:3000"` (service name,
   internal network) and the matching `api_key_env` names.
3. Rename the `alpha`/`beta` tenants in `compose.yml` to your slugs
   (services, anchors, volumes, networks, env var names — keep them in
   lockstep with `veille.toml` and `/etc/veille/env`).
4. `docker compose --env-file /etc/veille/env up -d` — then run the
   onboarding checklist in `docs/deployment.md` per tenant (create the
   veille user, grant account shares, mint a `read` API key, verify counts).
5. Manual cycle to verify end to end:
   `docker compose --env-file /etc/veille/env run --rm veille run --once`
6. Install `veille.service` + `veille.timer` into `/etc/systemd/system/`,
   then `systemctl daemon-reload && systemctl enable --now veille.timer`.

## Backup

The store is the single `veille-store` volume plus each tenant's Postgres.
veille is a oneshot — between timer runs nothing holds the SQLite file open
(WAL checkpointed on clean close), so a plain volume copy is consistent.
Nightly, offset well away from the 21:30 cycle:

```
docker run --rm -v deploy_veille-store:/src:ro -v /var/backups/veille:/dst \
  docker.io/library/alpine cp -a /src/. /dst/
```

(`deploy_` is the compose project prefix; adjust if `COMPOSE_PROJECT_NAME`
is set. The veille image itself ships no shell tools by design.)
Postgres: `docker compose exec alpha-db pg_dump …` per tenant. History must
outlive any Sure instance (PRP §2.7).
