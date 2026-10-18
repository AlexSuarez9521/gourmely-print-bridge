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
//! never take printing offline. The PEM on disk is validated for key match,
//! trust chain, hostname, and current validity before selection. A bad override
//! falls back only if the embedded pair independently validates; otherwise
//! startup fails with an actionable diagnostic.

use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::{CertificateDer, UnixTime};
use std::path::PathBuf;
use std::sync::Arc;

/// Where a renewal drops the new PEM pair.
///
/// `%PROGRAMDATA%\GourmelyPrint\certs` — machine-wide (every Windows account on
/// the till shares one bridge) and, unlike `%PROGRAMFILES%`, writable without
/// fighting UAC on every renewal. Debug builds may redirect this with
/// `GOURMELYPRINT_CERT_DIR` for isolated tests; release builds cannot.
pub fn cert_dir() -> Option<PathBuf> {
    #[cfg(debug_assertions)]
    {
        if let Some(dir) = std::env::var_os("GOURMELYPRINT_CERT_DIR") {
            return Some(PathBuf::from(dir));
        }
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

/// Parse a PEM pair and prove that its private key matches the leaf certificate.
fn matching_chain(cert: &[u8], key: &[u8]) -> Result<Vec<CertificateDer<'static>>, String> {
    let mut cert_reader = std::io::BufReader::new(cert);
    let certs = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("parse certificate chain: {error}"))?;
    if certs.is_empty() {
        return Err("certificate chain is empty".into());
    }

    let mut key_reader = std::io::BufReader::new(key);
    let key = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|error| format!("parse private key: {error}"))?
        .ok_or("private key is missing")?;

    rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .and_then(|builder| {
        builder
            .with_no_client_auth()
            .with_single_cert(certs.clone(), key)
    })
    .map_err(|error| format!("certificate/key mismatch: {error}"))?;
    Ok(certs)
}

fn validate_pair_at(cert: &[u8], key: &[u8], host: &str, now: UnixTime) -> Result<(), String> {
    let certs = matching_chain(cert, key)?;
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
        Arc::new(roots),
        Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
    )
    .build()
    .map_err(|error| format!("TLS verifier: {error}"))?;
    let server_name = host
        .try_into()
        .map_err(|error| format!("invalid TLS hostname: {error}"))?;
    verifier
        .verify_server_cert(&certs[0], &certs[1..], &server_name, &[], now)
        .map_err(|error| format!("certificate is not valid for {host}: {error}"))?;
    Ok(())
}

/// Pick a validated disk pair, or independently validate the embedded fallback.
pub fn resolve(embedded_cert: &[u8], embedded_key: &[u8]) -> Result<TlsMaterial, String> {
    resolve_at(embedded_cert, embedded_key, UnixTime::now())
}

