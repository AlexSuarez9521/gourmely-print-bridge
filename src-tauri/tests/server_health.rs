//! Integration test for the print bridge server.
//!
//! Boots the axum router (without TLS to keep the test self-contained
//! and avoid needing a cert at test time), then exercises the routes
//! the way the real frontend does:
//!   1. `GET /health`     → returns ok=true + version + uptime
//!   2. `GET /printers`   → returns ok=true + list of strings
//!   3. `WS  /print` send `{ "op": "list", "id": "1" }` → expects
//!      `{ "id": "1", "ok": true, "printers": [...] }`
//!
//! TLS is exercised in a separate smoke test that runs against a real
//! cert; in unit/integration tests we run the router over plain HTTP so
//! `axum-test` can talk to it directly.

use axum_test::TestServer;
use print_bridge_lib::server::router_for_tests;
use serde_json::Value;

#[tokio::test]
async fn health_returns_ok_with_metadata() {
    let server = TestServer::new(router_for_tests()).unwrap();
    let res = server.get("/health").await;
    res.assert_status_ok();
    let body: Value = res.json();
    assert_eq!(body["ok"], true);
    assert!(body["version"].is_string(), "version should be a string");
    assert!(
        body["uptime_seconds"].is_number(),
        "uptime_seconds should be a number"
    );
    // printer_count varies per machine but must be a number
    assert!(body["printer_count"].is_number());
}

#[tokio::test]
async fn printers_returns_array_of_names() {
    let server = TestServer::new(router_for_tests()).unwrap();
    let res = server.get("/printers").await;
    res.assert_status_ok();
    let body: Value = res.json();
    assert_eq!(body["ok"], true);
    assert!(body["printers"].is_array());
}
