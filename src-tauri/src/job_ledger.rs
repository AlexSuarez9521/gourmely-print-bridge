//! On-disk record of which print jobs this station has already taken.
//!
//! # Why it exists, and why the order of operations is the whole point
//!
//! The server's queue is durable and re-delivers: a job that was emitted to a
//! bridge that vanished without acking stays claimable and comes back on the
//! next `connection` (`claimReplayableJobs` re-sends `pending` **and** `sent`).
//! That is the right call server-side — the alternative is losing a comanda in
//! silence — but it means the bridge is the component that has to make delivery
//! idempotent.
//!
//! In-memory is not enough. The interesting crash is the bridge dying (power
//! cut at the till, Windows update, someone closing it) *between* printing and
//! recording, and a `HashSet` dies with the process. So the ledger is a file.
//!
//! # And a file is not enough either, if two processes share it
//!
//! Everything below is a `HashMap` in front of that file, and a map is private
//! to its process. Two bridges running at once each keep their own, each sees a
//! job neither has recorded, and **both print it** — the relay made that
//! reachable, because an outbound socket needs no port to fight over. Worse,
//! [`JobLedger::compact`] used to rewrite the whole file from its own map, so
//! one process's housekeeping erased the other's idempotency records and the
//! risk of reprinting outlived the second instance.
//!
//! So opening the ledger now takes an exclusive [`InstanceLock`] beside it and
//! holds it for as long as the ledger lives. A second bridge cannot open the
//! ledger, and the relay will not connect without one — which is the point:
//! the server never has two sockets to push to. It keeps asking for it, so the
//! copy that lost comes back by itself once the other is closed.
//!
//! **The job id is written BEFORE the bytes reach the spooler.** That ordering
//! is deliberate and it is not the obvious one:
//!
//! * record-after-print: a crash in the window leaves no trace, the server
//!   re-delivers, and **the ticket prints twice**. Nobody sees the duplicate
//!   until a customer gets two receipts or the kitchen cooks the order twice.
//! * record-before-print: a crash in the window leaves a claim with no result,
//!   the bridge refuses to reprint it and acks a failure, and **the ticket may
//!   not have come out**. That failure is visible — it lands in `print:status`
//!   and someone reprints on purpose.
//!
//! Trading an invisible duplicate for a visible miss is the trade R8 asks for.
//!
//! # Format
//!
//! Append-only JSONL. One line per state change, last line wins. Appending is
//! O(1) and survives a torn write (a truncated final line is dropped on load),
//! which a rewrite-the-whole-file design does not. Compaction — which also
//! evicts anything older than [`RETENTION`] — happens on load and whenever the
//! file grows past [`COMPACT_AT_ENTRIES`], so the file cannot grow without end.
//!
//! # A damaged file must not be a permanent outage
//!
//! Everything above is about not printing twice. This part is about printing at
//! all. [`JobLedger::open`] is the first thing `relay::supervise` does, and
//! `acquire_ledger` retries it for ever — deliberately, because the usual reason
//! it fails is a second bridge that somebody is about to close. But the retry is
//! only right for a condition that **goes away on its own**, and a file that
//! cannot be read is not one: it fails identically every fifteen seconds until
//! the end of time, with the process alive, the tray icon normal and `/health`
//! answering. That was reproduced on the real binary — a ledger with a non-UTF-8
//! tail, the shape a dirty shutdown leaves on NTFS, and the relay never joined
//! the room again.
//!
//! So loading is written to give ground in the smallest steps it can:
//!
//! 1. **A line it cannot read is skipped, and the rest is kept.** Bad bytes and
//!    bad JSON get the same treatment the torn tail always got. Nothing else in
//!    the file is worth losing over one damaged record, and every id that
//!    survives is a ticket that will not print twice.
//! 2. **Only when the file cannot be read at all** — an I/O error, not a
//!    decoding one — is it renamed aside as `…jsonl.corrupt-<unix>` and a fresh
//!    one started. This is the step that costs something: the ids in that file
//!    are gone, so a job the server still has in `sent` can come back and print a
//!    second time. It is chosen anyway, because the alternative is a till that
//!    never prints again and says nothing, and because a duplicate ticket is
//!    visible on paper within minutes while a mute bridge is found by a customer
//!    asking for a receipt. The file is preserved, never deleted, so the record
//!    can still be read by hand.
//! 3. **If even the rename fails**, `open` still errors and the relay keeps
//!    retrying — that one really is a condition a human can clear (permissions,
//!    antivirus, a full disk), and it now shows up as its own state instead of
//!    "Reconectando…".

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{BridgeError, BridgeResult};
use crate::instance_lock::InstanceLock;

/// How long a job id is remembered.
///
/// The server's most generous TTL is 30 minutes (`TTL_MS_BY_KIND.comanda`) and
/// it never replays an expired job, so anything older than that can no longer
/// come back. A day of margin covers clock skew and a bridge that was off for a
/// while, and still keeps the file to a few hundred lines.
const RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// Compact once the file passes this many lines. A busy till writes two lines
/// per ticket, so this is roughly a thousand tickets.
const COMPACT_AT_ENTRIES: usize = 2_000;

