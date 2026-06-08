/// Build-time and runtime configuration for the print bridge.
///
/// V1 ships the cert paths and allowed-origins hardcoded. V1.5 will move
/// these to a `config.toml` next to the binary so support can debug a
/// stuck client install without rebuilding.

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
    // Dev origins — keep these only while we're testing locally. Strip
    // before release builds.
    "http://localhost:3000",
    "http://localhost:1420",
];

/// Maximum size of a single print payload in bytes (after base64 decode).
/// A receipt is < 10 KB; we cap at 1 MB to keep one bad actor from
/// hogging RAM by streaming MB of binary into the WSS.
pub const MAX_PRINT_BYTES: usize = 1_024 * 1_024;
