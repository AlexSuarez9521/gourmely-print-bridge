//! The outbound relay: the bridge's permanent connection to GourmelyHub.
//!
//! # Why the connection goes outwards
//!
//! Until now printing rode on whatever browser happened to have the POS open in
//! the shop. If nobody did, the comanda never came out and the API answered
//! `{triggered:true}` — a silent failure, the worst kind in a kitchen. And a
//! waiter on 4G outside the shop could not print at all: the API lives on a VPS
//! and the printer sits behind the shop's NAT on a 192.168 address.
//!
//! Inverting it fixes both. The till dials *out* and keeps the socket open, so
//! there is no port to forward, no static IP to buy, and it behaves the same on
//! fibre, on a phone hotspot and behind CGNAT. The server pushes jobs down it.
//!
//! # Protocol (read off the server, not off the design document)
//!
//! Socket.IO v5 over Engine.IO v4 at namespace `/print`, path `/socket.io`.
//! Frame-level details live in [`crate::engineio`]; the events are:
//!
//! * handshake `auth: { stationToken }` — **and nothing else this side sends is
//!   trusted**. The server resolves the row for that token and derives tenant,
//!   branch and roles from it, then joins the rooms itself. Cross-tenant
//!   delivery is impossible by construction, and this side gets something for
//!   free: it cannot mis-address itself even with a bug.
//! * `station-hello { bridgeVersion, os, printers }` — presence. The server's
//!   window is three minutes (`STATION_PRESENCE_TTL_MS`); we send it on connect
//!   and every minute. Falling outside it marks the station offline and the
//!   server routes the ticket down the old local path instead.
//! * `print-job { jobId, role, kind, osPrinterName, escposBase64, copies,
//!   stationId, orderId, createdAt }` — inbound work.
//! * `print-result { jobId, ok, error }` — the ack that moves the row to a
//!   terminal state. Without it the job stays `sent` and comes back on the next
//!   reconnect, which is why the ledger — not the ack — is what stops
//!   duplicates.
//!
//! # Reconnection
//!
//! Infinite, exponential, capped at [`MAX_BACKOFF`]. A power cut, a router
//! reboot or a night with no internet must not leave the till mute until
//! somebody notices; it has to come back on its own. Three refinements that
//! matter in a real shop:
//!
//! * **A revoked token is not a network problem.** The server words those two
//!   differently on purpose, so the bridge backs off to [`REVOKED_BACKOFF`] and
//!   says so in the UI — while still retrying, because the fix (re-pair from
//!   the panel) happens on the other side.
//! * **The backoff resets only after the connection has held** for
//!   [`HEALTHY_AFTER`]. Resetting on "connected" alone turns a crash-loop into
//!   a tight loop.
//! * **Connected means the namespace acked.** Not "the TCP socket opened".
//!   Getting that wrong is what made the off-the-shelf client unusable here
//!   (see [`crate::engineio`]) and it is the exact shape of silent failure this
//!   whole feature exists to delete.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{mpsc, Notify};
use tokio_tungstenite::tungstenite::Message;

use crate::config::{SettingsStore, MAX_PRINT_BYTES};
use crate::engineio::{self, Incoming};
use crate::error::{BridgeError, BridgeResult};
use crate::job_ledger::{Claim, JobLedger};
use crate::printer;

/// The namespace the server mounts the relay on.
const NAMESPACE: &str = "/print";

/// First retry delay, and the unit the exponential grows from.
const BASE_BACKOFF: Duration = Duration::from_secs(1);
/// Cap. Short enough that a shop coming back after a night's outage prints
/// within the minute; long enough not to be a packet storm against the API.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Applied when the server says the token is invalid or revoked.
const REVOKED_BACKOFF: Duration = Duration::from_secs(300);
/// Cap on the wait between attempts to take the print ledger.
///
/// Shorter than [`MAX_BACKOFF`] because of who is waiting: taking the ledger
/// fails when another copy of the bridge holds it, and the fix is somebody at
/// the till closing that copy and then standing there watching for printing to
/// come back. Fifteen seconds is the longest that should take.
const LEDGER_RETRY_MAX: Duration = Duration::from_secs(15);
/// How many ledger attempts between the reminders in the log. At the cap above
/// this is roughly one every five minutes — enough for a support log to show
/// the condition lasted, without a line every fifteen seconds all night.
const LEDGER_LOG_EVERY: u32 = 20;
/// How long a connection must hold before the backoff resets.
const HEALTHY_AFTER: Duration = Duration::from_secs(30);
/// Presence beat. The server expires presence at three minutes.
const HELLO_EVERY: Duration = Duration::from_secs(60);
/// How long the beat waits for the spooler to list the printers before giving
/// up on a fresh list.
///
/// Enumerating printers on Windows walks the spooler, and a driver whose device
/// is switched off — the everyday state of a thermal printer at the till — can
/// hold that call for a long time. Ten seconds is generous for a healthy
/// machine and far inside the server's three-minute presence window, so a wedged
/// spooler costs an out-of-date printer list, never the station's presence.
const PRINTER_LIST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for the namespace ack before giving up on the handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);
/// Depth of the print queue. Deep enough for the burst a reconnect replay
/// brings, shallow enough that a wedged spooler is noticed instead of
/// swallowing the evening's tickets.
const PRINT_QUEUE_DEPTH: usize = 256;

const EV_PRINT_JOB: &str = "print-job";
const EV_PRINT_RESULT: &str = "print-result";
const EV_STATION_HELLO: &str = "station-hello";

/// A job as it comes off the wire. Field names are the server's
/// (`printJobPayload` in `realtime/socket.ts`); do not "tidy" them.
#[derive(Debug, Clone, Deserialize)]
pub struct PrintJob {
    #[serde(rename = "jobId")]
    pub job_id: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub kind: String,
    #[serde(rename = "osPrinterName", default)]
    pub os_printer_name: Option<String>,
    #[serde(rename = "escposBase64")]
    pub escpos_base64: String,
    #[serde(default = "one")]
    pub copies: u32,
    #[serde(rename = "orderId", default)]
    pub order_id: Option<String>,
}

fn one() -> u32 {
    1
}

