//! Build-time constants and the on-disk settings of the print bridge.
//!
//! The TLS material is NO LONGER hardcoded here: `tls_material::resolve` reads
//! it from `%PROGRAMDATA%\GourmelyPrint\certs` when present and falls back to
//! the pair embedded at compile time, so renewing the certificate no longer
//! needs a rebuild.
//!
//! Everything the operator can change at runtime — which server this till talks
//! to, the station token it got when it was paired, the secret the local POS
//! must present — lives in `%PROGRAMDATA%\GourmelyPrint\config.json`, next to
//! the certs and for the same reason: machine-wide (one bridge serves every
//! Windows account on the till) and writable without fighting UAC.
//!
//! The origin allowlist below is still compile-time. Moving it to the settings
//! file — so support can point one install at a staging origin without a
//! rebuild — is the remaining half of that idea.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL, Engine as _};
use serde::{Deserialize, Serialize};

use crate::error::{BridgeError, BridgeResult};

/// The HTTPS port the bridge listens on. Matches what
/// `apps/platform-web/lib/print-bridge.ts` connects to.
pub const BIND_PORT: u16 = 8181;

/// The DNS name the cert is issued for. Frontend MUST connect via this
/// name (NOT raw `127.0.0.1`) for the TLS handshake to succeed.
pub const BRIDGE_HOST: &str = "localhost.gourmelyhub.busticco.com";

/// Browser origins that may open a WebSocket to the bridge. Any request
/// whose `Origin` header is not in this list is rejected with HTTP 403
/// before the WebSocket upgrade. Prevents random sites the cashier
/// happens to visit from talking to the printer.
pub const ALLOWED_ORIGINS: &[&str] = &[
    "https://app-gourmelyhub.busticco.com",
    "https://gourmelyhub.busticco.com",
    // The bridge's OWN settings window (Tauri WebView2) fetches /health
    // for the status badge. Its origin is tauri.localhost on Windows /
    // tauri://localhost on macOS — without these the badge showed
    // "Sin conexión" even though the service was up (CORS-blocked the
    // self-fetch). 2026-06-09 fix.
    "http://tauri.localhost",
    "https://tauri.localhost",
    "tauri://localhost",
    // Dev origins — keep these only while we're testing locally. Strip
    // before release builds.
    "http://localhost:3000",
    "http://localhost:1420",
];

/// Maximum size of a single print payload in bytes (after base64 decode).
/// A receipt is < 10 KB; we cap at 1 MB to keep one bad actor from
/// hogging RAM by streaming MB of binary into the WSS.
pub const MAX_PRINT_BYTES: usize = 1_024 * 1_024;

/// Where the relay points when nobody has said otherwise.
pub const DEFAULT_SERVER_URL: &str = "https://api-gourmelyhub.busticco.com";

/// How the local WSS decides whether a caller may print.
///
/// # Why three modes and not a boolean
///
/// Until this change the local socket had **no** authentication at all, and the
/// origin allowlist that the module header advertises never rejected anything:
/// `CorsLayer` only decorates responses, and a browser does not apply CORS to a
/// WebSocket handshake. Any page open on the till could print.
///
/// Closing it fully means the web must start presenting a secret, and the web
/// is another repo shipping on its own clock. Breaking printing at the till to
/// win a security property is a bad trade, so the knob has three positions:
///
/// * [`LocalAuth::OriginOrSecret`] — the default and the safe value for a new
///   install. A caller passes if it presents the shared secret **or** if it
///   arrives with an allowlisted `Origin`. A browser cannot forge `Origin`, so
///   this shuts the door on exactly the attack that was open (a random tab on
///   the cashier's Chrome) without the web changing a line.
/// * [`LocalAuth::Secret`] — the end state: the secret is mandatory for
///   everyone, origin or not. Turn this on once the POS sends it.
/// * [`LocalAuth::Off`] — escape hatch for support. Documented, never default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocalAuth {
    Off,
    /// The default, and the safe value for a new install.
    #[default]
    OriginOrSecret,
    Secret,
}

/// Everything the bridge remembers between restarts.
///
/// `station_token` is the credential the relay authenticates with, so this file
/// is as sensitive as a password: it lives under `%PROGRAMDATA%` and is never
/// logged, never returned by `/health`, and never sent to the settings window.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Base URL of the GourmelyHub API this till reports to.
    pub server_url: Option<String>,
    /// Opaque station credential handed out by `POST /v1/print-relay/pair`.
    pub station_token: Option<String>,
    pub station_id: Option<String>,
    pub tenant_id: Option<String>,
    pub branch_id: Option<String>,
    /// Human name of the station, so the settings window can say "Caja
    /// Principal" instead of a uuid.
    pub label: Option<String>,
    pub roles: Vec<String>,
    /// Printer to use when a job arrives without an explicit OS printer name.
    pub default_printer: Option<String>,
    pub local_auth: LocalAuth,
    /// Shared secret for the local WSS. Generated on first run.
    pub local_secret: Option<String>,
}

impl Settings {
    pub fn is_paired(&self) -> bool {
        self.station_token.is_some()
    }

    /// Where the relay connects. Trailing slashes are trimmed so callers can
    /// concatenate paths without doubling the separator.
    pub fn server_url(&self) -> String {
        let raw = self
            .server_url
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(DEFAULT_SERVER_URL);
        raw.trim().trim_end_matches('/').to_string()
    }
}

/// Directory that holds `config.json` (and the cert override).
///
/// `GOURMELYPRINT_DATA_DIR` overrides it, which is what the integration tests
/// and support use to work on a throwaway copy instead of the real install.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("GOURMELYPRINT_DATA_DIR") {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("PROGRAMDATA").map(|p| PathBuf::from(p).join("GourmelyPrint"))
}

