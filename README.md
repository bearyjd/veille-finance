# veille

A read-only watch service that sits beside one or more self-hosted
[Sure](https://github.com/we-promise/sure) instances. On a schedule it pulls
account, transaction, and holdings data; materializes it into its own
append-only store; evaluates deterministic anomaly rules; and delivers a
per-tenant digest plus immediate alerts on high-severity findings.

Built for delegated financial oversight **with consent**: a second set of eyes
on accounts you don't own, where the account owner sees exactly what the
watcher sees, in the same cycle, every cycle. That symmetry is an invariant,
not a setting.

## Design constraints

- **Read-only, structurally.** The Sure adapter exposes reads only, and the
  upstream credential is a `read`-scoped API key the server refuses writes for.
- **Rules decide, the model narrates.** Findings come from deterministic rule
  code. An (optional) LLM writes prose about findings; it can never create,
  suppress, or re-rank one.
- **No public ingress.** A CLI invoked by a timer. It listens on nothing.
- **History survives the upstream.** Snapshots are append-only in a single
  SQLite file that outlives any Sure instance.

See `docs/adr/` for architecture decisions and `docs/phase0/` for the
empirical study of Sure's API surface that this design rests on.

## Status

Pre-1.0, under active development. Phase 1 (adapter + store + sync) in
progress.

## License

AGPL-3.0-or-later. Copyright (C) 2026 Ventoux Advisory LLC.
