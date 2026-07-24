//! The unified engine error, surfaced identically over both transports.
//!
//! The load-bearing property (RPC-PROTOCOL.md §7.2): an engine-side handler
//! failure must `Display` as the *raw* failure text on both transports, so the
//! app's substring checks (`profile_err_msg`) match byte-for-byte whether the
//! call went over D-Bus or JSON-RPC. On D-Bus we wrap `zbus::Error` and let it
//! render itself; over RPC we carry `error.message` verbatim.

use std::fmt;

/// An error from an engine call (or the transport underneath it).
#[derive(Debug)]
pub enum EngineError {
    /// A failure raised inside an engine method handler — the RPC `-32000`
    /// "engine error" (`str(exc)` on the wire), or a D-Bus `MethodError`.
    /// `Display` is the raw handler text, a contiguous substring, so
    /// `to_string().contains(...)` matches the same on both transports.
    Engine(String),

    /// A transport/protocol failure not attributable to an engine handler
    /// (socket closed, bad frame, auth rejected, discovery file missing, a
    /// non-`-32000` JSON-RPC error). `Display` is the message text.
    Transport(String),

    /// The raw D-Bus error, wrapped so `Display` is byte-identical to
    /// `zbus::Error::to_string()` (Linux only).
    #[cfg(unix)]
    Dbus(zbus::Error),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EngineError::Engine(m) | EngineError::Transport(m) => f.write_str(m),
            #[cfg(unix)]
            EngineError::Dbus(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            #[cfg(unix)]
            EngineError::Dbus(e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(unix)]
impl From<zbus::Error> for EngineError {
    fn from(e: zbus::Error) -> Self {
        EngineError::Dbus(e)
    }
}
