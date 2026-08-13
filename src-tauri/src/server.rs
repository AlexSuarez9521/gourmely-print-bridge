//! HTTPS + WSS server.
//!
//! Listens on `wss://localhost.gourmelyhub.busticco.com:8181`. The cert
//! is a real Let's Encrypt cert (DNS-01 challenge over a public DNS
//! record that points to `127.0.0.1`), so Chrome accepts it without
//! popups or self-signed warnings.
//!
//! Routes:
//!   - `GET  /health`    → bridge metadata for the POS UI status badge
//!   - `GET  /printers`  → list of OS printer names
//!   - `WS   /print`     → upgrade to WebSocket; expects JSON messages
//!
//! # Who is allowed to print through this socket
//!
//! The module used to claim that "every request whose `Origin` header is not in
//! `config::ALLOWED_ORIGINS` is rejected with HTTP 403 before the WS upgrade".
//! **It was not.** `CorsLayer` decorates responses; it does not reject them,
//! and a browser does not apply CORS to a WebSocket handshake in the first
//! place. So the allowlist was documentation, the socket had no authentication
//! of any kind, and any page open on the till could print whatever it liked.
//!
//! [`guard`] is that check, for real:
//!
//! * the `Origin` is now enforced in the handler, with an actual 403. This one
//!   costs the POS nothing — its origin is already on the list — and a browser
//!   cannot forge the header, which is precisely why it closes the hole.
//! * a shared secret, presented as `?token=`, `Authorization: Bearer …` or the
//!   `x-gourmelyprint-token` header, covers the callers that are not browsers
//!   and can therefore say whatever they want in `Origin`.
//!
//! [`crate::config::LocalAuth`] decides how strictly the two combine. The
//! default, `origin-or-secret`, is the strongest setting that does not require
//! the web to ship first; `secret` is the end state.

