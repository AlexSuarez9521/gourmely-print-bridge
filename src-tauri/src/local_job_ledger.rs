//! Durable acknowledgements for local WSS print intents, separate from relay job IDs.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, MutexGuard,
    },
};

use serde::{Deserialize, Serialize};

use crate::{
    error::{BridgeError, BridgeResult},
    instance_lock::InstanceLock,
};

const FORMAT_VERSION: u8 = 1;
const MAX_INTENT_ID_BYTES: usize = 128;
// Compaction removes repeated transitions, never an intent ID. Until the API
// defines a replay horizon, the number of unique IDs can only grow.
const COMPACT_AFTER_REDUNDANT_LINES: usize = if cfg!(test) { 8 } else { 1_024 };
const COMPACT_RETRY_AFTER_LINES: usize = if cfg!(test) { 8 } else { 256 };

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
    lines: usize,
    compact_retry_at: usize,
    /// A partial write or failed fsync makes future claims unsafe in this process.
    blocked: bool,
}

pub struct LocalJobLedger {
    _lock: InstanceLock,
    path: PathBuf,
    inner: Mutex<Inner>,
    available: AtomicBool,
}

impl LocalJobLedger {
    pub fn open(path: impl AsRef<Path>) -> BridgeResult<Self> {
        Self::open_with_sync(path, sync_parent)
    }

    fn open_with_sync(
        path: impl AsRef<Path>,
        sync_ledger_parent: impl FnOnce(&Path) -> io::Result<()>,
    ) -> BridgeResult<Self> {
        let path = path.as_ref();
        let parent = parent_dir(path);
        std::fs::create_dir_all(parent)?;
        let lock = InstanceLock::acquire(path.with_extension("lock"))?;
        durable_probe(path, sync_parent)?;
        let (mut file, _) = open_file(path)?;
        // A Fresh claim must never precede durable creation of its ledger name.
        // Re-sync on every open so a previous failed sync cannot be bypassed.
        file.sync_all()?;
        sync_ledger_parent(parent)?;
        let (seen, lines) = load_records(&mut file)?;
        Ok(Self {
            _lock: lock,
            path: path.to_path_buf(),
            inner: Mutex::new(Inner {
                file,
                seen,
                lines,
                compact_retry_at: 0,
                blocked: false,
            }),
            available: AtomicBool::new(true),
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
        append(&mut inner, &record, &self.available)?;
        inner.seen.insert(intent_id.into(), record);
        compact_best_effort(&mut inner, &self.path, &self.available)?;
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
        append(&mut inner, &record, &self.available)?;
        inner.seen.insert(intent_id.into(), record);
        compact_best_effort(&mut inner, &self.path, &self.available)?;
        Ok(())
    }

    pub fn is_available(&self) -> bool {
        self.available.load(Ordering::Acquire) && !self.inner.is_poisoned()
    }

    #[cfg(test)]
    pub(crate) fn fail_writes_for_tests(&self) {
        self.inner.lock().unwrap().file = File::open(&self.path).unwrap();
    }

    /// Reuse the existing process lock and ledger object: in-flight sockets
    /// may still hold an Arc to this value while storage is repaired.
    pub fn recover(&self) -> BridgeResult<()> {
        self.recover_with_sync(sync_parent)
    }

    fn recover_with_sync(
        &self,
        sync_probe_parent: impl FnOnce(&Path) -> io::Result<()>,
    ) -> BridgeResult<()> {
        let mut inner = self.inner()?;
        if !inner.blocked {
            return Ok(());
        }
        durable_probe(&self.path, sync_probe_parent)?;
        // Never recreate a missing journal: that could turn a printed intent
        // into Fresh. The retained lock protects this same journal identity.
        let mut file = open_existing_file(&self.path)?;
        file.sync_all()?;
        sync_parent(parent_dir(&self.path))?;
        let (seen, lines) = load_records(&mut file)?;
        for (id, previous) in &inner.seen {
            let Some(reloaded) = seen.get(id) else {
                return Err(BridgeError::Config(
                    "local print ledger lost a previously durable intent".into(),
                ));
            };
            if previous.fingerprint != reloaded.fingerprint
                || (previous.outcome.is_some() && previous.outcome != reloaded.outcome)
            {
                return Err(BridgeError::Config(
                    "local print ledger changed a previously durable intent".into(),
                ));
            }
        }
        inner.file = file;
        inner.seen = seen;
        inner.lines = lines;
        inner.compact_retry_at = 0;
        inner.blocked = false;
        self.available.store(true, Ordering::Release);
        Ok(())
    }

    fn inner(&self) -> BridgeResult<MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| BridgeError::Config("local print ledger lock is poisoned".into()))
    }
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn load_records(file: &mut File) -> BridgeResult<(HashMap<String, Record>, usize)> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err(BridgeError::Config(
            "local print ledger has a torn final record".into(),
        ));
    }
    let mut seen: HashMap<String, Record> = HashMap::new();
    let mut lines = 0;
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        lines += 1;
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
    Ok((seen, lines))
}

