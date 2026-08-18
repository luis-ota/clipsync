//! Tipos de erro da crate `clipsync-core`.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("websocket error: {0}")]
    WebSocket(String),

    #[error("http error: {0}")]
    Http(String),

    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("mDNS error: {0}")]
    Mdns(#[from] mdns_sd::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("clipboard backend unavailable: {0}")]
    Clipboard(String),

    #[error("invalid protocol message: {0}")]
    Protocol(String),

    #[error("pairing failed: {0}")]
    Pairing(String),

    #[error("payload too large: {size} bytes (max {max})")]
    PayloadTooLarge { size: usize, max: usize },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
