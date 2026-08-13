//! GourmelyPrint Bridge — entry point.
//!
//! Wiring:
//!   1. Tauri builds the desktop chrome (window + tray) and registers
//!      auto-start/opener/updater plugins.
//!   2. A tokio task spawned at startup runs the WSS server that the
//!      POS web app connects to. The server stays alive for the entire
//!      app lifetime and shuts down cleanly when the user quits.
//!   3. A second task runs the `relay`: an outbound, permanent connection
//!      to GourmelyHub that receives print jobs from anywhere — which is
//!      what lets a waiter on mobile data send a comanda to the kitchen.
//!   4. Both paths end in the same `printer` module, which talks to the
//!      Windows print spooler. There is exactly one way to put ink on
//!      paper in this program.
//!
//! And, before any of it: **exactly one of these processes may exist**. See
//! [`run`] and [`instance_lock`] — two bridges on one till print every ticket
//! twice, and the relay is what made that reachable.

pub mod config;
pub mod engineio;
pub mod error;
pub mod instance_lock;
pub mod job_ledger;
pub mod pairing;
pub mod printer;
pub mod relay;
pub mod server;
pub mod tls_material;
pub mod tray;

use tauri::{Manager, WindowEvent};
use tauri_plugin_autostart::MacosLauncher;

/// TLS material embedded at compile time so the installer is a single
/// self-contained .exe. The chain is the public Let's Encrypt cert
/// (safe to bundle — it's served on every TLS handshake) plus the
/// private key, which is acceptable to embed because every customer
/// shares the same `localhost.gourmelyhub.busticco.com` cert (the DNS
/// record only resolves to 127.0.0.1, so the attack surface is local).
/// Same security model as QZ Tray's `localhost.qz.io` cert bundling.
const CERT_PEM: &[u8] = include_bytes!("../certs/fullchain.pem");
const KEY_PEM: &[u8] = include_bytes!("../certs/privkey.pem");

/// Tauri command: returns the list of printers registered on this
/// machine. Exposed both to the WSS clients (via `server::handle`) and
/// to the Tauri webview UI (settings page can show what's available).
#[tauri::command]
fn list_printers() -> Result<Vec<String>, String> {
    printer::list().map_err(|e| e.to_string())
}

/// Tauri command: send a test ticket so the operator can verify a
/// printer is alive without going through the POS web.
#[tauri::command]
fn test_print(printer_name: String) -> Result<u32, String> {
    let bytes = printer::test_ticket_bytes();
    printer::print_raw(&printer_name, &bytes).map_err(|e| e.to_string())
}

/// Tauri command: enable or disable auto-start with Windows. Backed by
/// the autostart plugin which writes/removes the HKCU\Run entry.
#[tauri::command]
async fn set_autostart(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    if enabled {
        manager.enable().map_err(|e| e.to_string())
    } else {
        manager.disable().map_err(|e| e.to_string())
    }
}

/// Tauri command: read current auto-start status. Used by the settings
/// UI to hydrate the toggle on open.
#[tauri::command]
fn is_autostart_enabled(app: tauri::AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

// ── Estación remota ──────────────────────────────────────────────────

/// Tauri command: redeem a pairing code.
///
/// The code is typed by whoever is standing at the till, so everything about
/// this path is written for them: it normalises what they type, it never
/// retries a burnt code behind their back, and the error it returns is the
/// server's own sentence, not a status code.
#[tauri::command]
async fn pair_station(
    app: tauri::AppHandle,
    code: String,
    server_url: Option<String>,
) -> Result<relay::RelayStatus, String> {
    let settings = app.state::<config::SettingsStore>().inner().clone();
    let relay = app.state::<relay::RelayHandle>().inner().clone();

    let url = server_url
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| settings.snapshot().server_url());

    let printers = printer::list().unwrap_or_default();
    pairing::pair(&settings, &url, &code, printers)
        .await
        .map_err(|e| e.to_string())?;

    // Reconnect at once instead of waiting out the idle wait: the operator is
    // watching the screen and expects "Conectado" now, not in a minute.
    relay.reconnect();
    Ok(relay.status())
}

/// Tauri command: forget the pairing on this machine.
#[tauri::command]
fn unpair_station(app: tauri::AppHandle) -> Result<relay::RelayStatus, String> {
    let settings = app.state::<config::SettingsStore>().inner().clone();
    let relay = app.state::<relay::RelayHandle>().inner().clone();
    pairing::forget(&settings).map_err(|e| e.to_string())?;
    relay.reconnect();
    Ok(relay.status())
}

/// Tauri command: everything the "Estación" panel paints. Never includes the
/// station token.
#[tauri::command]
fn relay_status(app: tauri::AppHandle) -> relay::RelayStatus {
    app.state::<relay::RelayHandle>().status()
}

/// Tauri command: the local secret the POS will have to present once the web
/// starts sending it. Shown in the settings window so support can copy it
/// without opening `config.json` with a text editor.
#[tauri::command]
fn local_secret(app: tauri::AppHandle) -> Option<String> {
    app.state::<config::SettingsStore>().snapshot().local_secret
}

