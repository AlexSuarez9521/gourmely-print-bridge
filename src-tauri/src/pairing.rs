//! Pairing: turning a code the owner reads off the screen into a station token.
//!
//! The owner opens Configuración → Impresión → Estaciones in GourmelyHub, hits
//! "Vincular estación" and gets an eight-character code (`FJTX-35AH`). Someone
//! at the till types it into the bridge. This module exchanges it, once, for
//! the opaque station token and hands it to [`SettingsStore`] to persist.
//!
//! # What the server requires of us
//!
//! `POST /v1/print-relay/pair` is the only endpoint of the module that takes no
//! JWT — the bridge has no user session — so it is also the most fortified one:
//! the code is single-use and consumed atomically, it lives ten minutes, and
//! five failures from one IP get that IP blocked for fifteen. Practical
//! consequences for this side:
//!
//! * **Never retry a `code` on a transport error.** If the request reached the
//!   server the code is already burnt, and a retry only spends one of the five
//!   attempts before the block. We surface the failure and let the operator ask
//!   for a new code, which is one click.
//! * **The token is returned exactly once, in this response.** There is no
//!   endpoint to read it back. Failing to persist it means the station is
//!   paired server-side and mute here, which looks like a network problem and
//!   is not one — so a write failure is reported as a hard error, not logged
//!   and shrugged off.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::SettingsStore;
use crate::error::{BridgeError, BridgeResult};

/// Generous enough for a till on a bad ADSL line, short enough that the UI does
/// not look frozen. The code is valid for ten minutes, so a timeout is
/// recoverable by asking for another one.
const PAIR_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Serialize)]
struct MachineInfo {
    os: String,
    hostname: String,
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    printers: Vec<String>,
}

#[derive(Debug, Serialize)]
struct PairRequest {
    code: String,
    #[serde(rename = "machineInfo")]
    machine_info: MachineInfo,
}

/// The `data` half of the server's envelope.
#[derive(Debug, Clone, Deserialize)]
pub struct PairResult {
    #[serde(rename = "stationToken")]
    pub station_token: String,
    #[serde(rename = "stationId")]
    pub station_id: String,
    #[serde(rename = "tenantId")]
    pub tenant_id: String,
    #[serde(rename = "branchId")]
    pub branch_id: String,
    pub label: String,
    #[serde(default)]
    pub roles: Vec<String>,
}

/// Mirrors the API envelope: `{success, data}` or `{success, error:{code,message}}`.
#[derive(Debug, Deserialize)]
struct ApiEnvelope<T> {
    #[serde(default)]
    success: bool,
    data: Option<T>,
    error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[allow(dead_code)]
    code: Option<String>,
    message: Option<String>,
}

/// Normalises what the operator typed the same way the server does, so an
/// obviously malformed code never spends one of the five attempts.
///
/// Uppercases, drops everything that is not `[0-9A-Z]`, and re-inserts the
/// dash. Mirrors `normalizePairingCode` in `print-relay/service.ts`; the
/// alphabet is Crockford-ish base32 without `O/0/I/1` precisely because someone
/// is reading it off a screen and typing it on a till keyboard.
pub fn normalize_code(raw: &str) -> String {
    let clean: String = raw
        .to_uppercase()
        .chars()
        .filter(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
        .collect();
    if clean.len() == 8 {
        format!("{}-{}", &clean[..4], &clean[4..])
    } else {
        clean
    }
}

fn machine_info(printers: Vec<String>) -> MachineInfo {
    MachineInfo {
        os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        hostname: hostname(),
        bridge_version: env!("CARGO_PKG_VERSION").to_string(),
        printers,
    }
}

/// Best-effort machine name for the station list in the web UI. Not identity —
/// the server derives that from the code — so an unknown host is harmless.
fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "desconocido".into())
}