/// What the settings window shows. Everything here is derived; the station
/// token never leaves the process.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayStatus {
    pub paired: bool,
    pub connected: bool,
    pub server_url: String,
    pub label: Option<String>,
    pub roles: Vec<String>,
    /// Last transport or auth error, in the words the operator needs.
    pub last_error: Option<String>,
    /// True when the server rejected the token. The UI turns this into
    /// "vuelve a vincular la estación" instead of a generic "sin conexión".
    pub token_rejected: bool,
    /// True while another copy of the bridge on this machine holds the print
    /// ledger, so this one is deliberately standing down.
    ///
    /// Its own flag rather than a shade of `connected`, because the two need
    /// opposite reactions and only one of them is a fault. "Reconectando…" tells
    /// the cashier to check the internet; this state is about a second bridge on
    /// their own desktop and the fix is to close it. Showing them the wrong one
    /// sends support after a network problem that does not exist.
    pub blocked_by_another_instance: bool,
    pub jobs_printed: u64,
    pub jobs_failed: u64,
}

/// Handle the rest of the app talks to.
#[derive(Clone)]
pub struct RelayHandle {
    settings: SettingsStore,
    status: Arc<RwLock<RelayStatus>>,
    /// Poked when the pairing changes, so the supervisor drops its backoff and
    /// reconnects with the new token instead of waiting it out.
    wake: Arc<Notify>,
}

impl RelayHandle {
    pub fn status(&self) -> RelayStatus {
        let mut snapshot = match self.status.read() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let settings = self.settings.snapshot();
        snapshot.paired = settings.is_paired();
        snapshot.server_url = settings.server_url();
        snapshot.label = settings.label.clone();
        snapshot.roles = settings.roles.clone();
        snapshot
    }

    /// Reconnect now: the token changed (paired, re-paired or forgotten).
    pub fn reconnect(&self) {
        self.wake.notify_waiters();
        self.wake.notify_one();
    }

    fn mutate(&self, f: impl FnOnce(&mut RelayStatus)) {
        let mut guard = match self.status.write() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut guard);
    }
}

/// The printer list the presence beat announces, and the cache behind it.
///
/// # Why the cache is here and not in the beat task
///
/// `printers_or_last_known` falls back to "the last list we managed to read"
/// when the spooler does not answer inside [`PRINTER_LIST_TIMEOUT`], and a
/// thermal printer switched off — the everyday state of a till — is exactly
/// what makes the spooler not answer. That fallback only means anything if the
/// last known list survives.
///
/// It did not. The `Vec` holding it was declared **inside** the `station-hello`
/// task, and `run_connection` builds that task afresh for every connection. So
/// every reconnect reset "last known" to empty, and the first hello after a
/// dropped socket announced `printers: []` on a till whose spooler was slow —
/// telling the server this station has no printers at all. A reconnect is not
/// an exotic event; it is what happens after every router reboot and every
/// night the shop's internet drops.
///
/// Owning it here, in a value `supervise` keeps for the life of the process,
/// is what makes the cache outlive the socket.
///
/// The enumerator is a field for the same reason it is a parameter of
/// `printers_or_last_known`: a test needs a spooler it can wedge on demand, and
/// the real one walks the machine's actual print queues.
#[derive(Clone)]
struct PrinterBeat {
    last_known: Arc<StdMutex<Vec<String>>>,
    enumerate: Arc<dyn Fn() -> BridgeResult<Vec<String>> + Send + Sync>,
    limit: Duration,
}

impl PrinterBeat {
    fn new(enumerate: impl Fn() -> BridgeResult<Vec<String>> + Send + Sync + 'static) -> Self {
        Self {
            last_known: Arc::new(StdMutex::new(Vec::new())),
            enumerate: Arc::new(enumerate),
            limit: PRINTER_LIST_TIMEOUT,
        }
    }

