// Unified error type module

use std::fmt;

/// Unified error type
#[derive(Debug)]
pub enum Error {
    /// I/O error (network, file, etc.)
    Io(std::io::Error),

    /// plist parsing/serialization error
    Plist(String),

    /// RPC protocol error (malformed message, missing fields, etc.)
    Rpc(String),

    /// WebInspector protocol error
    WebInspector(String),

    /// Device connection error (libimobiledevice FFI related)
    DeviceConnection(String),

    /// TLS/SSL error
    Tls(String),

    /// usbmuxd communication error
    Usbmuxd(String),

    /// Port binding/allocation error
    PortBind(String),

    /// Channel closed (mpsc channel closed)
    ChannelClosed,

    /// Async task error (tokio JoinError)
    TaskJoin(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {}", e),
            Error::Plist(msg) => write!(f, "plist error: {}", msg),
            Error::Rpc(msg) => write!(f, "RPC error: {}", msg),
            Error::WebInspector(msg) => write!(f, "WebInspector error: {}", msg),
            Error::DeviceConnection(msg) => write!(f, "device connection error: {}", msg),
            Error::Tls(msg) => write!(f, "TLS error: {}", msg),
            Error::Usbmuxd(msg) => write!(f, "usbmuxd error: {}", msg),
            Error::PortBind(msg) => write!(f, "port error: {}", msg),
            Error::ChannelClosed => write!(f, "internal channel closed"),
            Error::TaskJoin(msg) => write!(f, "async task error: {}", msg),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<plist::Error> for Error {
    fn from(e: plist::Error) -> Self {
        Error::Plist(e.to_string())
    }
}

impl From<tokio::task::JoinError> for Error {
    fn from(e: tokio::task::JoinError) -> Self {
        Error::TaskJoin(e.to_string())
    }
}

/// Convenience type alias
pub type Result<T> = std::result::Result<T, Error>;
