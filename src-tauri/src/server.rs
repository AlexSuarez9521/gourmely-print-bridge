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

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

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
use tokio::sync::Semaphore;
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
const MAX_ACTIVE_LOCAL_PRINTS: usize = 4;

#[derive(Clone)]
pub struct ServerState {
    pub started_at: Instant,
    pub version: &'static str,
    pub settings: SettingsStore,
    /// The relay, when this process has one. `None` in the tests that mount the
    /// router on its own — `/health` then reports the relay as `unknown` rather
    /// than inventing a healthy-looking answer.
    pub relay: Option<RelayHandle>,
    /// Advertised only while durable local intent storage is writable.
    local_ledger: Arc<LocalLedgerManager>,
    pub print_raw: Arc<PrintRawFn>,
    pub print_slots: Arc<Semaphore>,
}

struct LocalLedgerManager {
    path: Option<PathBuf>,
    ledger: RwLock<Option<Arc<LocalJobLedger>>>,
}

impl LocalLedgerManager {
    fn from_path(path: Option<PathBuf>) -> Self {
        let ledger = open_local_ledger(path.clone());
        Self {
            path,
            ledger: RwLock::new(ledger),
        }
    }

    fn from_ledger(ledger: Option<Arc<LocalJobLedger>>) -> Self {
        Self {
            path: None,
            ledger: RwLock::new(ledger),
        }
    }

    fn available(&self) -> Option<Arc<LocalJobLedger>> {
        self.ledger
            .read()
            .ok()?
            .as_ref()
            .filter(|ledger| ledger.is_available())
            .cloned()
    }

    fn needs_retry(&self) -> bool {
        self.available().is_none()
            && (self.path.is_some() || self.ledger.read().is_ok_and(|ledger| ledger.is_some()))
    }

    fn retry(&self) -> BridgeResult<()> {
        let current = self
            .ledger
            .read()
            .map_err(|_| BridgeError::Config("local ledger manager lock is poisoned".into()))?
            .clone();
        if let Some(ledger) = current {
            return ledger.recover();
        }
        let path = self.path.as_ref().ok_or_else(|| {
            BridgeError::Config("local print intent data directory is unavailable".into())
        })?;
        let reopened = Arc::new(LocalJobLedger::open(path)?);
        *self
            .ledger
            .write()
            .map_err(|_| BridgeError::Config("local ledger manager lock is poisoned".into()))? =
            Some(reopened);
        Ok(())
    }
}

async fn local_ledger_recovery_loop(manager: Arc<LocalLedgerManager>, interval: Duration) {
    let mut ticks = tokio::time::interval(interval);
    loop {
        ticks.tick().await;
        if !manager.needs_retry() {
            continue;
        }
        let retrying = Arc::clone(&manager);
        match tokio::task::spawn_blocking(move || retrying.retry()).await {
            Ok(Ok(())) => tracing::info!("durable local print intents recovered"),
            Ok(Err(error)) => {
                tracing::debug!(%error, "durable local print intents remain unavailable")
            }
            Err(error) => tracing::error!(%error, "local intent recovery task failed"),
        }
    }
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
    /// A failed Windows credential migration never silently looks healthy.
    configuration_security: Option<String>,
    local_print_intents: HealthLocalPrintIntents,
}

#[derive(Serialize)]
struct HealthLocalPrintIntents {
    ok: bool,
    needs_attention: bool,
    detail: &'static str,
}