    /// The last list read successfully, empty only before the first one.
    fn snapshot(&self) -> Vec<String> {
        match self.last_known.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Re-reads the printers for the next `station-hello`, remembering the
    /// answer. Returns the cached list when the spooler does not produce one.
    async fn refresh(&self) -> Vec<String> {
        let enumerate = Arc::clone(&self.enumerate);
        let current =
            printers_or_last_known(self.snapshot(), self.limit, move || enumerate()).await;
        match self.last_known.lock() {
            Ok(mut guard) => *guard = current.clone(),
            Err(poisoned) => *poisoned.into_inner() = current.clone(),
        }
        current
    }
}

/// Why a connection attempt ended. The distinction drives the backoff.
#[derive(Debug)]
enum Ended {
    /// Socket closed, server unreachable, protocol hiccup. Retry soon.
    Transport(String),
    /// The server refused the token. Retry slowly and tell the operator.
    Rejected(String),
}

/// Outgoing text frames, from anywhere in the process to the live socket.
type Outbound = mpsc::UnboundedSender<String>;

/// Starts the supervisor. Returns immediately; the loop lives for the process.
///
/// # What stops a second relay
///
/// Not this function. It used to carry a `running` flag with a comment claiming
/// it made `spawn` idempotent — the flag was created fresh inside `spawn`, so
/// the compare-exchange always won and it prevented exactly nothing. It read as
/// protection against the very duplicate that shipped.
///
/// The guarantee lives one level down, in the exclusive lock
/// [`JobLedger::open`] takes: whoever cannot open the ledger never connects, so
/// the server never has two sockets for one till to push a job down. That works
/// across processes, which no flag in this address space can.
///
/// "Never connects" is for as long as the other copy is there — not for ever.
/// See [`acquire_ledger`]: the loser waits and keeps asking, so closing the
/// duplicate brings this one back with no restart. Standing down permanently
/// was its own outage.
pub fn spawn(settings: SettingsStore) -> RelayHandle {
    let ledger_path = crate::config::data_dir()
        .map(|d| d.join("print-jobs.jsonl"))
        .unwrap_or_else(|| PathBuf::from("print-jobs.jsonl"));
    spawn_with(settings, ledger_path, PrinterBeat::new(printer::list))
}

/// [`spawn`] with the two things a test needs to control: where the ledger
/// lives and what enumerating the printers does.
///
/// Taking the path as an argument rather than reading `data_dir()` inside the
/// supervisor also removes a process-global from the hot path — two supervisors
/// in one test binary no longer have to agree on an environment variable.
fn spawn_with(settings: SettingsStore, ledger_path: PathBuf, printers: PrinterBeat) -> RelayHandle {
    let handle = RelayHandle {
        settings,
        status: Arc::new(RwLock::new(RelayStatus::default())),
        wake: Arc::new(Notify::new()),
    };

    let worker = handle.clone();
    tokio::spawn(async move { supervise(worker, ledger_path, printers).await });
    handle
}

/// The forever loop.
async fn supervise(handle: RelayHandle, ledger_path: PathBuf, printers: PrinterBeat) {
    // One ledger and one print worker for the life of the process, not per
    // connection: the whole point of the ledger is to outlive the socket.
    //
    // This blocks the relay until the ledger is ours — see `acquire_ledger` for
    // why it waits rather than giving up.
    let ledger = acquire_ledger(&handle, &ledger_path).await;

    let outbound: Arc<StdMutex<Option<Outbound>>> = Arc::new(StdMutex::new(None));
    let (tx, rx) = mpsc::channel::<PrintJob>(PRINT_QUEUE_DEPTH);
    tokio::spawn(print_worker(rx, ledger, outbound.clone(), handle.clone()));

    let mut attempt: u32 = 0;

    loop {
        let settings = handle.settings.snapshot();
        let Some(token) = settings.station_token.clone() else {
            handle.mutate(|s| {
                s.connected = false;
                s.token_rejected = false;
                s.last_error = None;
            });
            tracing::info!("relay idle: this bridge is not paired to a station yet");
            handle.wake.notified().await;
            attempt = 0;
            continue;
        };

        let url = engineio::websocket_url(&settings.server_url());
        tracing::info!(url = %url, "relay connecting");

        let started = Instant::now();
        let ended = run_connection(&handle, &url, &token, &tx, &outbound, &printers).await;
        let held = started.elapsed();

        if let Ok(mut guard) = outbound.lock() {
            *guard = None;
        }
        handle.mutate(|s| s.connected = false);

        let delay = match ended {
            Ended::Rejected(msg) => {
                tracing::error!(err = %msg, "relay rejected: the station token is not valid");
                handle.mutate(|s| {
                    s.token_rejected = true;
                    s.last_error = Some(
                        "El servidor rechazó esta estación. Genera un código nuevo en \
                         Configuración → Impresión → Estaciones y vuelve a vincularla."
                            .into(),
                    );
                });
                REVOKED_BACKOFF
            }
            Ended::Transport(msg) => {
                if held >= HEALTHY_AFTER {
                    attempt = 0;
                }
                tracing::warn!(err = %msg, held_secs = held.as_secs(), attempt, "relay disconnected");
                handle.mutate(|s| {
                    s.token_rejected = false;
                    s.last_error = Some(msg.clone());
                });
                let delay = backoff(attempt);
                attempt = attempt.saturating_add(1);
                delay
            }
        };

        tracing::info!(seconds = delay.as_secs(), "relay retrying");
        // A pairing change must not have to wait out a five-minute backoff.
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = handle.wake.notified() => { attempt = 0; }
        }
    }
}

/// Takes the print ledger, waiting for as long as that takes.
///
/// # Why it waits instead of standing down for good
///
/// Failing to take the ledger means another copy of the bridge holds it, and
/// this one must not connect: two relays authenticate with the same station
/// token, land in the same room, and the server pushes every job down both —
/// one order, two comandas, and nothing in the logs to say why. That part was
/// right and has not changed. **Returning** was the mistake.
///
/// The supervisor used to end here. Nothing restarted it, and
/// [`RelayHandle::reconnect`] only pokes a `Notify` that no longer has a
/// listener — so re-pairing from Ajustes did not revive it either. What that
/// leaves is the worst shape a bridge can have: a live process with its tray
/// icon and its settings window, both working, that will never print again for
/// as long as the till stays on. Nobody finds out until a customer asks for
/// their receipt.
///
/// And it is reachable. `tauri-plugin-single-instance` only kills the second
/// copy when `FindWindowW` locates the first one's window; a copy started
/// before that window exists, under a different user, or outside Tauri
/// altogether, survives — loses the lock — and used to die here in silence.
///
/// So: keep retrying, because the other copy can be closed and this one should
/// come back on its own when it is; and keep saying so, because a bridge that
/// is standing down looks exactly like a healthy one from the outside. The
/// wait is interruptible by [`RelayHandle::reconnect`], so pairing does not
/// have to sit through it.
///
/// Both failure kinds retry. A refused lock is the one this exists for, but an
/// I/O error is no better an argument for giving up permanently: a data folder
/// that was briefly unreadable — antivirus, a network profile, a disk that
/// filled up and got cleared — must not cost the till its remote printing until
/// somebody thinks to restart the app.
async fn acquire_ledger(handle: &RelayHandle, path: &Path) -> JobLedger {
    let mut attempt: u32 = 0;
    loop {
        match JobLedger::open(path) {
            Ok(ledger) => {
                if attempt > 0 {
                    tracing::info!(path = %path.display(), attempt,
                        "the print ledger is free; the relay is starting");
                }
                handle.mutate(|s| {
                    s.blocked_by_another_instance = false;
                    // Only clear a message this loop is responsible for.
                    if attempt > 0 {
                        s.last_error = None;
                    }
                });
                return ledger;
            }
            Err(BridgeError::AlreadyRunning(msg)) => {
                // The operator-facing sentence is already inside the error —
                // it names the process and says what to close.
                if attempt.is_multiple_of(LEDGER_LOG_EVERY) {
                    tracing::error!(path = %path.display(), attempt, "{msg}");
                }
                handle.mutate(|s| {
                    s.connected = false;
                    s.blocked_by_another_instance = true;
                    s.last_error = Some(msg);
                });
            }
            Err(e) => {
                if attempt.is_multiple_of(LEDGER_LOG_EVERY) {
                    tracing::error!(err = %e, path = %path.display(), attempt,
                        "cannot open the print ledger; the relay is waiting for it");
                }
                handle.mutate(|s| {
                    s.connected = false;
                    s.blocked_by_another_instance = false;
                    s.last_error = Some(format!(
                        "No se puede escribir el registro de impresiones en {}: {e}. \
                         El bridge lo seguirá intentando.",
                        path.display()
                    ));
                });
            }
        }

        let delay = backoff_to(attempt, LEDGER_RETRY_MAX);
        attempt = attempt.saturating_add(1);
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = handle.wake.notified() => {}
        }
    }
}

