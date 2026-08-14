//! Two bridges on one till, one print job, one ticket.
//!
//! # The regression this is here to keep out
//!
//! Reproduced by the verifier against the real API: two relay processes alive
//! at once, a single job enqueued, **two ESC/POS files on disk** and the same
//! `jobId` claimed twice. The relay is what made it reachable — an outbound
//! socket needs no port to fight over, so where a second copy used to fail to
//! take 8181 and sit there inert, it now authenticates with the same station
//! token, joins the same room, and the server pushes every job down both.
//!
//! # Why this runs without GourmelyHub
//!
//! It stands up its own Engine.IO v4 / Socket.IO v5 endpoint — forty lines,
//! speaking the frames [`print_bridge_lib::engineio`] already documents — and
//! does the one thing the real server does that matters here: **push the job to
//! every socket that joined `/print`**. That is not a simplification, it is the
//! mechanism. A server cannot tell two sockets of one till apart, so the fix has
//! to be that the second bridge never connects.
//!
//! Two live relays in one process rather than two processes, because the lock
//! is taken per open *handle* on both platforms the bridge runs on (Windows
//! `LockFileEx`, POSIX `flock`), so a second handle is a faithful stand-in for a
//! second process and the test stays fast and hermetic. The two-process version
//! of the same proof is in the PR description, run against the live API.
//!
//! # It bites
//!
//! Take the `InstanceLock::acquire` line out of `JobLedger::open` and this test
//! fails on the assertion it was written for: two sockets in the room, two
//! files in the sink, two claims in the ledger.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use print_bridge_lib::{
    config::{Settings, SettingsStore},
    relay,
};

/// The ESC/POS a real ticket starts with, so the assertion checks the payload
/// survived the round trip and not merely that a file appeared.
const TICKET: &[u8] = b"\x1b@GourmelyPrint\n\n\n\x1dV\x01";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gmly-two-instances-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
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

/// A stand-in for GourmelyHub's `/print` namespace.
#[derive(Clone)]
struct FakeHub {
    addr: SocketAddr,
    /// One sender per socket that completed the namespace handshake — i.e. the
    /// room. Its length is the number of bridges the server would push to.
    room: Arc<Mutex<Vec<mpsc::UnboundedSender<String>>>>,
    acks: Arc<Mutex<Vec<serde_json::Value>>>,
    hellos: Arc<AtomicUsize>,
}

impl FakeHub {
    async fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind the fake hub");
        let addr = listener.local_addr().expect("local addr");
        let hub = Self {
            addr,
            room: Arc::new(Mutex::new(Vec::new())),
            acks: Arc::new(Mutex::new(Vec::new())),
            hellos: Arc::new(AtomicUsize::new(0)),
        };

        let accepting = hub.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let hub = accepting.clone();
                tokio::spawn(async move { hub.serve(stream).await });
            }
        });
        hub
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn in_room(&self) -> usize {
        self.room.lock().map(|r| r.len()).unwrap_or(0)
    }

    fn acks(&self) -> Vec<serde_json::Value> {
        self.acks.lock().map(|a| a.clone()).unwrap_or_default()
    }

    /// What the server does when a ticket is routed to this station: one
    /// `print-job`, pushed to everyone in the room.
    fn push_job(&self, job_id: &str) -> usize {
        let frame = format!(
            r#"42/print,["print-job",{{"jobId":"{job_id}","role":"receipt","kind":"receipt","osPrinterName":"GourmelyFake","escposBase64":"{}","copies":1,"orderId":null}}]"#,
            BASE64.encode(TICKET)
        );
        let room = match self.room.lock() {
            Ok(r) => r,
            Err(poisoned) => poisoned.into_inner(),
        };
        for socket in room.iter() {
            let _ = socket.send(frame.clone());
        }
        room.len()
    }

    async fn serve(self, stream: tokio::net::TcpStream) {
        let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        let (mut sink, mut source) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();

        // OPEN. Long ping interval: this test is about who is in the room, and
        // a heartbeat firing mid-assertion is noise.
        if sink
            .send(Message::text(
                r#"0{"sid":"fake","upgrades":[],"pingInterval":300000,"pingTimeout":200000,"maxPayload":1000000}"#,
            ))
            .await
            .is_err()
        {
            return;
        }

        let writer = tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if sink.send(Message::text(frame)).await.is_err() {
                    break;
                }
            }
        });

        while let Some(Ok(msg)) = source.next().await {
            let Message::Text(text) = msg else { continue };
            let text = text.to_string();

            if text.starts_with("40/print,") {
                // Accept the station, then — and only then — put it in the
                // room, exactly as Socket.IO does after its middleware resolves.
                if tx
                    .send(r#"40/print,{"sid":"fake-print"}"#.to_string())
                    .is_err()
                {
                    break;
                }
                match self.room.lock() {
                    Ok(mut room) => room.push(tx.clone()),
                    Err(poisoned) => poisoned.into_inner().push(tx.clone()),
                }
            } else if text.contains("\"station-hello\"") {
                self.hellos.fetch_add(1, Ordering::SeqCst);
            } else if text.contains("\"print-result\"") {
                if let Some(body) = text.split_once(',').map(|(_, rest)| rest) {
                    if let Ok(serde_json::Value::Array(mut args)) =
                        serde_json::from_str::<serde_json::Value>(body)
                    {
                        if args.len() == 2 {
                            let payload = args.remove(1);
                            match self.acks.lock() {
                                Ok(mut acks) => acks.push(payload),
                                Err(poisoned) => poisoned.into_inner().push(payload),
                            }
                        }
                    }
                }
            }
        }
        writer.abort();
    }
}

