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
//!
//! # What `/health` promises, and what it used to imply
//!
//! `ok` has always meant one thing: *this process is up and can put ink on
//! paper through the local socket*. It answered `true` unconditionally, which
//! was accurate for that question and a lie for the one everybody actually asks
//! it. `apps/platform-web/lib/print-bridge.ts` polls this endpoint to paint the
//! bridge badge and report the version, so a bridge whose **relay** was dead —
//! standing down for a second copy, or unable to open its print ledger — looked
//! perfectly healthy from the POS. That is worse than no signal: with the relay
//! down the station stops sending `station-hello`, the server marks it offline
//! after three minutes and quietly routes the ticket back through whatever
//! browser is open in the shop. The silent failure the relay exists to delete,
//! made permanent, with a green badge over it.
//!
//! So the payload now carries the relay's real state, and the compatibility
//! rule is explicit:
//!
//! * `ok`, `version`, `uptime_seconds` and `printer_count` keep their names,
//!   their types and their meanings. A caller reading only `ok` is a caller
//!   asking "is the bridge installed and running", and that answer has not
//!   changed. `tests/server_health.rs` pins it.
//! * everything about the remote path lives under `relay`, which is new and
//!   additive. `relay.ok` is the honest "can a ticket reach this till from the
//!   server right now"; `relay.state` says which of the six situations it is,
//!   and `relay.detail` is the sentence to show a human.
//!
//! Turning the top-level `ok` false on a relay hiccup was the other option and
//! it is a worse one: a router blip would tell the POS the bridge is missing and
//! stop the local printing that still works perfectly.