/// `BASE_BACKOFF * 2^attempt`, capped, plus up to 20% jitter so a shop with
/// several tills does not reconnect them all on the same tick.
fn backoff(attempt: u32) -> Duration {
    backoff_to(attempt, MAX_BACKOFF)
}

/// [`backoff`] against an explicit ceiling, because the two things this
/// supervisor waits for deserve different ones: a server that is down, and a
/// second copy of the bridge somebody is about to close.
fn backoff_to(attempt: u32, cap: Duration) -> Duration {
    let exp = BASE_BACKOFF
        .checked_mul(1u32 << attempt.min(6))
        .unwrap_or(cap)
        .min(cap);
    let jitter_ms = (exp.as_millis() as u64 / 5).max(1);
    exp + Duration::from_millis(spread(jitter_ms))
}

/// Tiny non-cryptographic spread. Deliberately not `rand`: this decides when to
/// retry, nothing anybody could exploit, and it keeps a dependency out.
fn spread(bound: u64) -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    if bound == 0 {
        return 0;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 % bound)
        .unwrap_or(0)
}

/// One connection, from TCP to close. Returns why it ended.
async fn run_connection(
    handle: &RelayHandle,
    url: &str,
    token: &str,
    jobs: &mpsc::Sender<PrintJob>,
    outbound: &Arc<StdMutex<Option<Outbound>>>,
    printers: &PrinterBeat,
) -> Ended {
    let (stream, _response) = match tokio_tungstenite::connect_async(url).await {
        Ok(pair) => pair,
        Err(e) => return Ended::Transport(format!("no se pudo conectar: {e}")),
    };
    let (mut sink, mut source) = stream.split();

    // The OPEN packet has to come first. Anything else here means we are not
    // talking to an Engine.IO v4 endpoint at all (a proxy, a login page, the
    // wrong URL) and saying so beats timing out in silence.
    let open = match next_text(&mut source, HANDSHAKE_TIMEOUT).await {
        Ok(Some(frame)) => match engineio::parse(&frame) {
            Incoming::Open(open) => *open,
            other => {
                return Ended::Transport(format!(
                    "el servidor no abrió una sesión Engine.IO (recibido: {other:?})"
                ))
            }
        },
        Ok(None) => return Ended::Transport("el servidor cerró antes del saludo".into()),
        Err(e) => return Ended::Transport(e),
    };
    tracing::debug!(sid = %open.sid, ping_interval = open.ping_interval, "engine.io session open");

    // Join the namespace and WAIT for the ack before sending anything else.
    // Sending an event first is what makes Socket.IO drop the whole client.
    if let Err(e) = sink
        .send(Message::text(engineio::connect_packet(
            NAMESPACE,
            &json!({ "stationToken": token }),
        )))
        .await
    {
        return Ended::Transport(format!("no se pudo enviar el saludo: {e}"));
    }

    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match next_text(&mut source, left).await {
            Ok(Some(frame)) => match engineio::parse(&frame) {
                Incoming::Connected { ref namespace, .. } if namespace == NAMESPACE => break,
                Incoming::ConnectError { message, .. } => return Ended::Rejected(message),
                Incoming::Ping => {
                    if let Err(e) = sink.send(Message::text(engineio::pong())).await {
                        return Ended::Transport(format!("no se pudo responder al latido: {e}"));
                    }
                }
                Incoming::Close => {
                    return Ended::Transport("el servidor cerró durante el saludo".into())
                }
                _ => {}
            },
            Ok(None) => {
                return Ended::Transport(
                    "el servidor cerró la conexión sin aceptar la estación".into(),
                )
            }
            Err(e) => return Ended::Transport(e),
        }
    }

    tracing::info!("relay connected");
    handle.mutate(|s| {
        s.connected = true;
        s.token_rejected = false;
        s.last_error = None;
    });

    // One writer owns the sink. Acks come from the print worker, heartbeats
    // from the reader, presence from a timer: funnelling them through a channel
    // is what keeps a single `SplitSink` from needing a lock on every frame.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    if let Ok(mut guard) = outbound.lock() {
        *guard = Some(out_tx.clone());
    }

    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if sink.send(Message::text(frame)).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    // Presence, starting immediately: the server only routes to a station it
    // has seen inside the window, and the connect alone leaves `last_seen_at`
    // frozen at connection time.
    //
    // The printer list comes from `PrinterBeat`, which `supervise` owns: this
    // task is rebuilt on every connection, and the cache behind "last known
    // list" has to survive that or the first hello after any reconnect
    // announces no printers at all.
    let hello_tx = out_tx.clone();
    let hello_printers = printers.clone();
    let hello = tokio::spawn(async move {
        loop {
            let printers = hello_printers.refresh().await;
            let frame = engineio::event_packet(
                NAMESPACE,
                EV_STATION_HELLO,
                &json!({
                    "bridgeVersion": env!("CARGO_PKG_VERSION"),
                    "os": format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
                    "printers": printers,
                }),
            );
            if hello_tx.send(frame).is_err() {
                break;
            }
            tokio::time::sleep(HELLO_EVERY).await;
        }
    });

    // The server pings every `ping_interval`; going quiet for longer than
    // interval + timeout means the link is gone even though TCP has not
    // noticed — a mobile connection dropping is exactly that shape.
    let silence_limit = Duration::from_millis(open.ping_interval + open.ping_timeout);

    let ended = loop {
        match next_text(&mut source, silence_limit).await {
            Ok(Some(frame)) => match engineio::parse(&frame) {
                Incoming::Ping => {
                    if out_tx.send(engineio::pong().to_string()).is_err() {
                        break Ended::Transport("el escritor del socket se cerró".into());
                    }
                }
                Incoming::Event {
                    ref namespace,
                    ref name,
                    ref args,
                } if namespace == NAMESPACE && name == EV_PRINT_JOB => {
                    for arg in args {
                        match serde_json::from_value::<PrintJob>(arg.clone()) {
                            Ok(job) => {
                                tracing::info!(job_id = %job.job_id, kind = %job.kind, role = %job.role, "print-job received");
                                // `try_send`, not `send`: blocking the reader on
                                // a full queue also blocks the heartbeat, and a
                                // slow printer would look like a dead link.
                                if let Err(e) = jobs.try_send(job) {
                                    tracing::error!(err = %e, "print queue full; the server will replay the job");
                                    handle.mutate(|s| {
                                        s.last_error = Some(
                                            "La cola de impresión está llena. Revisa que la impresora responda.".into(),
                                        )
                                    });
                                }
                            }
                            Err(e) => tracing::error!(err = %e, "print-job payload did not parse"),
                        }
                    }
                }
                Incoming::ConnectError { message, .. } => break Ended::Rejected(message),
                Incoming::Disconnected { .. } => {
                    break Ended::Transport("el servidor desvinculó la estación".into())
                }
                Incoming::Close => break Ended::Transport("el servidor cerró la conexión".into()),
                Incoming::Event { name, .. } => {
                    tracing::debug!(event = %name, "ignoring an event the bridge does not handle");
                }
                other => tracing::trace!(?other, "frame ignored"),
            },
            Ok(None) => break Ended::Transport("la conexión se cerró".into()),
            Err(e) => break Ended::Transport(e),
        }
    };

    hello.abort();
    let _ = out_tx.send(engineio::disconnect_packet(NAMESPACE));
    drop(out_tx);
    if let Ok(mut guard) = outbound.lock() {
        *guard = None;
    }
    let _ = writer.await;
    ended
}

