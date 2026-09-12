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

/// Why a build row was created.
///
/// Persisted and serialized as its `i32` discriminant, like [`BuildState`]. The
/// retry budget for builds the reaper abandons is *derived* from this column —
/// the count of consecutive `TimeoutRetry` rows — rather than tracked in a
/// mutable counter, so the reason a build exists has to live somewhere a build
/// history can walk, and the rows themselves are the only history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(i32)]
pub enum BuildTrigger {
    /// Operator add or explicit rebuild (button, CLI).
    User = 0,
    /// A version check found the package outdated and requeued it.
    AutoUpdate = 1,
    /// Automatic retry of a build the server abandoned (this design).
    TimeoutRetry = 2,
}

impl BuildTrigger {
    /// The database/wire representation.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        self as i32
    }

    /// Parse the database/wire representation.
    ///
    /// Returns `None` for an unrecognised value rather than guessing, so an
    /// unknown trigger from a newer server is visible instead of silently
    /// counting as a user request.
    #[must_use]
    pub const fn from_i32(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::User),
            1 => Some(Self::AutoUpdate),
            2 => Some(Self::TimeoutRetry),
            _ => None,
        }
    }
}

pub struct BuildTriggers;

/// The integer constants for [`BuildTrigger`], for call sites comparing raw
/// `i32`s against the database column.
impl BuildTriggers {
    pub const USER: i32 = BuildTrigger::User.as_i32();
    pub const AUTO_UPDATE: i32 = BuildTrigger::AutoUpdate.as_i32();
    pub const TIMEOUT_RETRY: i32 = BuildTrigger::TimeoutRetry.as_i32();
}

/// Why a build stopped, when the stop was decided by the server rather than
/// reported by the worker.
///
/// `None` on the row means the worker reported a terminal outcome and its
/// `CompleteReport.reason` text is the record; these codes cover the cases only
/// the server can produce: a manual cancel and the two abandonment paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(i32)]
pub enum EndReason {
    /// An operator cancelled the build (`Action::Cancel`).
    Canceled = 0,
    /// The owning worker went silent past its lease.
    LeaseExpired = 1,
    /// The build ran past the backstop deadline while still heartbeating.
    MaxDuration = 2,
}

impl EndReason {
    /// The database/wire representation.
    #[must_use]
    pub const fn as_i32(self) -> i32 {
        self as i32
    }

    /// Parse the database/wire representation.
    #[must_use]
    pub const fn from_i32(value: i32) -> Option<Self> {
        match value {
            0 => Some(Self::Canceled),
            1 => Some(Self::LeaseExpired),
            2 => Some(Self::MaxDuration),
            _ => None,
        }
    }
}

pub struct EndReasons;

/// The integer constants for [`EndReason`].
impl EndReasons {
    pub const CANCELED: i32 = EndReason::Canceled.as_i32();
    pub const LEASE_EXPIRED: i32 = EndReason::LeaseExpired.as_i32();
    pub const MAX_DURATION: i32 = EndReason::MaxDuration.as_i32();
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

    /// Triggers round-trip through their wire representation, and unknown
    /// values are not guessed (which would silently count them as `user`).
    #[test]
    fn triggers_round_trip() {
        for (value, trigger) in [
            (BuildTriggers::USER, BuildTrigger::User),
            (BuildTriggers::AUTO_UPDATE, BuildTrigger::AutoUpdate),
            (BuildTriggers::TIMEOUT_RETRY, BuildTrigger::TimeoutRetry),
        ] {
            assert_eq!(BuildTrigger::from_i32(value), Some(trigger));
            assert_eq!(trigger.as_i32(), value);
        }
        assert_eq!(BuildTrigger::from_i32(99), None);
    }

    /// Same for end reasons; an unparsed value must not read as a known one.
    #[test]
    fn end_reasons_round_trip() {
        for (value, reason) in [
            (EndReasons::CANCELED, EndReason::Canceled),
            (EndReasons::LEASE_EXPIRED, EndReason::LeaseExpired),
            (EndReasons::MAX_DURATION, EndReason::MaxDuration),
        ] {
            assert_eq!(EndReason::from_i32(value), Some(reason));
            assert_eq!(reason.as_i32(), value);
        }
        assert_eq!(EndReason::from_i32(99), None);
    }
}