/// After a compaction fails, how many more lines to accept before trying
/// again.
///
/// Without it a till whose disk is full retries a full read-and-rewrite of an
/// ever-growing file on *every* ticket, which is the worst moment to add I/O.
/// Roughly a hundred tickets is often enough to pick the disk back up soon
/// after somebody clears it, and rare enough to cost nothing while it is
/// broken.
const COMPACT_RETRY_AFTER: usize = 200;

/// What the bridge knows about one job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobState {
    /// Claimed and handed to the spooler; no outcome recorded yet. Seeing this
    /// on a *replay* means the bridge died mid-print.
    Claimed,
    Printed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    #[serde(rename = "jobId")]
    job_id: String,
    state: JobState,
    /// Unix seconds. Stored rather than derived from the file mtime so
    /// compaction can evict per entry.
    at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// What a damaged ledger cost on the way in, when it cost anything.
///
/// Kept on the open ledger — rather than only logged — because the two cases
/// below have different consequences for the restaurant and somebody has to be
/// able to find out which one happened without reading a log file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerRecovery {
    /// Lines that were not readable text were dropped; every other record was
    /// kept. The reprint risk is limited to the jobs those lines named.
    Salvaged { lines: usize },
    /// The file could not be read at all, so it was renamed aside and a new one
    /// started. Every id it held is gone: a job the server has not seen acked
    /// can print a second time.
    SetAside { kept: PathBuf, reason: String },
}

impl LedgerRecovery {
    /// The sentence for whoever is looking at the till, not for the log.
    pub fn message(&self) -> String {
        match self {
            Self::Salvaged { lines } => format!(
                "El registro de impresiones tenía {lines} línea(s) ilegibles y se descartaron. \
                 El resto se conservó: si alguno de esos trabajos vuelve del servidor, podría \
                 imprimirse otra vez."
            ),
            Self::SetAside { kept, .. } => format!(
                "El registro de impresiones no se podía leer. Se guardó una copia en {} y se \
                 empezó uno nuevo para no dejar la caja sin imprimir; algún ticket reciente \
                 podría salir por segunda vez.",
                kept.display()
            ),
        }
    }
}

/// The answer to "have I seen this job before?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// First time. Print it.
    Fresh,
    /// Already finished. Re-ack with this outcome; do NOT print again.
    AlreadyDone { ok: bool, error: Option<String> },
    /// Claimed but never finished — the bridge died mid-print. Do NOT print
    /// again (that is the duplicate we refuse to risk) and ack a failure so a
    /// human decides.
    Interrupted,
}

pub struct JobLedger {
    path: PathBuf,
    /// Cross-process exclusion, held for as long as this ledger exists.
    ///
    /// Nothing reads it: its whole job is to make [`JobLedger::open`] fail in a
    /// second bridge. Every method below therefore runs under it by
    /// construction, `compact` included — which is what makes the rewrite safe.
    _lock: InstanceLock,
    seen: HashMap<String, Entry>,
    /// Lines currently in the file, to decide when to compact.
    lines: usize,
    /// Line count past which the next compaction is attempted. Normally
    /// [`COMPACT_AT_ENTRIES`]; pushed out by [`COMPACT_RETRY_AFTER`] each time
    /// one fails, so a broken disk is retried periodically instead of on every
    /// single ticket.
    compact_at: usize,
    /// Set when this ledger only exists because damage was worked around. The
    /// relay copies it into its status so the UI and `/health` can say so.
    recovery: Option<LedgerRecovery>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Where the exclusive lock for `ledger` lives: `print-jobs.jsonl` →
/// `print-jobs.lock`, in the same directory, so one data folder is one bridge.
pub fn lock_path_for(ledger: &Path) -> PathBuf {
    ledger.with_extension("lock")
}

impl JobLedger {
    /// Opens (or creates) the ledger at `path` and drops expired entries.
    ///
    /// Fails with [`BridgeError::AlreadyRunning`] when another bridge already
    /// has it. That failure is not an inconvenience to work around — it is the
    /// mechanism, and the relay is expected to stand down on it.
    ///
    /// Stand down, not stop: `relay::acquire_ledger` retries and says so in the
    /// UI, because the other copy can be closed and a bridge that gave up for
    /// good was a till that never printed again.
    ///
    /// A damaged file is **not** one of the failures: see the module header for
    /// why a corrupt ledger is salvaged or set aside instead of blocking, and
    /// what that costs.
    pub fn open(path: impl Into<PathBuf>) -> BridgeResult<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Before the first read: everything below reads the file and rewrites
        // it, and doing either while another bridge is doing the same is how
        // the duplicate happens.
        let lock = InstanceLock::acquire(lock_path_for(&path))?;
        let (load, recovery) = match read_all(&path) {
            Ok(load) => {
                let recovery = (load.unreadable > 0).then_some(LedgerRecovery::Salvaged {
                    lines: load.unreadable,
                });
                (load, recovery)
            }
            // The file exists and cannot be read: a bad sector, a handle
            // somebody else holds without sharing, a device that went away.
            // It is preserved under another name and this bridge starts a new
            // one, because waiting does not fix any of those.
            //
            // The `?` on the rename is doing more than propagating an error,
            // and it is worth spelling out: **a file that is merely busy cannot
            // be renamed either.** A backup agent or an antivirus holding the
            // ledger open for a few seconds fails the read *and* fails the
            // move, so this arm returns `Err`, `acquire_ledger` waits, and the
            // records come back intact when the handle closes — exactly the old
            // behaviour, for exactly the case where the old behaviour was right.
            // What gets through to the quarantine is the set of failures where
            // the metadata is fine and the contents are not, which are the
            // permanent ones. Verified on Windows by
            // `a_ledger_that_cannot_even_be_moved_aside_still_fails_loudly`.
            Err(e) => {
                let kept = set_aside(&path)?;
                tracing::error!(
                    err = %e,
                    kept = %kept.display(),
                    "the print ledger could not be read; it was preserved under a new name and \
                     an empty one started. Jobs the server replays from before this point may \
                     print a second time"
                );
                (
                    Load::default(),
                    Some(LedgerRecovery::SetAside {
                        kept,
                        reason: e.to_string(),
                    }),
                )
            }
        };
        if let Some(recovery) = &recovery {
            tracing::error!(path = %path.display(), "{}", recovery.message());
        }
        let mut ledger = Self {
            path,
            _lock: lock,
            seen: load.seen,
            lines: load.lines,
            compact_at: COMPACT_AT_ENTRIES,
            recovery,
        };
        // Best effort, deliberately. `read_all` above is what this ledger
        // actually needs to be correct; compaction only evicts expired entries
        // and shrinks the file. Refusing to open over a failed *housekeeping*
        // step would hand a full disk the power to stop the till printing
        // altogether — and the entries it failed to evict make old jobs read as
        // `AlreadyDone` rather than `Fresh`, which is the safe direction.
        ledger.compact_best_effort();
        Ok(ledger)
    }

