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
}

pub type BridgeResult<T> = Result<T, BridgeError>;
