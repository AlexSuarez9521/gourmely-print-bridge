//! End-to-end smoke test for the TLS-terminated bridge server.
//!
//! Boots `server::serve()` with the real Let's Encrypt cert (so we
//! exercise the actual rustls path that ships to production) and hits
//! `/health` via `reqwest` over HTTPS. Anything wrong in cert parsing,
//! TLS handshake, or the bind to `localhost.gourmelyhub.busticco.com`
//! shows up here.
//!
//! Marked `#[ignore]` because it:
//!   - Reads cert files from `certs/` (only present in dev)
//!   - Binds TCP 8181 (must be free)
//!   - Requires DNS `localhost.gourmelyhub.busticco.com → 127.0.0.1`
//! Run explicitly with: `cargo test --test tls_smoke -- --ignored --nocapture`

use std::time::Duration;

#[tokio::test]
#[ignore = "requires cert files + free port 8181 + DNS record; run with `--ignored`"]
async fn tls_health_returns_200_with_real_cert() {
    // Read cert + key from disk at test time so the smoke test exercises
    // the same wire path as production (production embeds via
    // include_bytes!, tests just read from the same `certs/` folder).
    let cert = std::fs::read("certs/fullchain.pem").expect("certs/fullchain.pem must exist for smoke test");
    let key = std::fs::read("certs/privkey.pem").expect("certs/privkey.pem must exist for smoke test");

    let server_task = tokio::spawn(async move {
        print_bridge_lib::server::serve(&cert, &key).await
    });

    // Tiny pause for the listener to be ready. axum_server is fast; 1s
    // is generous but reliable on CI runners with cold caches.
    tokio::time::sleep(Duration::from_secs(1)).await;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("build reqwest client");

    let response = client
        .get("https://localhost.gourmelyhub.busticco.com:8181/health")
        .send()
        .await
        .expect("HTTPS request failed — TLS handshake or DNS broken?");

    assert_eq!(response.status().as_u16(), 200);
    let body: serde_json::Value = response.json().await.expect("response was not JSON");
    assert_eq!(body["ok"], true);
    assert!(body["version"].is_string());
    assert!(body["uptime_seconds"].is_number());

    println!("/health body: {}", body);

    // Hit /printers too — exercises the printer module on real OS APIs.
    let printers: serde_json::Value = client
        .get("https://localhost.gourmelyhub.busticco.com:8181/printers")
        .send()
        .await
        .expect("printers request failed")
        .json()
        .await
        .expect("printers response was not JSON");
    assert_eq!(printers["ok"], true);
    assert!(printers["printers"].is_array());
    println!("/printers body: {}", printers);

    server_task.abort();
}