/// The printer list for the next `station-hello`, without ever letting the
/// spooler take the runtime with it.
///
/// # Two separate ways this used to hurt
///
/// The beat called `printer::list()` straight from its async task. That call is
/// a synchronous walk of the Windows print spooler and it blocks — for seconds,
/// on the everyday case of a till whose thermal printer is switched off. Inside
/// an async task a blocking call does not just delay *that* task: it owns a
/// runtime worker for the whole duration, and everything scheduled on it stops.
/// `deliver` already knew this and used `spawn_blocking`; the beat did not.
///
/// So: onto a blocking thread, **and** on a clock. `spawn_blocking` alone would
/// still leave the beat itself waiting on a spooler that never answers, and a
/// station whose presence lapses is a station the server stops routing to — it
/// quietly falls back to the local path. Past [`PRINTER_LIST_TIMEOUT`] the beat
/// goes out with the last list we managed to read, which is stale at worst and
/// almost always identical.
async fn printers_or_last_known<F>(
    previous: Vec<String>,
    limit: Duration,
    enumerate: F,
) -> Vec<String>
where
    F: FnOnce() -> BridgeResult<Vec<String>> + Send + 'static,
{
    match tokio::time::timeout(limit, tokio::task::spawn_blocking(enumerate)).await {
        Ok(Ok(Ok(printers))) => printers,
        Ok(Ok(Err(e))) => {
            tracing::warn!(err = %e, "could not list the printers for the heartbeat; beating with the last known list");
            previous
        }
        Ok(Err(e)) => {
            tracing::warn!(err = %e, "the printer enumeration task did not finish");
            previous
        }
        Err(_) => {
            // The blocking thread is still stuck in the spooler; it will finish
            // on its own and its result is simply dropped. Abandoning it costs
            // one pool thread, which is cheaper than a station going offline.
            tracing::warn!(
                seconds = limit.as_secs(),
                "the print spooler did not answer; beating with the last known printer list"
            );
            previous
        }
    }
}

/// Next text frame, or a description of why there is not one.
///
/// Control frames are handled where they belong: tungstenite answers Ping/Pong
/// at the WebSocket level itself, and those are a different thing from the
/// Engine.IO heartbeat that travels as text.
async fn next_text<S>(source: &mut S, timeout: Duration) -> Result<Option<String>, String>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match tokio::time::timeout(timeout, source.next()).await {
            Err(_) => {
                return Err(format!(
                    "sin señal del servidor durante {} s",
                    timeout.as_secs()
                ))
            }
            Ok(None) => return Ok(None),
            Ok(Some(Err(e))) => return Err(format!("error de red: {e}")),
            Ok(Some(Ok(Message::Text(text)))) => return Ok(Some(text.to_string())),
            Ok(Some(Ok(Message::Close(_)))) => return Ok(None),
            // Binary payloads only appear for binary events, which this
            // protocol never uses: the ticket travels base64 inside the JSON.
            Ok(Some(Ok(_))) => continue,
        }
    }
}