fn durable_probe(path: &Path, sync_dir: impl FnOnce(&Path) -> io::Result<()>) -> io::Result<()> {
    let parent = parent_dir(path);
    let probe = parent.join(format!(".local-print-probe-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x8000_0000); // FILE_FLAG_WRITE_THROUGH
        }
        let mut file = options.open(&probe)?;
        file.write_all(b"durability probe\n")?;
        file.sync_all()?;
        drop(file);
        sync_dir(parent)?;
        std::fs::remove_file(&probe)?;
        sync_parent(parent)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&probe);
    }
    result
}

fn open_existing_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).append(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x8000_0000);
    }
    options.open(path)
}

fn open_file(path: &Path) -> io::Result<(File, bool)> {
    let mut create = OpenOptions::new();
    create.read(true).append(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // CreateFileW FILE_FLAG_WRITE_THROUGH flushes NTFS metadata changes.
        // The later sync_all maps to FlushFileBuffers, including file metadata.
        // See Microsoft Learn: CreateFileW and File Caching.
        create.custom_flags(0x8000_0000);
    }
    match create.open(path) {
        Ok(file) => Ok((file, true)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let mut existing = OpenOptions::new();
            existing.read(true).append(true);
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                existing.custom_flags(0x8000_0000);
            }
            Ok((existing.open(path)?, false))
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> io::Result<()> {
    File::open(parent)?.sync_all()
}

#[cfg(windows)]
fn sync_parent(_parent: &Path) -> io::Result<()> {
    // Windows has no portable directory fsync. The write-through file handle
    // plus sync_all above is the documented file-and-metadata flush path.
    Ok(())
}

fn compact_best_effort(inner: &mut Inner, path: &Path, available: &AtomicBool) -> BridgeResult<()> {
    compact_best_effort_with_sync(inner, path, available, sync_parent)
}

fn compact_best_effort_with_sync(
    inner: &mut Inner,
    path: &Path,
    available: &AtomicBool,
    sync_dir: impl FnOnce(&Path) -> io::Result<()>,
) -> BridgeResult<()> {
    if inner.blocked
        || inner.lines < inner.compact_retry_at
        || inner.lines.saturating_sub(inner.seen.len()) < COMPACT_AFTER_REDUNDANT_LINES
    {
        return Ok(());
    }
    if let Err(error) = compact_with_sync(inner, path, available, sync_dir) {
        inner.compact_retry_at = inner.lines.saturating_add(COMPACT_RETRY_AFTER_LINES);
        if inner.blocked {
            return Err(error);
        }
        tracing::warn!(%error, path = %path.display(), "local intent compaction deferred without evicting IDs");
    }
    Ok(())
}

fn compact_with_sync(
    inner: &mut Inner,
    path: &Path,
    available: &AtomicBool,
    sync_dir: impl FnOnce(&Path) -> io::Result<()>,
) -> BridgeResult<()> {
    compact_with_io(
        inner,
        path,
        available,
        sync_dir,
        write_compacted_records,
        |from, to| std::fs::rename(from, to),
    )
}

fn write_compacted_records(file: &mut File, seen: &HashMap<String, Record>) -> BridgeResult<()> {
    let mut ids: Vec<_> = seen.keys().collect();
    ids.sort();
    for id in ids {
        serde_json::to_writer(&mut *file, &seen[id]).map_err(|error| {
            BridgeError::Config(format!("cannot encode compacted print intent: {error}"))
        })?;
        file.write_all(b"\n")?;
    }
    Ok(())
}

fn compact_with_io(
    inner: &mut Inner,
    path: &Path,
    available: &AtomicBool,
    sync_dir: impl FnOnce(&Path) -> io::Result<()>,
    write_records: impl FnOnce(&mut File, &HashMap<String, Record>) -> BridgeResult<()>,
    replace: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> BridgeResult<()> {
    if inner.blocked {
        return Err(BridgeError::Config("local print ledger is blocked".into()));
    }
    // Re-read the durable journal rather than serializing only this process's
    // map. The retained process lock prevents another bridge from writing it.
    let loaded = open_existing_file(path)
        .map_err(BridgeError::from)
        .and_then(|mut file| load_records(&mut file));
    let (seen, _) = match loaded {
        Ok(records) => records,
        Err(error) => {
            inner.blocked = true;
            available.store(false, Ordering::Release);
            return Err(error);
        }
    };
    for (id, previous) in &inner.seen {
        let Some(on_disk) = seen.get(id) else {
            inner.blocked = true;
            available.store(false, Ordering::Release);
            return Err(BridgeError::Config(
                "local print ledger lost a durable intent".into(),
            ));
        };
        if previous.fingerprint != on_disk.fingerprint
            || (previous.outcome.is_some() && previous.outcome != on_disk.outcome)
        {
            inner.blocked = true;
            available.store(false, Ordering::Release);
            return Err(BridgeError::Config(
                "local print ledger changed a durable intent".into(),
            ));
        }
    }

    let tmp = path.with_extension("jsonl.tmp");
    // Only this process owns the ledger lock; a leftover temp after a crash is
    // not authoritative and must not prevent future compaction attempts.
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let written = (|| -> BridgeResult<File> {
        let mut options = OpenOptions::new();
        options.read(true).append(true).create_new(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x8000_0000); // FILE_FLAG_WRITE_THROUGH
        }
        let mut file = options.open(&tmp)?;
        #[cfg(windows)]
        crate::windows_acl::preserve_ledger_acl_for_temp(path, &tmp)?;
        #[cfg(unix)]
        std::fs::set_permissions(&tmp, std::fs::metadata(path)?.permissions())?;
        write_records(&mut file, &seen)?;
        file.sync_all()?;
        Ok(file)
    })();
    let file = match written {
        Ok(file) => file,
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
    };
    if let Err(error) = replace(&tmp, path) {
        drop(file);
        let _ = std::fs::remove_file(&tmp);
        return Err(error.into());
    }
    // After rename, the old append handle points at the replaced file. Any
    // durability error must block appends until recovery reopens this path.
    if let Err(error) = sync_dir(parent_dir(path)) {
        inner.blocked = true;
        available.store(false, Ordering::Release);
        return Err(error.into());
    }
    inner.file = file;
    inner.lines = seen.len();
    inner.seen = seen;
    inner.compact_retry_at = 0;
    Ok(())
}

fn append(inner: &mut Inner, record: &Record, available: &AtomicBool) -> BridgeResult<()> {
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
        available.store(false, Ordering::Release);
        return Err(BridgeError::Io(error));
    }
    inner.lines += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        path::PathBuf,
        sync::{mpsc, Arc, Barrier},
        time::Duration,
    };

    use super::{
        compact_best_effort_with_sync, compact_with_io, compact_with_sync, open_file, Claim,
        LocalJobLedger, Outcome, COMPACT_AFTER_REDUNDANT_LINES,
    };

    fn ledger_path() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bridge-local-ledger-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("test directory");
        dir.join("local-print-intents.jsonl")
    }

    #[test]
    fn ledger_open_requires_parent_sync_before_it_can_accept_claims() {
        let path = ledger_path();
        let parent = path.parent().unwrap().to_path_buf();
        let mut synced = false;
        let ledger = LocalJobLedger::open_with_sync(&path, |candidate| {
            assert_eq!(candidate, parent);
            synced = true;
            Ok(())
        })
        .expect("open after parent sync");
        assert!(synced);
        drop(ledger);

        let before = std::fs::read(&path).expect("read ledger");
        assert!(
            LocalJobLedger::open_with_sync(&path, |_| Err(io::Error::other("sync failed")))
                .is_err()
        );
        assert_eq!(std::fs::read(&path).expect("read after refusal"), before);
    }

    #[test]
    fn initial_file_creation_has_exactly_one_winner() {
        let path = ledger_path();
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let (_file, created) = open_file(&path).expect("open ledger file");
                    created
                })
            })
            .collect();
        let created = handles
            .into_iter()
            .map(|handle| handle.join().expect("worker"))
            .filter(|created| *created)
            .count();
        assert_eq!(created, 1);
        assert!(path.is_file());
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
    fn repeated_transitions_compact_without_forgetting_any_intent() {
        let path = ledger_path();
        let ledger = LocalJobLedger::open(&path).unwrap();
        for index in 0..10 {
            let id = format!("completed-{index}");
            let fingerprint = format!("payload-{index}");
            assert_eq!(ledger.claim(&id, &fingerprint).unwrap(), Claim::Fresh);
            ledger
                .finish(
                    &id,
                    Outcome {
                        job_id: index + 1,
                        error: (index == 3).then(|| "printer offline".into()),
                    },
                )
                .unwrap();
        }
        assert_eq!(ledger.claim("uncertain", "pending").unwrap(), Claim::Fresh);
        let lines = std::fs::read_to_string(&path).unwrap().lines().count();
        assert!(lines <= 15, "redundant transition lines remained: {lines}");
        drop(ledger);

        let reopened = LocalJobLedger::open(&path).unwrap();
        for index in 0..10 {
            let id = format!("completed-{index}");
            let fingerprint = format!("payload-{index}");
            assert_eq!(
                reopened.claim(&id, &fingerprint).unwrap(),
                Claim::Done(Outcome {
                    job_id: index + 1,
                    error: (index == 3).then(|| "printer offline".into()),
                })
            );
            assert_eq!(reopened.claim(&id, "different").unwrap(), Claim::Conflict);
        }
        assert_eq!(
            reopened.claim("uncertain", "pending").unwrap(),
            Claim::Interrupted
        );
    }

    #[test]
    fn failed_temp_creation_preserves_bytes_and_existing_claims() {
        let path = ledger_path();
        let ledger = LocalJobLedger::open(&path).unwrap();
        assert_eq!(ledger.claim("stable", "digest").unwrap(), Claim::Fresh);
        let before = std::fs::read(&path).unwrap();
        let tmp = path.with_extension("jsonl.tmp");
        std::fs::create_dir(&tmp).unwrap();
        assert!(compact_with_sync(
            &mut ledger.inner.lock().unwrap(),
            &path,
            &ledger.available,
            super::sync_parent,
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(ledger.is_available());
        assert_eq!(
            ledger.claim("stable", "digest").unwrap(),
            Claim::Interrupted
        );
        std::fs::remove_dir(&tmp).unwrap();
    }

    #[test]
    fn partial_temp_write_or_failed_rename_keeps_the_prior_journal() {
        let path = ledger_path();
        let ledger = LocalJobLedger::open(&path).unwrap();
        assert_eq!(ledger.claim("stable", "digest").unwrap(), Claim::Fresh);
        let before = std::fs::read(&path).unwrap();
        let tmp = path.with_extension("jsonl.tmp");

        assert!(compact_with_io(
            &mut ledger.inner.lock().unwrap(),
            &path,
            &ledger.available,
            super::sync_parent,
            |file, _| {
                use std::io::Write;
                file.write_all(b"partial")?;
                Err(crate::error::BridgeError::Io(io::Error::other("disk full")))
            },
            |from, to| std::fs::rename(from, to),
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!tmp.exists());

        assert!(compact_with_io(
            &mut ledger.inner.lock().unwrap(),
            &path,
            &ledger.available,
            super::sync_parent,
            super::write_compacted_records,
            |_, _| Err(io::Error::other("rename failed")),
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!tmp.exists());
        assert!(ledger.is_available());
        assert_eq!(
            ledger.claim("stable", "digest").unwrap(),
            Claim::Interrupted
        );
    }

    #[test]
    fn failed_directory_sync_after_replace_blocks_until_recovery() {
        let path = ledger_path();
        let ledger = LocalJobLedger::open(&path).unwrap();
        assert_eq!(ledger.claim("stable", "digest").unwrap(), Claim::Fresh);
        ledger
            .finish(
                "stable",
                Outcome {
                    job_id: 74,
                    error: None,
                },
            )
            .unwrap();
        let result = compact_with_sync(
            &mut ledger.inner.lock().unwrap(),
            &path,
            &ledger.available,
            |_| Err(io::Error::other("directory sync failed")),
        );
        assert!(result.is_err());
        assert!(!ledger.is_available());
        assert!(ledger.claim("new", "digest").is_err());
        ledger.recover().unwrap();
        assert_eq!(
            ledger.claim("stable", "digest").unwrap(),
            Claim::Done(Outcome {
                job_id: 74,
                error: None,
            })
        );
        drop(ledger);
        assert_eq!(
            LocalJobLedger::open(&path)
                .unwrap()
                .claim("stable", "digest")
                .unwrap(),
            Claim::Done(Outcome {
                job_id: 74,
                error: None,
            })
        );
    }

    #[test]
    fn post_replace_sync_failure_is_not_reported_as_a_fresh_claim() {
        let path = ledger_path();
        let ledger = LocalJobLedger::open(&path).unwrap();
        assert_eq!(ledger.claim("stable", "digest").unwrap(), Claim::Fresh);
        let mut inner = ledger.inner.lock().unwrap();
        inner.lines = inner.seen.len() + COMPACT_AFTER_REDUNDANT_LINES;
        let result = compact_best_effort_with_sync(&mut inner, &path, &ledger.available, |_| {
            Err(io::Error::other("directory sync failed"))
        });
        assert!(result.is_err(), "a post-replace failure must fail closed");
        drop(inner);
        assert!(!ledger.is_available());
        assert!(ledger.claim("another", "digest").is_err());
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
    fn blocked_ledger_recovers_in_place_only_after_a_durable_probe() {
        let path = ledger_path();
        let ledger = LocalJobLedger::open(&path).expect("open ledger");
        assert_eq!(ledger.claim("printing", "digest").unwrap(), Claim::Fresh);
        ledger.inner.lock().unwrap().file = std::fs::File::open(&path).unwrap();
        assert!(ledger
            .finish(
                "printing",
                Outcome {
                    job_id: 7,
                    error: None,
                },
            )
            .is_err());
        assert!(!ledger.is_available());
        assert!(LocalJobLedger::open(&path).is_err(), "the lock stays held");

        let before = std::fs::read(&path).unwrap();
        assert!(ledger
            .recover_with_sync(|_| Err(io::Error::other("probe sync failed")))
            .is_err());
        assert!(!ledger.is_available());
        assert_eq!(std::fs::read(&path).unwrap(), before);

        ledger.recover().expect("repaired storage");
        assert!(ledger.is_available());
        assert_eq!(
            ledger.claim("printing", "digest").unwrap(),
            Claim::Interrupted
        );
        assert_eq!(ledger.claim("new", "digest").unwrap(), Claim::Fresh);
    }

    #[test]
    fn recovery_refuses_a_replaced_ledger_that_lost_prior_claims() {
        let path = ledger_path();
        let ledger = LocalJobLedger::open(&path).unwrap();
        assert_eq!(ledger.claim("stable", "digest").unwrap(), Claim::Fresh);
        ledger.inner.lock().unwrap().file = std::fs::File::open(&path).unwrap();
        assert!(ledger.claim("blocked", "digest").is_err());
        std::fs::write(&path, b"").unwrap();
        assert!(ledger.recover().is_err());
        assert!(!ledger.is_available());
        assert!(ledger.claim("another", "digest").is_err());
    }

    #[test]
    fn availability_does_not_wait_for_a_durable_recovery_probe() {
        let path = ledger_path();
        let ledger = Arc::new(LocalJobLedger::open(&path).unwrap());
        ledger.inner.lock().unwrap().file = std::fs::File::open(&path).unwrap();
        assert!(ledger.claim("blocked", "digest").is_err());

        let (entered_tx, entered_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let recovering = Arc::clone(&ledger);
        let recovery = std::thread::spawn(move || {
            recovering.recover_with_sync(|parent| {
                entered_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
                super::sync_parent(parent)
            })
        });
        entered_rx.recv().unwrap();
        let (available_tx, available_rx) = mpsc::channel();
        let checking = Arc::clone(&ledger);
        let check = std::thread::spawn(move || available_tx.send(checking.is_available()).unwrap());
        let observed = available_rx.recv_timeout(Duration::from_millis(150));
        resume_tx.send(()).unwrap();
        recovery.join().unwrap().unwrap();
        check.join().unwrap();
        assert!(!observed.unwrap(), "availability blocked an async caller");
        assert!(ledger.is_available());
    }

    #[test]
    fn a_bare_relative_ledger_name_opens_in_the_current_directory() {
        if std::env::var_os("BRIDGE_RELATIVE_LEDGER_CHILD").is_some() {
            let ledger = LocalJobLedger::open("local-print-intents.jsonl").unwrap();
            assert_eq!(ledger.claim("relative", "digest").unwrap(), Claim::Fresh);
            ledger.inner.lock().unwrap().file =
                std::fs::File::open("local-print-intents.jsonl").unwrap();
            assert!(ledger.claim("blocked", "digest").is_err());
            ledger.recover().unwrap();
            assert_eq!(
                ledger.claim("relative", "digest").unwrap(),
                Claim::Interrupted
            );
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("bridge-relative-ledger-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "local_job_ledger::tests::a_bare_relative_ledger_name_opens_in_the_current_directory",
                "--nocapture",
            ])
            .env("BRIDGE_RELATIVE_LEDGER_CHILD", "1")
            .current_dir(&dir)
            .output()
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
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
