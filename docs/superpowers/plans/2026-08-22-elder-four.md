# v1.2 "Elder Four" Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the four market-derived elder-watch detections — missed-deposit escalation, duplicate-charge, balance-floor, recurring-price-change — as pure rules over existing store data.

**Architecture:** Every feature is a `Rule` (pure function of `EvalContext`) in `src/rules/`, registered in `all_rules()`; thresholds live in `RuleThresholds` (config, not code). No schema changes, no new store queries, no new delivery surfaces: severity alone decides push (Alert) vs digest-only. One shared monthly-series helper is extracted so `recurring-missing` and `recurring-price-change` qualify series identically.

**Tech Stack:** Rust (edition 2024), chrono, serde_json; tests via `#[cfg(test)]` + `crate::rules::test_support`.

**Spec:** §Spec below (derived from the 2026-08-22 market survey; order confirmed by JD).

## Global Constraints

- PRP §2.4: findings are created only by rule code; nothing suppresses, escalates, or de-escalates one outside the rule.
- PRP §7: thresholds live in config (`RuleThresholds`), never hardcoded per deployment.
- `RuleThresholds` keeps `#[serde(deny_unknown_fields, default)]`; every new field gets a doc comment and a `Default` value.
- Rules never touch store or network; integer minor-unit money math only (`amount_minor` signed, positive = inflow).
- Dedupe keys are episode identities: re-detection of the same condition must produce the same key.
- Per-transaction rules judge only `ctx.recent_transactions()` (35-day window).
- Gates before every commit: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`.
- Repo is PR-gated: work on branch `v1.2-elder-four`; direct pushes to main are rejected.
- Digest/evaluate golden outputs change only when a new rule legitimately fires on fixtures — verify each diff by reading it, never blind-update.

## Spec

1. **missed deposit escalates** — `recurring-missing` already detects missed inflow series (proven by its `missed_deposits_fire_too` test) but emits `Warn`, which reaches Dad only in the Sunday digest. A missed pension/Social Security/payroll deposit is the most time-critical elder signal in the market survey (EverSafe, Carefull): it must be `Alert` so it pushes via ntfy immediately. Missed *bills* stay `Warn`. (This is the report's "missing-deposit" item, implemented as direction-aware severity rather than a duplicate rule — DRY.)
2. **duplicate-charge** (Carefull, Rocket Money) — same account + counterparty + exact outflow amount, at least twice within a small day window, above a noise floor. `Warn`.
3. **balance-floor** (Copilot, Rocket) — absolute per-tenant floor for cash accounts; latest balance below it is an `Alert` (imminent-overdraft class). Opt-in via config; liability accounts (normally-negative history) exempt.
4. **recurring-price-change** (Rocket Money, Copilot) — an established monthly series charges noticeably more than its prior usual amount. `Info` (digest-only; tune severity after real noise levels are known). Increases only.

## File Structure

- `src/rules/mod.rs` — register new rules in `all_rules()`; declare new modules.
- `src/rules/recurring_missing.rs` — Task 1 severity change; Task 4 refactor onto shared helper.
- `src/rules/duplicate_charge.rs` — new (Task 2).
- `src/rules/balance_floor.rs` — new (Task 3).
- `src/rules/series.rs` — new shared monthly-series qualifier (Task 4).
- `src/rules/recurring_price_change.rs` — new (Task 5).
- `src/config.rs` — new `RuleThresholds` fields (Tasks 2, 3, 5).
- `config/example.toml` — document new thresholds (Task 6).

---

### Task 0: Branch

- [ ] **Step 1: Create the working branch**

```bash
git switch -c v1.2-elder-four
```

---

### Task 1: Missed deposits escalate to Alert

**Files:**
- Modify: `src/rules/recurring_missing.rs` (finding construction ~line 146; tests)

**Interfaces:**
- Consumes: existing `representatives: Vec<&&Transaction>` local in `RecurringMissing::evaluate`.
- Produces: findings with `evidence["direction"]` ∈ `"inflow" | "outflow"`; inflow series ⇒ `Severity::Alert`. No signature changes.

- [ ] **Step 1: Extend the existing inflow test to assert Alert + direction (failing)**

In `src/rules/recurring_missing.rs` tests, replace `missed_deposits_fire_too` with:

```rust
    #[test]
    fn missed_deposits_fire_as_alert() {
        let txns = vec![
            txn("p1", "a1", "2026-05-01", 250_000, "Acme Payroll"),
            txn("p2", "a1", "2026-06-01", 250_000, "Acme Payroll"),
            txn("p3", "a1", "2026-07-01", 250_000, "Acme Payroll"),
        ];
        let findings = RecurringMissing.evaluate(&ctx_with_transactions(txns));
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].severity,
            Severity::Alert,
            "a missed deposit must push immediately, not wait for Sunday"
        );
        assert_eq!(findings[0].evidence["direction"], "inflow");
        assert_eq!(
            findings[0].dedupe_key,
            "recurring-missing:acme payroll:2026-08"
        );
    }