/// The single consumer of the print queue.
///
/// One task, so tickets reach the spooler in the order the server sent them —
/// the replay arrives in `seq` order and a comanda printed out of order is a
/// kitchen mistake. It owns the ledger outright, which is what keeps "claim
/// then print" free of locking games.
async fn print_worker(
    mut rx: mpsc::Receiver<PrintJob>,
    mut ledger: JobLedger,
    outbound: Arc<StdMutex<Option<Outbound>>>,
    handle: RelayHandle,
) {
    let printed = AtomicU64::new(0);
    let failed = AtomicU64::new(0);

    while let Some(job) = rx.recv().await {
        let job_id = job.job_id.clone();

        // ── The order that matters (R8) ─────────────────────────────────
        // The claim is written and fsynced BEFORE a single byte reaches the
        // spooler. See `job_ledger` for why this way round.
        let claim = match ledger.claim(&job_id) {
            Ok(c) => c,
            Err(e) => {
                // We cannot prove this job has not printed already, so we do
                // not print it. A silent duplicate receipt is worse than a
                // missing one that says so.
                tracing::error!(job_id = %job_id, err = %e, "cannot record the job; refusing to print it");
                ack(&outbound, &job_id, false, Some(format!(
                    "El bridge no pudo registrar el trabajo antes de imprimirlo ({e}); no se imprimió para evitar duplicados."
                )));
                continue;
            }
        };

        let fresh = claim == Claim::Fresh;
        let (ok, error) = match claim {
            Claim::AlreadyDone { ok, error } => {
                tracing::info!(job_id = %job_id, ok, "job already handled; re-acking without printing");
                (ok, error)
            }
            Claim::Interrupted => {
                tracing::warn!(job_id = %job_id, "job was claimed but never finished; not reprinting");
                (
                    false,
                    Some(
                        "El bridge se reinició mientras se imprimía este trabajo. \
                         No se reimprimió solo, para no duplicarlo: revisa la impresora y \
                         reimprime desde Ventas si hace falta."
                            .to_string(),
                    ),
                )
            }
            Claim::Fresh => match deliver(&handle, job).await {
                Ok(()) => (true, None),
                Err(e) => (false, Some(e)),
            },
        };

        if fresh {
            if let Err(e) = ledger.finish(&job_id, ok, error.clone()) {
                // Leaves the entry as `Claimed`, which reads as `Interrupted`
                // on a replay: no reprint, a visible failure. Safe direction.
                tracing::error!(job_id = %job_id, err = %e, "could not record the print result");
            }
            if ok {
                printed.fetch_add(1, Ordering::Relaxed);
            } else {
                failed.fetch_add(1, Ordering::Relaxed);
            }
            handle.mutate(|s| {
                s.jobs_printed = printed.load(Ordering::Relaxed);
                s.jobs_failed = failed.load(Ordering::Relaxed);
            });
        }

        ack(&outbound, &job_id, ok, error);
    }
}

/// Resolves the printer, decodes the bytes and hands them to the spooler.
///
/// Runs on a blocking thread: the spooler call is synchronous and can sit for
/// seconds on a printer that is switched off, and stalling the runtime would
/// take the socket down with it.
async fn deliver(handle: &RelayHandle, job: PrintJob) -> Result<(), String> {
    let settings = handle.settings.snapshot();
    let target = job
        .os_printer_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| settings.default_printer.clone())
        .ok_or_else(|| {
            "El trabajo no trae nombre de impresora y esta estación no tiene una por defecto."
                .to_string()
        })?;

    let bytes = BASE64
        .decode(job.escpos_base64.as_bytes())
        .map_err(|e| format!("los bytes del ticket no son base64 válido: {e}"))?;
    if bytes.len() > MAX_PRINT_BYTES {
        return Err(format!(
            "el ticket pesa {} bytes, por encima del máximo de {MAX_PRINT_BYTES}",
            bytes.len()
        ));
    }

    let copies = job.copies.clamp(1, 10);
    let name = target.clone();
    tokio::task::spawn_blocking(move || {
        for _ in 0..copies {
            printer::print_raw(&name, &bytes)?;
        }
        Ok::<(), crate::error::BridgeError>(())
    })
    .await
    .map_err(|e| format!("la tarea de impresión no terminó: {e}"))?
    .map_err(|e| format!("{e}"))?;

    tracing::info!(job_id = %job.job_id, printer = %target, copies, "printed");
    Ok(())
}