impl HealthLocalPrintIntents {
    fn of(manager: &LocalLedgerManager) -> Self {
        if manager.available().is_some() {
            Self {
                ok: true,
                needs_attention: false,
                detail: "La impresión local con ID está disponible.",
            }
        } else {
            Self {
                ok: false,
                needs_attention: true,
                detail: "Impresión segura pausada: revise el espacio del directorio de datos. Si fallaron los permisos, cierre Bridge y ejecute 'print-bridge repair-acl --account <usuario-de-caja>' como administrador con la sesión de caja activa; si persiste, contacte soporte. No borre el historial de impresiones.",
            }
        }
    }
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
    /// `endpoint-insecure` · `ledger-unavailable` · `unpaired` · `unknown`.
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
        Self::from_status(status)
    }

    fn from_status(status: crate::relay::RelayStatus) -> Self {
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
        configuration_security: s.settings.security_issue().map(str::to_owned),
        local_print_intents: HealthLocalPrintIntents::of(&s.local_ledger),
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
                capabilities: state.local_ledger.available().map(|_| PrintCapabilities {
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
            run_print(state, id, name, bytes, print_intent_id).await
        }
        WsRequest::Test { id, printer: name } => {
            let bytes = printer::test_ticket_bytes();
            run_print(state, id, name, bytes, None).await
        }
    }
}

async fn run_print(
    state: &ServerState,
    id: String,
    printer_name: String,
    bytes: Vec<u8>,
    intent_id: Option<String>,
) -> WsResponse {
    let permit = match Arc::clone(&state.print_slots).acquire_owned().await {
        Ok(permit) => permit,
        Err(error) => return WsResponse::err(id, format!("local print pool closed: {error}")),
    };
    let state = state.clone();
    let fallback_id = id.clone();
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if let Some(intent_id) = intent_id {
            return print_with_intent(&state, id, printer_name, &bytes, intent_id);
        }
        match (state.print_raw)(&printer_name, &bytes) {
            Ok(job_id) => WsResponse::PrintOk {
                id,
                ok: true,
                job_id,
                print_intent_id: None,
            },
            Err(error) => WsResponse::err(id, error.to_string()),
        }
    })
    .await
    {
        Ok(response) => response,
        Err(error) => WsResponse::err(fallback_id, format!("local print task failed: {error}")),
    }
}

