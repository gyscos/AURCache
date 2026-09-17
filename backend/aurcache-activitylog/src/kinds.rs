//! The kinds this build knows about.
//!
//! A `kind` is stored as text, so a server reading a row written by a newer
//! version shows it rather than failing to parse the listing. These constants
//! are for the few places the *server* has to recognise one -- the boot marker
//! the "since the last restart" filter counts back to -- not a registry every
//! event has to be added to.

/// The server process starting. The marker "since the last restart" counts
/// back to, which is why it is named here rather than only where it is emitted.
pub const SERVER_START: &str = "server.start";
