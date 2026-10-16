//! Durable acknowledgements for local WSS print intents, separate from relay job IDs.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{Mutex, MutexGuard},
};

use serde::{Deserialize, Serialize};

use crate::{
    error::{BridgeError, BridgeResult},
    instance_lock::InstanceLock,
};

const FORMAT_VERSION: u8 = 1;
const MAX_INTENT_ID_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub job_id: u32,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    Fresh,
    Done(Outcome),
    /// A claim without a terminal record might already have reached the spooler.
    Interrupted,
    /// The same ID names different printer/bytes, so neither outcome is safe to reuse.
    Conflict,
}

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    version: u8,
    #[serde(rename = "printIntentId")]
    intent_id: String,
    fingerprint: String,
    outcome: Option<Outcome>,
}

struct Inner {
    file: File,
    seen: HashMap<String, Record>,
    /// A partial write or failed fsync makes future claims unsafe in this process.
    blocked: bool,
}

pub struct LocalJobLedger {
    _lock: InstanceLock,
    inner: Mutex<Inner>,
}

impl LocalJobLedger {
    pub fn open(path: impl AsRef<Path>) -> BridgeResult<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let lock = InstanceLock::acquire(path.with_extension("lock"))?;
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?;
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            return Err(BridgeError::Config(
                "local print ledger has a torn final record".into(),
            ));
        }

        let mut seen: HashMap<String, Record> = HashMap::new();
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let record: Record = serde_json::from_slice(line).map_err(|error| {
                BridgeError::Config(format!("local print ledger has an invalid record: {error}"))
            })?;
            if record.version != FORMAT_VERSION
                || record.intent_id.is_empty()
                || record.intent_id.len() > MAX_INTENT_ID_BYTES
                || record.fingerprint.is_empty()
            {
                return Err(BridgeError::Config(
                    "local print ledger has an unsupported record".into(),
                ));
            }
            if let Some(previous) = seen.get(&record.intent_id) {
                if previous.fingerprint != record.fingerprint
                    || (previous.outcome.is_some() && previous.outcome != record.outcome)
                {
                    return Err(BridgeError::Config(
                        "local print ledger has conflicting records".into(),
                    ));
                }
            }
            seen.insert(record.intent_id.clone(), record);
        }
        Ok(Self {
            _lock: lock,
            inner: Mutex::new(Inner {
                file,
                seen,
                blocked: false,
            }),
        })
    }

    pub fn claim(&self, intent_id: &str, fingerprint: &str) -> BridgeResult<Claim> {
        if intent_id.is_empty() || intent_id.len() > MAX_INTENT_ID_BYTES || fingerprint.is_empty() {
            return Err(BridgeError::BadRequest(
                "invalid print intent identity".into(),
            ));
        }
        let mut inner = self.inner()?;
        if let Some(record) = inner.seen.get(intent_id) {
            if record.fingerprint != fingerprint {
                return Ok(Claim::Conflict);
            }
            return Ok(match &record.outcome {
                Some(outcome) => Claim::Done(outcome.clone()),
                None => Claim::Interrupted,
            });
        }
        let record = Record {
            version: FORMAT_VERSION,
            intent_id: intent_id.into(),
            fingerprint: fingerprint.into(),
            outcome: None,
        };
        append(&mut inner, &record)?;
        inner.seen.insert(intent_id.into(), record);
        Ok(Claim::Fresh)
    }

    pub fn finish(&self, intent_id: &str, outcome: Outcome) -> BridgeResult<()> {
        let mut inner = self.inner()?;
        let previous = inner
            .seen
            .get(intent_id)
            .ok_or_else(|| BridgeError::BadRequest("print intent was not claimed".into()))?;
        if previous.outcome.is_some() {
            return Err(BridgeError::BadRequest(
                "print intent already finished".into(),
            ));
        }
        let mut record = previous.clone();
        record.outcome = Some(outcome);
        append(&mut inner, &record)?;
        inner.seen.insert(intent_id.into(), record);
        Ok(())
    }

    fn inner(&self) -> BridgeResult<MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| BridgeError::Config("local print ledger lock is poisoned".into()))
    }
}

