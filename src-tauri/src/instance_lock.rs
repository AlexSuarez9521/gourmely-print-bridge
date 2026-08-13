//! One bridge per till, enforced by the operating system.
//!
//! # The failure this exists to make impossible
//!
//! Before the relay, a second copy of the bridge was harmless by accident: it
//! could not take port 8181, so it sat there inert and printed nothing. The
//! relay removed that accident. An outbound socket needs no port, so the second
//! copy authenticates with the same station token, joins the same room, and the
//! server — which has no way to tell two sockets of one till apart — pushes
//! every job down both. **Two processes, one job, two tickets.**
//!
//! And the trigger is an ordinary Tuesday: the X button hides the window to the
//! tray, autostart already launched one at login, and the cashier double-clicks
//! the desktop shortcut because nothing seems to be running.
//!
//! [`crate::job_ledger`] cannot save this on its own. It is a `HashMap` over a
//! shared file: two processes each keep their own map, each sees a job it has
//! never recorded, and both print it. Idempotency inside one process says
//! nothing about two.
//!
//! # Why a file lock, and not a PID file
//!
//! A PID file is a promise the process has to keep. This one is kept by the
//! kernel: the lock is a property of an open handle, so it is released when the
//! handle closes **including when the process is killed, crashes, or the till
//! loses power**. There is no stale state to clean up and no liveness check to
//! get wrong. `tauri-plugin-single-instance` is the other net (see
//! [`crate::run`]); this one is the one that still holds when the app is not
//! running under Tauri at all — an integration test, a support build, a dev
//! `cargo run` next to an installed copy.
//!
//! # Why the lock file is separate from the ledger, and stays empty
//!
//! On Windows `LockFileEx` is a *mandatory* byte-range lock. While it is held,
//! reading the same bytes through any other handle fails with
//! `ERROR_LOCK_VIOLATION` (os error 33) — verified on this platform. Locking
//! `print-jobs.jsonl` itself would therefore make the ledger unreadable to the
//! very code that has to read it. So the lock sits on a file of its own that
//! nothing ever reads, and the identity of the holder goes into a plain sidecar
//! next to it, which anyone can read.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{BridgeError, BridgeResult};

/// Appended to the lock file's name to get the sidecar that names the holder.
const OWNER_SUFFIX: &str = ".owner";

/// An exclusive, OS-level claim on one resource, held for as long as the value
/// lives.
///
/// Dropping it releases the lock; so does the process ending, by any route.
pub struct InstanceLock {
    owner_path: PathBuf,
    /// The handle **is** the lock. Keeping it alive is the entire mechanism,
    /// which is why it is a field nothing ever reads.
    _file: File,
}

impl InstanceLock {
    /// Takes the lock at `path`, or explains who already has it.
    ///
    /// Never blocks: a bridge that waited here would hang at startup behind a
    /// copy of itself that is running perfectly well.
    pub fn acquire(path: impl AsRef<Path>) -> BridgeResult<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // `truncate(false)`: the file's content is irrelevant, and truncating it
        // would be a write to a file another process may hold locked.
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)?;

        let owner_path = owner_path_for(path);
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(BridgeError::AlreadyRunning(held_by(&owner_path)))
            }
            Err(TryLockError::Error(e)) => return Err(BridgeError::Io(e)),
        }

        // Best effort from here on: the lock is already ours, and failing to
        // write a support hint is not a reason to refuse to print.
        record_owner(&owner_path);
        Ok(Self {
            owner_path,
            _file: file,
        })
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Tidy up so a later reader cannot mistake a dead process for a live
        // one. Only reachable on a graceful exit — after a kill the sidecar
        // survives, which is harmless: it is read only while the lock is held,
        // and whoever holds it rewrote it on the way in.
        let _ = std::fs::remove_file(&self.owner_path);
    }
}