/// Redeems `code` against `server_url` and persists the result.
///
/// On success the settings hold the station token and the relay can be told to
/// reconnect. On failure nothing is written: a half-applied pairing (token
/// saved, station id missing) would be worse than none.
pub async fn pair(
    settings: &SettingsStore,
    server_url: &str,
    code: &str,
    printers: Vec<String>,
) -> BridgeResult<PairResult> {
    let code = normalize_code(code);
    if code.len() < 4 {
        return Err(BridgeError::Pairing(
            "El código de vinculación está incompleto. Son 8 caracteres, como FJTX-35AH.".into(),
        ));
    }

    let base = server_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err(BridgeError::Pairing(
            "Falta la dirección del servidor de GourmelyHub.".into(),
        ));
    }
    let url = format!("{base}/v1/print-relay/pair");

    let client = reqwest::Client::builder()
        .timeout(PAIR_TIMEOUT)
        .user_agent(format!("GourmelyPrintBridge/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| BridgeError::Pairing(format!("no se pudo preparar la conexión: {e}")))?;

    let response = client
        .post(&url)
        .json(&PairRequest {
            code: code.clone(),
            machine_info: machine_info(printers),
        })
        .send()
        .await
        .map_err(|e| {
            BridgeError::Pairing(format!(
                "No se pudo contactar a {base}. Revisa la conexión a internet. ({e})"
            ))
        })?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| BridgeError::Pairing(format!("respuesta ilegible del servidor: {e}")))?;

    let envelope: ApiEnvelope<PairResult> = serde_json::from_str(&body).map_err(|_| {
        // A proxy, a captive portal or the wrong URL: an HTML page where JSON
        // was expected. Say so instead of dumping a parse error.
        BridgeError::Pairing(format!(
            "El servidor respondió algo que no es de GourmelyHub (HTTP {}). \
             Comprueba la dirección del servidor.",
            status.as_u16()
        ))
    })?;

    if !envelope.success || envelope.data.is_none() {
        let message = envelope
            .error
            .and_then(|e| e.message)
            .unwrap_or_else(|| format!("La vinculación falló (HTTP {}).", status.as_u16()));
        return Err(BridgeError::Pairing(message));
    }

    let result = envelope.data.expect("checked just above");

    settings.update(|s| {
        s.server_url = Some(base.to_string());
        s.station_token = Some(result.station_token.clone());
        s.station_id = Some(result.station_id.clone());
        s.tenant_id = Some(result.tenant_id.clone());
        s.branch_id = Some(result.branch_id.clone());
        s.label = Some(result.label.clone());
        s.roles = result.roles.clone();
    })?;

    tracing::info!(
        station_id = %result.station_id,
        label = %result.label,
        roles = ?result.roles,
        "station paired"
    );
    Ok(result)
}

/// Forgets the pairing locally. Does **not** revoke it server-side — that is
/// the owner's "Desvincular" in the web, and it is the one that actually kills
/// the token. This only stops *this* machine from connecting, which is what
/// someone re-imaging a till wants.
pub fn forget(settings: &SettingsStore) -> BridgeResult<()> {
    settings.update(|s| {
        s.station_token = None;
        s.station_id = None;
        s.tenant_id = None;
        s.branch_id = None;
        s.label = None;
        s.roles.clear();
    })?;
    tracing::info!("station pairing forgotten on this machine");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_what_a_human_types() {
        assert_eq!(normalize_code("fjtx-35ah"), "FJTX-35AH");
        assert_eq!(normalize_code("FJTX35AH"), "FJTX-35AH");
        assert_eq!(normalize_code(" fjtx 35 ah "), "FJTX-35AH");
        assert_eq!(normalize_code("FJTX–35AH"), "FJTX-35AH"); // en dash
    }

    #[test]
    fn leaves_a_wrong_length_alone_instead_of_inventing_a_dash() {
        // Better a clean 404 from the server than a code we reshaped into
        // something the operator never typed.
        assert_eq!(normalize_code("FJTX"), "FJTX");
        assert_eq!(normalize_code("FJTX-35AHZZ"), "FJTX35AHZZ");
    }

    #[tokio::test]
    async fn an_incomplete_code_never_reaches_the_server() {
        // Five failures block the IP for fifteen minutes. A typo of three
        // characters must not spend one of them.
        let settings = SettingsStore::in_memory(Default::default());
        let err = pair(&settings, "http://127.0.0.1:1", "AB", vec![])
            .await
            .expect_err("should refuse");
        assert!(matches!(err, BridgeError::Pairing(_)));
        assert!(err.to_string().contains("8 caracteres"));
    }
}