```

And add a mixed-direction guard test:

```rust
    #[test]
    fn mixed_direction_series_stays_warn() {
        // Regular |amounts|, alternating sign: not clearly income.
        let txns = vec![
            txn("m1", "a1", "2026-05-05", 9_000, "Odd Ledger"),
            txn("m2", "a1", "2026-06-04", -9_000, "Odd Ledger"),
            txn("m3", "a1", "2026-07-06", 9_000, "Odd Ledger"),
        ];
        let findings = RecurringMissing.evaluate(&ctx_with_transactions(txns));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warn);
        assert_eq!(findings[0].evidence["direction"], "outflow");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib recurring_missing -- --nocapture`
Expected: FAIL — `missed_deposits_fire_as_alert` asserts `Alert`, current code emits `Warn`; `direction` key absent.

- [ ] **Step 3: Implement direction-aware severity**

In `RecurringMissing::evaluate`, after the regularity check (`if min_amount == 0 || …`) and before `findings.push`, insert:

```rust
            // Direction decides severity: a missed bill reads fine in the
            // Sunday digest, but a missed deposit (pension, payroll, Social
            // Security) is the most time-critical signal this tool watches —
            // Alert, so the push channel carries it the same day.
            let is_inflow = representatives.iter().all(|t| t.amount_minor > 0);
```

In the `Finding` literal, change `severity: Severity::Warn,` to:

```rust
                severity: if is_inflow {
                    Severity::Alert
                } else {
                    Severity::Warn
                },
```

And add to the `evidence` json (after `"currency"`):

```rust
                    "direction": if is_inflow { "inflow" } else { "outflow" },
```

The `summary` string stays byte-identical — digest goldens must not move in this task.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test --lib recurring_missing`
Expected: PASS, all recurring_missing tests green (existing bill tests still assert `Warn`).

- [ ] **Step 5: Full gates and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add src/rules/recurring_missing.rs
git commit -m "feat: missed recurring deposits escalate to Alert severity"
```

---

### Task 2: duplicate-charge rule

**Files:**
- Modify: `src/config.rs` (`RuleThresholds` struct + `Default`)
- Create: `src/rules/duplicate_charge.rs`
- Modify: `src/rules/mod.rs` (module decl + `all_rules()`)

**Interfaces:**
- Consumes: `ctx.recent_transactions()`, `t.is_transfer`, `t.counterparty_key`, `t.account_external_id`; `ctx.thresholds.duplicate_window_days: u32`, `ctx.thresholds.duplicate_floor_minor: i64` (new).
- Produces: rule id `"duplicate-charge"`, dedupe `duplicate-charge:{account}:{counterparty}:{amount_minor}:{first_date}`, `Severity::Warn`.

- [ ] **Step 1: Add threshold fields**

In `src/config.rs`, append to `RuleThresholds` (after `recurring_day_window`):

```rust
    /// `duplicate-charge`: two identical charges within this many days are a
    /// suspected duplicate.
    pub duplicate_window_days: u32,
    /// `duplicate-charge`: identical charges below this amount (minor units)
    /// are routine, not a defect — two identical coffees must stay quiet.
    pub duplicate_floor_minor: i64,
```

And to `impl Default for RuleThresholds` (after `recurring_day_window: 5,`):

```rust
            duplicate_window_days: 3,
            duplicate_floor_minor: 2_500,
```

- [ ] **Step 2: Write the failing tests**

Create `src/rules/duplicate_charge.rs`:

```rust
//! `duplicate-charge`: the same outflow amount hit the same account from the
//! same counterparty twice within a small window — a double-billed charge or
//! double-submitted payment (market survey: Carefull, Rocket Money). Exact
//! amount match on purpose: near-misses are ordinary repeat spending.
//! Transfers and inflows are not charges; amounts under the floor are noise.

use std::collections::BTreeMap;

use serde_json::json;

use super::{EvalContext, Rule};
use crate::domain::{Finding, Severity, Transaction};

pub struct DuplicateCharge;