use std::{net::SocketAddr, time::Instant};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::{Deserialize, Serialize};
use tower_http::cors::{Any, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::{
    config::{LocalAuth, SettingsStore, ALLOWED_ORIGINS, BIND_PORT, MAX_PRINT_BYTES},
    error::BridgeError,
    printer,
};

/// Process-wide server state. Shared (Arc) across all axum handlers.
#[derive(Clone)]
pub struct ServerState {
    pub started_at: Instant,
    pub version: &'static str,
    pub settings: SettingsStore,
}

/// `?token=…` on the upgrade URL.
///
/// A browser `WebSocket` constructor cannot set headers, so the query string is
/// the only place the web can put the secret when that side ships. The other
/// two forms exist for native callers, which can.
#[derive(Debug, Default, Deserialize)]
pub struct AuthQuery {
    pub token: Option<String>,
}

/// Constant-time comparison. A local secret is not worth a timing attack over
/// loopback, but getting this wrong by habit elsewhere is, so it is written the
/// way credentials are always written.
fn secret_matches(expected: &str, presented: &str) -> bool {
    if expected.len() != presented.len() {
        return false;
    }
    expected
        .bytes()
        .zip(presented.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn origin_allowed(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(|origin| ALLOWED_ORIGINS.contains(&origin))
        .unwrap_or(false)
}

fn presented_secret(headers: &HeaderMap, query: &AuthQuery) -> Option<String> {
    if let Some(token) = query.token.as_deref().filter(|t| !t.is_empty()) {
        return Some(token.to_string());
    }
    if let Some(value) = headers
        .get("x-gourmelyprint-token")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
    {
        return Some(value.to_string());
    }
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// Decides whether this caller may drive the printer. `Ok(())` or the 403 the
/// caller gets, with a message that says which of the two doors it missed.
pub fn guard(state: &ServerState, headers: &HeaderMap, query: &AuthQuery) -> Result<(), String> {
    let settings = state.settings.snapshot();
    let mode = settings.local_auth;
    if mode == LocalAuth::Off {
        return Ok(());
    }

    let secret_ok = match (
        settings.local_secret.as_deref(),
        presented_secret(headers, query),
    ) {
        (Some(expected), Some(presented)) => secret_matches(expected, &presented),
        _ => false,
    };
    if secret_ok {
        return Ok(());
    }

    match mode {
        LocalAuth::Off => Ok(()),
        LocalAuth::Secret => {
            Err("esta impresión necesita el token local del bridge (localAuth=secret)".to_string())
        }
        LocalAuth::OriginOrSecret => {
            if origin_allowed(headers) {
                Ok(())
            } else {
                Err(
                    "origen no autorizado y sin token local: esta página no puede imprimir"
                        .to_string(),
                )
            }
        }
    }
}

#[derive(Serialize)]
struct HealthResponse {
    ok: bool,
    version: &'static str,
    uptime_seconds: u64,
    printer_count: usize,
}

#[derive(Serialize)]
struct PrintersResponse {
    ok: bool,
    printers: Vec<String>,
}

/// JSON message vocabulary on the WebSocket. Kept narrow on purpose;
/// extensions go through new ops, never overloaded fields.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum WsRequest {
    /// List all printers known to the OS.
    List { id: String },
    /// Print raw bytes to the named printer. `data` is base64-encoded
    /// so the wire stays JSON-friendly without forcing the frontend to
    /// build binary frames.
    Print {
        id: String,
        printer: String,
        data: String,
    },
    /// Print a small test ticket — same as the tray "Test print" button
    /// but driven from the web UI.
    Test { id: String, printer: String },
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum WsResponse {
    PrintersOk {
        id: String,
        ok: bool,
        printers: Vec<String>,
    },
    PrintOk {
        id: String,
        ok: bool,
        #[serde(rename = "jobId")]
        job_id: u32,
    },
    Err {
        id: String,
        ok: bool,
        error: String,
    },
}

impl WsResponse {
    fn err(id: impl Into<String>, msg: impl Into<String>) -> Self {
        Self::Err {
            id: id.into(),
            ok: false,
            error: msg.into(),
        }
    }
}

/// Build the axum router. Kept as a free fn so tests can mount it
/// without standing up TLS.
fn build_router(state: ServerState) -> Router {
    // CORS: only the origins we know POS lives at. We DON'T use `Any`
    // here because that would let any page on the cashier's browser
    // open a WS to us — defeats the whole guard.
    let allowed: Vec<HeaderValue> = ALLOWED_ORIGINS
        .iter()
        .filter_map(|o| HeaderValue::from_str(o).ok())
        .collect();

    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST])
        .allow_origin(allowed)
        .allow_headers(Any);

    // Private Network Access: when a public origin (the POS at
    // app-gourmelyhub.busticco.com) fetches a loopback address, Chrome
    // sends a PNA preflight and blocks the request unless the response
    // carries `Access-Control-Allow-Private-Network: true`. Without this
    // every customer would have to disable chrome://flags by hand —
    // breaking the one-click install promise. We set it on every
    // response (cheap, harmless on non-preflight requests). 2026-06-09.
    let pna = SetResponseHeaderLayer::overriding(
        HeaderName::from_static("access-control-allow-private-network"),
        HeaderValue::from_static("true"),
    );

    Router::new()
        .route("/health", get(health_handler))
        .route("/printers", get(printers_handler))
        .route("/print", get(print_ws_handler))
        .layer(cors)
        .layer(pna)
        .with_state(state)
}

async fn health_handler(State(s): State<ServerState>) -> Json<HealthResponse> {
    let printer_count = printer::list().map(|v| v.len()).unwrap_or(0);
    Json(HealthResponse {
        ok: true,
        version: s.version,
        uptime_seconds: s.started_at.elapsed().as_secs(),
        printer_count,
    })
}

async fn printers_handler() -> Response {
    match printer::list() {
        Ok(printers) => Json(PrintersResponse { ok: true, printers }).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Note the argument order: the guard's inputs are extracted **before**
/// `WebSocketUpgrade`. Axum runs extractors in declaration order, so with the
/// upgrade first a caller that is not allowed here but also did not send a
/// well-formed handshake got a 400 from the extractor and the guard never ran.
/// The answer to "may you print?" should not depend on how well the request was
/// formed, and a 403 is also the honest thing to log.
async fn print_ws_handler(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Query(query): Query<AuthQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(reason) = guard(&state, &headers, &query) {
        let origin = headers
            .get(axum::http::header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("(sin origen)");
        tracing::warn!(origin, reason, "rejected a print socket");
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "ok": false, "error": reason })),
        )
            .into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, _state: ServerState) {
    tracing::info!("ws client connected");
    while let Some(msg) = socket.recv().await {
        let Ok(msg) = msg else {
            tracing::warn!("ws recv error, closing");
            break;
        };
        match msg {
            Message::Text(text) => {
                let response = handle_text(&text).await;
                let body = serde_json::to_string(&response)
                    .unwrap_or_else(|_| r#"{"ok":false,"error":"serialize"}"#.into());
                if socket.send(Message::Text(body)).await.is_err() {
                    break;
                }
            }
            Message::Binary(_) => {
                let body = serde_json::to_string(&WsResponse::err(
                    "",
                    "binary frames not supported; send JSON text",
                ))
                .unwrap();
                let _ = socket.send(Message::Text(body)).await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    tracing::info!("ws client disconnected");
}

async fn handle_text(text: &str) -> WsResponse {
    let req: WsRequest = match serde_json::from_str(text) {
        Ok(r) => r,
        Err(e) => return WsResponse::err("", format!("invalid json: {e}")),
    };

    match req {
        WsRequest::List { id } => match printer::list() {
            Ok(printers) => WsResponse::PrintersOk {
                id,
                ok: true,
                printers,
            },
            Err(e) => WsResponse::err(id, e.to_string()),
        },
        WsRequest::Print {
            id,
            printer: name,
            data,
        } => {
            // Base64 decode + length guard. We don't trust the wire.
            let bytes = match BASE64.decode(data.as_bytes()) {
                Ok(b) => b,
                Err(e) => return WsResponse::err(id, format!("bad base64: {e}")),
            };
            if bytes.len() > MAX_PRINT_BYTES {
                return WsResponse::err(
                    id,
                    format!("payload {} > max {}", bytes.len(), MAX_PRINT_BYTES),
                );
            }
            match printer::print_raw(&name, &bytes) {
                Ok(job_id) => WsResponse::PrintOk {
                    id,
                    ok: true,
                    job_id,
                },
                Err(e) => WsResponse::err(id, e.to_string()),
            }
        }
        WsRequest::Test { id, printer: name } => {
            let bytes = printer::test_ticket_bytes();
            match printer::print_raw(&name, &bytes) {
                Ok(job_id) => WsResponse::PrintOk {
                    id,
                    ok: true,
                    job_id,
                },
                Err(e) => WsResponse::err(id, e.to_string()),
            }
        }
    }
}

/// Boot the TLS-terminated axum server. Blocks the current task forever
/// (or until the runtime is dropped). Caller should spawn this in a
/// dedicated tokio task so the Tauri event loop stays responsive.
///
/// `cert_pem` and `key_pem` are passed as byte slices (not paths) so the
/// production build can embed the cert via `include_bytes!`. That means
/// the installer drops a single self-contained `.exe` — no loose `.pem`
/// files in `C:\Program Files` and no permission errors when the
/// installer can't reach `%PROGRAMFILES%`.
pub async fn serve(
    cert_pem: &[u8],
    key_pem: &[u8],
    settings: SettingsStore,
) -> Result<(), BridgeError> {
    // rustls 0.23 requires picking a crypto provider explicitly at process
    // startup. We pick aws-lc-rs (FIPS-able, default in rustls). Safe to
    // call multiple times — `.ok()` swallows the "already installed" err
    // that fires when the test harness boots the server twice in one run.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let state = ServerState {
        started_at: Instant::now(),
        version: env!("CARGO_PKG_VERSION"),
        settings,
    };
    tracing::info!(
        mode = ?state.settings.snapshot().local_auth,
        "local print socket authentication"
    );

    let tls = axum_server::tls_rustls::RustlsConfig::from_pem(cert_pem.to_vec(), key_pem.to_vec())
        .await
        .map_err(|e| BridgeError::Tls(format!("load cert/key: {e}")))?;

    // Bind to all interfaces by default so the WSS works both via the
    // DNS name (which resolves to 127.0.0.1) and via `localhost`. Note
    // that the Origin allowlist is still enforced — opening the port
    // alone doesn't let arbitrary remote clients in.
    let addr: SocketAddr = ([127, 0, 0, 1], BIND_PORT).into();
    tracing::info!("listening on https://{}", addr);

    axum_server::bind_rustls(addr, tls)
        .serve(build_router(state).into_make_service())
        .await
        .map_err(|e| BridgeError::Io(std::io::Error::other(e)))?;
    Ok(())
}

/// Router with the local guard turned OFF, for the tests that are about the
/// routes rather than about who may reach them.
#[allow(dead_code)]
pub fn router_for_tests() -> Router {
    router_with_settings(crate::config::Settings {
        local_auth: LocalAuth::Off,
        ..Default::default()
    })
}

/// Router wired to a specific settings snapshot, so a test can pin the auth
/// mode and the secret without touching `%PROGRAMDATA%`.
#[allow(dead_code)]
pub fn router_with_settings(settings: crate::config::Settings) -> Router {
    build_router(ServerState {
        started_at: Instant::now(),
        version: "test",
        settings: SettingsStore::in_memory(settings),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;

    fn headers(origin: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(o) = origin {
            h.insert(
                axum::http::header::ORIGIN,
                HeaderValue::from_str(o).expect("header"),
            );
        }
        h
    }

    fn state(local_auth: LocalAuth) -> ServerState {
        ServerState {
            started_at: Instant::now(),
            version: "test",
            settings: SettingsStore::in_memory(Settings {
                local_auth,
                local_secret: Some("s3cr3t".into()),
                ..Default::default()
            }),
        }
    }

    /// The hole this change closes. Before it, this call succeeded.
    #[test]
    fn a_random_page_on_the_till_cannot_print() {
        let denied = guard(
            &state(LocalAuth::OriginOrSecret),
            &headers(Some("https://cupones-gratis.example")),
            &AuthQuery::default(),
        );
        assert!(denied.is_err());
    }

    /// The compatibility guarantee: the POS as it ships today, which sends no
    /// token at all, must keep printing.
    #[test]
    fn the_pos_keeps_printing_without_changing_a_line() {
        for origin in ALLOWED_ORIGINS {
            assert!(
                guard(
                    &state(LocalAuth::OriginOrSecret),
                    &headers(Some(origin)),
                    &AuthQuery::default()
                )
                .is_ok(),
                "{origin} should be allowed"
            );
        }
    }

    #[test]
    fn a_caller_with_no_origin_needs_the_secret() {
        // curl, a script, anything native: it can lie about Origin, so the
        // origin door is not open to it.
        assert!(guard(
            &state(LocalAuth::OriginOrSecret),
            &headers(None),
            &AuthQuery::default()
        )
        .is_err());
        assert!(guard(
            &state(LocalAuth::OriginOrSecret),
            &headers(None),
            &AuthQuery {
                token: Some("s3cr3t".into())
            }
        )
        .is_ok());
    }

    #[test]
    fn secret_mode_ignores_the_origin() {
        // The end state, once the web presents the token.
        assert!(guard(
            &state(LocalAuth::Secret),
            &headers(Some(ALLOWED_ORIGINS[0])),
            &AuthQuery::default()
        )
        .is_err());
        assert!(guard(
            &state(LocalAuth::Secret),
            &headers(Some(ALLOWED_ORIGINS[0])),
            &AuthQuery {
                token: Some("s3cr3t".into())
            }
        )
        .is_ok());
    }

    #[test]
    fn off_is_the_old_behaviour_and_lets_everything_through() {
        assert!(guard(
            &state(LocalAuth::Off),
            &headers(Some("https://cualquier-cosa.example")),
            &AuthQuery::default()
        )
        .is_ok());
    }

    #[test]
    fn the_secret_travels_in_three_shapes() {
        let expected = AuthQuery {
            token: Some("s3cr3t".into()),
        };
        assert!(guard(&state(LocalAuth::Secret), &headers(None), &expected).is_ok());

        let mut bearer = HeaderMap::new();
        bearer.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer s3cr3t"),
        );
        assert!(guard(&state(LocalAuth::Secret), &bearer, &AuthQuery::default()).is_ok());

        let mut custom = HeaderMap::new();
        custom.insert("x-gourmelyprint-token", HeaderValue::from_static("s3cr3t"));
        assert!(guard(&state(LocalAuth::Secret), &custom, &AuthQuery::default()).is_ok());
    }

    #[test]
    fn a_wrong_secret_is_a_wrong_secret() {
        assert!(guard(
            &state(LocalAuth::Secret),
            &headers(None),
            &AuthQuery {
                token: Some("s3cr3".into())
            }
        )
        .is_err());
        assert!(guard(
            &state(LocalAuth::Secret),
            &headers(None),
            &AuthQuery {
                token: Some("s3cr3tt".into())
            }
        )
        .is_err());
    }
}
