//! Build states.
//!
//! The build-queue `Action` used to live here; it moved to `aurcache-db`
//! because it carries database models and this crate must stay usable from a
//! browser.

pub use crate::build_state::{BuildState, BuildStates, BuildTrigger, BuildTriggers, EndReason, EndReasons};
