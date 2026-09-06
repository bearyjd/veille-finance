#![allow(clippy::expect_used, clippy::unwrap_used)]

//! ApiSureSource contract: authentication header, pagination, `since`
//! filtering, health derivation, and loud failure on non-success statuses.

use chrono::{DateTime, TimeZone, Utc};
use serde_json::json;
use veille::source::SureSource;
use veille::source::api::ApiSureSource;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn pagination(page: u32, total_pages: u32, total_count: u64) -> serde_json::Value {
    json!({ "page": page, "per_page": 100, "total_count": total_count, "total_pages": total_pages })
}

fn wire_account(id: &str, name: &str, institution: Option<&str>) -> serde_json::Value {
    json!({
        "id": id, "name": name,
        "balance": "$1.00", "balance_cents": 100,
        "cash_balance": "$1.00", "cash_balance_cents": 100,
        "currency": "USD", "classification": "asset",
        "account_type": "depository", "subtype": null, "status": "active",
        "institution_name": institution, "institution_domain": null,
        "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
    })
}

fn wire_transaction(id: &str, date: &str) -> serde_json::Value {
    json!({
        "id": id, "date": date,
        "amount": "-$1.00", "amount_cents": 100, "signed_amount_cents": -100,
        "currency": "USD", "name": "t", "notes": null,
        "external_id": null, "source": null, "classification": "expense",
        "account": { "id": "a-1", "name": "A", "account_type": "depository" },
        "category": null, "merchant": null, "tags": [], "transfer": null,
        "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
    })
}

async fn source_for(server: &MockServer) -> ApiSureSource {
    ApiSureSource::new(&server.uri(), "test-key".into()).expect("valid base url")
}

#[tokio::test]
async fn accounts_sends_api_key_and_paginates_to_total_pages() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/accounts"))
        .and(header("X-Api-Key", "test-key"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accounts": [wire_account("a-1", "One", None)],
            "pagination": pagination(1, 2, 2)
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/accounts"))
        .and(header("X-Api-Key", "test-key"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accounts": [wire_account("a-2", "Two", None)],
            "pagination": pagination(2, 2, 2)
        })))
        .expect(1)
        .mount(&server)
        .await;

    let accounts = source_for(&server)
        .await
        .accounts()
        .await
        .expect("accounts");
    let ids: Vec<&str> = accounts.iter().map(|a| a.external_id.as_str()).collect();
    assert_eq!(ids, ["a-1", "a-2"]);
}

#[tokio::test]
async fn transactions_pass_start_date_only_for_incremental_sync() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/transactions"))
        .and(query_param("start_date", "2026-07-10"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "transactions": [wire_transaction("t-1", "2026-07-11")],
            "pagination": pagination(1, 1, 1)
        })))
        .expect(1)
        .mount(&server)
        .await;

    let since = Utc
        .with_ymd_and_hms(2026, 7, 10, 0, 0, 0)
        .single()
        .expect("ts");
    let txns = source_for(&server)
        .await
        .transactions(since)
        .await
        .expect("txns");
    assert_eq!(txns.len(), 1);

    // Full backfill (since == MIN) must omit start_date entirely: Sure 500s on
    // an unparseable date, and "no filter" is the correct request.
    let server2 = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "transactions": [],
            "pagination": pagination(1, 1, 0)
        })))
        .expect(1)
        .mount(&server2)
        .await;
    let txns = source_for(&server2)
        .await
        .transactions(DateTime::<Utc>::MIN_UTC)
        .await
        .expect("backfill");
    assert!(txns.is_empty());
    let requests = server2.received_requests().await.expect("requests");
    assert!(
        !requests[0].url.query().unwrap_or("").contains("start_date"),
        "backfill request must not carry start_date"
    );
}

#[tokio::test]
async fn non_success_status_is_a_loud_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/accounts"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": "unauthorized", "message": "Invalid API key"
        })))
        .mount(&server)
        .await;

    let err = source_for(&server)
        .await
        .accounts()
        .await
        .expect_err("must fail");
    assert!(
        err.to_string().contains("401"),
        "error should name the status: {err}"
    );
}