fn print_with_intent(
    state: &ServerState,
    request_id: String,
    printer_name: String,
    bytes: &[u8],
    intent_id: String,
) -> WsResponse {
    let Some(ledger) = state.local_ledger.available() else {
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

fn open_local_ledger(path: Option<PathBuf>) -> Option<Arc<LocalJobLedger>> {
    let Some(path) = path else {
        tracing::warn!("durable local print intents disabled: no data directory");
        return None;
    };
    match LocalJobLedger::open(path) {
        Ok(ledger) => Some(Arc::new(ledger)),
        Err(error) => {
            tracing::error!(%error, "durable local print intents disabled; legacy printing remains available");
            None
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
        local_ledger: Arc::new(LocalLedgerManager::from_path(
            crate::config::data_dir().map(|dir| dir.join("local-print-intents.jsonl")),
        )),
        print_raw: Arc::new(printer::print_raw),
        print_slots: Arc::new(Semaphore::new(MAX_ACTIVE_LOCAL_PRINTS)),
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

    let recovery = Arc::clone(&state.local_ledger);
    tokio::select! {
        result = axum_server::bind_rustls(addr, tls).serve(build_router(state).into_make_service()) => {
            result.map_err(|e| BridgeError::Io(std::io::Error::other(e)))?;
        }
        _ = local_ledger_recovery_loop(recovery, Duration::from_secs(5)) => unreachable!(),
    }
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
        local_ledger: Arc::new(LocalLedgerManager::from_ledger(None)),
        print_raw: Arc::new(printer::print_raw),
        print_slots: Arc::new(Semaphore::new(MAX_ACTIVE_LOCAL_PRINTS)),
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
        local_ledger: Arc::new(LocalLedgerManager::from_ledger(None)),
        print_raw: Arc::new(printer::print_raw),
        print_slots: Arc::new(Semaphore::new(MAX_ACTIVE_LOCAL_PRINTS)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use crate::relay::{RelayState, RelayStatus};
    use futures_util::{SinkExt, StreamExt};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tokio_tungstenite::{
        tungstenite::Message as ClientMessage, MaybeTlsStream, WebSocketStream,
    };

    type ClientSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

    #[tokio::test]
    async fn health_explains_acl_repair_without_exposing_credentials() {
        let state = ServerState {
            started_at: Instant::now(),
            version: "test",
            settings: SettingsStore::in_memory(Settings {
                station_token: Some("private-token".into()),
                ..Default::default()
            })
            .with_security_issue("Run the bridge once as administrator, then restart normally."),
            relay: None,
            local_ledger: Arc::new(LocalLedgerManager::from_ledger(None)),
            print_raw: Arc::new(printer::print_raw),
            print_slots: Arc::new(Semaphore::new(MAX_ACTIVE_LOCAL_PRINTS)),
        };
        let body = serde_json::to_value(health_handler(State(state)).await.0).unwrap();
        assert_eq!(body["ok"], true);
        assert!(body["configuration_security"]
            .as_str()
            .unwrap()
            .contains("administrator"));
        assert!(!body.to_string().contains("private-token"));
    }

    #[test]
    fn health_explains_a_saved_plaintext_endpoint_requires_repairing() {
        let status = RelayStatus {
            state: RelayState::EndpointInsecure,
            paired: true,
            last_error: Some(
                "Dirección del servidor insegura: usa HTTPS; vuelve a vincular la estación.".into(),
            ),
            ..Default::default()
        };
        let body = serde_json::to_value(HealthRelay::from_status(status)).expect("health JSON");
        assert_eq!(body["state"], "endpoint-insecure");
        assert_eq!(body["ok"], false);
        assert_eq!(body["needs_attention"], true);
        assert!(body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("HTTPS"));
        assert!(body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("vincular"));
    }

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
            let recovery = Arc::clone(&state.local_ledger);
            tokio::select! {
                result = axum::serve(listener, build_router(state)) => result.expect("serve test socket"),
                _ = local_ledger_recovery_loop(recovery, Duration::from_millis(25)) => unreachable!(),
            }
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
            local_ledger: Arc::new(LocalLedgerManager::from_ledger(ledger)),
            print_raw: fake.clone(),
            print_slots: Arc::new(Semaphore::new(MAX_ACTIVE_LOCAL_PRINTS)),
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

        // A directory at the ledger path simulates failed startup storage.
        let unavailable = open_local_ledger(Some(path.parent().unwrap().to_path_buf()));
        assert!(unavailable.is_none());
        let (server, url) = ws_test_server(make_state(unavailable)).await;
        let health_url = url.replace("ws://", "http://").replace("/print", "/health");
        let health: serde_json::Value = reqwest::get(health_url)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health["ok"], true);
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
        let legacy = ws_exchange(
            &mut socket,
            serde_json::json!({
                "op":"print", "id":"legacy-after-ledger-failure", "printer":"Fake", "data":data
            }),
        )
        .await;
        assert_eq!(legacy["ok"], true);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        server.abort();
    }

    #[tokio::test]
    async fn repaired_storage_restores_intent_capability_on_the_same_socket() {
        let path = std::env::temp_dir()
            .join(format!("bridge-live-recovery-{}", uuid::Uuid::new_v4()))
            .join("local-print-intents.jsonl");
        std::fs::create_dir_all(&path).unwrap(); // A directory cannot be opened as a ledger file.
        let calls = Arc::new(AtomicUsize::new(0));
        let fake = {
            let calls = Arc::clone(&calls);
            Arc::new(move |_printer: &str, _bytes: &[u8]| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(84)
            })
        };
        let state = ServerState {
            started_at: Instant::now(),
            version: "test",
            settings: SettingsStore::in_memory(Settings {
                local_auth: LocalAuth::Off,
                ..Default::default()
            }),
            relay: None,
            local_ledger: Arc::new(LocalLedgerManager::from_path(Some(path.clone()))),
            print_raw: fake,
            print_slots: Arc::new(Semaphore::new(MAX_ACTIVE_LOCAL_PRINTS)),
        };
        let (server, url) = ws_test_server(state).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let data = BASE64.encode(b"ticket");
        let before = ws_exchange(&mut socket, serde_json::json!({"op":"list","id":"before"})).await;
        assert!(before.get("capabilities").is_none());
        let health_url = url.replace("ws://", "http://").replace("/print", "/health");
        let health: serde_json::Value = reqwest::get(&health_url)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health["ok"], true);
        assert_eq!(health["local_print_intents"]["ok"], false);
        assert!(health["local_print_intents"]["detail"]
            .as_str()
            .unwrap()
            .contains("administrador"));
        assert!(health["local_print_intents"]["detail"]
            .as_str()
            .unwrap()
            .contains("repair-acl --account"));
        let unsupported = ws_exchange(
            &mut socket,
            serde_json::json!({
                "op":"print","id":"unsupported","printer":"Fake","data":data,
                "printIntentId":"stable"
            }),
        )
        .await;
        assert_eq!(unsupported["ok"], false);
        let legacy = ws_exchange(
            &mut socket,
            serde_json::json!({
                "op":"print","id":"legacy","printer":"Fake","data":data
            }),
        )
        .await;
        assert_eq!(legacy["ok"], true);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        std::fs::remove_dir(&path).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let list = ws_exchange(
                    &mut socket,
                    serde_json::json!({
                        "op":"list","id":"after"
                    }),
                )
                .await;
                if list["capabilities"]["printIntentId"] == true {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("same-process ledger recovery");
        let health: serde_json::Value = reqwest::get(health_url)
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(health["local_print_intents"]["ok"], true);
        for attempt in ["first", "retry"] {
            let response = ws_exchange(
                &mut socket,
                serde_json::json!({
                    "op":"print","id":attempt,"printer":"Fake","data":data,
                    "printIntentId":"stable"
                }),
            )
            .await;
            assert_eq!(response["ok"], true);
            assert_eq!(response["printIntentId"], "stable");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn live_ledger_write_failure_withdraws_capability_without_blocking_legacy_prints() {
        let path = std::env::temp_dir()
            .join(format!(
                "bridge-live-ledger-failure-{}",
                uuid::Uuid::new_v4()
            ))
            .join("local-print-intents.jsonl");
        let ledger = Arc::new(LocalJobLedger::open(path).unwrap());
        let manager = Arc::new(LocalLedgerManager::from_ledger(Some(Arc::clone(&ledger))));
        let calls = Arc::new(AtomicUsize::new(0));
        let fake = {
            let calls = Arc::clone(&calls);
            Arc::new(move |_printer: &str, _bytes: &[u8]| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(91)
            })
        };
        let state = ServerState {
            started_at: Instant::now(),
            version: "test",
            settings: SettingsStore::in_memory(Settings::default()),
            relay: None,
            local_ledger: Arc::clone(&manager),
            print_raw: fake,
            print_slots: Arc::new(Semaphore::new(MAX_ACTIVE_LOCAL_PRINTS)),
        };
        ledger.fail_writes_for_tests();
        let failed = handle_text(
            &serde_json::json!({
                "op":"print","id":"failed","printer":"Fake","data":BASE64.encode(b"ticket"),
                "printIntentId":"stable"
            })
            .to_string(),
            &state,
        )
        .await;
        assert_eq!(serde_json::to_value(failed).unwrap()["ok"], false);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let list = serde_json::to_value(handle_text(r#"{"op":"list","id":"list"}"#, &state).await)
            .unwrap();
        assert!(list.get("capabilities").is_none());
        let health = serde_json::to_value(health_handler(State(state.clone())).await.0).unwrap();
        assert_eq!(health["local_print_intents"]["ok"], false);
        let legacy = handle_text(
            &serde_json::json!({
                "op":"print","id":"legacy","printer":"Fake","data":BASE64.encode(b"ticket")
            })
            .to_string(),
            &state,
        )
        .await;
        assert_eq!(serde_json::to_value(legacy).unwrap()["ok"], true);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        manager.retry().unwrap();
        let list = serde_json::to_value(handle_text(r#"{"op":"list","id":"list"}"#, &state).await)
            .unwrap();
        assert_eq!(list["capabilities"]["printIntentId"], true);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocked_local_spooler_does_not_delay_health_or_another_socket() {
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        for mode in ["legacy", "intent", "test"] {
            let entered = Arc::new(AtomicBool::new(false));
            let fake = {
                let entered = Arc::clone(&entered);
                Arc::new(move |_printer: &str, _bytes: &[u8]| {
                    entered.store(true, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_secs(2));
                    Ok(91)
                })
            };
            let ledger = if mode == "intent" {
                let path = std::env::temp_dir()
                    .join(format!("bridge-blocked-spooler-{}", uuid::Uuid::new_v4()))
                    .join("local-print-intents.jsonl");
                Some(Arc::new(LocalJobLedger::open(path).unwrap()))
            } else {
                None
            };
            let state = ServerState {
                started_at: Instant::now(),
                version: "test",
                settings: SettingsStore::in_memory(Settings {
                    local_auth: LocalAuth::Off,
                    ..Default::default()
                }),
                relay: None,
                local_ledger: Arc::new(LocalLedgerManager::from_ledger(ledger)),
                print_raw: fake,
                print_slots: Arc::new(Semaphore::new(MAX_ACTIVE_LOCAL_PRINTS)),
            };
            let (server, url) = ws_test_server(state).await;
            let (mut first, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
            let health_url = url.replace("ws://", "http://").replace("/print", "/health");
            let watcher_url = url.clone();
            let watcher = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !entered.load(Ordering::SeqCst) {
                    assert!(Instant::now() < deadline, "spooler never started");
                    std::thread::sleep(Duration::from_millis(1));
                }
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async move {
                    let started = Instant::now();
                    let health: serde_json::Value = reqwest::get(health_url)
                        .await
                        .unwrap()
                        .json()
                        .await
                        .unwrap();
                    let (mut second, _) = tokio_tungstenite::connect_async(&watcher_url)
                        .await
                        .unwrap();
                    let list = ws_exchange(
                        &mut second,
                        serde_json::json!({"op":"list","id":"another-socket"}),
                    )
                    .await;
                    (started.elapsed(), health, list)
                })
            });
            let request = match mode {
                "intent" => serde_json::json!({
                    "op":"print", "id":"blocked", "printer":"Fake",
                    "data":BASE64.encode(b"ticket"), "printIntentId":"blocked-intent"
                }),
                "test" => serde_json::json!({"op":"test","id":"blocked","printer":"Fake"}),
                _ => serde_json::json!({
                    "op":"print", "id":"blocked", "printer":"Fake",
                    "data":BASE64.encode(b"ticket")
                }),
            };
            let printing = tokio::spawn(async move { ws_exchange(&mut first, request).await });
            tokio::task::yield_now().await;
            let (elapsed, health, list) =
                tokio::task::spawn_blocking(move || watcher.join().unwrap())
                    .await
                    .unwrap();
            assert_eq!(health["ok"], true, "{mode}");
            assert_eq!(list["ok"], true, "{mode}");
            assert!(
                elapsed < Duration::from_millis(1500),
                "{mode} blocked other requests for {elapsed:?}"
            );
            let printed = printing.await.unwrap();
            assert_eq!(printed["ok"], true, "{mode}");
            assert_eq!(printed["jobId"], 91, "{mode}");
            server.abort();
        }
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
            local_ledger: Arc::new(LocalLedgerManager::from_ledger(None)),
            print_raw: Arc::new(printer::print_raw),
            print_slots: Arc::new(Semaphore::new(MAX_ACTIVE_LOCAL_PRINTS)),
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