fn paired_station(server_url: &str) -> SettingsStore {
    SettingsStore::in_memory(Settings {
        server_url: Some(server_url.to_string()),
        // The same token on both, because that is the real case: one install,
        // launched twice.
        station_token: Some("station-token-under-test".into()),
        default_printer: Some("GourmelyFake".into()),
        ..Default::default()
    })
}

fn tickets_in(sink: &PathBuf) -> Vec<PathBuf> {
    std::fs::read_dir(sink)
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn two_bridges_on_one_till_print_one_ticket() {
    let data_dir = scratch("data");
    let sink_dir = scratch("sink");
    // Both relays read these, which is the point: one install, one
    // %PROGRAMDATA%, one ledger — and now one lock.
    std::env::set_var("GOURMELYPRINT_DATA_DIR", &data_dir);
    std::env::set_var("GOURMELYPRINT_SINK_DIR", &sink_dir);

    let hub = FakeHub::start().await;
    println!("[two] fake hub on {}", hub.addr);

    // ── The bridge that was already running ─────────────────────────────
    // Staggered, not simultaneous, because that is the case that happens: the
    // autostart entry launched one at login and the till has been printing on
    // it all morning.
    let first = relay::spawn(paired_station(&hub.url()));
    let up = {
        let first = first.clone();
        wait_for(Duration::from_secs(20), move || first.status().connected).await
    };
    assert!(up, "the first bridge never connected: {:?}", first.status());
    println!("[two] first bridge connected: {:?}", first.status());

    // ── The cashier double-clicks the shortcut ──────────────────────────
    let second = relay::spawn(paired_station(&hub.url()));

    // Long enough for a second socket to reach the room if it is going to.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let (winner, loser) = (first.status(), second.status());
    println!("[two] still connected: {winner:?}");
    println!("[two] second launch:   {loser:?}");

    // ── One job ─────────────────────────────────────────────────────────
    // Pushed to whoever is in the room, exactly as the server does. How many
    // are in there is the whole question, so it is not asserted first: the
    // count of tickets below is the answer the restaurant cares about, and it
    // is the assertion that should name the failure.
    let pushed = hub.push_job("job-under-test");
    println!("[two] pushed one print-job to {pushed} socket(s) in the room");

    let printed = {
        let sink = sink_dir.clone();
        wait_for(Duration::from_secs(20), move || {
            !tickets_in(&sink).is_empty()
        })
        .await
    };
    assert!(printed, "nothing ever printed: {:?}", winner);

    // Long enough that a second ticket would have landed by now.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // ── One ticket, one record, one ack ─────────────────────────────────
    let tickets = tickets_in(&sink_dir);
    for path in &tickets {
        let bytes = std::fs::read(path).expect("read ticket");
        println!(
            "[two] {} ({} bytes)",
            path.file_name().unwrap_or_default().to_string_lossy(),
            bytes.len()
        );
        assert_eq!(bytes, TICKET, "the ESC/POS did not survive the round trip");
    }
    assert_eq!(
        tickets.len(),
        1,
        "ONE job produced {} tickets — this is the duplicate the fix exists to stop",
        tickets.len()
    );

    let ledger = std::fs::read_to_string(data_dir.join("print-jobs.jsonl")).expect("ledger");
    println!("[two] ledger:\n{ledger}");
    assert_eq!(
        ledger.matches("\"claimed\"").count(),
        1,
        "the job was claimed more than once:\n{ledger}"
    );
    assert_eq!(
        ledger.matches("\"printed\"").count(),
        1,
        "the job was recorded as printed more than once:\n{ledger}"
    );

    let acks = hub.acks();
    println!("[two] acks: {acks:?}");
    assert_eq!(acks.len(), 1, "the server was acked twice for one job");
    assert_eq!(acks[0]["jobId"], "job-under-test");
    assert_eq!(acks[0]["ok"], true);

    // ── Why there was only one ──────────────────────────────────────────
    // The diagnosis, after the symptom. If the count above is right for the
    // wrong reason — say the second bridge connected but its ledger happened
    // to answer from cache — these are what say so.
    assert_eq!(
        hub.in_room(),
        1,
        "the server has {} sockets for one till; every job it routes goes down all of them",
        hub.in_room()
    );
    assert!(!loser.connected, "the second bridge connected anyway");
    let reason = loser.last_error.clone().unwrap_or_default();
    assert!(
        reason.contains("ya está abierto"),
        "the second bridge must say why it stood down, in words the till can act on: {reason:?}"
    );

    // ── And the survivor is not crippled ────────────────────────────────
    // Standing down must not cost the till its printing: the bridge that won
    // the lock keeps working, presence and all.
    assert!(winner.connected, "the working bridge was knocked off");
    assert!(
        hub.hellos.load(Ordering::SeqCst) >= 1,
        "the connected bridge never sent presence; the server would route around it"
    );
    let after = first.status();
    assert_eq!(after.jobs_printed, 1);
    assert_eq!(after.jobs_failed, 0);
    assert_eq!(second.status().jobs_printed, 0);
}