fn owner_path_for(lock: &Path) -> PathBuf {
    let mut name = lock.as_os_str().to_os_string();
    name.push(OWNER_SUFFIX);
    PathBuf::from(name)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Writes `<pid> <unix seconds>` beside the lock, for the message the loser
/// shows and for a support log.
fn record_owner(path: &Path) {
    let line = format!("{} {}\n", std::process::id(), now_secs());
    let written = File::create(path).and_then(|mut f| f.write_all(line.as_bytes()));
    if let Err(e) = written {
        tracing::debug!(path = %path.display(), err = %e, "could not note who holds the instance lock");
    }
}

/// The sentence the second instance shows. Written for whoever is standing at
/// the till, so it says what to do, not what went wrong.
///
/// The pid is a bonus, not a promise: two copies launched in the same instant
/// can have the loser read the sidecar before the winner has written it. The
/// sentence has to work without it, so it is a parenthesis and never the
/// subject. And it says "no va a imprimir" rather than "se cerró", because both
/// callers use it — Tauri does end the second process, the relay only stands
/// down — and a message that claims the wrong one sends support the wrong way.
fn held_by(owner_path: &Path) -> String {
    let who = std::fs::read_to_string(owner_path)
        .ok()
        .and_then(|raw| raw.split_whitespace().next().map(str::to_string))
        .filter(|pid| !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit()))
        .map(|pid| format!(" (proceso {pid})"))
        .unwrap_or_default();
    format!(
        "GourmelyPrint Bridge ya está abierto en este equipo{who}. \
         Esta copia no va a imprimir: dos bridges en la misma caja sacan cada ticket dos veces. \
         Usa el que ya está funcionando desde el icono junto al reloj."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_lock(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gmly-lock-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("print-jobs.lock")
    }

    /// The whole point, in three lines.
    ///
    /// Two handles on one lock file conflict on both platforms the bridge runs
    /// on — Windows `LockFileEx` locks the file, POSIX `flock` locks the open
    /// file description — so a second *handle* stands in for a second *process*
    /// and the guarantee is testable without spawning one.
    #[test]
    fn a_second_claim_is_refused_while_the_first_is_alive() {
        let path = temp_lock("second");
        let first = InstanceLock::acquire(&path).expect("first claim");
        match InstanceLock::acquire(&path) {
            Err(BridgeError::AlreadyRunning(msg)) => {
                assert!(
                    msg.contains("ya está abierto"),
                    "the message must tell the operator what happened: {msg}"
                );
            }
            Err(other) => panic!("wrong error: {other}"),
            Ok(_) => panic!("TWO instances took the same lock — every ticket would print twice"),
        }
        drop(first);
    }

    /// Releasing has to be automatic, or a crash at the till locks printing out
    /// until someone deletes a file nobody has heard of.
    #[test]
    fn the_lock_is_released_when_the_holder_goes_away() {
        let path = temp_lock("released");
        {
            let _held = InstanceLock::acquire(&path).expect("first claim");
        }
        InstanceLock::acquire(&path).expect("the next bridge must be able to start");
    }

    #[test]
    fn the_message_names_the_process_that_has_it() {
        let path = temp_lock("names");
        let _held = InstanceLock::acquire(&path).expect("first claim");
        let Err(BridgeError::AlreadyRunning(msg)) = InstanceLock::acquire(&path) else {
            panic!("the second claim must fail");
        };
        assert!(
            msg.contains(&format!("proceso {}", std::process::id())),
            "support needs to know which process to close: {msg}"
        );
    }

    /// The sidecar is a hint, not a dependency: a bridge whose data directory
    /// was wiped mid-run must still refuse the second instance.
    #[test]
    fn a_missing_owner_note_still_refuses_the_second_instance() {
        let path = temp_lock("noowner");
        let _held = InstanceLock::acquire(&path).expect("first claim");
        let _ = std::fs::remove_file(owner_path_for(&path));
        assert!(matches!(
            InstanceLock::acquire(&path),
            Err(BridgeError::AlreadyRunning(_))
        ));
    }
}