fn resolve_at(
    embedded_cert: &[u8],
    embedded_key: &[u8],
    now: UnixTime,
) -> Result<TlsMaterial, String> {
    let embedded = || {
        validate_pair_at(embedded_cert, embedded_key, crate::config::BRIDGE_HOST, now).map_err(
            |error| format!("no usable local TLS certificate; install a valid PEM pair: {error}"),
        )?;
        Ok(TlsMaterial {
            cert: embedded_cert.to_vec(),
            key: embedded_key.to_vec(),
            source: "embedded",
        })
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

    if let Err(error) = validate_pair_at(&cert, &key, crate::config::BRIDGE_HOST, now) {
        tracing::error!(
            dir = %dir.display(),
            %error,
            "certificate override is unusable; checking embedded certificate"
        );
        return embedded();
    }

    tracing::info!(dir = %dir.display(), "serving the certificate found on disk");
    Ok(TlsMaterial {
        cert,
        key,
        source: "disk",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    const EMBEDDED_CERT: &[u8] = include_bytes!("../certs/fullchain.pem");
    const EMBEDDED_KEY: &[u8] = include_bytes!("../certs/privkey.pem");
    const UNRELATED_KEY: &[u8] = include_bytes!("../tests/fixtures/unrelated-key.pem");

    /// The shipped certificate's validity window, read from the certificate
    /// itself. Fixed dates broke every renewal: a pair issued after the pinned
    /// "now" was "not valid yet", and a pinned "after expiry" could fall inside
    /// the new window. The first two UTCTime values (tag 0x17, 13 bytes) in a
    /// leaf certificate's DER are notBefore and notAfter: names never use
    /// UTCTime, and Let's Encrypt dates stay in UTCTime until 2050.
    fn embedded_validity() -> (u64, u64) {
        let der = rustls_pemfile::certs(&mut std::io::BufReader::new(EMBEDDED_CERT))
            .next()
            .expect("the embedded chain has a leaf")
            .expect("the leaf is valid PEM");
        let bytes = der.as_ref();
        let mut times = bytes
            .windows(15)
            .filter(|w| w[0] == 0x17 && w[1] == 0x0d && w[14] == b'Z')
            .map(|w| utc_time_to_unix(&w[2..14]));
        let not_before = times.next().expect("notBefore");
        let not_after = times.next().expect("notAfter");
        (not_before, not_after)
    }

    /// "YYMMDDHHMMSS" (UTCTime, years 1950–2049) to seconds since the epoch.
    fn utc_time_to_unix(digits: &[u8]) -> u64 {
        let n = |i: usize| u64::from((digits[i] - b'0') * 10 + (digits[i + 1] - b'0'));
        let yy = n(0);
        let year = if yy < 50 { 2000 + yy } else { 1900 + yy };
        let (month, day) = (n(2), n(4));
        // Days from civil (Howard Hinnant), valid for the Gregorian calendar.
        let y = if month <= 2 { year - 1 } else { year } as i64;
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let m = month as i64;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        days as u64 * 86_400 + n(6) * 3_600 + n(8) * 60 + n(10)
    }

    /// A moment one day inside the shipped certificate's validity.
    fn valid_at() -> rustls::pki_types::UnixTime {
        rustls::pki_types::UnixTime::since_unix_epoch(std::time::Duration::from_secs(
            embedded_validity().0 + 86_400,
        ))
    }

    #[test]
    fn the_validity_window_is_read_from_the_certificate() {
        let (not_before, not_after) = embedded_validity();
        // Let's Encrypt issues 90-day certificates.
        assert_eq!(not_after - not_before, 90 * 86_400 - 1);
        // Spot-check the date arithmetic against a known value: 2026-10-01T08:41:47Z.
        assert_eq!(utc_time_to_unix(b"261001084147"), 1_790_844_107);
    }

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
    fn the_shipped_certificate_validates_at_its_issue_date() {
        assert!(
            validate_pair_at(EMBEDDED_CERT, EMBEDDED_KEY, config::BRIDGE_HOST, valid_at()).is_ok()
        );
    }

    #[test]
    fn rejects_garbage_and_swapped_pairs() {
        assert!(matching_chain(b"not a pem at all", EMBEDDED_KEY).is_err());
        assert!(matching_chain(EMBEDDED_CERT, b"").is_err());
        // Cert where the key is expected, and vice versa.
        assert!(matching_chain(EMBEDDED_KEY, EMBEDDED_CERT).is_err());
    }

    #[test]
    fn falls_back_to_embedded_when_nothing_on_disk() {
        let material =
            with_cert_dir(None, || resolve_at(EMBEDDED_CERT, EMBEDDED_KEY, valid_at())).unwrap();
        assert_eq!(material.source, "embedded");
        assert_eq!(material.cert, EMBEDDED_CERT);
    }

    #[test]
    fn prefers_a_valid_pair_on_disk() {
        let dir = temp_dir("valid");
        std::fs::write(dir.join("fullchain.pem"), EMBEDDED_CERT).unwrap();
        std::fs::write(dir.join("privkey.pem"), EMBEDDED_KEY).unwrap();

        let material = with_cert_dir(Some(&dir), || {
            resolve_at(b"ignored", b"ignored", valid_at())
        })
        .unwrap();

        assert_eq!(material.source, "disk");
        assert_eq!(material.cert, EMBEDDED_CERT);
    }

    #[test]
    fn a_corrupt_file_on_disk_does_not_take_printing_offline() {
        let dir = temp_dir("corrupt");
        std::fs::write(dir.join("fullchain.pem"), b"<html>404</html>").unwrap();
        std::fs::write(dir.join("privkey.pem"), EMBEDDED_KEY).unwrap();

        let material = with_cert_dir(Some(&dir), || {
            resolve_at(EMBEDDED_CERT, EMBEDDED_KEY, valid_at())
        })
        .unwrap();

        assert_eq!(material.source, "embedded");
    }

    #[test]
    fn half_a_pair_is_ignored() {
        // A renewal interrupted after writing the cert but before the key.
        let dir = temp_dir("half");
        std::fs::write(dir.join("fullchain.pem"), EMBEDDED_CERT).unwrap();

        let material = with_cert_dir(Some(&dir), || {
            resolve_at(EMBEDDED_CERT, EMBEDDED_KEY, valid_at())
        })
        .unwrap();

        assert_eq!(material.source, "embedded");
    }

    #[test]
    fn mismatched_but_parseable_disk_pair_falls_back_to_embedded() {
        let dir = temp_dir("mismatched-key");
        std::fs::write(dir.join("fullchain.pem"), EMBEDDED_CERT).unwrap();
        std::fs::write(dir.join("privkey.pem"), UNRELATED_KEY).unwrap();
        assert!(
            rustls_pemfile::private_key(&mut std::io::BufReader::new(UNRELATED_KEY))
                .unwrap()
                .is_some()
        );

        let material = with_cert_dir(Some(&dir), || {
            resolve_at(EMBEDDED_CERT, EMBEDDED_KEY, valid_at())
        })
        .unwrap();

        assert_eq!(material.source, "embedded");
        assert_eq!(material.cert, EMBEDDED_CERT);
        assert_eq!(material.key, EMBEDDED_KEY);
    }

    #[test]
    fn rejects_expired_pair_even_when_key_matches() {
        let after_expiry = rustls::pki_types::UnixTime::since_unix_epoch(
            std::time::Duration::from_secs(embedded_validity().1 + 86_400),
        );
        assert!(
            validate_pair_at(EMBEDDED_CERT, EMBEDDED_KEY, config::BRIDGE_HOST, valid_at()).is_ok()
        );
        assert!(validate_pair_at(
            EMBEDDED_CERT,
            EMBEDDED_KEY,
            config::BRIDGE_HOST,
            after_expiry
        )
        .is_err());
    }

    #[test]
    fn rejects_certificate_for_another_hostname() {
        assert!(
            validate_pair_at(EMBEDDED_CERT, EMBEDDED_KEY, "wrong.invalid", valid_at()).is_err()
        );
    }

    #[test]
    fn invalid_embedded_pair_requires_action_instead_of_serving_it() {
        let error = with_cert_dir(None, || resolve_at(b"invalid", b"invalid", valid_at()))
            .err()
            .expect("must refuse invalid embedded pair");
        assert!(error.contains("install a valid PEM pair"), "{error}");
    }
}