    /// What opening this ledger cost, when it cost anything. `None` on the
    /// ordinary path, which is almost always.
    pub fn recovery(&self) -> Option<&LedgerRecovery> {
        self.recovery.as_ref()
    }

    /// Claims `job_id` **and flushes to disk before returning**.
    ///
    /// The `sync_all` is not ceremony: without it the line sits in the OS cache
    /// and a power cut at the till loses exactly the record this whole module
    /// exists to keep.
    pub fn claim(&mut self, job_id: &str) -> BridgeResult<Claim> {
        if let Some(previous) = self.seen.get(job_id) {
            return Ok(match previous.state {
                JobState::Claimed => Claim::Interrupted,
                JobState::Printed => Claim::AlreadyDone {
                    ok: true,
                    error: None,
                },
                JobState::Failed => Claim::AlreadyDone {
                    ok: false,
                    error: previous.error.clone(),
                },
            });
        }
        self.append(Entry {
            job_id: job_id.to_string(),
            state: JobState::Claimed,
            at: now_secs(),
            error: None,
        })?;
        Ok(Claim::Fresh)
    }

    /// Records the outcome. Best-effort by design: if this write fails the job
    /// stays `Claimed`, which on a replay reads as `Interrupted` — it will not
    /// reprint, it will report a failure. Safe direction.
    pub fn finish(&mut self, job_id: &str, ok: bool, error: Option<String>) -> BridgeResult<()> {
        self.append(Entry {
            job_id: job_id.to_string(),
            state: if ok {
                JobState::Printed
            } else {
                JobState::Failed
            },
            at: now_secs(),
            error: error.map(|e| e.chars().take(500).collect()),
        })
    }

    fn append(&mut self, entry: Entry) -> BridgeResult<()> {
        let mut line = serde_json::to_string(&entry)
            .map_err(|e| BridgeError::Config(format!("serializar registro de impresión: {e}")))?;
        line.push('\n');

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())?;
        file.sync_all()?;

