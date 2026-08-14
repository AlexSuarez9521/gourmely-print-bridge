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
use print_bridge_lib::config::{LocalAuth, Settings};
use print_bridge_lib::server::{router_for_tests, router_with_settings};
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

/// What `/health` says about the relay, and the promise it makes to the caller
/// that has been reading it since before the relay existed.
///
/// # Why this is a whole module
///
/// `apps/platform-web/lib/print-bridge.ts` polls this endpoint to paint the
/// bridge badge and to report the version to GourmelyHub. It answered `ok:true`
/// no matter what the relay was doing, so a till whose relay was dead — standing
/// down for a second copy, or unable to open its print ledger — looked healthy
/// from the POS while the server marked the station offline and quietly routed
/// the ticket through a browser. Nobody was told anything.
mod relay_state {
    use super::*;
    use print_bridge_lib::config::SettingsStore;
    use print_bridge_lib::job_ledger::lock_path_for;
    use print_bridge_lib::relay::{self, RelayState};
    use std::time::{Duration, Instant};

    async fn health_of(router: axum::Router) -> Value {
        let server = TestServer::new(router).expect("test server");
        let res = server.get("/health").await;
        res.assert_status_ok();
        let body: Value = res.json();
        println!("{}", serde_json::to_string_pretty(&body).expect("pretty"));
        body
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gmly-health-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    /// The compatibility half, stated as an assertion rather than a hope: the
    /// four fields that existed before are still there, with the same names,
    /// the same types and the same meanings.
    #[tokio::test]
    async fn the_fields_the_pos_already_reads_did_not_move() {
        let body = health_of(router_for_tests()).await;
        assert_eq!(
            body["ok"], true,
            "`ok` is the local print path and a caller reading only it must keep working"
        );
        assert!(body["version"].is_string());
        assert!(body["uptime_seconds"].is_number());
        assert!(body["printer_count"].is_number());
    }

    /// A process with no relay must say so rather than look connected.
    #[tokio::test]
    async fn a_bridge_without_a_relay_says_unknown_not_ok() {
        let body = health_of(router_for_tests()).await;
        assert_eq!(body["relay"]["state"], "unknown");
        assert_eq!(body["relay"]["ok"], false);
    }

    /// The reported case: another bridge holds the print ledger, this one is
    /// standing down, and until now `/health` said `ok:true` and nothing else.
    ///
    /// Revert `HealthResponse` to the four old fields and this fails on the
    /// missing `relay` — which is precisely the information the POS never had.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_relay_standing_down_is_visible_from_the_pos() {
        let dir = scratch("standby");
        // The relay resolves its ledger from `data_dir()`, and this test needs
        // it to be the file the "other bridge" below is holding — not the
        // %PROGRAMDATA% of whoever is running the suite.
        std::env::set_var("GOURMELYPRINT_DATA_DIR", &dir);
        let ledger = dir.join("print-jobs.jsonl");
        let _other_bridge =
            print_bridge_lib::instance_lock::InstanceLock::acquire(lock_path_for(&ledger))
                .expect("the other bridge takes the lock");

        let relay = relay::spawn_on(
            &tokio::runtime::Handle::current(),
            SettingsStore::in_memory(Settings {
                server_url: Some("http://127.0.0.1:1".into()),
                station_token: Some("station-token-under-test".into()),
                label: Some("Caja Principal".into()),
                ..Default::default()
            }),
        );

        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline && relay.status().state != RelayState::Standby {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let body = health_of(print_bridge_lib::server::router_with_relay(relay.clone())).await;
        assert_eq!(
            body["ok"], true,
            "the local socket still prints; that half is genuinely fine"
        );
        assert_eq!(
            body["relay"]["state"], "standby",
            "the POS cannot tell this apart from a bridge that is printing"
        );
        assert_eq!(
            body["relay"]["ok"], false,
            "the POS is still told a dead relay can take tickets"
        );
        assert_eq!(
            body["relay"]["needs_attention"], true,
            "shown as something that will fix itself, so nobody looks at it"
        );
        assert_eq!(body["relay"]["paired"], true);
        assert_eq!(body["relay"]["connected"], false);
        assert!(
            body["relay"]["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("ya está abierto"),
            "no sentence for whoever has to fix it: {}",
            body["relay"]["detail"]
        );
    }
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

/// The local auth hole, exercised over a REAL socket.
///
/// It has to be a real one. `axum-test` drives the router through a mock
/// transport that never carries hyper's upgrade state, so `WebSocketUpgrade`
/// rejects with 426 before the handler body runs and every case looks the same
/// — a test that would have "passed" while testing nothing. Binding an
/// ephemeral port and doing an actual handshake is the only way to see the
/// guard's answer.
///
/// Both halves matter and neither alone is enough: a 403 for everyone would
/// "close the hole" by breaking printing at the till.
mod local_auth {
    use super::*;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    /// Boots the guarded router on an ephemeral port. Returns its address; the
    /// server task lives as long as the test.
    async fn guarded_server() -> String {
        let router = router_with_settings(Settings {
            local_auth: LocalAuth::OriginOrSecret,
            local_secret: Some("el-secreto-local".into()),
            ..Default::default()
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("ws://{addr}")
    }

    /// Attempts the handshake and reports the HTTP status the bridge answered:
    /// 101 when it let the caller in, 403 when the guard turned it away.
    async fn handshake(base: &str, path: &str, origin: Option<&str>) -> u16 {
        let mut request = format!("{base}{path}")
            .into_client_request()
            .expect("build request");
        if let Some(origin) = origin {
            request.headers_mut().insert(
                "origin",
                HeaderValue::from_str(origin).expect("origin header"),
            );
        }
        match tokio_tungstenite::connect_async(request).await {
            Ok((_stream, response)) => response.status().as_u16(),
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                response.status().as_u16()
            }
            Err(e) => panic!("handshake failed for an unexpected reason: {e}"),
        }
    }

    #[tokio::test]
    async fn a_page_that_is_not_the_pos_cannot_print() {
        let base = guarded_server().await;
        assert_eq!(
            handshake(&base, "/print", Some("https://cupones-gratis.example")).await,
            403,
            "any page open on the till could print"
        );
    }

    #[tokio::test]
    async fn the_pos_is_not_broken_by_the_guard() {
        let base = guarded_server().await;
        assert_eq!(
            handshake(
                &base,
                "/print",
                Some("https://app-gourmelyhub.busticco.com")
            )
            .await,
            101,
            "the POS as it ships today, which sends no token, must keep printing"
        );
    }

    #[tokio::test]
    async fn a_native_caller_needs_the_secret_and_needs_it_right() {
        let base = guarded_server().await;
        // No Origin at all: curl, a script, another program on the machine.
        assert_eq!(
            handshake(&base, "/print?token=el-secreto-local", None).await,
            101
        );
        assert_eq!(
            handshake(&base, "/print?token=casi-el-secreto", None).await,
            403
        );
        assert_eq!(handshake(&base, "/print", None).await, 403);
    }
}