#[tokio::test]
async fn health_joins_accounts_with_syncs() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accounts": [wire_account("a-1", "Linked", Some("First National"))],
            "pagination": pagination(1, 1, 1)
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/syncs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "id": "s-1", "status": "completed", "in_progress": false, "terminal": true,
                "syncable": { "type": "Account", "id": "a-1", "name": "Linked" },
                "parent_id": null, "children_count": 0,
                "window_start_date": null, "window_end_date": null,
                "pending_at": null, "syncing_at": "2026-08-15T06:00:00Z",
                "completed_at": "2026-08-15T06:00:05Z", "failed_at": null, "error": null,
                "created_at": "2026-08-15T06:00:00Z", "updated_at": "2026-08-15T06:00:05Z"
            }],
            "meta": pagination(1, 1, 1)
        })))
        .mount(&server)
        .await;

    let health = source_for(&server).await.health().await.expect("health");
    assert_eq!(health.institutions.len(), 1);
    assert_eq!(health.institutions[0].institution, "First National");
    assert_eq!(
        health.institutions[0].last_successful_sync_at,
        Utc.with_ymd_and_hms(2026, 8, 15, 6, 0, 5).single()
    );
}

#[tokio::test]
async fn redirects_are_refused_and_leak_nothing() {
    // A compromised upstream must not be able to bounce our API key to a
    // third party: redirects are a contract violation, not something to follow.
    let attacker = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&attacker)
        .await;

    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/accounts"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", format!("{}/collect", attacker.uri()).as_str()),
        )
        .mount(&upstream)
        .await;

    let err = source_for(&upstream)
        .await
        .accounts()
        .await
        .expect_err("redirect must fail");
    assert!(
        err.to_string().contains("302"),
        "error should surface the redirect status: {err}"
    );
    assert!(
        attacker.received_requests().await.expect("reqs").is_empty(),
        "no request may follow the redirect"
    );
}

#[tokio::test]
async fn runaway_total_pages_hits_a_loud_cap() {
    // A server that always claims more pages must produce an error, not an
    // unbounded crawl.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accounts": [wire_account("a-1", "One", None)],
            "pagination": { "page": 1, "per_page": 100, "total_count": 999999, "total_pages": 9999 }
        })))
        .mount(&server)
        .await;

    let err = source_for(&server)
        .await
        .accounts()
        .await
        .expect_err("must refuse runaway pagination");
    assert!(
        err.to_string().contains("page"),
        "error should mention pagination: {err}"
    );
}

#[tokio::test]
async fn duplicate_rows_across_pages_are_deduped_by_id() {
    // A server that ignores the page parameter would otherwise double every
    // account snapshot in one run.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accounts": [wire_account("a-1", "One", None)],
            "pagination": pagination(1, 2, 2)
        })))
        .mount(&server)
        .await;

    let accounts = source_for(&server)
        .await
        .accounts()
        .await
        .expect("accounts");
    assert_eq!(
        accounts.len(),
        1,
        "same id served twice must collapse to one"
    );
}

#[tokio::test]
async fn contradictory_sign_and_classification_is_a_contract_error() {
    let server = MockServer::start().await;
    let mut bad = wire_transaction("t-bad", "2026-08-01");
    bad["classification"] = json!("expense");
    bad["signed_amount_cents"] = json!(100); // positive, but claims expense
    Mock::given(method("GET"))
        .and(path("/api/v1/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "transactions": [bad],
            "pagination": pagination(1, 1, 1)
        })))
        .mount(&server)
        .await;

    let err = source_for(&server)
        .await
        .transactions(DateTime::<Utc>::MIN_UTC)
        .await
        .expect_err("inconsistent money data must not be stored");
    assert!(
        err.to_string().contains("classification"),
        "error should name the inconsistency: {err}"
    );
}

fn wire_holding(account: &str, ticker: &str, date: &str, amount: &str) -> serde_json::Value {
    json!({
        "id": format!("h-{account}-{ticker}-{date}"), "date": date,
        "qty": "3.21", "price": "$1.00", "amount": amount,
        "currency": "USD", "cost_basis_source": "calculated",
        "account": { "id": account, "name": account, "account_type": "investment" },
        "security": { "id": format!("sec-{ticker}"), "ticker": ticker, "name": ticker },
        "avg_cost": null,
        "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
    })
}