        // From here the claim is durable and known. Nothing below may turn
        // this call into an error: `print_worker` reads an `Err` from `claim`
        // as "I could not record it, so I must not print it", and refusing to
        // print a job that *is* recorded costs the till a ticket for no safety
        // in return.
        self.lines += 1;
        self.seen.insert(entry.job_id.clone(), entry);
        if self.lines > self.compact_at {
            self.compact_best_effort();
        }
        Ok(())
    }

    /// Compacts, and treats failure as what it is: housekeeping that did not
    /// happen.
    ///
    /// The file stays as it was, the map stays as it was (see [`Self::compact`]
    /// for why that second half is not free), and the only lasting effect is a
    /// ledger longer than we would like. The alternative — propagating — turns
    /// a full disk into a till that stops printing, which is a far worse
    /// failure than a log file that keeps growing.
    fn compact_best_effort(&mut self) {
        if let Err(e) = self.compact() {
            self.compact_at = self.lines.saturating_add(COMPACT_RETRY_AFTER);
            tracing::error!(
                err = %e,
                path = %self.path.display(),
                lines = self.lines,
                retry_at = self.compact_at,
                "could not compact the print ledger; keeping the existing file and \
                 the idempotency records in memory, printing continues"
            );
        }
    }

    /// Rewrites the file with one line per live job, dropping anything past
    /// [`RETENTION`]. Atomic: a crash mid-compaction leaves the old file.
    ///
    /// # It re-reads the file; it does not dump the map
    ///
    /// This used to serialise `self.seen` straight over the file, which is only
    /// correct while exactly one process owns it. It did not: two bridges
    /// shared one `%PROGRAMDATA%`, so whichever compacted first **deleted every
    /// job id the other had recorded** — and a deleted id is a job that reads
    /// as `Fresh` on the next replay and prints a second time. The damage
    /// outlived the second instance, because the file it left behind was short
    /// a day's worth of records.
    ///
    /// Reading the file back and folding this process's knowledge into it can
    /// only ever add records, never drop someone else's. Under the lock taken
    /// in [`JobLedger::open`] there is no second writer to merge with today —
    /// this is what keeps the outcome harmless if that ever stops being true.
    ///
    /// # Nothing in memory is given up until the new file is in place
    ///
    /// This is the other half of the same rule, and it is the one that was
    /// missing. Compaction used to start by *draining* `self.seen` into the
    /// merged map — that is, it emptied the idempotency table **before**
    /// serialising, creating the temp file, writing it, fsyncing it and
    /// renaming it. Any of those five can fail, and the one that fails in real
    /// life is precisely the write: the till's disk is full. On that path
    /// `compact` returned `Err` with the map empty, because the only line that
    /// refills it (`self.seen = live`) sits at the very end of the happy path.
    ///
    /// The blast radius is a reprint, which is the one outcome this module
    /// exists to prevent. An emptied map answers the next [`JobLedger::claim`]
    /// with [`Claim::Fresh`] for a job it printed minutes ago, and the server
    /// replays exactly those jobs on the next reconnect. It is not a rare
    /// corner either: `append` calls this every time the file passes
    /// [`COMPACT_AT_ENTRIES`], i.e. in the middle of an ordinary busy service.
    ///
    /// So the merge now reads `self.seen` instead of draining it, and the two
    /// fields are reassigned only once every fallible step is behind us. A
    /// failed compaction is then exactly what it should be: housekeeping that
    /// did not happen, on a file that is still the old good one, with the
    /// process's memory of what it printed fully intact.
    fn compact(&mut self) -> BridgeResult<()> {
        // What is genuinely on disk, not what this process remembers writing.
        let mut live = read_all(&self.path)?.seen;

        // Fold our own view on top. `append` writes the line before it updates
        // the map, so in a healthy run this adds nothing; it matters when the
        // file was replaced under us and our memory is the newer copy.
        //
        // By reference: `self.seen` has to survive everything below failing.
        for (job_id, entry) in &self.seen {
            match live.get(job_id) {
                Some(on_disk) if !supersedes(entry, on_disk) => {}
                _ => {
                    live.insert(job_id.clone(), entry.clone());
                }
            }
        }

        let cutoff = now_secs().saturating_sub(RETENTION.as_secs());
        live.retain(|_, e| e.at >= cutoff);

        let mut body = String::new();
        for entry in live.values() {
            let line = serde_json::to_string(entry).map_err(|e| {
                BridgeError::Config(format!("serializar registro de impresión: {e}"))
            })?;
            body.push_str(&line);
            body.push('\n');
        }

        replace_file(&self.path, &self.path.with_extension("jsonl.tmp"), &body)?;

        // Past this point nothing can fail, so the map can safely become the
        // file. Never move any of these above the write.
        self.lines = live.len();
        self.seen = live;
        self.compact_at = COMPACT_AT_ENTRIES;
        Ok(())
    }

    #[cfg(test)]
    pub fn tracked(&self) -> usize {
        self.seen.len()
    }
}