fn append(inner: &mut Inner, record: &Record) -> BridgeResult<()> {
    if inner.blocked {
        return Err(BridgeError::Config(
            "local print ledger is blocked after a write failure".into(),
        ));
    }
    let mut line = serde_json::to_vec(record)
        .map_err(|error| BridgeError::Config(format!("cannot encode print intent: {error}")))?;
    line.push(b'\n');
    if let Err(error) = inner
        .file
        .write_all(&line)
        .and_then(|_| inner.file.sync_all())
    {
        inner.blocked = true;
        return Err(BridgeError::Io(error));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{Arc, Barrier},
    };

    use super::{Claim, LocalJobLedger, Outcome};

    fn ledger_path() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bridge-local-ledger-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("test directory");
        dir.join("local-print-intents.jsonl")
    }

    #[test]
    fn finished_intent_survives_restart_and_rejects_a_different_fingerprint() {
        let path = ledger_path();
        let ledger = LocalJobLedger::open(&path).expect("open ledger");
        assert_eq!(
            ledger.claim("intent-1", "printer-and-bytes-a").unwrap(),
            Claim::Fresh
        );
        let outcome = Outcome {
            job_id: 42,
            error: None,
        };
        ledger.finish("intent-1", outcome.clone()).expect("finish");
        drop(ledger);

        let reopened = LocalJobLedger::open(&path).expect("reopen ledger");
        assert_eq!(
            reopened.claim("intent-1", "printer-and-bytes-a").unwrap(),
            Claim::Done(outcome)
        );
        assert_eq!(
            reopened.claim("intent-1", "printer-and-bytes-b").unwrap(),
            Claim::Conflict
        );
    }

    #[test]
    fn failed_intent_replays_the_same_failure_after_restart() {
        let path = ledger_path();
        let ledger = LocalJobLedger::open(&path).expect("open ledger");
        assert_eq!(ledger.claim("intent-2", "digest").unwrap(), Claim::Fresh);
        let outcome = Outcome {
            job_id: 0,
            error: Some("spooler unavailable".into()),
        };
        ledger.finish("intent-2", outcome.clone()).expect("finish");
        drop(ledger);

        let reopened = LocalJobLedger::open(&path).expect("reopen ledger");
        assert_eq!(
            reopened.claim("intent-2", "digest").unwrap(),
            Claim::Done(outcome)
        );
    }

    #[test]
    fn interrupted_claim_is_uncertain_not_fresh_after_restart() {
        let path = ledger_path();
        let ledger = LocalJobLedger::open(&path).expect("open ledger");
        assert_eq!(ledger.claim("intent-3", "digest").unwrap(), Claim::Fresh);
        drop(ledger);

        let reopened = LocalJobLedger::open(&path).expect("reopen ledger");
        assert_eq!(
            reopened.claim("intent-3", "digest").unwrap(),
            Claim::Interrupted
        );
    }

    #[test]
    fn concurrent_retries_create_only_one_fresh_claim() {
        let ledger = Arc::new(LocalJobLedger::open(ledger_path()).expect("open ledger"));
        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let ledger = Arc::clone(&ledger);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    ledger.claim("intent-4", "digest").expect("claim")
                })
            })
            .collect();
        let claims: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().expect("worker"))
            .collect();
        assert_eq!(
            claims
                .iter()
                .filter(|claim| **claim == Claim::Fresh)
                .count(),
            1
        );
        assert_eq!(
            claims
                .iter()
                .filter(|claim| **claim == Claim::Interrupted)
                .count(),
            7
        );
    }

    #[test]
    fn corrupt_or_unopenable_ledger_fails_without_changing_existing_bytes() {
        let path = ledger_path();
        std::fs::write(&path, b"not-json\n").expect("write corrupt ledger");
        let before = std::fs::read(&path).expect("read corrupt ledger");
        assert!(LocalJobLedger::open(&path).is_err());
        assert_eq!(std::fs::read(&path).expect("read after refusal"), before);

        let directory = ledger_path().parent().expect("parent").to_path_buf();
        assert!(LocalJobLedger::open(directory).is_err());
    }

    #[test]
    fn a_live_write_failure_blocks_further_claims_without_touching_disk() {
        let path = ledger_path();
        let ledger = LocalJobLedger::open(&path).expect("open ledger");
        let before = std::fs::read(&path).expect("read ledger");
        ledger.inner.lock().expect("state").file = std::fs::File::open(&path).expect("read only");

        assert!(ledger.claim("intent-5", "digest").is_err());
        assert!(ledger.claim("intent-5", "digest").is_err());
        assert_eq!(std::fs::read(&path).expect("read after refusal"), before);
    }

    #[test]
    fn a_failed_finish_keeps_the_claim_uncertain_after_restart() {
        let path = ledger_path();
        let ledger = LocalJobLedger::open(&path).expect("open ledger");
        assert_eq!(ledger.claim("intent-6", "digest").unwrap(), Claim::Fresh);
        let before = std::fs::read(&path).expect("read claim");
        ledger.inner.lock().expect("state").file = std::fs::File::open(&path).expect("read only");
        assert!(ledger
            .finish(
                "intent-6",
                Outcome {
                    job_id: 7,
                    error: None
                }
            )
            .is_err());
        assert_eq!(
            ledger.claim("intent-6", "digest").unwrap(),
            Claim::Interrupted
        );
        assert!(ledger.claim("another", "digest").is_err());
        assert_eq!(std::fs::read(&path).expect("read after refusal"), before);
        drop(ledger);
        assert_eq!(
            LocalJobLedger::open(&path)
                .unwrap()
                .claim("intent-6", "digest")
                .unwrap(),
            Claim::Interrupted
        );
    }

    #[test]
    fn rejected_commands_leave_prior_durable_output_unchanged() {
        let path = ledger_path();
        let ledger = LocalJobLedger::open(&path).expect("open ledger");
        assert_eq!(ledger.claim("stable", "digest").unwrap(), Claim::Fresh);
        let outcome = Outcome {
            job_id: 9,
            error: None,
        };
        ledger.finish("stable", outcome.clone()).expect("finish");
        let before = std::fs::read(&path).expect("read outcome");
        assert_eq!(
            ledger.claim("stable", "different").unwrap(),
            Claim::Conflict
        );
        assert!(ledger.claim("", "digest").is_err());
        assert!(ledger.finish("unknown", outcome.clone()).is_err());
        assert!(ledger.finish("stable", outcome.clone()).is_err());
        assert_eq!(std::fs::read(&path).expect("read after refusal"), before);
        assert_eq!(
            ledger.claim("stable", "digest").unwrap(),
            Claim::Done(outcome)
        );
    }

    #[test]
    fn a_second_ledger_cannot_own_the_same_file_at_once() {
        let path = ledger_path();
        let first = LocalJobLedger::open(&path).expect("first owner");
        assert!(LocalJobLedger::open(&path).is_err());
        drop(first);
        assert!(LocalJobLedger::open(&path).is_ok());
    }
}
