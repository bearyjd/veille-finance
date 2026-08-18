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
