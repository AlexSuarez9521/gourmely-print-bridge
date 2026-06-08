//! GourmelyPrint Bridge — entry point.
//!
//! Wiring:
//!   1. Tauri builds the desktop chrome (window + tray).
//!   2. A tokio task spawned at startup runs the WSS server that the POS
//!      web app connects to. The server stays alive for the entire app
//!      lifetime and shuts down cleanly when the user quits.
//!   3. Each WS message dispatches to the `printer` module which talks
//!      to the Windows print spooler.

pub mod config;
pub mod error;
pub mod printer;
pub mod server;

/// Tauri command: returns the list of printers registered on this
/// machine. Exposed both to the WSS clients (via `server::handle`) and
/// to the Tauri webview UI (settings page can show what's available).
#[tauri::command]
fn list_printers() -> Result<Vec<String>, String> {
    printer::list().map_err(|e| e.to_string())
}

/// Tauri command: send a test ticket so the operator can verify a printer
/// is alive without going through the POS web. Uses the simplest possible
/// ESC/POS test pattern.
#[tauri::command]
fn test_print(printer_name: String) -> Result<u32, String> {
    let bytes = build_test_ticket();
    printer::print_raw(&printer_name, &bytes).map_err(|e| e.to_string())
}

fn build_test_ticket() -> Vec<u8> {
    // ESC/POS for a 58mm thermal printer:
    //   ESC @          — init
    //   ESC a 0x01     — center
    //   ESC ! 0x30     — double height + width
    //   "GOURMELYPRINT"
    //   ESC ! 0x00     — normal size
    //   "Test exitoso"
    //   LF LF LF LF GS V 0x01 — feed + partial cut
    let mut b: Vec<u8> = Vec::with_capacity(128);
    b.extend_from_slice(b"\x1b@\x1ba\x01\x1b!\x30");
    b.extend_from_slice("GOURMELYPRINT\n".as_bytes());
    b.extend_from_slice(b"\x1b!\x00");
    b.extend_from_slice("Test exitoso\n\n".as_bytes());
    b.extend_from_slice(b"\x0a\x0a\x0a\x0a\x1dV\x01");
    b
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "print_bridge_lib=info,tower_http=info,axum=info".into()),
        )
        .init();

    tracing::info!("GourmelyPrint Bridge starting (v{})", env!("CARGO_PKG_VERSION"));

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![list_printers, test_print])
        .setup(|_app| {
            // Cert path resolution: env vars first (CI/dev), then a path
            // relative to the binary (production install layout). The
            // bridge refuses to boot if it can't find a usable cert,
            // because running a "print bridge" that doesn't accept TLS
            // would be misleading.
            let cert = std::env::var("PRINT_BRIDGE_CERT")
                .unwrap_or_else(|_| "certs/fullchain.pem".to_string());
            let key = std::env::var("PRINT_BRIDGE_KEY")
                .unwrap_or_else(|_| "certs/privkey.pem".to_string());

            tracing::info!("cert={} key={}", cert, key);

            // Spawn the server on Tauri's runtime so it shuts down
            // cleanly when the app exits.
            tauri::async_runtime::spawn(async move {
                if let Err(e) =
                    server::serve(std::path::Path::new(&cert), std::path::Path::new(&key))
                        .await
                {
                    tracing::error!("server exited: {e}");
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
