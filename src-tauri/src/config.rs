//! Build-time and runtime configuration for the print bridge.
//!
//! The TLS material is NO LONGER hardcoded here: `tls_material::resolve` reads
//! it from `%PROGRAMDATA%\GourmelyPrint\certs` when present and falls back to
//! the pair embedded at compile time, so renewing the certificate no longer
//! needs a rebuild.
//!
//! The origin allowlist below is still compile-time. Moving it to a
//! `config.toml` next to the binary — so support can point one install at a
//! staging origin without a rebuild — is the remaining half of that idea.

/// The HTTPS port the bridge listens on. Matches what
/// `apps/platform-web/lib/print-bridge.ts` connects to.
pub const BIND_PORT: u16 = 8181;

/// The DNS name the cert is issued for. Frontend MUST connect via this
/// name (NOT raw `127.0.0.1`) for the TLS handshake to succeed.
pub const BRIDGE_HOST: &str = "localhost.gourmelyhub.busticco.com";

/// Browser origins that may open a WebSocket to the bridge. Any request
/// whose `Origin` header is not in this list is rejected with HTTP 403
/// before the WebSocket upgrade. Prevents random sites the cashier
/// happens to visit from talking to the printer.
pub const ALLOWED_ORIGINS: &[&str] = &[
    "https://app-gourmelyhub.busticco.com",
    "https://gourmelyhub.busticco.com",
    // The bridge's OWN settings window (Tauri WebView2) fetches /health
    // for the status badge. Its origin is tauri.localhost on Windows /
    // tauri://localhost on macOS — without these the badge showed
    // "Sin conexión" even though the service was up (CORS-blocked the
    // self-fetch). 2026-06-09 fix.
    "http://tauri.localhost",
    "https://tauri.localhost",
    "tauri://localhost",
    // Dev origins — keep these only while we're testing locally. Strip
    // before release builds.
    "http://localhost:3000",
    "http://localhost:1420",
];

/// Maximum size of a single print payload in bytes (after base64 decode).
/// A receipt is < 10 KB; we cap at 1 MB to keep one bad actor from
/// hogging RAM by streaming MB of binary into the WSS.
pub const MAX_PRINT_BYTES: usize = 1_024 * 1_024;
