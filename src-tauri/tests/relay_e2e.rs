//! End-to-end test of the relay against a REAL GourmelyHub API.
//!
//! This is the test the whole feature is for. It drives the same code the
//! shipped bridge runs — `pairing`, `relay`, `job_ledger`, `printer` — and
//! checks the complete round trip:
//!
//! ```text
//! redeem the code → connect to /print → the server enqueues a job
//!   → the job arrives → it prints → print-result goes back
//!   → the row reaches a terminal state
//! ```
//!
//! Everything except the Tauri window, which is chrome: the window does not
//! print, this path does.
//!
//! `#[ignore]` for the same reason `tls_smoke` is: it needs a live server. Run
//! it explicitly with the environment below.
//!
//! ```powershell
//! $env:GMLY_E2E_URL      = "http://localhost:4000"
//! $env:GMLY_E2E_CODE     = "FJTX-35AH"     # from Configuración → Impresión
//! $env:GMLY_E2E_JWT      = "<admin access token>"
//! $env:GMLY_E2E_PRINTER  = "<uuid of a printer pinned to the station>"
//! $env:GMLY_E2E_BRANCH   = "<uuid of its branch>"
//! cargo test --test relay_e2e -- --ignored --nocapture
//! ```
//!
//! No physical thermal printer is needed: `GOURMELYPRINT_SINK_DIR` sends the
//! ESC/POS bytes to a file. What is under test is the channel, and the bytes
//! landing on disk prove they made it all the way through.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use print_bridge_lib::{config::SettingsStore, pairing, relay};

