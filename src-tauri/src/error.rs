use thiserror::Error;

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("printer '{0}' not found on this system")]
    PrinterNotFound(String),

    #[error("print spooler call failed: {0}")]
    SpoolerFailed(String),

    #[error("invalid request: {0}")]
    BadRequest(String),

    #[error("payload exceeds limit ({0} bytes, max {1})")]
    PayloadTooLarge(usize, usize),

    #[error("base64 decode failed: {0}")]
    Base64(#[from] base64::DecodeError),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("tls configuration: {0}")]
    Tls(String),

    /// Reading or writing `config.json`, or anything else about the persisted
    /// settings. Separate from `Io` because the operator-facing message is
    /// different: this one is "the bridge cannot remember its pairing".
    #[error("configuración: {0}")]
    Config(String),

    /// Exchanging the pairing code for a station token failed. The message is
    /// the server's own — it is written for the person standing at the till.
    #[error("{0}")]
    Pairing(String),

    /// The relay could not talk to the server (network, DNS, TLS, protocol).
    #[error("relay: {0}")]
    Relay(String),
}

pub type BridgeResult<T> = Result<T, BridgeError>;