#[tokio::test]
async fn holdings_history_reduces_to_latest_position_per_account_and_security() {
    // /api/v1/holdings is a historical series (one row per account, security,
    // date — Phase 0 capture: total_count 3342, chronological). The adapter
    // must keep only the newest valuation of each position, bounded by a
    // start_date window so it does not crawl years of history.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/holdings"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "holdings": [
                wire_holding("acct-1", "VXUS", "2023-08-01", "$430.14"),
                wire_holding("acct-1", "VXUS", "2026-08-10", "$512.00"),
                wire_holding("acct-1", "VXUS", "2026-08-15", "$520.55"),
                wire_holding("acct-2", "VTI", "2026-08-14", "$1,000.00"),
            ],
            "pagination": pagination(1, 1, 4)
        })))
        .mount(&server)
        .await;

    let holdings = source_for(&server)
        .await
        .holdings()
        .await
        .expect("holdings");
    assert_eq!(holdings.len(), 2, "one position per (account, security)");
    let vxus = holdings
        .iter()
        .find(|h| h.symbol == "VXUS")
        .expect("VXUS kept");
    assert_eq!(
        vxus.market_value_minor,
        Some(52_055),
        "newest valuation wins"
    );
    assert_eq!(vxus.as_of_date.to_string(), "2026-08-15");

    let requests = server.received_requests().await.expect("reqs");
    assert!(
        requests[0].url.query().unwrap_or("").contains("start_date"),
        "history fetch must be windowed, not unbounded"
    );
}

#[tokio::test]
async fn rate_limited_requests_honor_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/accounts"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "1")
                .set_body_json(json!({ "error": "rate_limit_exceeded" })),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accounts": [wire_account("a-1", "One", None)],
            "pagination": pagination(1, 1, 1)
        })))
        .mount(&server)
        .await;

    let accounts = source_for(&server)
        .await
        .accounts()
        .await
        .expect("succeeds after honoring Retry-After");
    assert_eq!(accounts.len(), 1);
}

#[tokio::test]
async fn health_reuses_the_accounts_fetch_and_every_request_is_a_get() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accounts": [wire_account("a-1", "One", Some("First National"))],
            "pagination": pagination(1, 1, 1)
        })))
        .expect(1) // accounts() + health() must share one fetch
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/syncs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [], "meta": pagination(1, 1, 0)
        })))
        .mount(&server)
        .await;

    let source = source_for(&server).await;
    source.accounts().await.expect("accounts");
    source.health().await.expect("health");

    for request in server.received_requests().await.expect("reqs") {
        assert_eq!(
            request.method.to_string(),
            "GET",
            "read-only invariant: {}",
            request.url
        );
    }
}

/// Security fix (2026-09-05 audit, input-parsing lens): `reqwest`'s `.json()`
/// buffers a whole response body with no cap, so a compromised or MITM'd
/// upstream could answer a single page with gigabytes and OOM the process.
/// The page-count and `per_page` caps do not bound what the server actually
/// sends. The client now reads with a byte ceiling, like `narrate/llm.rs`.
#[tokio::test]
async fn oversized_response_body_is_refused_not_buffered() {
    let server = MockServer::start().await;
    // One well-formed row, then a name field far past any legitimate page.
    let huge_name = "A".repeat(5 * 1024 * 1024);
    Mock::given(method("GET"))
        .and(path("/api/v1/accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accounts": [wire_account("a-1", &huge_name, None)],
            "pagination": pagination(1, 1, 1)
        })))
        .mount(&server)
        .await;

    let source = source_for(&server).await;
    let err = source
        .accounts()
        .await
        .expect_err("an oversized body must be refused, not buffered");
    let message = err.to_string();
    assert!(
        message.contains("exceeded"),
        "expected a size-ceiling error, got: {message}"
    );
}

/// The ceiling must not reject ordinary traffic: a normal page still parses.
#[tokio::test]
async fn ordinary_response_body_is_still_accepted() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accounts": [wire_account("a-1", "One", None)],
            "pagination": pagination(1, 1, 1)
        })))
        .mount(&server)
        .await;

    let source = source_for(&server).await;
    let accounts = source.accounts().await.expect("a normal page still parses");
    assert_eq!(accounts.len(), 1);
}
