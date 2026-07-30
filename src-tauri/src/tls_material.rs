//! Where the bridge gets its TLS certificate from.
//!
//! # Why this exists
//!
//! The cert used to be *only* `include_bytes!`-embedded, which made the
//! installer a single self-contained file — a good property we keep. The cost
//! was hidden until it bit: **renewing the cert required recompiling and
//! shipping a new MSI**, because the bytes live inside the binary.
//!
//! That cost is about to get much worse. Let's Encrypt certs last 90 days
//! today, drop to 64 days in February 2027 and to 45 days in 2028, and the
//! CA/Browser Forum caps *every* public CA at 200 days since March 2026, 100
//! days from March 2027 and 47 days from 2029. There is no longer, paid or
//! free, option anywhere. Tying a release to each renewal means 4 releases a
//! year today and 8 by 2029.
//!
//! So: look for a cert on disk first, fall back to the embedded one. A renewal
//! becomes "drop two files and restart the bridge" — no rebuild, no release, no
//! auto-update round trip.
//!
//! # Failure behaviour, on purpose
//!
//! The embedded cert is the **safety net, not the fallback of last resort**: a
//! truncated download, a half-written file or a swapped cert/key pair must
//! never take printing offline. The PEM on disk is parsed *before* it is
//! trusted, and any problem logs loudly and keeps the embedded material. The
//! only way to end up with no TLS is for the embedded cert itself to be broken,
//! which the build would have caught.
//!
//! # What this does NOT do yet
//!
//! It does not compare expiry dates: if a file is present and parses, it wins.
//! That is safe for the automated renewal (which only ever writes a fresh
//! cert), but a human who drops an *expired* file would take the bridge down
//! until they remove it. Comparing `notAfter` needs an X.509 parser
//! (`x509-parser`), deliberately left out of this change to keep it small.

use std::path::PathBuf;

/// Where a renewal drops the new PEM pair.
///
/// `%PROGRAMDATA%\GourmelyPrint\certs` — machine-wide (every Windows account on
/// the till shares one bridge) and, unlike `%PROGRAMFILES%`, writable without
/// fighting UAC on every renewal. Overridable with `GOURMELYPRINT_CERT_DIR`,
/// which is what support uses to test a cert on one machine without touching
/// the real path.
pub fn cert_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("GOURMELYPRINT_CERT_DIR") {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("PROGRAMDATA").map(|p| PathBuf::from(p).join("GourmelyPrint").join("certs"))
}

/// Resolved TLS material plus where it came from, so the log line and the
/// settings window can say which one is live.
pub struct TlsMaterial {
    pub cert: Vec<u8>,
    pub key: Vec<u8>,
    /// `"disk"` or `"embedded"` — logged at startup and shown in support logs.
    pub source: &'static str,
}

/// Does this pair actually parse as a certificate chain and a private key?
///
/// A pure parse check via `rustls-pemfile`: no crypto provider needed (unlike
/// `RustlsConfig::from_pem`, which panics unless a provider was installed
/// first), no async, no I/O. Catches the realistic failures — empty file, HTML
/// error page saved as `.pem`, cert and key swapped, truncated download.
fn pem_pair_parses(cert: &[u8], key: &[u8]) -> bool {
    let mut cert_reader = std::io::BufReader::new(cert);
    let certs = rustls_pemfile::certs(&mut cert_reader).collect::<Result<Vec<_>, _>>();
    let chain_ok = matches!(certs, Ok(ref chain) if !chain.is_empty());

    let mut key_reader = std::io::BufReader::new(key);
    let key_ok = matches!(rustls_pemfile::private_key(&mut key_reader), Ok(Some(_)));

    chain_ok && key_ok
}

