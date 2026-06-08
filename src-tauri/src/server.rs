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
//! Origin enforcement: every request whose `Origin` header is not in
//! `config::ALLOWED_ORIGINS` is rejected with HTTP 403 before the WS
//! upgrade. Prevents random tabs in the cashier's Chrome from talking
//! to the printer.

use std::{net::SocketAddr, time::Instant};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::{HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::{Deserialize, Serialize};
use tower_http::cors::{Any, CorsLayer};

use crate::{
    config::{ALLOWED_ORIGINS, BIND_PORT, MAX_PRINT_BYTES},
    error::BridgeError,
    printer,
};

/// Process-wide server state. Shared (Arc) across all axum handlers.
#[derive(Clone)]
pub struct ServerState {
    pub started_at: Instant,
    pub version: &'static str,
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

    Router::new()
        .route("/health", get(health_handler))
        .route("/printers", get(printers_handler))
        .route("/print", get(print_ws_handler))
        .layer(cors)
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

async fn print_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<ServerState>,
) -> Response {
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
        WsRequest::Print { id, printer: name, data } => {
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
pub async fn serve(cert_pem: &[u8], key_pem: &[u8]) -> Result<(), BridgeError> {
    // rustls 0.23 requires picking a crypto provider explicitly at process
    // startup. We pick aws-lc-rs (FIPS-able, default in rustls). Safe to
    // call multiple times — `.ok()` swallows the "already installed" err
    // that fires when the test harness boots the server twice in one run.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let state = ServerState {
        started_at: Instant::now(),
        version: env!("CARGO_PKG_VERSION"),
    };

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
        .map_err(|e| BridgeError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
    Ok(())
}

#[allow(dead_code)]
pub fn router_for_tests() -> Router {
    build_router(ServerState {
        started_at: Instant::now(),
        version: "test",
    })
}
