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

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{BridgeError, BridgeResult};

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
    seen: HashMap<String, Entry>,
    /// Lines currently in the file, to decide when to compact.
    lines: usize,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl JobLedger {
    /// Opens (or creates) the ledger at `path` and drops expired entries.
    pub fn open(path: impl Into<PathBuf>) -> BridgeResult<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let (seen, lines) = read_all(&path)?;
        let mut ledger = Self { path, seen, lines };
        ledger.compact()?;
        Ok(ledger)
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

        self.lines += 1;
        self.seen.insert(entry.job_id.clone(), entry);
        if self.lines > COMPACT_AT_ENTRIES {
            self.compact()?;
        }
        Ok(())
    }

    /// Rewrites the file with one line per live job, dropping anything past
    /// [`RETENTION`]. Atomic: a crash mid-compaction leaves the old file.
    fn compact(&mut self) -> BridgeResult<()> {
        let cutoff = now_secs().saturating_sub(RETENTION.as_secs());
        self.seen.retain(|_, e| e.at >= cutoff);

        let mut body = String::new();
        for entry in self.seen.values() {
            let line = serde_json::to_string(entry).map_err(|e| {
                BridgeError::Config(format!("serializar registro de impresión: {e}"))
            })?;
            body.push_str(&line);
            body.push('\n');
        }

        let tmp = self.path.with_extension("jsonl.tmp");
        {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(body.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        self.lines = self.seen.len();
        Ok(())
    }

    #[cfg(test)]
    pub fn tracked(&self) -> usize {
        self.seen.len()
    }
}

/// Reads every line, last state per job wins. A malformed line — the torn tail
/// of an interrupted write — is skipped, not fatal: refusing to boot over it
/// would take printing down for a partial line nobody can fix by hand.
fn read_all(path: &Path) -> BridgeResult<(HashMap<String, Entry>, usize)> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((HashMap::new(), 0)),
        Err(e) => return Err(e.into()),
    };
    let mut seen = HashMap::new();
    let mut lines = 0usize;
    for line in BufReader::new(file).lines() {
        let line = line?;
        lines += 1;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Entry>(&line) {
            Ok(entry) => {
                seen.insert(entry.job_id.clone(), entry);
            }
            Err(e) => tracing::warn!(err = %e, "skipping unreadable line in the print ledger"),
        }
    }
    Ok((seen, lines))
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
}