impl Rule for DuplicateCharge {
    fn id(&self) -> &'static str {
        "duplicate-charge"
    }

    fn evaluate(&self, ctx: &EvalContext) -> Vec<Finding> {
        let window = i64::from(ctx.thresholds.duplicate_window_days);
        let floor = ctx.thresholds.duplicate_floor_minor;

        let mut clusters: BTreeMap<(&str, &str, i64), Vec<&Transaction>> = BTreeMap::new();
        for t in ctx.recent_transactions() {
            if t.is_transfer || t.amount_minor >= 0 || t.amount_minor.saturating_neg() < floor {
                continue;
            }
            let Some(key) = t.counterparty_key.as_deref() else {
                continue;
            };
            clusters
                .entry((t.account_external_id.as_str(), key, t.amount_minor))
                .or_default()
                .push(t);
        }

        let mut findings = Vec::new();
        for ((account, counterparty, amount_minor), txns) in clusters {
            // Context transactions are ascending (posted_at, external_id), so
            // adjacent pairs are the closest-in-time candidates.
            let Some(pair) = txns
                .windows(2)
                .find(|p| (p[1].posted_at - p[0].posted_at).num_days() <= window)
            else {
                continue;
            };
            let (first, second) = (pair[0], pair[1]);
            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: Severity::Warn,
                subject: format!("account:{}", ctx.account_name(account)),
                summary: format!(
                    "\u{2018}{}\u{2019} charged {} twice within {} day(s): {} and {}.",
                    counterparty,
                    crate::domain::format_minor(amount_minor, &first.currency),
                    window,
                    first.posted_at,
                    second.posted_at,
                ),
                evidence: json!({
                    "account": account,
                    "counterparty": counterparty,
                    "amount_minor": amount_minor,
                    "currency": first.currency,
                    "occurrences": txns.iter().map(|t| t.posted_at.to_string()).collect::<Vec<_>>(),
                    "window_days": window,
                }),
                dedupe_key: format!(
                    "duplicate-charge:{account}:{counterparty}:{amount_minor}:{}",
                    first.posted_at
                ),
                detected_at: ctx.now,
            });
        }
        findings
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::rules::test_support::{ctx_with_transactions, txn};

    // `now` in tests is 2026-08-18; the recent window reaches back to Jul 14.

    #[test]
    fn fires_on_identical_charges_close_together() {
        let txns = vec![
            txn("d1", "a1", "2026-08-10", -12_999, "Streaming Co"),
            txn("d2", "a1", "2026-08-11", -12_999, "Streaming Co"),
        ];
        let findings = DuplicateCharge.evaluate(&ctx_with_transactions(txns));
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(
            f.dedupe_key,
            "duplicate-charge:a1:streaming co:-12999:2026-08-10"
        );
        assert_eq!(f.evidence["occurrences"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn does_not_fire_outside_the_window() {
        let txns = vec![
            txn("d1", "a1", "2026-08-01", -12_999, "Streaming Co"),
            txn("d2", "a1", "2026-08-10", -12_999, "Streaming Co"),
        ];
        assert!(
            DuplicateCharge
                .evaluate(&ctx_with_transactions(txns))
                .is_empty()
        );
    }

    #[test]
    fn different_amounts_are_not_duplicates() {
        let txns = vec![
            txn("d1", "a1", "2026-08-10", -12_999, "Streaming Co"),
            txn("d2", "a1", "2026-08-11", -13_999, "Streaming Co"),
        ];
        assert!(
            DuplicateCharge
                .evaluate(&ctx_with_transactions(txns))
                .is_empty()
        );
    }

    #[test]
    fn small_amounts_stay_quiet() {
        // Two identical coffees under the $25.00 default floor.
        let txns = vec![
            txn("d1", "a1", "2026-08-10", -650, "Corner Cafe"),
            txn("d2", "a1", "2026-08-10", -650, "Corner Cafe"),
        ];
        assert!(
            DuplicateCharge
                .evaluate(&ctx_with_transactions(txns))
                .is_empty()
        );
    }

    #[test]
    fn inflows_and_transfers_are_not_charges() {
        let mut transfer_a = txn("t1", "a1", "2026-08-10", -50_000, "To Savings");
        transfer_a.is_transfer = true;
        let mut transfer_b = txn("t2", "a1", "2026-08-11", -50_000, "To Savings");
        transfer_b.is_transfer = true;
        let txns = vec![
            txn("i1", "a1", "2026-08-10", 250_000, "Acme Payroll"),
            txn("i2", "a1", "2026-08-11", 250_000, "Acme Payroll"),
            transfer_a,
            transfer_b,
        ];
        assert!(
            DuplicateCharge
                .evaluate(&ctx_with_transactions(txns))
                .is_empty()
        );
    }

    #[test]
    fn separate_accounts_do_not_pair() {
        let txns = vec![
            txn("d1", "a1", "2026-08-10", -12_999, "Streaming Co"),
            txn("d2", "a2", "2026-08-10", -12_999, "Streaming Co"),
        ];
        assert!(
            DuplicateCharge
                .evaluate(&ctx_with_transactions(txns))
                .is_empty()
        );
    }

    #[test]
    fn three_occurrences_emit_one_finding_keyed_on_the_first() {
        let txns = vec![
            txn("d1", "a1", "2026-08-09", -12_999, "Streaming Co"),
            txn("d2", "a1", "2026-08-10", -12_999, "Streaming Co"),
            txn("d3", "a1", "2026-08-11", -12_999, "Streaming Co"),
        ];
        let findings = DuplicateCharge.evaluate(&ctx_with_transactions(txns));
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].dedupe_key,
            "duplicate-charge:a1:streaming co:-12999:2026-08-09"
        );
        assert_eq!(findings[0].evidence["occurrences"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn window_comes_from_config() {
        let mut ctx = ctx_with_transactions(vec![
            txn("d1", "a1", "2026-08-01", -12_999, "Streaming Co"),
            txn("d2", "a1", "2026-08-10", -12_999, "Streaming Co"),
        ]);
        ctx.thresholds.duplicate_window_days = 10;
        assert_eq!(DuplicateCharge.evaluate(&ctx).len(), 1);
    }
}
```

- [ ] **Step 3: Register and run to verify failure first**

In `src/rules/mod.rs` add `pub mod duplicate_charge;` to the module list and `Box::new(duplicate_charge::DuplicateCharge),` to `all_rules()` (after `RecurringMissing`). Before writing the implementation body you may verify the red state by stubbing `evaluate` to `Vec::new()`; with the full file above, instead verify by running:

Run: `cargo test --lib duplicate_charge`
Expected: PASS only if implementation matches tests — treat any failure as a spec mismatch to fix in the implementation, not the tests.

- [ ] **Step 4: Full gates**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: all suites green. If `tests/evaluate.rs` goldens now include duplicate-charge findings from fixtures, read the diff: fixture data with real same-amount pairs is a legitimate fire — update the golden only after confirming the pair exists in the fixture JSON.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs src/rules/mod.rs src/rules/duplicate_charge.rs
git commit -m "feat: duplicate-charge rule — identical outflows within a day window"
```

---

### Task 3: balance-floor rule

**Files:**
- Modify: `src/config.rs` (`RuleThresholds` + `Default`)
- Create: `src/rules/balance_floor.rs`
- Modify: `src/rules/mod.rs`

**Interfaces:**
- Consumes: `ctx.balances` (`BTreeMap<String, Vec<BalancePoint>>`), `ctx.account_name/currency`; new `ctx.thresholds.balance_floor_minor: Option<i64>`.
- Produces: rule id `"balance-floor"`, dedupe `balance-floor:{account_external_id}` (open-until-cleared episode), `Severity::Alert`.

- [ ] **Step 1: Add the threshold field**

`src/config.rs`, `RuleThresholds` (after the duplicate fields):

```rust
    /// `balance-floor`: alert when a cash account's latest balance is below
    /// this absolute amount (minor units). `None` disables the rule — set it
    /// per tenant to the account owner's real comfort line.
    pub balance_floor_minor: Option<i64>,
```

`Default`:

```rust
            balance_floor_minor: None,
```

- [ ] **Step 2: Write the rule + tests**

Create `src/rules/balance_floor.rs`:

```rust
//! `balance-floor`: a cash account's latest balance is below an absolute,
//! per-tenant floor (market survey: Copilot, Rocket Money). Complements
//! `balance-band` (statistical): the band asks "is this unusual for this
//! account?", the floor asks "is this simply too low, full stop?" — the
//! imminent-overdraft class of finding, so it is an Alert.
//!
//! Liability accounts are exempt: an account whose typical observed balance
//! is negative (credit cards, lines of credit) lives below any cash floor by
//! nature. The exemption keys on the median of the history, not "ever
//! negative" — a checking account that once overdrafted is exactly the
//! account this rule must keep watching.

use serde_json::json;

use super::{EvalContext, Rule};
use crate::domain::{format_minor, Finding, Severity};

pub struct BalanceFloor;

impl Rule for BalanceFloor {
    fn id(&self) -> &'static str {
        "balance-floor"
    }

    fn evaluate(&self, ctx: &EvalContext) -> Vec<Finding> {
        let Some(floor) = ctx.thresholds.balance_floor_minor else {
            return Vec::new();
        };
        let mut findings = Vec::new();
        for (account_id, series) in &ctx.balances {
            let Some(latest) = series.last() else {
                continue;
            };
            if median_minor(series.iter().map(|p| p.balance_minor)) < 0 {
                continue;
            }
            if latest.balance_minor >= floor {
                continue;
            }
            let currency = ctx.account_currency(account_id);
            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: Severity::Alert,
                subject: format!("account:{}", ctx.account_name(account_id)),
                summary: format!(
                    "{} balance is {}, below the {} floor.",
                    ctx.account_name(account_id),
                    format_minor(latest.balance_minor, currency),
                    format_minor(floor, currency),
                ),
                evidence: json!({
                    "account": account_id,
                    "balance_minor": latest.balance_minor,
                    "floor_minor": floor,
                    "currency": currency,
                    "as_of": latest.date.to_string(),
                }),
                dedupe_key: format!("balance-floor:{account_id}"),
                detected_at: ctx.now,
            });
        }
        findings
    }
}

/// Median balance of the observed history; 0 for an empty series.
fn median_minor(values: impl Iterator<Item = i64>) -> i64 {
    let mut sorted: Vec<i64> = values.collect();
    if sorted.is_empty() {
        return 0;
    }
    sorted.sort_unstable();
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        sorted[n / 2 - 1].midpoint(sorted[n / 2])
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::rules::test_support::empty_ctx;
    use crate::rules::BalancePoint;

    fn ctx_with_series(account: &str, points: &[(&str, i64)]) -> super::super::EvalContext {
        let mut ctx = empty_ctx();
        ctx.thresholds.balance_floor_minor = Some(50_000);
        ctx.balances.insert(
            account.to_string(),
            points
                .iter()
                .map(|(d, b)| BalancePoint {
                    date: d.parse().expect("date"),
                    balance_minor: *b,
                })
                .collect(),
        );
        ctx.account_names
            .insert(account.to_string(), "Everyday Checking".to_string());
        ctx.account_currencies
            .insert(account.to_string(), "USD".to_string());
        ctx
    }

    #[test]
    fn fires_below_the_floor() {
        let ctx = ctx_with_series("a1", &[("2026-08-16", 90_000), ("2026-08-17", 30_000)]);
        let findings = BalanceFloor.evaluate(&ctx);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Alert);
        assert_eq!(f.dedupe_key, "balance-floor:a1");
        assert_eq!(f.evidence["floor_minor"], 50_000);
    }

    #[test]
    fn silent_at_or_above_the_floor() {
        let ctx = ctx_with_series("a1", &[("2026-08-17", 50_000)]);
        assert!(BalanceFloor.evaluate(&ctx).is_empty());
    }

    #[test]
    fn none_disables_the_rule() {
        let mut ctx = ctx_with_series("a1", &[("2026-08-17", 1_000)]);
        ctx.thresholds.balance_floor_minor = None;
        assert!(BalanceFloor.evaluate(&ctx).is_empty());
    }

    #[test]
    fn liability_accounts_are_exempt() {
        // Median history negative: a credit card, not a cash account.
        let ctx = ctx_with_series(
            "card",
            &[
                ("2026-08-14", -120_000),
                ("2026-08-15", -90_000),
                ("2026-08-16", -100_000),
            ],
        );
        assert!(BalanceFloor.evaluate(&ctx).is_empty());
    }

    #[test]
    fn an_overdrafted_checking_account_still_fires() {
        // Median positive, latest negative: this is the emergency, not an
        // exemption.
        let ctx = ctx_with_series(
            "a1",
            &[
                ("2026-08-14", 200_000),
                ("2026-08-15", 150_000),
                ("2026-08-16", -4_000),
            ],
        );
        let findings = BalanceFloor.evaluate(&ctx);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].evidence["balance_minor"], -4_000);
    }

    #[test]
    fn dedupe_key_is_stable_across_days() {
        let day_one = ctx_with_series("a1", &[("2026-08-16", 30_000)]);
        let day_two = ctx_with_series("a1", &[("2026-08-16", 30_000), ("2026-08-17", 20_000)]);
        assert_eq!(
            BalanceFloor.evaluate(&day_one)[0].dedupe_key,
            BalanceFloor.evaluate(&day_two)[0].dedupe_key,
            "a continuing breach is one episode, not one finding per day"
        );
    }
}
```

Note: if `format_minor` is not exported as `crate::domain::format_minor` in scope, import it exactly as `balance_band.rs`/`recurring_missing.rs` reference it (`crate::domain::format_minor(...)` fully qualified is used there — match that style if the bare import fails clippy or compilation). If `i64::midpoint` is unavailable on the pinned toolchain, use `(sorted[n / 2 - 1] + sorted[n / 2]) / 2` — history balances are far from `i64` extremes.

- [ ] **Step 3: Register and run**

`src/rules/mod.rs`: add `pub mod balance_floor;` and `Box::new(balance_floor::BalanceFloor),` in `all_rules()`.

Run: `cargo test --lib balance_floor`
Expected: PASS (fix implementation, not tests, on mismatch).

- [ ] **Step 4: Full gates**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: green; goldens unchanged (rule defaults to `None` so fixtures cannot fire it).

- [ ] **Step 5: Commit**

```bash
git add src/config.rs src/rules/mod.rs src/rules/balance_floor.rs
git commit -m "feat: balance-floor rule — absolute cash-account floor, Alert severity"
```

---

### Task 4: Extract the shared monthly-series qualifier

**Files:**
- Create: `src/rules/series.rs`
- Modify: `src/rules/recurring_missing.rs` (consume the helper; all existing tests stay untouched and green)
- Modify: `src/rules/mod.rs` (`pub(crate) mod series;`)

**Interfaces:**
- Produces (exact, used by Tasks 4–5):

```rust
/// A counterparty that has behaved like a monthly series: activity in each of
/// the last `min_occurrences` consecutive calendar months ending at its most
/// recent occurrence, one representative per month (closest to the streak
/// median amount, later date breaking ties). Regularity is NOT judged here —
/// each rule applies its own regularity test to the representative subset it
/// cares about.
pub(crate) struct MonthlySeries<'a> {
    pub counterparty: &'a str,
    /// Ascending by month; one per streak month.
    pub representatives: Vec<&'a Transaction>,
    /// (year, month) of the most recent occurrence.
    pub last_month: (i32, u32),
    /// Most recent occurrence overall (for typical-amount display).
    pub last: &'a Transaction,
}

pub(crate) fn monthly_series<'a>(ctx: &'a EvalContext) -> Vec<MonthlySeries<'a>>;
/// Max ≤ 1.5 × min over the given amounts (absolute minor units); false when
/// empty or any amount is zero.
pub(crate) fn amounts_regular(amounts: &[u128]) -> bool;
pub(crate) fn median_u128(values: Vec<u128>) -> u128;
pub(crate) fn next_month(month: (i32, u32)) -> (i32, u32);
pub(crate) fn prev_month(month: (i32, u32)) -> (i32, u32);
pub(crate) fn format_month(month: (i32, u32)) -> String;
pub(crate) fn clamp_to_month(month: (i32, u32), day: u32) -> chrono::NaiveDate;
pub(crate) fn median_day(sorted_days: &[u32]) -> u32;
```

- [ ] **Step 1: Move, don't rewrite**

Create `src/rules/series.rs` by moving from `recurring_missing.rs`: the counterparty grouping, streak check, representative selection (verbatim logic), and the helper fns `median_u128`, `median_day`, `next_month`, `prev_month`, `format_month`, `clamp_to_month`. `monthly_series` performs, per counterparty with a `counterparty_key`: last-month computation, `min_occurrences` streak check (from `ctx.thresholds.recurring_min_occurrences`; returns no series when 0), representative-per-month selection against the streak-wide median — exactly the moved code — and returns qualifying `MonthlySeries` values sorted by counterparty. `amounts_regular` is the extracted `min_amount == 0 || max_amount * 2 > min_amount * 3` check, inverted to a positive predicate.

- [ ] **Step 2: Re-point recurring-missing**

`RecurringMissing::evaluate` becomes: call `series::monthly_series(ctx)`; for each series apply, in order — the expected-month recency gate (`next_month(last_month)` must be current or previous month), `amounts_regular` over ALL representatives, then the existing `median_day`/`due_by` judgment and the byte-identical `Finding` construction (including Task 1's direction logic, now computed from `series.representatives`).

- [ ] **Step 3: Prove the refactor is invisible**

Run: `cargo test --lib recurring_missing && cargo test`
Expected: every existing test passes UNCHANGED — zero test-file edits in this task. Any behavioral diff (including golden churn) is a refactor bug: fix `series.rs`, never a test.

- [ ] **Step 4: Gates and commit**

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
git add src/rules/series.rs src/rules/recurring_missing.rs src/rules/mod.rs
git commit -m "refactor: extract shared monthly-series qualifier from recurring-missing"
```

---

### Task 5: recurring-price-change rule

**Files:**
- Modify: `src/config.rs` (`RuleThresholds` + `Default`)
- Create: `src/rules/recurring_price_change.rs`
- Modify: `src/rules/mod.rs`

**Interfaces:**
- Consumes: `series::monthly_series`, `series::amounts_regular`, `series::format_month` (Task 4 signatures); new `ctx.thresholds.recurring_price_increase_pct: u32`.
- Produces: rule id `"recurring-price-change"`, dedupe `recurring-price-change:{counterparty}:{yyyy-mm}`, `Severity::Info`.

- [ ] **Step 1: Add the threshold field**

`src/config.rs`, `RuleThresholds`:

```rust
    /// `recurring-price-change`: a monthly series' newest charge exceeding
    /// its prior usual amount by more than this percentage fires.
    pub recurring_price_increase_pct: u32,
```

`Default`:

```rust
            recurring_price_increase_pct: 20,
```

- [ ] **Step 2: Write the rule + tests**

Create `src/rules/recurring_price_change.rs`:

```rust
//! `recurring-price-change`: an established monthly series charged
//! noticeably more this month than its prior usual amount (market survey:
//! Rocket Money, Copilot). Regularity is judged over the PRIOR months only,
//! so a large jump cannot disqualify its own series. Increases only — a
//! price drop is good news, and good news is not a finding. Info severity:
//! digest-visible, never pushed; escalate only if real noise levels warrant.

use serde_json::json;

use super::series::{amounts_regular, format_month, median_u128, monthly_series};
use super::{EvalContext, Rule};
use crate::domain::{Finding, Severity};

/// Increases below this many minor units never fire regardless of
/// percentage: a $0.60 bump on a $2.99 service is not worth a line.
pub const MIN_INCREASE_MINOR: u128 = 500;

pub struct RecurringPriceChange;

impl Rule for RecurringPriceChange {
    fn id(&self) -> &'static str {
        "recurring-price-change"
    }

    fn evaluate(&self, ctx: &EvalContext) -> Vec<Finding> {
        use chrono::Datelike;

        let pct = u128::from(ctx.thresholds.recurring_price_increase_pct);
        if pct == 0 {
            return Vec::new();
        }
        let today = ctx.now.date_naive();
        let current_month = (today.year(), today.month());

        let mut findings = Vec::new();
        for series in monthly_series(ctx) {
            // Only a charge in the evaluation month is "the new price".
            if series.last_month != current_month {
                continue;
            }
            let (prior, newest): (Vec<_>, Vec<_>) = series
                .representatives
                .iter()
                .partition(|t| (t.posted_at.year(), t.posted_at.month()) != current_month);
            let Some(newest) = newest.first() else {
                continue;
            };
            let prior_amounts: Vec<u128> = prior
                .iter()
                .map(|t| u128::from(t.amount_minor.unsigned_abs()))
                .collect();
            if prior_amounts.is_empty() || !amounts_regular(&prior_amounts) {
                continue;
            }
            let usual = median_u128(prior_amounts);
            let new_amount = u128::from(newest.amount_minor.unsigned_abs());
            if new_amount <= usual
                || new_amount - usual < MIN_INCREASE_MINOR
                || new_amount * 100 <= usual * (100 + pct)
            {
                continue;
            }
            let increase_pct = (new_amount - usual) * 100 / usual;
            findings.push(Finding {
                rule_id: self.id().to_string(),
                severity: Severity::Info,
                subject: format!("counterparty:{}", series.counterparty),
                summary: format!(
                    "\u{2018}{}\u{2019} charged {} this month, up about {}% from its usual {}.",
                    series.counterparty,
                    crate::domain::format_minor(newest.amount_minor, &newest.currency),
                    increase_pct,
                    crate::domain::format_minor(
                        i64::try_from(usual).unwrap_or(i64::MAX) * newest.amount_minor.signum(),
                        &newest.currency
                    ),
                ),
                evidence: json!({
                    "counterparty": series.counterparty,
                    "month": format_month(current_month),
                    "new_amount_minor": newest.amount_minor,
                    "usual_amount_minor": usual.to_string(),
                    "increase_pct": increase_pct.to_string(),
                    "currency": newest.currency,
                }),
                dedupe_key: format!(
                    "recurring-price-change:{}:{}",
                    series.counterparty,
                    format_month(current_month)
                ),
                detected_at: ctx.now,
            });
        }
        findings
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::rules::test_support::{ctx_with_transactions, txn};

    // `now` in tests is 2026-08-18.

    fn subscription(new_amount: i64) -> Vec<crate::domain::Transaction> {
        vec![
            txn("s1", "a1", "2026-05-12", -9_99, "Streaming Co"),
            txn("s2", "a1", "2026-06-12", -9_99, "Streaming Co"),
            txn("s3", "a1", "2026-07-12", -9_99, "Streaming Co"),
            txn("s4", "a1", "2026-08-12", new_amount, "Streaming Co"),
        ]
    }

    #[test]
    fn fires_on_a_clear_price_increase() {
        // $9.99 → $15.99: +60%, over the 20% default and the $5 min delta.
        let findings =
            RecurringPriceChange.evaluate(&ctx_with_transactions(subscription(-15_99)));
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Info);
        assert_eq!(
            f.dedupe_key,
            "recurring-price-change:streaming co:2026-08"
        );
    }

    #[test]
    fn a_jump_beyond_regularity_still_fires() {
        // Prior months are regular; the doubled charge itself must not
        // disqualify the series (regularity judged on prior months only).
        let findings =
            RecurringPriceChange.evaluate(&ctx_with_transactions(subscription(-29_99)));
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn small_percentage_stays_quiet() {
        // $9.99 → $10.99: +10%, under the 20% default.
        assert!(
            RecurringPriceChange
                .evaluate(&ctx_with_transactions(subscription(-10_99)))
                .is_empty()
        );
    }

    #[test]
    fn tiny_absolute_increase_stays_quiet() {
        // Prior $2.99, new $3.99: +33% but under MIN_INCREASE_MINOR.
        let txns = vec![
            txn("s1", "a1", "2026-05-12", -2_99, "Tiny App"),
            txn("s2", "a1", "2026-06-12", -2_99, "Tiny App"),
            txn("s3", "a1", "2026-07-12", -2_99, "Tiny App"),
            txn("s4", "a1", "2026-08-12", -3_99, "Tiny App"),
        ];
        assert!(
            RecurringPriceChange
                .evaluate(&ctx_with_transactions(txns))
                .is_empty()
        );
    }

    #[test]
    fn decreases_never_fire() {
        assert!(
            RecurringPriceChange
                .evaluate(&ctx_with_transactions(subscription(-4_99)))
                .is_empty()
        );
    }

    #[test]
    fn no_current_month_charge_means_no_judgment() {
        // (That situation belongs to recurring-missing, not this rule.)
        let txns = vec![
            txn("s1", "a1", "2026-05-12", -9_99, "Streaming Co"),
            txn("s2", "a1", "2026-06-12", -9_99, "Streaming Co"),
            txn("s3", "a1", "2026-07-12", -9_99, "Streaming Co"),
        ];
        assert!(
            RecurringPriceChange
                .evaluate(&ctx_with_transactions(txns))
                .is_empty()
        );
    }

    #[test]
    fn irregular_prior_months_are_not_a_series() {
        let txns = vec![
            txn("s1", "a1", "2026-05-12", -2_00, "Corner Store"),
            txn("s2", "a1", "2026-06-12", -9_00, "Corner Store"),
            txn("s3", "a1", "2026-07-12", -30_00, "Corner Store"),
            txn("s4", "a1", "2026-08-12", -50_00, "Corner Store"),
        ];
        assert!(
            RecurringPriceChange
                .evaluate(&ctx_with_transactions(txns))
                .is_empty()
        );
    }

    #[test]
    fn threshold_pct_comes_from_config() {
        let mut ctx = ctx_with_transactions(subscription(-15_99));
        ctx.thresholds.recurring_price_increase_pct = 80;
        assert!(RecurringPriceChange.evaluate(&ctx).is_empty());
    }
}
```

Note on streak counting: `monthly_series` qualifies a streak of `recurring_min_occurrences` months ending at the LAST month — for a current-month charge with 3 prior months and default `min_occurrences = 3`, the streak is (Jun, Jul, Aug) and May's representative also exists only if May is inside the streak window. If the moved logic yields exactly `min_occurrences` representatives (Jun, Jul, Aug), the prior set is (Jun, Jul) — two months of priors under the default. The tests above encode the intended OBSERVABLE behavior; if the helper's month-window math makes `prior_amounts` come out empty for the 4-month fixtures, widen the fixtures by one month rather than weakening assertions — the invariant that matters is: regular priors + current jump ⇒ fire.

- [ ] **Step 3: Register and run**

`src/rules/mod.rs`: add `pub mod recurring_price_change;` and `Box::new(recurring_price_change::RecurringPriceChange),` in `all_rules()`.

Run: `cargo test --lib recurring_price_change`
Expected: PASS.

- [ ] **Step 4: Full gates**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: green; inspect any evaluate-golden diff as in Task 2.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs src/rules/mod.rs src/rules/recurring_price_change.rs
git commit -m "feat: recurring-price-change rule — monthly series charging above its usual"
```

---

### Task 6: Config docs, gates, PR

**Files:**
- Modify: `config/example.toml` (threshold documentation)
- Modify: `docs/private/deploy/veille.toml` mirror + host copy (deployment values — operator step, documented not scripted)

- [ ] **Step 1: Document the new thresholds in `config/example.toml`**

Under the tenant section where thresholds are documented (matching the existing comment style), add:

```toml
# Per-tenant rule thresholds (all optional; defaults shown):
# [tenants.thresholds]
# duplicate_window_days = 3          # duplicate-charge: identical outflows within N days
# duplicate_floor_minor = 2500       # duplicate-charge: ignore pairs under this amount
# balance_floor_minor = 50000        # balance-floor: absent = rule disabled (opt-in)
# recurring_price_increase_pct = 20  # recurring-price-change: fire above this % increase
```

(If example.toml already has a thresholds block, extend it in place instead of adding a second one — one block, alphabetical with the existing keys' ordering style.)

- [ ] **Step 2: Full validation suite**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: everything green, 4 new rule modules, `all_rules()` returns 9 rules (existing test count grows accordingly).

- [ ] **Step 3: Push and open the PR**

```bash
git push -u origin v1.2-elder-four
gh pr create --title "feat: v1.2 elder four — deposit alerts, duplicate charges, balance floor, price changes" \
  --body "Implements docs/superpowers/plans/2026-08-22-elder-four.md: missed-deposit Alert escalation, duplicate-charge, balance-floor (opt-in), recurring-price-change (Info), shared monthly-series qualifier. Pure rules over existing store data; no schema changes; no new surfaces."
```

- [ ] **Step 4: Reviews per repo convention**

Before merge: run the `code-reviewer` pass on the branch diff and address CRITICAL/HIGH findings (project rule); rules changes are §2.4-adjacent, so a `security-reviewer`/adversarial pass is warranted given the project's review history. Merge via GitHub after `gate` is green (required check).

---

## Deferred by design (not in this plan)

- **v1.1 operational items** are config/runbook, not code: enable `large-transfer` and tune `new-counterparty` after a month of baselines (set values in the host `veille.toml`); validate holdings against real institutions. Tracked in `docs/private/deploy/NOTES.md`.
- **Alert-email fallback** (immediate Alert email via existing SMTP to all recipients) — build only if the ntfy app does not stick with Dad after launch. When triggered, plan it as its own small spec: it touches `src/deliver/` and the §2.2 audit trail, which this plan deliberately leaves untouched.
- **All v2 candidates** (spending baseline, upcoming-bills digest section, history report, ack path) — hypotheses pending real findings-noise data.