fn settings_path() -> Option<PathBuf> {
    data_dir().map(|d| d.join("config.json"))
}

/// 32 bytes of CSPRNG, base64url. Same shape as the station token the server
/// mints, for the same reason: it is a bearer credential.
///
/// Fails loudly rather than falling back to a weaker source: a predictable
/// "secret" is worse than no secret, because it looks like protection.
fn generate_secret() -> BridgeResult<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|e| BridgeError::Config(format!("el CSPRNG del sistema falló: {e}")))?;
    Ok(BASE64URL.encode(bytes))
}

/// Reads the settings file, tolerating every way it can be absent or broken.
///
/// A corrupt file must never stop the bridge from booting: the till would lose
/// printing over a JSON typo. It falls back to defaults and logs loudly, the
/// same posture as `tls_material`.
fn load_from_disk() -> Settings {
    let Some(path) = settings_path() else {
        tracing::warn!("no data directory resolvable; running with in-memory settings");
        return Settings::default();
    };
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Settings>(&bytes) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(path = %path.display(), err = %e, "config.json does not parse; starting from defaults");
                Settings::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        Err(e) => {
            tracing::error!(path = %path.display(), err = %e, "config.json unreadable; starting from defaults");
            Settings::default()
        }
    }
}

/// Writes the settings atomically: a half-written `config.json` would take the
/// station token with it and the till would silently stop printing remotely.
fn save_to_disk(settings: &Settings) -> BridgeResult<()> {
    let Some(path) = settings_path() else {
        return Err(BridgeError::Config(
            "no hay carpeta de datos donde guardar la configuración".into(),
        ));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(settings)
        .map_err(|e| BridgeError::Config(format!("serializar configuración: {e}")))?;

    let tmp = path.with_extension("json.tmp");
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&body)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Process-wide settings, shared by the local server, the relay and the UI.
///
/// A single `RwLock` and not a channel per consumer: reads are constant and
/// cheap (every WS frame checks the auth mode), writes happen when someone
/// pairs — twice in the life of an install.
#[derive(Clone)]
pub struct SettingsStore {
    inner: Arc<RwLock<Settings>>,
}

impl SettingsStore {
    /// Loads from disk and makes sure a local secret exists.
    ///
    /// Generating it here — at boot, not at first use — is what makes the
    /// secure default real: an install that has never been configured still
    /// comes up with a secret ready for the web to present.
    pub fn load() -> Self {
        let mut settings = load_from_disk();
        if settings.local_secret.is_none() {
            match generate_secret() {
                Ok(secret) => {
                    settings.local_secret = Some(secret);
                    if let Err(e) = save_to_disk(&settings) {
                        // Not fatal: the bridge runs with an in-memory secret
                        // and the POS keeps printing through the origin check.
                        // It just has to be re-issued on the next boot.
                        tracing::error!(err = %e, "could not persist the generated local secret");
                    }
                }
                Err(e) => tracing::error!(err = %e, "could not generate the local secret"),
            }
        }
        Self {
            inner: Arc::new(RwLock::new(settings)),
        }
    }

    /// In-memory store: nothing is read from or written to disk.
    ///
    /// Not `#[cfg(test)]` because the integration tests under `tests/` link
    /// against the library compiled *without* `cfg(test)`, and they are exactly
    /// the ones that need to pin an auth mode without touching `%PROGRAMDATA%`
    /// on the machine running them.
    pub fn in_memory(settings: Settings) -> Self {
        Self {
            inner: Arc::new(RwLock::new(settings)),
        }
    }

    /// A snapshot. Callers get a clone rather than a guard so a slow consumer
    /// can never hold the lock across an await point.
    pub fn snapshot(&self) -> Settings {
        match self.inner.read() {
            Ok(g) => g.clone(),
            // A poisoned lock means another thread panicked mid-write. The data
            // is still readable and losing printing over it helps nobody.
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Mutates and persists in one step. The in-memory copy is only updated
    /// when the write succeeded, so what the process believes and what survives
    /// a restart cannot drift.
    pub fn update(&self, f: impl FnOnce(&mut Settings)) -> BridgeResult<Settings> {
        let mut guard = match self.inner.write() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut next = guard.clone();
        f(&mut next);
        save_to_disk(&next)?;
        *guard = next.clone();
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_url_falls_back_and_trims() {
        let mut s = Settings::default();
        assert_eq!(s.server_url(), DEFAULT_SERVER_URL);
        s.server_url = Some("  https://api.example.com/  ".into());
        assert_eq!(s.server_url(), "https://api.example.com");
        s.server_url = Some("   ".into());
        assert_eq!(s.server_url(), DEFAULT_SERVER_URL);
    }

    #[test]
    fn local_auth_defaults_to_the_safe_value() {
        // The whole point of the change: a fresh install is not permissive.
        assert_eq!(Settings::default().local_auth, LocalAuth::OriginOrSecret);
    }

    #[test]
    fn unknown_fields_do_not_wipe_the_station_token() {
        // A config written by a NEWER bridge must still load on an older one:
        // failing here would drop the pairing and leave the till mute.
        let json = r#"{"stationToken":"tok","somethingFromTheFuture":42}"#;
        let parsed: Settings = serde_json::from_str(json).expect("parse");
        assert_eq!(parsed.station_token.as_deref(), Some("tok"));
        assert!(parsed.is_paired());
    }

    #[test]
    fn generated_secrets_are_unique_and_long() {
        let a = generate_secret().expect("csprng");
        let b = generate_secret().expect("csprng");
        assert_ne!(a, b);
        assert!(a.len() >= 40, "32 bytes base64url should be 43 chars");
    }
}