/// Writes `body` over `path` through `tmp`, so the ledger is either the old
/// file or the new one and never a half of either.
///
/// The temp file is removed when any step fails. Leaving it behind would be
/// mostly cosmetic — the name is fixed, so the next attempt truncates it — but
/// "mostly" is doing real work there: on a disk that filled up, the leftover is
/// occupying the very space the retry needs.
fn replace_file(path: &Path, tmp: &Path, body: &str) -> BridgeResult<()> {
    let written = (|| -> BridgeResult<()> {
        let mut file = std::fs::File::create(tmp)?;
        file.write_all(body.as_bytes())?;
        // Durability before visibility: renaming a file whose bytes are still
        // in the OS cache can leave an empty ledger after a power cut, which
        // reads as "nothing was ever printed".
        file.sync_all()?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(tmp);
        return written;
    }
    if let Err(e) = std::fs::rename(tmp, path) {
        let _ = std::fs::remove_file(tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Which of two records for the same job is the later truth.
///
/// Later timestamp wins. On a tie the terminal state wins, because a claim and
/// its outcome routinely land inside the same second (`at` is in seconds) and
/// reading them the other way round would turn a ticket that printed into a
/// phantom `Interrupted` — a failure reported for a job the customer is holding.
fn supersedes(candidate: &Entry, current: &Entry) -> bool {
    (candidate.at, finality(candidate.state)) > (current.at, finality(current.state))
}

fn finality(state: JobState) -> u8 {
    match state {
        JobState::Claimed => 0,
        JobState::Printed | JobState::Failed => 1,
    }
}

/// Renames a ledger that cannot be read out of the way, keeping it.
///
/// Never deletes. The file is the only record of what this till printed, and
/// even a file that this program cannot parse can be opened by a person with a
/// hex viewer when a customer swears they were charged twice. The name carries
/// the timestamp so a till that hits this twice does not overwrite the first
/// copy — which would be the one bit of evidence worth having.
fn set_aside(path: &Path) -> BridgeResult<PathBuf> {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".corrupt-{}", now_secs()));
    let kept = PathBuf::from(name);
    std::fs::rename(path, &kept)?;
    Ok(kept)
}

/// One pass over the ledger file.
#[derive(Default)]
struct Load {
    seen: HashMap<String, Entry>,
    /// Physical lines, so the caller can decide when to compact.
    lines: usize,
    /// Lines whose **bytes** were not text at all. Counted apart from the ones
    /// that were text but not JSON, because only this kind means the file was
    /// damaged rather than merely interrupted, and only this kind is worth
    /// telling the operator about.
    unreadable: usize,
}

/// Reads every line, last state per job wins.
///
/// Nothing in one line can stop the others from loading. A torn tail (text, not
/// JSON) has always been skipped — refusing to boot over a partial line nobody
/// can fix by hand would take printing down for nothing. Bytes that are not
/// valid UTF-8 now get exactly the same treatment, and that is the whole of the
/// fix: reading through `BufReader::lines` made one bad byte fail the **entire
/// read**, `JobLedger::open` returned `Err`, and `relay::acquire_ledger` retried
/// it every fifteen seconds for as long as the till stayed on. Every id in the
/// undamaged 99% of the file was lost with it, which is also why skipping beats
/// setting the file aside: the records that survive are the ones stopping
/// reprints.
///
/// Reading the file whole rather than streaming it is deliberate: the loader has
/// to look at raw bytes to be able to skip them, compaction keeps the file to a
/// few hundred lines, and a ledger that outgrew memory would be a bug elsewhere.
fn read_all(path: &Path) -> BridgeResult<Load> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Load::default()),
        Err(e) => return Err(e.into()),
    };
    if bytes.is_empty() {
        return Ok(Load::default());
    }

    let mut load = Load::default();
    // Drop the trailing newline before splitting so a well-formed file does not
    // report a phantom empty last line; `lines` feeds the compaction threshold.
    let body = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    for raw in body.split(|b| *b == b'\n') {
        load.lines += 1;
        let Ok(line) = std::str::from_utf8(raw) else {
            load.unreadable += 1;
            tracing::warn!(
                bytes = raw.len(),
                "skipping a line of the print ledger that is not readable text"
            );
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Entry>(line) {
            Ok(entry) => {
                load.seen.insert(entry.job_id.clone(), entry);
            }
            Err(e) => tracing::warn!(err = %e, "skipping unreadable line in the print ledger"),
        }
    }
    Ok(load)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gmly-ledger-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("print-jobs.jsonl")
    }

    #[test]
    fn a_new_job_is_fresh_and_a_repeat_is_not() {
        let mut ledger = JobLedger::open(temp_path("fresh")).expect("open");
        assert_eq!(ledger.claim("job-1").expect("claim"), Claim::Fresh);
        ledger.finish("job-1", true, None).expect("finish");
        assert_eq!(
            ledger.claim("job-1").expect("claim"),
            Claim::AlreadyDone {
                ok: true,
                error: None
            }
        );
    }

    #[test]
    fn a_failure_is_replayed_as_a_failure_not_as_a_reprint() {
        let mut ledger = JobLedger::open(temp_path("failed")).expect("open");
        ledger.claim("job-2").expect("claim");
        ledger
            .finish("job-2", false, Some("sin papel".into()))
            .expect("finish");
        assert_eq!(
            ledger.claim("job-2").expect("claim"),
            Claim::AlreadyDone {
                ok: false,
                error: Some("sin papel".into())
            }
        );
    }

    /// The regression that justifies writing before printing. Revert `claim` to
    /// record after the spooler call and this is the test that goes red.
    #[test]
    fn a_crash_between_claiming_and_printing_does_not_reprint() {
        let path = temp_path("crash");
        {
            let mut ledger = JobLedger::open(&path).expect("open");
            assert_eq!(ledger.claim("job-3").expect("claim"), Claim::Fresh);
            // …power cut here: `finish` never runs, the process dies.
        }
        let mut reborn = JobLedger::open(&path).expect("reopen");
        assert_eq!(reborn.claim("job-3").expect("claim"), Claim::Interrupted);
    }

    #[test]
    fn survives_a_torn_final_line() {
        let path = temp_path("torn");
        {
            let mut ledger = JobLedger::open(&path).expect("open");
            ledger.claim("job-4").expect("claim");
            ledger.finish("job-4", true, None).expect("finish");
        }
        // Simulate a write interrupted halfway through the last line.
        let mut raw = std::fs::read_to_string(&path).expect("read");
        raw.push_str("{\"jobId\":\"job-5\",\"sta");
        std::fs::write(&path, raw).expect("write");

        let mut reborn = JobLedger::open(&path).expect("reopen");
        assert_eq!(
            reborn.claim("job-4").expect("claim"),
            Claim::AlreadyDone {
                ok: true,
                error: None
            }
        );
        // The half-written one was never really claimed, so it prints.
        assert_eq!(reborn.claim("job-5").expect("claim"), Claim::Fresh);
    }

    #[test]
    fn compaction_evicts_expired_entries_and_shrinks_the_file() {
        let path = temp_path("compact");
        let stale = now_secs() - RETENTION.as_secs() - 60;
        let fresh = now_secs();
        let mut body = String::new();
        for i in 0..50 {
            body.push_str(&format!(
                "{{\"jobId\":\"old-{i}\",\"state\":\"printed\",\"at\":{stale}}}\n"
            ));
        }
        body.push_str(&format!(
            "{{\"jobId\":\"new\",\"state\":\"printed\",\"at\":{fresh}}}\n"
        ));
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, body).expect("seed");

        let mut ledger = JobLedger::open(&path).expect("open");
        assert_eq!(ledger.tracked(), 1, "only the fresh entry survives");
        // An evicted job is treated as new again — which is correct: the
        // server can no longer replay something that old.
        assert_eq!(ledger.claim("old-0").expect("claim"), Claim::Fresh);
        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(
            on_disk.lines().count() <= 2,
            "file was rewritten, not grown"
        );
    }

    // ── A file the loader cannot fully read ─────────────────────────────
    //
    // Reproduced on the shipped binary before this was written: a ledger with a
    // non-UTF-8 tail — what a power cut at the till leaves on NTFS — made
    // `open` fail, `acquire_ledger` retry every fifteen seconds for ever, and
    // the relay never join the room again. Process alive, tray icon normal,
    // `/health` answering `ok:true`, and not one ticket for the rest of the day.

    /// Bytes that are not text must cost their own line and nothing else.
    ///
    /// Put `BufReader::lines()` back in `read_all` and this goes red on the very
    /// first line: `open` returns `Err(Io)` — "stream did not contain valid
    /// UTF-8" — and the till is mute until someone deletes a file they have
    /// never heard of.
    #[test]
    fn a_ledger_with_unreadable_bytes_still_opens_and_keeps_the_rest() {
        let path = temp_path("corrupt-tail");
        let at = now_secs();
        let mut body =
            format!("{{\"jobId\":\"ticket-1\",\"state\":\"printed\",\"at\":{at}}}\n").into_bytes();
        // The tail of a block another file used to own.
        body.extend_from_slice(&[0xff, 0xfe, 0x80, 0x9f, 0xc3, 0x28, b'\n']);
        body.extend_from_slice(
            format!("{{\"jobId\":\"ticket-2\",\"state\":\"printed\",\"at\":{at}}}\n").as_bytes(),
        );
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, &body).expect("seed");

        {
            let mut ledger =
                JobLedger::open(&path).expect("a damaged line must not stop the till printing");

            // Both records survive — including the one AFTER the damage, which
            // a loader that gave up at the first bad byte would never reach.
            for id in ["ticket-1", "ticket-2"] {
                assert_eq!(
                    ledger.claim(id).expect("claim"),
                    Claim::AlreadyDone {
                        ok: true,
                        error: None
                    },
                    "{id} was lost with the damaged line and would print a second time"
                );
            }
            assert_eq!(
                ledger.recovery(),
                Some(&LedgerRecovery::Salvaged { lines: 1 }),
                "the operator is told nothing about a file that lost a record"
            );
        }

        // And the file heals: the compaction on open rewrote it without the
        // garbage, so the next boot is an ordinary one.
        let on_disk = std::fs::read(&path).expect("read");
        assert!(
            std::str::from_utf8(&on_disk).is_ok(),
            "the damaged bytes are still in the file"
        );
        assert!(
            JobLedger::open(&path).expect("reopen").recovery().is_none(),
            "the ledger reports damage it already cleaned up"
        );
    }

    /// A file that cannot be read at all is preserved and replaced, because the
    /// alternative is a bridge that never prints again.
    ///
    /// The injection is a directory where the ledger goes: `std::fs::read` fails
    /// on it for real, the same `?` a bad sector takes.
    #[test]
    fn a_ledger_that_cannot_be_read_is_kept_aside_instead_of_blocking_for_ever() {
        let path = temp_path("unreadable");
        std::fs::create_dir_all(&path).expect("a directory where the ledger goes");

        let mut ledger = JobLedger::open(&path)
            .expect("an unreadable ledger must not leave the till mute for ever");
        let Some(LedgerRecovery::SetAside { kept, reason }) = ledger.recovery().cloned() else {
            panic!(
                "expected the file to be set aside, got {:?}",
                ledger.recovery()
            );
        };
        println!(
            "[f2] kept the unreadable ledger at {} ({reason})",
            kept.display()
        );
        assert!(kept.exists(), "the evidence was deleted instead of kept");
        assert!(
            kept.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(".corrupt-")),
            "unhelpful name for the preserved file: {}",
            kept.display()
        );

        // And it works from here: this is the whole point of setting it aside.
        assert_eq!(ledger.claim("after").expect("claim"), Claim::Fresh);
        ledger.finish("after", true, None).expect("finish");
        assert_eq!(
            ledger.claim("after").expect("claim"),
            Claim::AlreadyDone {
                ok: true,
                error: None
            }
        );
    }

    /// The one case that still refuses to open — and must, because a human has
    /// to clear it. Windows only: `share_mode(0)` is how a file becomes
    /// unreadable *and* unrenameable, which is what antivirus looks like.
    #[cfg(windows)]
    #[test]
    fn a_ledger_that_cannot_even_be_moved_aside_still_fails_loudly() {
        use std::os::windows::fs::OpenOptionsExt;

        let path = temp_path("locked-out");
        std::fs::write(&path, b"{\"jobId\":\"x\",\"state\":\"printed\",\"at\":1}\n").expect("seed");
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0) // nobody else may open it, not even to rename it
            .open(&path)
            .expect("hold the ledger open with no sharing");

        let Err(err) = JobLedger::open(&path) else {
            panic!("a ledger that can be neither read nor moved must not look fine");
        };
        println!("[f2] refused, as it should: {err}");
        assert!(
            matches!(err, BridgeError::Io(_)),
            "the relay decides what to show from the error kind: {err:?}"
        );
    }

    // ── Two bridges on one till ─────────────────────────────────────────

    /// The regression the verifier reproduced, at its root.
    ///
    /// Before the lock this returned two working ledgers over one file, each
    /// with its own map, and every job that reached both printed twice.
    #[test]
    fn a_second_bridge_cannot_open_the_same_ledger() {
        let path = temp_path("second-bridge");
        let first = JobLedger::open(&path).expect("the first bridge opens it");
        match JobLedger::open(&path) {
            Err(BridgeError::AlreadyRunning(msg)) => {
                assert!(msg.contains("ya está abierto"), "unhelpful message: {msg}")
            }
            Err(other) => panic!("wrong error: {other}"),
            Ok(_) => panic!("two ledgers over one file — every ticket would print twice"),
        }
        drop(first);
        JobLedger::open(&path).expect("and the next bridge can, once the first is gone");
    }

    /// The lock lives beside the ledger, not on it: on Windows a locked byte
    /// range is unreadable through every other handle, so locking the ledger
    /// itself would lock out the code that has to read it.
    #[test]
    fn the_ledger_stays_readable_while_it_is_locked() {
        let path = temp_path("readable");
        let mut ledger = JobLedger::open(&path).expect("open");
        ledger.claim("job-r").expect("claim");
        let raw = std::fs::read_to_string(&path).expect("the ledger must stay readable");
        assert!(raw.contains("job-r"));
    }

    /// The medium finding: compaction used to rewrite the file from one
    /// process's map, wiping the other's idempotency records — so a job that
    /// had already printed came back as `Fresh` and printed again.
    ///
    /// Revert `compact` to serialising `self.seen` and this goes red: `other-1`
    /// disappears from the file and the reopened ledger prints it a second time.
    #[test]
    fn compaction_keeps_records_it_did_not_write_itself() {
        let path = temp_path("compact-foreign");
        let mut ledger = JobLedger::open(&path).expect("open");
        ledger.claim("mine-1").expect("claim");
        ledger.finish("mine-1", true, None).expect("finish");

        // A line this process never appended: what a second bridge would have
        // left behind, or what any concurrent writer looks like from here.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("append");
        writeln!(
            file,
            "{{\"jobId\":\"other-1\",\"state\":\"printed\",\"at\":{}}}",
            now_secs()
        )
        .expect("write foreign entry");
        drop(file);

        ledger.compact().expect("compact");

        let on_disk = std::fs::read_to_string(&path).expect("read");
        assert!(
            on_disk.contains("other-1"),
            "compaction deleted a record it did not write; that job will print again:\n{on_disk}"
        );
        assert!(on_disk.contains("mine-1"), "and it must keep its own");
        assert_eq!(
            ledger.claim("other-1").expect("claim"),
            Claim::AlreadyDone {
                ok: true,
                error: None
            },
            "the foreign record must still stop a reprint"
        );
    }

    // ── When compaction cannot write ────────────────────────────────────
    //
    // The rewrite is five fallible steps (serialise, create, write, fsync,
    // rename) and the one that fails at a till is the write: the disk is full.
    // Compaction used to `drain` the idempotency map into its merged copy
    // *before* all five, and only put it back on the happy path — so a failed
    // write left the process with an EMPTY table of what it had printed. Every
    // job the server replays after that reads as `Fresh` and prints a second
    // time.
    //
    // Both tests below break a real filesystem operation rather than injecting
    // a fake error, so they exercise the same `?` the disk would.

    /// Makes `File::create(tmp)` fail for real, by putting a directory where
    /// the temp file has to go. Fails on the FIRST write step — the earliest
    /// point at which the old code had already emptied the map.
    fn block_the_temp_file(ledger_path: &Path) -> PathBuf {
        let tmp = ledger_path.with_extension("jsonl.tmp");
        let _ = std::fs::remove_file(&tmp);
        std::fs::create_dir_all(&tmp).expect("a directory where the temp file goes");
        tmp
    }

    /// A ticket that already printed must not print again because the disk was
    /// full when the ledger tried to tidy itself up.
    #[test]
    fn a_failed_compaction_keeps_the_idempotency_it_had() {
        let path = temp_path("compact-fails");
        let mut ledger = JobLedger::open(&path).expect("open");
        ledger.claim("job-printed").expect("claim");
        ledger.finish("job-printed", true, None).expect("finish");
        ledger.claim("job-failed").expect("claim");
        ledger
            .finish("job-failed", false, Some("sin papel".into()))
            .expect("finish");
        let before = ledger.tracked();
        assert_eq!(before, 2, "both jobs are known before the disk breaks");

        block_the_temp_file(&path);
        let err = ledger
            .compact()
            .expect_err("compaction must fail when it cannot create its temp file");
        println!("[p1] compaction failed as intended: {err}");

        // The whole point: the map is untouched, so the replay is answered
        // from memory instead of reprinting.
        assert_eq!(
            ledger.tracked(),
            before,
            "compaction emptied the idempotency map on the way out; every replayed job reprints"
        );
        assert_eq!(
            ledger.claim("job-printed").expect("claim"),
            Claim::AlreadyDone {
                ok: true,
                error: None
            },
            "the ticket would have come out of the printer a SECOND time"
        );
        assert_eq!(
            ledger.claim("job-failed").expect("claim"),
            Claim::AlreadyDone {
                ok: false,
                error: Some("sin papel".into())
            },
        );

        // And the file it refused to replace is still the good one.
        let on_disk = std::fs::read_to_string(&path).expect("the old ledger must still be there");
        assert!(on_disk.contains("job-printed"), "on disk:\n{on_disk}");
        assert!(on_disk.contains("job-failed"), "on disk:\n{on_disk}");
    }

    /// The same guarantee when the failure lands as LATE as it can: the temp
    /// file is fully written and fsynced, and the rename is what fails.
    ///
    /// Windows only, because that is where the mechanism exists:
    /// `MoveFileEx(..., MOVEFILE_REPLACE_EXISTING)` refuses to replace a
    /// destination carrying the read-only attribute, while POSIX `rename`
    /// cares about the directory's permissions instead. The bridge ships on
    /// Windows, so this is the platform whose late failure matters.
    #[cfg(windows)]
    #[test]
    fn idempotency_survives_a_failure_at_the_very_last_step() {
        let path = temp_path("compact-rename-fails");
        let mut ledger = JobLedger::open(&path).expect("open");
        ledger.claim("job-late").expect("claim");
        ledger.finish("job-late", true, None).expect("finish");

        // Read-only destination: create/write/fsync all succeed, rename does not.
        let mut perms = std::fs::metadata(&path).expect("metadata").permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).expect("make the ledger read-only");

        let err = ledger
            .compact()
            .expect_err("renaming over a read-only file must fail on Windows");
        println!("[p1] compaction failed at the rename: {err}");

        assert_eq!(
            ledger.claim("job-late").expect("claim"),
            Claim::AlreadyDone {
                ok: true,
                error: None
            },
            "a failure at the last step still cost the map, so the ticket reprints"
        );

        // The temp file must not be left behind occupying the space the retry
        // needs.
        let tmp = path.with_extension("jsonl.tmp");
        assert!(
            !tmp.exists(),
            "the abandoned temp file was left on a disk that is probably full: {}",
            tmp.display()
        );

        // Clearing the attribute is the whole point here, and this is the
        // Windows-only arm of the test: the lint warns about `false` meaning
        // "world-writable" on Unix, which this never compiles as.
        let mut perms = std::fs::metadata(&path).expect("metadata").permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        std::fs::set_permissions(&path, perms).expect("restore");
    }

    /// The production shape of the same bug: nobody calls `compact` by hand.
    /// `append` calls it once the file passes [`COMPACT_AT_ENTRIES`], which is
    /// an ordinary busy evening, and the very next job is the one that
    /// reprints.
    #[test]
    fn a_compaction_that_fails_mid_service_neither_reprints_nor_stops_printing() {
        let path = temp_path("compact-mid-service");
        let mut ledger = JobLedger::open(&path).expect("open");

        // The evening's tickets, up to the line that triggers housekeeping.
        for i in 0..COMPACT_AT_ENTRIES / 2 {
            let id = format!("ticket-{i}");
            assert_eq!(ledger.claim(&id).expect("claim"), Claim::Fresh);
            ledger.finish(&id, true, None).expect("finish");
        }
        assert!(ledger.lines > COMPACT_AT_ENTRIES / 2, "the file grew");

        // The disk gives out exactly as the ledger decides to tidy up.
        block_the_temp_file(&path);
        let id = format!("ticket-{}", COMPACT_AT_ENTRIES / 2);
        assert_eq!(
            ledger
                .claim(&id)
                .expect("a claim must not fail over housekeeping"),
            Claim::Fresh,
            "the till stopped printing because a log file could not be shrunk"
        );
        ledger.finish(&id, true, None).expect("finish");

        // The server reconnects and replays the evening. Not one of them may
        // print again.
        for i in 0..=COMPACT_AT_ENTRIES / 2 {
            let id = format!("ticket-{i}");
            assert_eq!(
                ledger.claim(&id).expect("claim"),
                Claim::AlreadyDone {
                    ok: true,
                    error: None
                },
                "{id} was replayed and would have printed a second time"
            );
        }
    }

    /// A claim and its outcome share a second often enough that the tie-break
    /// is not theoretical: get it backwards and a printed ticket is reported as
    /// interrupted.
    #[test]
    fn an_outcome_beats_a_claim_recorded_in_the_same_second() {
        let at = now_secs();
        let claimed = Entry {
            job_id: "j".into(),
            state: JobState::Claimed,
            at,
            error: None,
        };
        let printed = Entry {
            job_id: "j".into(),
            state: JobState::Printed,
            at,
            error: None,
        };
        assert!(supersedes(&printed, &claimed));
        assert!(!supersedes(&claimed, &printed));
    }
}