/// Tauri command: pick the printer to use for jobs that arrive without a name
/// of their own (role-room delivery).
#[tauri::command]
fn set_default_printer(app: tauri::AppHandle, name: Option<String>) -> Result<(), String> {
    let settings = app.state::<config::SettingsStore>().inner().clone();
    settings
        .update(|s| s.default_printer = name.filter(|n| !n.trim().is_empty()))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "print_bridge_lib=info,tower_http=info,axum=info".into()),
        )
        .init();

    tracing::info!(
        "GourmelyPrint Bridge starting (v{})",
        env!("CARGO_PKG_VERSION")
    );

    let builder = tauri::Builder::default();

    // single-instance FIRST, before any other plugin gets to do work.
    //
    // A second launch of the bridge is not a cosmetic annoyance: both copies
    // dial out to GourmelyHub with the same station token, land in the same
    // room, and the server pushes every print job down both sockets. One order,
    // two comandas, and nothing in the logs that says why. Before the relay the
    // second copy was harmless only by accident — it could not take port 8181
    // and sat there inert.
    //
    // The callback runs in the copy that was already running, so the second
    // launch reads to the cashier as "the window opened", which is what they
    // wanted when they double-clicked. Meanwhile the plugin exits the new
    // process before it can print anything.
    //
    // This net does not cover a build that runs outside Tauri (a test, a
    // support harness, `cargo run` next to an installed copy); the exclusive
    // lock on the print ledger does, and it is the one the relay checks.
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
        tracing::warn!(
            ?argv,
            cwd,
            "a second instance tried to start; showing the one already running instead"
        );
        tray::show_main_window(app);
    }));

    builder
        // opener: lets the tray menu open files / URLs without spawning shell commands.
        .plugin(tauri_plugin_opener::init())
        // autostart: HKCU\...\Run on Windows (no-op flag for macOS — not in scope yet).
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            None,
        ))
        // updater: checks the signed latest.json manifest on the R2
        // download mirror. The frontend calls `check()` on boot and
        // prompts the user before downloading (one-click consent).
        .plugin(tauri_plugin_updater::Builder::new().build())
        // process: lets the frontend relaunch the app after an update
        // installs.
        .plugin(tauri_plugin_process::init())
        .invoke_handler(tauri::generate_handler![
            list_printers,
            test_print,
            set_autostart,
            is_autostart_enabled,
            pair_station,
            unpair_station,
            relay_status,
            local_secret,
            set_default_printer,
        ])
        // Close-to-tray: clicking the window's X must NOT quit the
        // bridge — that would silently kill printing for the whole POS.
        // Instead we hide the window back to the system tray. The only
        // way to actually quit is the tray menu's "Salir". This is the
        // expected behaviour for a background service.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(|app| {
            // Settings first: both the local server and the relay read from
            // this one store, and the local secret is generated here if the
            // install has never had one.
            let settings = config::SettingsStore::load();
            app.manage(settings.clone());

            // Install the system tray icon. Failure here is recoverable
            // (the WSS still works), but log loudly so a regression is
            // obvious in a support log.
            if let Err(e) = tray::install(app.handle()) {
                tracing::error!("failed to install tray icon: {e}");
            }

            // Outbound relay. Started even when the bridge is not paired yet:
            // it parks itself and wakes up the moment a code is redeemed, so
            // pairing needs no restart at the till.
            //
            // # Why the runtime is entered by hand
            //
            // `relay::spawn` starts its supervisor with `tokio::spawn`, and
            // **this hook does not run inside the async runtime** — Tauri calls
            // `setup` from `Builder::build`, on the main thread. Without the
            // guard below that call panics with *"there is no reactor running"*
            // and takes the whole app down before the window ever appears.
            //
            // It is not a hypothetical: the app on `feat/relay` exited 101 on
            // every launch, verified on a build of a47a19c itself. The feature
            // had only ever been exercised from `#[tokio::test]`, where a
            // runtime is already entered, so the relay tests were all green
            // while the shipping binary could not start.
            //
            // Entering the handle rather than switching to
            // `tauri::async_runtime::spawn` keeps `relay` free of any Tauri
            // dependency, which is what lets the integration tests drive the
            // same code with no desktop app around it.
            let runtime = tauri::async_runtime::handle();
            let relay = {
                let _entered = runtime.inner().enter();
                relay::spawn(settings.clone())
            };
            if settings.snapshot().is_paired() {
                tracing::info!("relay: station already paired, connecting");
            } else {
                tracing::info!("relay: no station paired yet — pair from Ajustes → Estación");
            }
            app.manage(relay);

            // Spawn the server on Tauri's runtime so it shuts down cleanly
            // when the app exits.
            //
            // The certificate is resolved at startup instead of being taken
            // straight from the embedded bytes: a renewal can now drop a new
            // PEM pair in %PROGRAMDATA%\GourmelyPrint\certs and restart the
            // bridge, with no rebuild and no new MSI. The embedded pair stays
            // as the safety net — see `tls_material` for why that ordering,
            // and for what happens when the file on disk is corrupt.
            let server_settings = settings;
            tauri::async_runtime::spawn(async move {
                let tls = tls_material::resolve(CERT_PEM, KEY_PEM);
                tracing::info!(source = tls.source, "TLS material resolved");
                if let Err(e) = server::serve(&tls.cert, &tls.key, server_settings).await {
                    tracing::error!("server exited: {e}");
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