/// Pick the TLS material to serve: the PEM pair on disk when it is present and
/// parses, otherwise the embedded one. Never fails — worst case it returns the
/// embedded material.
pub fn resolve(embedded_cert: &[u8], embedded_key: &[u8]) -> TlsMaterial {
    let embedded = || TlsMaterial {
        cert: embedded_cert.to_vec(),
        key: embedded_key.to_vec(),
        source: "embedded",
    };

    let Some(dir) = cert_dir() else {
        tracing::debug!("no cert directory resolvable; using the embedded certificate");
        return embedded();
    };

    let cert_path = dir.join("fullchain.pem");
    let key_path = dir.join("privkey.pem");

    if !cert_path.exists() && !key_path.exists() {
        tracing::debug!(dir = %dir.display(), "no certificate override on disk; using the embedded one");
        return embedded();
    }

    let (cert, key) = match (std::fs::read(&cert_path), std::fs::read(&key_path)) {
        (Ok(c), Ok(k)) => (c, k),
        (cert_result, key_result) => {
            // One of the two is missing or unreadable. Half a pair is never
            // usable, and it usually means a renewal was interrupted.
            tracing::error!(
                dir = %dir.display(),
                cert_err = ?cert_result.err(),
                key_err = ?key_result.err(),
                "certificate override present but unreadable; keeping the embedded certificate"
            );
            return embedded();
        }
    };

    if !pem_pair_parses(&cert, &key) {
        tracing::error!(
            dir = %dir.display(),
            "certificate override does not parse as a chain + private key; keeping the embedded certificate"
        );
        return embedded();
    }

    tracing::info!(dir = %dir.display(), "serving the certificate found on disk");
    TlsMaterial {
        cert,
        key,
        source: "disk",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMBEDDED_CERT: &[u8] = include_bytes!("../certs/fullchain.pem");
    const EMBEDDED_KEY: &[u8] = include_bytes!("../certs/privkey.pem");

    /// Isolates `GOURMELYPRINT_CERT_DIR` per test. Tests in one binary share
    /// the process environment, so they must not run in parallel over it —
    /// hence the mutex rather than `#[test]` alone.
    fn with_cert_dir<T>(dir: Option<&std::path::Path>, body: impl FnOnce() -> T) -> T {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        match dir {
            Some(d) => std::env::set_var("GOURMELYPRINT_CERT_DIR", d),
            None => std::env::set_var("GOURMELYPRINT_CERT_DIR", "\\\\?\\C:\\nonexistent-gmly-test"),
        }
        let out = body();
        std::env::remove_var("GOURMELYPRINT_CERT_DIR");
        out
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gmly-tls-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn the_shipped_certificate_parses() {
        // Guards the safety net itself: if the embedded pair ever stops
        // parsing, every fallback path in this module is worthless.
        assert!(pem_pair_parses(EMBEDDED_CERT, EMBEDDED_KEY));
    }

    #[test]
    fn rejects_garbage_and_swapped_pairs() {
        assert!(!pem_pair_parses(b"not a pem at all", EMBEDDED_KEY));
        assert!(!pem_pair_parses(EMBEDDED_CERT, b""));
        // Cert where the key is expected, and vice versa.
        assert!(!pem_pair_parses(EMBEDDED_KEY, EMBEDDED_CERT));
    }

    #[test]
    fn falls_back_to_embedded_when_nothing_on_disk() {
        let material = with_cert_dir(None, || resolve(EMBEDDED_CERT, EMBEDDED_KEY));
        assert_eq!(material.source, "embedded");
        assert_eq!(material.cert, EMBEDDED_CERT);
    }

    #[test]
    fn prefers_a_valid_pair_on_disk() {
        let dir = temp_dir("valid");
        std::fs::write(dir.join("fullchain.pem"), EMBEDDED_CERT).unwrap();
        std::fs::write(dir.join("privkey.pem"), EMBEDDED_KEY).unwrap();

        let material = with_cert_dir(Some(&dir), || resolve(b"ignored", b"ignored"));

        assert_eq!(material.source, "disk");
        assert_eq!(material.cert, EMBEDDED_CERT);
    }

    #[test]
    fn a_corrupt_file_on_disk_does_not_take_printing_offline() {
        let dir = temp_dir("corrupt");
        std::fs::write(dir.join("fullchain.pem"), b"<html>404</html>").unwrap();
        std::fs::write(dir.join("privkey.pem"), EMBEDDED_KEY).unwrap();

        let material = with_cert_dir(Some(&dir), || resolve(EMBEDDED_CERT, EMBEDDED_KEY));

        assert_eq!(material.source, "embedded");
    }

    #[test]
    fn half_a_pair_is_ignored() {
        // A renewal interrupted after writing the cert but before the key.
        let dir = temp_dir("half");
        std::fs::write(dir.join("fullchain.pem"), EMBEDDED_CERT).unwrap();

        let material = with_cert_dir(Some(&dir), || resolve(EMBEDDED_CERT, EMBEDDED_KEY));

        assert_eq!(material.source, "embedded");
    }
}
