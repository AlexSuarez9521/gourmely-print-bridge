//! A bridge that lost the print ledger stands down, says so, and comes back on
//! its own the moment the ledger is free.
//!
//! # The failure this exists to keep out
//!
//! Standing down was already right: two relays authenticate with the same
//! station token, land in the same room, and the server pushes every job down
//! both — one order, two comandas. What was wrong is that the supervisor
//! **returned**. Nothing restarted it, and `RelayHandle::reconnect` only pokes a
//! `Notify` with no listener left, so re-pairing from Ajustes did not revive it
//! either.
//!
//! That leaves the worst shape a bridge can have: a live process, an icon by
//! the clock, a settings window that answers — and no ticket, ever again, until
//! somebody restarts it. Nobody finds out until a customer asks for a receipt.
//!
//! And it is reachable in production. `tauri-plugin-single-instance` only kills
//! the second copy once `FindWindowW` finds the first one's window; a copy that
//! starts before that window exists, or under another user, or outside Tauri,
//! survives and loses the lock.
//!
//! # What is asserted, in order
//!
//! 1. Blocked: it never joins the room, and **nothing prints** — the safety
//!    property, unchanged.
//! 2. It says why, in the words the till can act on.
//! 3. The other copy closes. Nobody touches this one.
//! 4. It connects by itself and prints the next ticket.
//!
//! Step 4 is the point. Comment out the retry in `acquire_ledger` and steps 1
//! and 2 still pass — it is the ticket at the end that says the bridge is alive
//! rather than merely quiet.
//!
//! # Why the "other copy" is a lock and not a second relay
//!
//! The lock is taken per open *handle* on both platforms the bridge runs on
//! (Windows `LockFileEx`, POSIX `flock`), so holding it directly is exactly
//! what a second process does to this one — and unlike a second relay, the test
//! can let go of it at the moment it chooses, which is the whole of step 3.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use print_bridge_lib::{
    config::{Settings, SettingsStore},
    instance_lock::InstanceLock,
    job_ledger::lock_path_for,
    relay,
};

const TICKET: &[u8] = b"\x1b@GourmelyPrint\n\n\n\x1dV\x01";

/// Generous: after a few refused attempts the retry has backed off to the
/// order of ten seconds, and the point is that it arrives, not how fast.
const RECOVERY_BUDGET: Duration = Duration::from_secs(60);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gmly-standby-{name}"));
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
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

fn tickets_in(sink: &PathBuf) -> Vec<PathBuf> {
    std::fs::read_dir(sink)
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// A stand-in for GourmelyHub's `/print` namespace: accepts stations and pushes
/// a job to everyone in the room.
#[derive(Clone)]
struct FakeHub {
    addr: SocketAddr,
    room: Arc<Mutex<Vec<mpsc::UnboundedSender<String>>>>,
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
            if text.to_string().starts_with("40/print,") {
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
            }
        }
        writer.abort();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bridge_that_lost_the_ledger_stands_down_and_recovers_by_itself() {
    let data_dir = scratch("data");
    let sink_dir = scratch("sink");
    std::env::set_var("GOURMELYPRINT_DATA_DIR", &data_dir);
    std::env::set_var("GOURMELYPRINT_SINK_DIR", &sink_dir);

    let hub = FakeHub::start().await;
    println!("[standby] fake hub on {}", hub.addr);

    // ── The bridge that got there first ──────────────────────────────────
    // Only its lock, which is the whole of what it does to this one.
    let ledger_path = data_dir.join("print-jobs.jsonl");
    let other_bridge =
        InstanceLock::acquire(lock_path_for(&ledger_path)).expect("the other bridge takes the lock");
    println!("[standby] another bridge holds {}", ledger_path.display());

    let bridge = relay::spawn(SettingsStore::in_memory(Settings {
        server_url: Some(hub.url()),
        station_token: Some("station-token-under-test".into()),
        default_printer: Some("GourmelyFake".into()),
        ..Default::default()
    }));

    // ── 1 & 2. It stands down, and it says why ───────────────────────────
    let stood_down = {
        let bridge = bridge.clone();
        wait_for(Duration::from_secs(20), move || {
            bridge.status().blocked_by_another_instance
        })
        .await
    };
    let status = bridge.status();
    println!("[standby] blocked: {status:?}");
    assert!(
        stood_down,
        "the bridge never reported standing down: {status:?}"
    );
    assert!(!status.connected, "it connected while another copy had the ledger");
    assert_eq!(
        hub.in_room(),
        0,
        "the server has a socket for a bridge that must not print"
    );
    let reason = status.last_error.clone().unwrap_or_default();
    assert!(
        reason.contains("ya está abierto"),
        "the operator is given nothing to act on: {reason:?}"
    );
    assert!(
        !status.token_rejected,
        "shown as a token problem, which sends support to the wrong panel"
    );

    // And a job routed to this till right now must not print here.
    hub.push_job("job-while-blocked");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        tickets_in(&sink_dir).is_empty(),
        "a bridge without the ledger printed anyway: {:?}",
        tickets_in(&sink_dir)
    );

    // ── 3. The other copy is closed. Nothing else is touched. ────────────
    println!("[standby] closing the other bridge");
    drop(other_bridge);

    // ── 4. It comes back on its own, and prints ──────────────────────────
    // No reconnect(), no re-pairing, no restart: if this needs a nudge it is
    // not fixed.
    let recovered = {
        let bridge = bridge.clone();
        wait_for(RECOVERY_BUDGET, move || bridge.status().connected).await
    };
    let status = bridge.status();
    println!("[standby] recovered: {status:?}");
    assert!(
        recovered,
        "the bridge never came back after the other one closed — a live process \
         that will not print again until someone restarts it: {status:?}"
    );
    assert!(
        !status.blocked_by_another_instance,
        "it connected but still reports standing down"
    );
    assert_eq!(hub.in_room(), 1, "it is not in the room the server pushes to");

    let pushed = hub.push_job("job-after-recovery");
    println!("[standby] pushed a job to {pushed} socket(s)");
    let printed = {
        let sink = sink_dir.clone();
        wait_for(Duration::from_secs(20), move || !tickets_in(&sink).is_empty()).await
    };
    let tickets = tickets_in(&sink_dir);
    assert!(
        printed,
        "it reported connected but never printed: {:?}",
        bridge.status()
    );
    assert_eq!(
        tickets.len(),
        1,
        "expected exactly the ticket pushed after recovery, got {tickets:?}"
    );
    assert_eq!(
        std::fs::read(&tickets[0]).expect("read ticket"),
        TICKET,
        "the ESC/POS did not survive the round trip"
    );

    // The one pushed while it was blocked was never claimed — the server still
    // has it and will replay it, which is the correct outcome.
    let ledger = std::fs::read_to_string(&ledger_path).expect("ledger");
    println!("[standby] ledger:\n{ledger}");
    assert!(
        !ledger.contains("job-while-blocked"),
        "the standing-down bridge claimed a job it never printed:\n{ledger}"
    );
    assert!(ledger.contains("job-after-recovery"), "ledger:\n{ledger}");
}
