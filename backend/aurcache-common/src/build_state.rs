//! Build states, free of any database dependency.
//!
//! Kept out of [`crate::builder`] because that module's `Action` carries
//! database models, which pulls sea-orm and sqlx in with it. A browser
//! frontend needs the states and nothing else, so they live here and the
//! database half of this crate is an optional feature.

use serde::{Deserialize, Serialize};

/// The state of a build, as stored in the database and sent over the API.
///
/// Persisted and serialized as its `i32` discriminant, so the wire format and
/// the schema are unchanged — but consumers can match on it exhaustively
/// instead of on bare integers. A new state then becomes a compile error in
/// every caller that has to handle it, rather than a number nobody recognises
/// silently falling into an "unknown" arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(i32)]
pub enum BuildState {
    /// Currently building.
    Active = 0,
    /// Built successfully.
    Successful = 1,
    /// The build failed.
    Failed = 2,
    /// Waiting for a worker to claim it.
    Enqueued = 3,
    /// Queued, but cannot start yet: one or more dependency builds have not
    /// completed successfully.
    WaitingForDeps = 4,
}

impl BuildState {
    /// The database/wire representation.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        self as i32
    }

    /// Parse the database/wire representation.
    ///
    /// Returns `None` for an unrecognised value rather than guessing, so a
    /// forward-incompatible state from a newer server is visible instead of
    /// being silently rendered as something it is not.
    #[must_use]
    pub const fn from_i32(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Active),
            1 => Some(Self::Successful),
            2 => Some(Self::Failed),
            3 => Some(Self::Enqueued),
            4 => Some(Self::WaitingForDeps),
            _ => None,
        }
    }

    /// Whether the build has not settled yet — it is running, queued, or held
    /// behind a dependency. `Successful` and `Failed` are the terminal states.
    ///
    /// The UI uses this to decide how eagerly to re-poll: a page showing a
    /// build in one of these states is a page worth refreshing briskly.
    #[must_use]
    pub const fn is_in_progress(self) -> bool {
        matches!(self, Self::Active | Self::Enqueued | Self::WaitingForDeps)
    }
}

pub struct BuildStates;

/// The integer constants remain, defined in terms of the enum so there is a
/// single source of truth, for the many call sites that compare raw `i32`s
/// against the database column.
impl BuildStates {
    pub const ACTIVE_BUILD: i32 = BuildState::Active.as_i32();
    pub const SUCCESSFUL_BUILD: i32 = BuildState::Successful.as_i32();
    pub const FAILED_BUILD: i32 = BuildState::Failed.as_i32();
    pub const ENQUEUED_BUILD: i32 = BuildState::Enqueued.as_i32();
    /// Build is queued but cannot start yet because one or more dependency
    /// builds have not completed successfully.
    pub const WAITING_FOR_DEPS: i32 = BuildState::WaitingForDeps.as_i32();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The constants and the enum must not drift apart.
    #[test]
    fn constants_match_the_enum() {
        for (value, state) in [
            (BuildStates::ACTIVE_BUILD, BuildState::Active),
            (BuildStates::SUCCESSFUL_BUILD, BuildState::Successful),
            (BuildStates::FAILED_BUILD, BuildState::Failed),
            (BuildStates::ENQUEUED_BUILD, BuildState::Enqueued),
            (BuildStates::WAITING_FOR_DEPS, BuildState::WaitingForDeps),
        ] {
            assert_eq!(BuildState::from_i32(value), Some(state));
            assert_eq!(state.as_i32(), value);
        }
    }

    #[test]
    fn an_unknown_value_is_not_guessed() {
        assert_eq!(BuildState::from_i32(99), None);
        assert_eq!(BuildState::from_i32(-1), None);
    }

    #[test]
    fn only_running_queued_and_waiting_count_as_in_progress() {
        assert!(BuildState::Active.is_in_progress());
        assert!(BuildState::Enqueued.is_in_progress());
        assert!(BuildState::WaitingForDeps.is_in_progress());
        assert!(!BuildState::Successful.is_in_progress());
        assert!(!BuildState::Failed.is_in_progress());
    }
}