fn env_or_skip(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => {
            eprintln!("skipping: {name} is not set");
            None
        }
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gmly-e2e-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Waits for `check` to hold, polling every 250 ms. Returns false on timeout so
/// the caller can print something better than "assertion failed".
async fn wait_for(limit: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a live GourmelyHub API and a fresh pairing code; see the module docs"]
async fn pairs_connects_prints_and_acks() {
    let (Some(url), Some(code), Some(jwt), Some(printer_id), Some(branch_id)) = (
        env_or_skip("GMLY_E2E_URL"),
        env_or_skip("GMLY_E2E_CODE"),
        env_or_skip("GMLY_E2E_JWT"),
        env_or_skip("GMLY_E2E_PRINTER"),
        env_or_skip("GMLY_E2E_BRANCH"),
    ) else {
        return;
    };

    // Isolated data dir: this test must never touch the config.json or the
    // ledger of the machine it runs on.
    let data_dir = scratch("data");
    let sink_dir = scratch("sink");
    std::env::set_var("GOURMELYPRINT_DATA_DIR", &data_dir);
    std::env::set_var("GOURMELYPRINT_SINK_DIR", &sink_dir);

    let settings = SettingsStore::load();
    assert!(
        !settings.snapshot().is_paired(),
        "the scratch install must start unpaired"
    );

    // ── 1. Pair ─────────────────────────────────────────────────────────
    let paired = pairing::pair(&settings, &url, &code, vec!["GourmelyFake".into()])
        .await
        .expect("pairing failed");
    println!(
        "[e2e] paired: station={} branch={} label={:?} roles={:?}",
        paired.station_id, paired.branch_id, paired.label, paired.roles
    );
    assert_eq!(paired.branch_id, branch_id, "paired to the wrong branch");
    assert!(
        settings.snapshot().station_token.is_some(),
        "the token must survive on disk; it is only returned once"
    );

    // ── 2. Connect ──────────────────────────────────────────────────────
    let handle = relay::spawn(settings.clone());
    let connected = {
        let handle = handle.clone();
        wait_for(Duration::from_secs(30), move || handle.status().connected).await
    };
    assert!(
        connected,
        "the relay never reported a connection: {:?}",
        handle.status()
    );
    println!("[e2e] relay connected");

    // Presence has to reach the server before it will route anything: an
    // "offline" station makes the API take the local path instead.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ── 3. The server enqueues a job ────────────────────────────────────
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("http client");
    let response = http
        .post(format!(
            "{}/v1/printers/{}/test",
            url.trim_end_matches('/'),
            printer_id
        ))
        .bearer_auth(&jwt)
        .header("X-Branch-Id", &branch_id)
        .send()
        .await
        .expect("could not ask the API to print");
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    println!("[e2e] POST /printers/{printer_id}/test -> {status} {body}");
    assert!(status.is_success(), "the API refused to enqueue the job");

    // ── 4. It arrives, prints and is acked ──────────────────────────────
    let sink = sink_dir.clone();
    let printed = wait_for(Duration::from_secs(30), move || {
        std::fs::read_dir(&sink)
            .map(|entries| entries.flatten().count() > 0)
            .unwrap_or(false)
    })
    .await;
    assert!(printed, "no ESC/POS file ever landed in {sink_dir:?}");

    let files: Vec<_> = std::fs::read_dir(&sink_dir)
        .expect("read sink")
        .flatten()
        .collect();
    for file in &files {
        let bytes = std::fs::read(file.path()).expect("read ticket");
        println!(
            "[e2e] printed {} ({} bytes)",
            file.file_name().to_string_lossy(),
            bytes.len()
        );
        assert!(!bytes.is_empty(), "the ticket that printed was empty");
        // ESC @ — the ESC/POS initialise the server's generator always emits.
        // Checking the bytes and not just the file proves the payload survived
        // base64 → socket → decode intact.
        assert_eq!(&bytes[..2], b"\x1b@", "these are not ESC/POS bytes");
    }

    let handle_for_count = handle.clone();
    let acked = wait_for(Duration::from_secs(10), move || {
        handle_for_count.status().jobs_printed >= 1
    })
    .await;
    let final_status = handle.status();
    println!("[e2e] relay status: {final_status:?}");
    assert!(
        acked,
        "the bridge printed but never counted the job as done: {final_status:?}"
    );
    assert_eq!(final_status.jobs_failed, 0, "a job failed");

    // The ack is sent right after the ledger is written; give the socket a
    // moment so the caller can check `print_jobs.status` server-side.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // ── 5. The ledger remembers, so a replay does not reprint ───────────
    let ledger = std::fs::read_to_string(data_dir.join("print-jobs.jsonl"))
        .expect("the ledger must exist after a print");
    println!("[e2e] ledger:\n{ledger}");
    // Order, not just presence: the claim line has to be on disk BEFORE the
    // outcome line, because that ordering *is* R8. Asserting only that both
    // exist would pass just as happily on a bridge that printed first and wrote
    // afterwards — the version that duplicates tickets after a power cut.
    let claimed_at = ledger
        .find("\"claimed\"")
        .expect("the claim must be written BEFORE printing (R8)");
    let printed_at = ledger
        .find("\"printed\"")
        .expect("the outcome must be recorded after printing");
    assert!(
        claimed_at < printed_at,
        "the outcome was recorded before the claim — that is the ordering that duplicates tickets"
    );
}

/// The other half of R8: a job the server re-delivers must NOT print again.
///
/// Reuses the install `pairs_connects_prints_and_acks` left behind — the same
/// station token and, crucially, the same ledger. Between the two runs the job
/// row is put back to `sent` server-side, which is exactly what a bridge that
/// died without acking leaves in the queue and what `claimReplayableJobs`
/// re-sends on the next connection.
///
/// ```powershell
/// # after the first test, from psql:
/// #   update print_jobs set status='sent', expires_at=now()+interval '5 min';
/// $env:GMLY_E2E_DATA_DIR = "$env:TEMP\gmly-e2e-data"
/// $env:GMLY_E2E_SINK_DIR = "$env:TEMP\gmly-e2e-sink"
/// cargo test --test relay_e2e replay -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the install left by pairs_connects_prints_and_acks and a replayed job"]
async fn a_replayed_job_is_not_reprinted() {
    let (Some(data_dir), Some(sink_dir)) = (
        env_or_skip("GMLY_E2E_DATA_DIR"),
        env_or_skip("GMLY_E2E_SINK_DIR"),
    ) else {
        return;
    };
    let sink_dir = PathBuf::from(sink_dir);

    std::env::set_var("GOURMELYPRINT_DATA_DIR", &data_dir);
    std::env::set_var("GOURMELYPRINT_SINK_DIR", &sink_dir);

    let count_tickets = || {
        std::fs::read_dir(&sink_dir)
            .map(|e| e.flatten().count())
            .unwrap_or(0)
    };
    let before = count_tickets();
    println!("[e2e] tickets on disk before reconnecting: {before}");
    assert!(before > 0, "run the first test first: nothing has printed");

    let settings = SettingsStore::load();
    assert!(
        settings.snapshot().is_paired(),
        "this test reuses the pairing from the first one"
    );

    let handle = relay::spawn(settings);
    let connected = {
        let handle = handle.clone();
        wait_for(Duration::from_secs(30), move || handle.status().connected).await
    };
    assert!(
        connected,
        "the relay did not reconnect: {:?}",
        handle.status()
    );
    println!("[e2e] reconnected; waiting for the replay");

    // Long enough for the replay burst to arrive and be answered.
    tokio::time::sleep(Duration::from_secs(6)).await;

    let after = count_tickets();
    let status = handle.status();
    println!("[e2e] tickets after: {after} · status: {status:?}");
    assert_eq!(
        after, before,
        "the replayed job PRINTED AGAIN — the disk ledger is not doing its job"
    );
    assert_eq!(
        status.jobs_printed, 0,
        "nothing new should have been printed on this connection"
    );
}

/// Rotating the token kicks the bridge off and then refuses it — and pairing
/// again brings it back without restarting anything.
///
/// This is the failure the supervisor has to tell apart from a flaky link. The
/// server disconnects the live socket the moment the owner rotates, and the
/// reconnection is then rejected: hammering that every second would be wrong,
/// and reporting "sin conexión" would send someone to check the router instead
/// of the panel.
///
/// ```powershell
/// $env:GMLY_E2E_DATA_DIR = "$env:TEMP\gmly-e2e-data"
/// $env:GMLY_E2E_SINK_DIR = "$env:TEMP\gmly-e2e-sink"
/// $env:GMLY_E2E_URL      = "http://localhost:4000"
/// $env:GMLY_E2E_JWT      = "<admin access token>"
/// $env:GMLY_E2E_STATION  = "<station uuid>"
/// $env:GMLY_E2E_BRANCH   = "<branch uuid>"
/// cargo test --test relay_e2e rotation -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the install left by pairs_connects_prints_and_acks; consumes the pairing"]
async fn survives_a_token_rotation() {
    let (Some(data_dir), Some(sink), Some(url), Some(jwt), Some(station), Some(branch)) = (
        env_or_skip("GMLY_E2E_DATA_DIR"),
        env_or_skip("GMLY_E2E_SINK_DIR"),
        env_or_skip("GMLY_E2E_URL"),
        env_or_skip("GMLY_E2E_JWT"),
        env_or_skip("GMLY_E2E_STATION"),
        env_or_skip("GMLY_E2E_BRANCH"),
    ) else {
        return;
    };
    std::env::set_var("GOURMELYPRINT_DATA_DIR", &data_dir);
    std::env::set_var("GOURMELYPRINT_SINK_DIR", &sink);

    let settings = SettingsStore::load();
    assert!(
        settings.snapshot().is_paired(),
        "start from a paired install"
    );

    let handle = relay::spawn(settings.clone());
    {
        let handle = handle.clone();
        assert!(
            wait_for(Duration::from_secs(30), move || handle.status().connected).await,
            "never connected to begin with"
        );
    }
    println!("[e2e] connected with the old token");

    // ── The owner rotates the token from the panel ──────────────────────
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("http client");
    let body = http
        .post(format!(
            "{}/v1/print-relay/stations/{}/rotate",
            url.trim_end_matches('/'),
            station
        ))
        .bearer_auth(&jwt)
        .header("X-Branch-Id", &branch)
        .send()
        .await
        .expect("rotate failed")
        .text()
        .await
        .expect("rotate body");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("rotate json");
    let new_code = parsed["data"]["pairingCode"]
        .as_str()
        .expect("rotate must return a fresh pairing code")
        .to_string();
    println!("[e2e] rotated; new code = {new_code}");

    // ── The bridge notices, and knows WHY ───────────────────────────────
    let rejected = {
        let handle = handle.clone();
        wait_for(Duration::from_secs(60), move || {
            handle.status().token_rejected
        })
        .await
    };
    let status = handle.status();
    println!("[e2e] after rotation: {status:?}");
    assert!(
        rejected,
        "the bridge never realised its token was revoked: {status:?}"
    );
    assert!(
        !status.connected,
        "it should not still claim to be connected"
    );
    assert!(
        status
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("vincular"),
        "the message must send the operator to the panel, not to the router"
    );

    // ── Pairing again recovers, with no restart ─────────────────────────
    pairing::pair(&settings, &url, &new_code, vec!["GourmelyFake".into()])
        .await
        .expect("re-pairing failed");
    handle.reconnect();

    let back = {
        let handle = handle.clone();
        wait_for(Duration::from_secs(30), move || handle.status().connected).await
    };
    println!("[e2e] after re-pairing: {:?}", handle.status());
    assert!(
        back,
        "the bridge did not come back after being paired again — note this must \
         happen without waiting out the five-minute revoked backoff"
    );
}

/// Watches the relay for a while, printing every state change.
///
/// The point is the requirement that cannot be unit-tested: a power cut or a
/// dead internet link must not leave the till mute *for ever*. Run this, then
/// kill the API from another shell and bring it back; the log has to show
/// disconnect → retries → reconnected, with nobody touching the bridge.
///
/// ```powershell
/// $env:GMLY_E2E_DATA_DIR  = "$env:TEMP\gmly-e2e-data"
/// $env:GMLY_E2E_WATCH_SECS = "90"
/// cargo test --test relay_e2e watches -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual: bounce the API while this runs"]
async fn watches_the_relay_through_an_outage() {
    let Some(data_dir) = env_or_skip("GMLY_E2E_DATA_DIR") else {
        return;
    };
    std::env::set_var("GOURMELYPRINT_DATA_DIR", &data_dir);
    let seconds: u64 = std::env::var("GMLY_E2E_WATCH_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(90);

    let settings = SettingsStore::load();
    assert!(
        settings.snapshot().is_paired(),
        "start from a paired install"
    );
    let handle = relay::spawn(settings);

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut last = String::new();
    while Instant::now() < deadline {
        let s = handle.status();
        let line = format!(
            "connected={} rejected={} error={:?}",
            s.connected, s.token_rejected, s.last_error
        );
        if line != last {
            println!(
                "[watch +{:>3}s] {line}",
                seconds - deadline.saturating_duration_since(Instant::now()).as_secs()
            );
            last = line;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    println!("[watch] final: {:?}", handle.status());
}
