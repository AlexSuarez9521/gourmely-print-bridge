//! Does the thing we ship actually start?
//!
//! # The gap this closes
//!
//! Every other test in this repo links the library. Not one of them had ever
//! executed `print-bridge.exe`, and `cargo test --all-targets` still does not
//! run it by itself — so the most expensive regression this project has had
//! sailed through a fully green suite.
//!
//! That regression: `relay::spawn` ends in `tokio::spawn`, Tauri's `setup` hook
//! runs on the main thread outside the runtime, and calling one from the other
//! panics with *"there is no reactor running"*. The app exited **101** on every
//! launch, before the window, before the server, before anything. The relay
//! tests were green throughout, because `#[tokio::test]` enters a runtime for
//! you — the one condition production never has.
//!
//! It was verified again while writing this: revert the guard in `lib.rs` and
//! **all 69 tests still pass** while the binary dies in 300 ms. So the only
//! honest test is the one below — start the executable, and ask it.
//!
//! # What "alive" means here
//!
//! Not "the process is still in the table". A Tauri app that panicked in
//! `setup` and one that is working are both processes for a moment. So this
//! asks the binary three questions that only a fully booted bridge can answer:
//!
//! 1. `/health` replies over TLS — the axum server task is running;
//! 2. it reports a real relay state, not `unknown` — `relay::spawn_on` returned
//!    a handle and `lib.rs` wired it into the server;
//! 3. it took the print ledger — the supervisor loop is actually turning, which
//!    is the part `tokio::spawn` was needed for in the first place.
//!
//! # Why it does not use port 8181
//!
//! Because the machine running it may have a bridge installed, and a test that
//! probed *that* bridge's `/health` would pass no matter what this build does.
//! `GOURMELYPRINT_BIND_PORT` exists for this.

use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Generous: a cold debug build on a CI runner is slower than anything at a
/// till, and the failure this test is for happens in under a second.
const BOOT_BUDGET: Duration = Duration::from_secs(90);