use std::{net::SocketAddr, sync::Arc, time::Instant};

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
use sha2::{Digest, Sha256};
use tower_http::cors::{Any, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::{
    config::{bind_port, LocalAuth, SettingsStore, ALLOWED_ORIGINS, MAX_PRINT_BYTES},
    error::{BridgeError, BridgeResult},
    local_job_ledger::{Claim, LocalJobLedger, Outcome},
    printer,
    relay::RelayHandle,
};

/// Process-wide server state. Shared (Arc) across all axum handlers.
pub type PrintRawFn = dyn Fn(&str, &[u8]) -> BridgeResult<u32> + Send + Sync;

#[derive(Clone)]
pub struct ServerState {
    pub started_at: Instant,
    pub version: &'static str,
    pub settings: SettingsStore,
    /// The relay, when this process has one. `None` in the tests that mount the
    /// router on its own — `/health` then reports the relay as `unknown` rather
    /// than inventing a healthy-looking answer.
    pub relay: Option<RelayHandle>,
    /// Available only when durable local intent storage opened successfully.
    pub local_ledger: Option<Arc<LocalJobLedger>>,
    pub print_raw: Arc<PrintRawFn>,
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

/// The `/health` body.
///
/// The first four fields are load-bearing for callers that already exist and
/// must not move; see the module header. `relay` is the addition.
#[derive(Serialize)]
struct HealthResponse {
    /// The local print path: this process is up and the socket is serving.
    /// Never a statement about the relay — see `relay.ok` for that.
    ok: bool,
    version: &'static str,
    uptime_seconds: u64,
    printer_count: usize,
    relay: HealthRelay,
}

/// The relay, as `/health` reports it.
///
/// Hand-mapped from [`RelayStatus`] instead of embedding it, for two reasons:
/// this endpoint is snake_case and the Tauri command is camelCase, and the
/// station's label and error text are the only parts of the relay's status a
/// remote caller has any business seeing. `tests/server_health.rs` pins the
/// names, because a rename here is invisible to the POS — the field just
/// arrives `undefined` and every check on it takes the healthy branch.
#[derive(Serialize)]
struct HealthRelay {
    /// `connected` · `connecting` · `rejected` · `standby` ·
    /// `ledger-unavailable` · `unpaired` · `unknown`.
    state: String,
    /// Can the server route a ticket to this till right now? This is the field
    /// that answers the question the top-level `ok` was being asked.
    ok: bool,
    /// True while nothing will improve without somebody acting on this machine
    /// or in the panel. A plain outage is *not* one of these.
    needs_attention: bool,
    paired: bool,
    connected: bool,
    /// One sentence for a human, already in the operator's language.
    detail: String,
    label: Option<String>,
    /// Set when the print ledger had to be salvaged or set aside: the bridge is
    /// printing, and a recent ticket could come out twice.
    ledger_recovered: Option<String>,
    jobs_printed: u64,
    jobs_failed: u64,
}

impl HealthRelay {
    fn of(relay: Option<&RelayHandle>) -> Self {
        let Some(status) = relay.map(|r| r.status()) else {
            return Self {
                state: "unknown".to_string(),
                ok: false,
                needs_attention: false,
                paired: false,
                connected: false,
                detail: "Este proceso no tiene relay: solo sirve la impresión local.".to_string(),
                label: None,
                ledger_recovered: None,
                jobs_printed: 0,
                jobs_failed: 0,
            };
        };
        Self {
            state: status.state.as_str().to_string(),
            ok: status.state.is_serving(),
            needs_attention: status.state.needs_a_human(),
            paired: status.paired,
            connected: status.connected,
            detail: status.detail(),
            label: status.label.clone(),
            ledger_recovered: status.ledger_recovered.clone(),
            jobs_printed: status.jobs_printed,
            jobs_failed: status.jobs_failed,
        }
    }
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
        #[serde(rename = "printIntentId")]
        print_intent_id: Option<String>,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        capabilities: Option<PrintCapabilities>,
    },
    PrintOk {
        id: String,
        ok: bool,
        #[serde(rename = "jobId")]
        job_id: u32,
        #[serde(rename = "printIntentId", skip_serializing_if = "Option::is_none")]
        print_intent_id: Option<String>,
    },
    Err {
        id: String,
        ok: bool,
        error: String,
    },
}

#[derive(Debug, Serialize)]
struct PrintCapabilities {
    #[serde(rename = "printIntentId")]
    print_intent_id: bool,
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
        relay: HealthRelay::of(s.relay.as_ref()),
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

async fn handle_socket(mut socket: WebSocket, state: ServerState) {
    tracing::info!("ws client connected");
    while let Some(msg) = socket.recv().await {
        let Ok(msg) = msg else {
            tracing::warn!("ws recv error, closing");
            break;
        };
        match msg {
            Message::Text(text) => {
                let response = handle_text(&text, &state).await;
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

async fn handle_text(text: &str, state: &ServerState) -> WsResponse {
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
                capabilities: state.local_ledger.as_ref().map(|_| PrintCapabilities {
                    print_intent_id: true,
                }),
            },
            Err(e) => WsResponse::err(id, e.to_string()),
        },
        WsRequest::Print {
            id,
            printer: name,
            data,
            print_intent_id,
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
            if let Some(intent_id) = print_intent_id {
                return print_with_intent(state, id, name, &bytes, intent_id);
            }
            match (state.print_raw)(&name, &bytes) {
                Ok(job_id) => WsResponse::PrintOk {
                    id,
                    ok: true,
                    job_id,
                    print_intent_id: None,
                },
                Err(e) => WsResponse::err(id, e.to_string()),
            }
        }
        WsRequest::Test { id, printer: name } => {
            let bytes = printer::test_ticket_bytes();
            match (state.print_raw)(&name, &bytes) {
                Ok(job_id) => WsResponse::PrintOk {
                    id,
                    ok: true,
                    job_id,
                    print_intent_id: None,
                },
                Err(e) => WsResponse::err(id, e.to_string()),
            }
        }
    }
}

fn print_with_intent(
    state: &ServerState,
    request_id: String,
    printer_name: String,
    bytes: &[u8],
    intent_id: String,
) -> WsResponse {
    let Some(ledger) = state.local_ledger.as_ref() else {
        return WsResponse::err(request_id, "local print intent ledger unavailable");
    };
    let mut digest = Sha256::new();
    digest.update(b"GourmelyPrint-local-intent-v1\0");
    digest.update((printer_name.len() as u64).to_be_bytes());
    digest.update(printer_name.as_bytes());
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
    let fingerprint = format!("{:x}", digest.finalize());
    let outcome = match ledger.claim(&intent_id, &fingerprint) {
        Ok(Claim::Fresh) => {
            let outcome = match (state.print_raw)(&printer_name, bytes) {
                Ok(job_id) => Outcome {
                    job_id,
                    error: None,
                },
                Err(error) => Outcome {
                    job_id: 0,
                    error: Some(error.to_string()),
                },
            };
            if let Err(error) = ledger.finish(&intent_id, outcome.clone()) {
                return WsResponse::err(request_id, error.to_string());
            }
            outcome
        }
        Ok(Claim::Done(outcome)) => outcome,
        Ok(Claim::Interrupted) => {
            return WsResponse::err(request_id, "print intent outcome is uncertain");
        }
        Ok(Claim::Conflict) => {
            return WsResponse::err(request_id, "print intent ID conflicts with prior payload");
        }
        Err(error) => return WsResponse::err(request_id, error.to_string()),
    };
    match outcome.error {
        Some(error) => WsResponse::err(request_id, error),
        None => WsResponse::PrintOk {
            id: request_id,
            ok: true,
            job_id: outcome.job_id,
            print_intent_id: Some(intent_id),
        },
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
    relay: Option<RelayHandle>,
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
        relay,
        local_ledger: crate::config::data_dir()
            .map(|dir| LocalJobLedger::open(dir.join("local-print-intents.jsonl")))
            .transpose()?
            .map(Arc::new),
        print_raw: Arc::new(printer::print_raw),
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
    let addr: SocketAddr = ([127, 0, 0, 1], bind_port()).into();
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
        relay: None,
        local_ledger: None,
        print_raw: Arc::new(printer::print_raw),
    })
}

/// Router wired to a live relay, for the tests that are about what `/health`
/// says when the remote path is in trouble.
#[allow(dead_code)]
pub fn router_with_relay(relay: RelayHandle) -> Router {
    build_router(ServerState {
        started_at: Instant::now(),
        version: "test",
        settings: SettingsStore::in_memory(crate::config::Settings {
            local_auth: LocalAuth::Off,
            ..Default::default()
        }),
        relay: Some(relay),
        local_ledger: None,
        print_raw: Arc::new(printer::print_raw),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tokio_tungstenite::{
        tungstenite::Message as ClientMessage, MaybeTlsStream, WebSocketStream,
    };

    type ClientSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

    async fn ws_exchange(
        socket: &mut ClientSocket,
        request: serde_json::Value,
    ) -> serde_json::Value {
        socket
            .send(ClientMessage::Text(request.to_string().into()))
            .await
            .expect("send request");
        let response = socket.next().await.expect("response frame").expect("frame");
        serde_json::from_str(&response.into_text().expect("text response")).expect("json response")
    }

    async fn ws_test_server(state: ServerState) -> (tokio::task::JoinHandle<()>, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let url = format!("ws://{}/print", listener.local_addr().expect("address"));
        let server = tokio::spawn(async move {
            axum::serve(listener, build_router(state))
                .await
                .expect("serve test socket");
        });
        (server, url)
    }

    #[tokio::test]
    async fn intent_capability_and_ack_are_durable_without_changing_legacy_prints() {
        let path = std::env::temp_dir()
            .join(format!("bridge-wss-intent-{}", uuid::Uuid::new_v4()))
            .join("local-print-intents.jsonl");
        let printed = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let fake = {
            let printed = Arc::clone(&printed);
            let calls = Arc::clone(&calls);
            Arc::new(move |_printer: &str, bytes: &[u8]| {
                printed.lock().unwrap().push(bytes.to_vec());
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(73)
            })
        };
        let make_state = |ledger| ServerState {
            started_at: Instant::now(),
            version: "test",
            settings: SettingsStore::in_memory(Settings {
                local_auth: LocalAuth::Off,
                ..Default::default()
            }),
            relay: None,
            local_ledger: ledger,
            print_raw: fake.clone(),
        };

        let first_ledger = Arc::new(crate::local_job_ledger::LocalJobLedger::open(&path).unwrap());
        let (server, url) = ws_test_server(make_state(Some(Arc::clone(&first_ledger)))).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("connect first socket");
        let list = ws_exchange(&mut socket, serde_json::json!({"op":"list","id":"list-1"})).await;
        assert_eq!(list["capabilities"]["printIntentId"], true);
        assert_eq!(list["id"], "list-1");
        let data = BASE64.encode(b"\x1b@\xff");
        let first = ws_exchange(
            &mut socket,
            serde_json::json!({
                "op":"print", "id":"attempt-1", "printer":"Fake", "data":data,
                "printIntentId":"intent-stable"
            }),
        )
        .await;
        assert_eq!(first["ok"], true);
        assert_eq!(first["id"], "attempt-1");
        assert_eq!(first["printIntentId"], "intent-stable");
        assert_eq!(first["jobId"], 73);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        socket.close(None).await.unwrap();
        drop(socket);
        server.abort();
        let _ = server.await;
        drop(first_ledger);

        // Axum's upgraded socket outlives this test's aborted listener and
        // briefly retains its ServerState clone. Production opens this ledger
        // once per process in lib.rs; pair/unpair and WS reconnect reuse it.
        let reopened = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match LocalJobLedger::open(&path) {
                    Ok(ledger) => break Arc::new(ledger),
                    Err(BridgeError::AlreadyRunning(_)) => {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    Err(error) => panic!("reopen local ledger: {error}"),
                }
            }
        })
        .await
        .expect("first socket releases local ledger");
        let (server, url) = ws_test_server(make_state(Some(reopened))).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("connect after restart");
        let retry = ws_exchange(
            &mut socket,
            serde_json::json!({
                "op":"print", "id":"attempt-2", "printer":"Fake", "data":data,
                "printIntentId":"intent-stable"
            }),
        )
        .await;
        assert_eq!(retry["id"], "attempt-2");
        assert_eq!(retry["ok"], true);
        assert_eq!(retry["printIntentId"], "intent-stable");
        assert_eq!(retry["jobId"], 73);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let collision = ws_exchange(&mut socket, serde_json::json!({
            "op":"print", "id":"attempt-3", "printer":"Fake", "data":BASE64.encode(b"different"),
            "printIntentId":"intent-stable"
        })).await;
        assert_eq!(collision["ok"], false);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let legacy = ws_exchange(
            &mut socket,
            serde_json::json!({
                "op":"print", "id":"legacy", "printer":"Fake", "data":data
            }),
        )
        .await;
        assert_eq!(legacy["ok"], true);
        assert!(legacy.get("printIntentId").is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(*printed.lock().unwrap(), vec![b"\x1b@\xff".to_vec(); 2]);
        server.abort();

        let (server, url) = ws_test_server(make_state(None)).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(&url)
            .await
            .expect("connect legacy-only socket");
        let list = ws_exchange(
            &mut socket,
            serde_json::json!({"op":"list","id":"legacy-list"}),
        )
        .await;
        assert!(list.get("capabilities").is_none());
        let unsupported = ws_exchange(
            &mut socket,
            serde_json::json!({
                "op":"print", "id":"unsupported", "printer":"Fake", "data":data,
                "printIntentId":"intent-stable"
            }),
        )
        .await;
        assert_eq!(unsupported["ok"], false);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        server.abort();
    }

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
            relay: None,
            local_ledger: None,
            print_raw: Arc::new(printer::print_raw),
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