/// Sends `print-result`. Best-effort by design: a lost ack leaves the job
/// `sent`, the next replay brings it back, and the ledger answers it again from
/// disk without printing.
fn ack(outbound: &Arc<StdMutex<Option<Outbound>>>, job_id: &str, ok: bool, error: Option<String>) {
    let sender = match outbound.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    let Some(sender) = sender else {
        tracing::warn!(job_id = %job_id, "no live socket to ack on; the server will replay");
        return;
    };
    let frame = engineio::event_packet(
        NAMESPACE,
        EV_PRINT_RESULT,
        &json!({ "jobId": job_id, "ok": ok, "error": error }),
    );
    if sender.send(frame).is_err() {
        tracing::warn!(job_id = %job_id, "could not queue print-result; the server will replay");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use std::net::SocketAddr;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gmly-relay-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    async fn wait_for(limit: Duration, mut check: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if check() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// A `PrinterBeat` wired to a spooler the test drives, on a short clock.
    fn beat_with(
        limit: Duration,
        enumerate: impl Fn() -> BridgeResult<Vec<String>> + Send + Sync + 'static,
    ) -> PrinterBeat {
        PrinterBeat {
            last_known: Arc::new(StdMutex::new(Vec::new())),
            enumerate: Arc::new(enumerate),
            limit,
        }
    }

    /// A spooler that answers the first call and then wedges forever — a
    /// thermal printer switched off after the first beat, which is the ordinary
    /// end of a shift.
    fn answers_once_then_hangs() -> impl Fn() -> BridgeResult<Vec<String>> + Send + Sync + 'static {
        let calls = Arc::new(AtomicU64::new(0));
        move || {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(vec!["Caja".to_string()])
            } else {
                std::thread::sleep(Duration::from_secs(3));
                Ok(vec!["arrived far too late".to_string()])
            }
        }
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        assert!(backoff(0) >= BASE_BACKOFF);
        assert!(backoff(0) < Duration::from_secs(2));
        assert!(backoff(3) >= Duration::from_secs(8));
        // Capped, plus at most 20% jitter.
        for attempt in [6u32, 10, 30, 1000] {
            assert!(
                backoff(attempt) <= MAX_BACKOFF + MAX_BACKOFF / 5,
                "attempt {attempt} blew past the cap"
            );
        }
    }

    #[test]
    fn parses_the_servers_print_job_payload_verbatim() {
        // Copied from `printJobPayload` in realtime/socket.ts. Renaming a field
        // on either side must break here, not in a kitchen.
        let raw = json!({
            "jobId": "0f1a5b7c-1111-2222-3333-444455556666",
            "role": "receipt",
            "kind": "receipt",
            "osPrinterName": "GourmelyFake",
            "escposBase64": "G0AaVGVzdAo=",
            "copies": 2,
            "stationId": "68f8c6fe-cb2a-4e97-b648-b0c12491df17",
            "orderId": null,
            "createdAt": "2026-08-13T01:52:06.905Z"
        });
        let job: PrintJob = serde_json::from_value(raw).expect("parse");
        assert_eq!(job.job_id, "0f1a5b7c-1111-2222-3333-444455556666");
        assert_eq!(job.os_printer_name.as_deref(), Some("GourmelyFake"));
        assert_eq!(job.copies, 2);
        assert!(job.order_id.is_none());
    }

    #[test]
    fn a_job_without_a_printer_name_is_still_a_valid_job() {
        // Role-room delivery leaves `osPrinterName` null and the default
        // printer covers it. Rejecting the payload would drop the ticket.
        let job: PrintJob = serde_json::from_value(json!({
            "jobId": "abc",
            "escposBase64": "",
            "osPrinterName": null
        }))
        .expect("parse");
        assert!(job.os_printer_name.is_none());
        assert_eq!(job.copies, 1, "copies defaults to one, never zero");
    }

    /// The bug, in the shape it has in production.
    ///
    /// `flavor = "current_thread"` is the point: one worker thread, so a
    /// blocking spooler call made straight from the async task owns the only
    /// thread there is and **nothing else on the runtime runs** — the socket
    /// reader, the writer, the acks, all of it. Revert
    /// `printers_or_last_known` to calling the closure inline and the ticker
    /// below stays at zero.
    #[tokio::test(flavor = "current_thread")]
    async fn a_stalled_spooler_does_not_freeze_the_rest_of_the_relay() {
        let ticks = Arc::new(AtomicU64::new(0));
        let ticker = {
            let ticks = ticks.clone();
            tokio::spawn(async move {
                loop {
                    ticks.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
        };

        let printers = printers_or_last_known(Vec::new(), Duration::from_secs(5), || {
            // A driver whose printer is switched off, in one line.
            std::thread::sleep(Duration::from_millis(400));
            Ok(vec!["Caja".to_string()])
        })
        .await;
        ticker.abort();

        assert_eq!(printers, vec!["Caja".to_string()]);
        let observed = ticks.load(Ordering::SeqCst);
        assert!(
            observed > 5,
            "the runtime was frozen while the spooler was busy: only {observed} ticks in 400 ms"
        );
    }

    /// A spooler that never answers must cost the printer list, never the
    /// station's presence: falling outside the server's three-minute window
    /// makes it route the ticket down the local path instead.
    #[tokio::test]
    async fn a_spooler_that_never_answers_beats_with_the_last_list() {
        let previous = vec!["Caja".to_string()];
        let started = Instant::now();
        let printers = printers_or_last_known(previous.clone(), Duration::from_millis(100), || {
            std::thread::sleep(Duration::from_millis(700));
            Ok(vec!["never arrives".to_string()])
        })
        .await;
        assert_eq!(printers, previous, "the beat must keep the list it had");
        assert!(
            started.elapsed() < Duration::from_millis(600),
            "the beat waited for the spooler instead of giving up: {:?}",
            started.elapsed()
        );
    }

    /// An enumeration that fails outright is the same story as one that hangs.
    #[tokio::test]
    async fn a_failing_enumeration_keeps_the_previous_list() {
        let previous = vec!["Caja".to_string()];
        let printers = printers_or_last_known(previous.clone(), Duration::from_secs(5), || {
            Err(crate::error::BridgeError::SpoolerFailed(
                "RPC_S_SERVER_UNAVAILABLE".into(),
            ))
        })
        .await;
        assert_eq!(printers, previous);
    }

    #[test]
    fn the_ack_frame_is_exactly_what_the_server_listens_for() {
        let frame = engineio::event_packet(
            NAMESPACE,
            EV_PRINT_RESULT,
            &json!({ "jobId": "j1", "ok": true, "error": null }),
        );
        assert_eq!(
            frame,
            r#"42/print,["print-result",{"error":null,"jobId":"j1","ok":true}]"#
        );
    }

    // ── The printer list across a reconnection (P3) ──────────────────────

    /// The cache has to outlive the beat task, because `run_connection` builds
    /// a new one for every connection.
    ///
    /// Move `last_known` back inside the task — the shape this had before — and
    /// the second call below starts from an empty list and returns one.
    #[tokio::test]
    async fn the_last_known_printer_list_survives_a_new_beat_task() {
        let beat = beat_with(Duration::from_millis(150), answers_once_then_hangs());

        assert_eq!(
            beat.refresh().await,
            vec!["Caja".to_string()],
            "the first beat reads the spooler"
        );
        // The socket dropped and `run_connection` spawned a brand-new hello
        // task. The spooler is wedged now, so the fallback is all there is.
        assert_eq!(
            beat.refresh().await,
            vec!["Caja".to_string()],
            "the reconnect wiped the last known list; this station now reports no printers"
        );
        assert_eq!(beat.snapshot(), vec!["Caja".to_string()]);
    }

    /// Captures the `station-hello` of every connection, and hangs up on the
    /// first one so the relay has to come back.
    struct HelloSpy {
        addr: SocketAddr,
        hellos: Arc<StdMutex<Vec<serde_json::Value>>>,
    }

    impl HelloSpy {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("bind the hello spy");
            let addr = listener.local_addr().expect("local addr");
            let hellos = Arc::new(StdMutex::new(Vec::new()));

            let captured = hellos.clone();
            tokio::spawn(async move {
                let mut connection = 0usize;
                while let Ok((stream, _)) = listener.accept().await {
                    let captured = captured.clone();
                    connection += 1;
                    // Hang up on the first connection only: the second has to
                    // stay open long enough to be read.
                    let hang_up = connection == 1;
                    tokio::spawn(async move { Self::serve(stream, captured, hang_up).await });
                }
            });
            Self { addr, hellos }
        }

        fn url(&self) -> String {
            format!("http://{}", self.addr)
        }

        fn hellos(&self) -> Vec<serde_json::Value> {
            match self.hellos.lock() {
                Ok(g) => g.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            }
        }

        async fn serve(
            stream: tokio::net::TcpStream,
            hellos: Arc<StdMutex<Vec<serde_json::Value>>>,
            hang_up: bool,
        ) {
            let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let (mut sink, mut source) = ws.split();
            // A long ping interval: heartbeats are not what this test watches.
            if sink
                .send(Message::text(
                    r#"0{"sid":"spy","upgrades":[],"pingInterval":300000,"pingTimeout":200000,"maxPayload":1000000}"#,
                ))
                .await
                .is_err()
            {
                return;
            }
            while let Some(Ok(msg)) = source.next().await {
                let Message::Text(text) = msg else { continue };
                let text = text.to_string();
                if text.starts_with("40/print,") {
                    if sink
                        .send(Message::text(r#"40/print,{"sid":"spy-print"}"#))
                        .await
                        .is_err()
                    {
                        return;
                    }
                } else if text.contains("\"station-hello\"") {
                    if let Some((_, body)) = text.split_once(',') {
                        if let Ok(serde_json::Value::Array(mut args)) =
                            serde_json::from_str::<serde_json::Value>(body)
                        {
                            if args.len() == 2 {
                                let payload = args.remove(1);
                                match hellos.lock() {
                                    Ok(mut g) => g.push(payload),
                                    Err(poisoned) => poisoned.into_inner().push(payload),
                                }
                            }
                        }
                    }
                    if hang_up {
                        // The router rebooted.
                        let _ = sink.close().await;
                        return;
                    }
                }
            }
        }
    }

    /// The whole of P3, through the real wiring: connect, beat, lose the
    /// socket, come back — and the station must still announce the printer it
    /// found the first time, even though the spooler has gone quiet since.
    ///
    /// This is the one that would have caught it. The unit test above proves
    /// the cache remembers; only this proves `run_connection` is holding the
    /// cache that remembers.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_station_hello_keeps_announcing_printers_after_a_reconnect() {
        let spy = HelloSpy::start().await;
        let ledger = temp_dir("hello-reconnect").join("print-jobs.jsonl");

        let settings = SettingsStore::in_memory(Settings {
            server_url: Some(spy.url()),
            station_token: Some("station-token-under-test".into()),
            ..Default::default()
        });
        let handle = spawn_with(
            settings,
            ledger,
            beat_with(Duration::from_millis(150), answers_once_then_hangs()),
        );

        let two = wait_for(Duration::from_secs(20), || spy.hellos().len() >= 2).await;
        let hellos = spy.hellos();
        println!("[p3] hellos captured: {hellos:#?}");
        assert!(
            two,
            "the relay never reconnected after the hang-up: {:?} / {:?}",
            handle.status(),
            hellos
        );

        assert_eq!(
            hellos[0]["printers"],
            json!(["Caja"]),
            "the first connection did not read the spooler at all"
        );
        assert_eq!(
            hellos[1]["printers"],
            json!(["Caja"]),
            "after reconnecting, the station told the server it has NO printers — \
             the server stops routing tickets to a station with an empty list"
        );
    }

    // ── Standing down for another bridge (P2) ────────────────────────────

    /// A relay that cannot take the ledger must not connect, must say so, and
    /// must keep trying — the three things the old `return` gave up on.
    ///
    /// The recovery half (the other copy closes and this one comes back by
    /// itself) needs a socket to prove, so it lives in
    /// `tests/standby_recovery.rs`.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_bridge_that_cannot_take_the_ledger_says_so_and_keeps_trying() {
        let dir = temp_dir("standby");
        let ledger = dir.join("print-jobs.jsonl");

        // The bridge that got there first.
        let held = crate::instance_lock::InstanceLock::acquire(
            crate::job_ledger::lock_path_for(&ledger),
        )
        .expect("the first bridge takes the lock");

        let settings = SettingsStore::in_memory(Settings {
            server_url: Some("http://127.0.0.1:1".into()),
            station_token: Some("station-token-under-test".into()),
            ..Default::default()
        });
        let handle = spawn_with(
            settings,
            ledger,
            beat_with(Duration::from_secs(1), || Ok(vec!["Caja".into()])),
        );

        let noticed = wait_for(Duration::from_secs(10), || {
            handle.status().blocked_by_another_instance
        })
        .await;
        let status = handle.status();
        println!("[p2] standby status: {status:?}");
        assert!(
            noticed,
            "the bridge stood down without telling anyone: {status:?}"
        );
        assert!(!status.connected, "it connected anyway");
        assert!(
            !status.token_rejected,
            "this is not a token problem and must not be shown as one"
        );
        let reason = status.last_error.clone().unwrap_or_default();
        assert!(
            reason.contains("ya está abierto"),
            "the message must name what to close: {reason:?}"
        );

        // Still alive, and the only honest way to ask.
        //
        // A flag that stays set proves nothing — a supervisor that returned
        // leaves it set too, which is exactly how this state fools everyone
        // looking at it. So: let go of the lock and watch whether anything
        // picks it up. Nothing nudges the relay here; if it needs one it is not
        // fixed.
        drop(held);
        let took_it = wait_for(Duration::from_secs(30), || {
            !handle.status().blocked_by_another_instance
        })
        .await;
        assert!(
            took_it,
            "nothing took the ledger after it was freed: the supervisor is gone, and this \
             bridge will not print again until the process is restarted — {:?}",
            handle.status()
        );

        // Terminal check: it is not merely reporting recovery, it is holding
        // the lock. A third claim on it has to be refused.
        assert!(
            matches!(
                crate::instance_lock::InstanceLock::acquire(crate::job_ledger::lock_path_for(
                    &dir.join("print-jobs.jsonl")
                )),
                Err(BridgeError::AlreadyRunning(_))
            ),
            "the relay cleared its standby flag without actually taking the ledger"
        );
    }

    #[test]
    fn the_ledger_wait_is_capped_shorter_than_the_network_one() {
        // Somebody is standing at the till waiting for this one.
        for attempt in [0u32, 3, 6, 50, 1000] {
            assert!(
                backoff_to(attempt, LEDGER_RETRY_MAX) <= LEDGER_RETRY_MAX + LEDGER_RETRY_MAX / 5,
                "attempt {attempt} blew past the ledger cap"
            );
        }
        assert!(LEDGER_RETRY_MAX < MAX_BACKOFF);
        // And the network backoff is unchanged by the refactor.
        assert!(backoff(0) >= BASE_BACKOFF);
        assert!(backoff(10) <= MAX_BACKOFF + MAX_BACKOFF / 5);
    }
}