/// Kills the bridge however the test ends, including on a panic. A stray Tauri
/// app holding a port would poison every run after it.
struct Bridge {
    child: Child,
    logs: PathBuf,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Bridge {
    /// Whatever the binary printed, for the failure message. A panic in `setup`
    /// says exactly what it was, and that sentence is the whole diagnosis.
    fn output(&self) -> String {
        let read = |name: &str| {
            std::fs::read_to_string(self.logs.join(name)).unwrap_or_else(|e| format!("<{e}>"))
        };
        format!(
            "--- stdout ---\n{}\n--- stderr ---\n{}",
            read("bridge.out"),
            read("bridge.err")
        )
    }

    fn exited(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            _ => None,
        }
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gmly-smoke-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// A port nobody is using, asked of the OS rather than guessed.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

/// A paired station pointing at a port with nothing on it.
///
/// Deliberate: the relay then has somewhere to go, fails to get there, and
/// settles in `connecting` — which is proof the supervisor task is *running*,
/// not merely that a handle was constructed. A bridge with no token would park
/// in `unpaired` without the loop ever turning.
fn write_paired_config(data_dir: &Path, dead_port: u16) {
    let config = serde_json::json!({
        "serverUrl": format!("http://127.0.0.1:{dead_port}"),
        "stationToken": "station-token-under-test",
        "label": "Caja Principal",
        "roles": ["receipt"],
        "defaultPrinter": "GourmelyFake",
        "localAuth": "origin-or-secret",
    });
    let mut file =
        std::fs::File::create(data_dir.join("config.json")).expect("write the scratch config");
    file.write_all(
        serde_json::to_string_pretty(&config)
            .expect("json")
            .as_bytes(),
    )
    .expect("write the scratch config");
}

#[tokio::test]
async fn the_binary_starts_and_serves() {
    let data_dir = scratch("data");
    let logs = scratch("logs");
    let port = free_port();
    let dead_port = std::iter::repeat_with(free_port)
        .find(|candidate| *candidate != port)
        .expect("a second free port");
    write_paired_config(&data_dir, dead_port);

    let stdout = std::fs::File::create(logs.join("bridge.out")).expect("stdout log");
    let stderr = std::fs::File::create(logs.join("bridge.err")).expect("stderr log");

    let child = Command::new(env!("CARGO_BIN_EXE_print-bridge"))
        .env("GOURMELYPRINT_DATA_DIR", &data_dir)
        .env("GOURMELYPRINT_BIND_PORT", port.to_string())
        // The sink keeps a stray job off a real printer if one is installed on
        // the machine running the tests.
        .env("GOURMELYPRINT_SINK_DIR", scratch("sink"))
        .env("RUST_LOG", "print_bridge_lib=debug")
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("could not start print-bridge.exe");
    let mut bridge = Bridge { child, logs };

    // Self-signed as far as this client is concerned: the shipped certificate is
    // issued for `localhost.gourmelyhub.busticco.com`, and a test must not
    // depend on that public DNS record resolving on the machine it runs on.
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(5))
        .build()
        .expect("http client");
    let url = format!("https://127.0.0.1:{port}/health");

    let deadline = Instant::now() + BOOT_BUDGET;
    let mut health: Option<serde_json::Value> = None;
    while Instant::now() < deadline {
        if let Some(code) = bridge.exited() {
            // Three ways a startup can end, and they need three different
            // people to look at them. The panic text on stderr is what tells
            // them apart, so it decides the sentence rather than the exit code
            // — a Tauri app that cannot create its webview also exits 101.
            let output = bridge.output();
            let hint = if code == 0 {
                "another GourmelyPrint Bridge is already running on this machine; \
                 close it and run the test again (the single-instance plugin exits \
                 the second copy on purpose)"
            } else if output.contains("no reactor running") {
                "THIS IS THE REGRESSION THIS TEST EXISTS FOR: the relay was started \
                 outside the tokio runtime. `lib.rs` must call `relay::spawn_on`, \
                 never `relay::spawn`"
            } else {
                "the bridge died during startup for some other reason — read the panic \
                 below before assuming it is the environment"
            };
            panic!("print-bridge.exe exited with code {code} before it served anything.\n{hint}\n{output}");
        }
        if let Ok(response) = client.get(&url).send().await {
            if response.status().is_success() {
                let body: serde_json::Value = response.json().await.expect("/health was not JSON");
                // The relay is set by the supervisor a beat after the server
                // starts answering; wait for it rather than racing it.
                if body["relay"]["state"] != "unpaired" {
                    health = Some(body);
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let Some(health) = health else {
        panic!(
            "print-bridge.exe never answered {url} inside {:?} — it is running but not serving\n{}",
            BOOT_BUDGET,
            bridge.output()
        );
    };
    println!(
        "[smoke] /health: {}",
        serde_json::to_string_pretty(&health).expect("pretty")
    );

    // ── 1. Still alive after it answered ────────────────────────────────
    assert!(
        bridge.exited().is_none(),
        "the bridge served once and then died\n{}",
        bridge.output()
    );

    // ── 2. The answers ──────────────────────────────────────────────────
    assert_eq!(health["ok"], true);
    assert_eq!(
        health["version"],
        env!("CARGO_PKG_VERSION"),
        "the binary under test is not the one this checkout builds"
    );
    assert!(health["printer_count"].is_number());

    // The relay reached the server, so `spawn_on` returned and `lib.rs` wired
    // it in. `unknown` here means the handle never arrived.
    assert_eq!(
        health["relay"]["state"],
        "connecting",
        "the relay supervisor is not running: {}\n{}",
        health["relay"],
        bridge.output()
    );
    assert_eq!(health["relay"]["paired"], true);
    assert_eq!(
        health["relay"]["ok"], false,
        "it claims the server can route tickets here, with nothing on the other end"
    );

    // ── 3. It got as far as taking the ledger ───────────────────────────
    // The supervisor's first act. A handle that exists but whose task never ran
    // would leave this file missing.
    let lock = data_dir.join("print-jobs.lock");
    assert!(
        lock.exists(),
        "the relay never took the print ledger, so its task is not running: {}\n{}",
        lock.display(),
        bridge.output()
    );
}
