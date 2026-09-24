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
    /// Built: the worker handed its artifacts over and is done with it, and
    /// the server is putting them in the repository.
    ///
    /// Its own state rather than more of `Active`, because what changes is who
    /// is responsible. `Active` is a worker holding a lease, and everything that
    /// polices leases -- the heartbeat, the reaper, revoking a worker, claim
    /// capacity -- finds builds by that state. A build being published has no
    /// lease and no worker left to lose it.
    Publishing = 5,
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
            5 => Some(Self::Publishing),
            _ => None,
        }
    }

    /// Every state, in discriminant order.
    pub const ALL: [Self; 6] = [
        Self::Active,
        Self::Successful,
        Self::Failed,
        Self::Enqueued,
        Self::WaitingForDeps,
        Self::Publishing,
    ];

    /// The state's name where one is written by hand: a filter on the builds
    /// list (`?status=active,publishing`), a CLI flag. Stable, lowercase and
    /// free of spaces, unlike a label meant for reading.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Successful => "successful",
            Self::Failed => "failed",
            Self::Enqueued => "enqueued",
            Self::WaitingForDeps => "waiting-for-deps",
            Self::Publishing => "publishing",
        }
    }

    /// The state a [`Self::key`] names.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.key() == key)
    }

    /// Parse a comma-separated list of keys, as the builds list takes them.
    ///
    /// # Errors
    /// A message naming the key that is not a state, and the ones that are.
    pub fn parse_keys(list: &str) -> Result<Vec<Self>, String> {
        list.split(',')
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(|key| {
                Self::from_key(key).ok_or_else(|| {
                    let known: Vec<_> = Self::ALL.iter().map(|s| s.key()).collect();
                    format!(
                        "{key:?} is not a build state (one of: {})",
                        known.join(", ")
                    )
                })
            })
            .collect()
    }

    /// Whether the build has not settled yet — it is running, queued, or held
    /// behind a dependency. `Successful` and `Failed` are the terminal states.
    ///
    /// The UI uses this to decide how eagerly to re-poll: a page showing a
    /// build in one of these states is a page worth refreshing briskly.
    #[must_use]
    pub const fn is_in_progress(self) -> bool {
        matches!(
            self,
            Self::Active | Self::Enqueued | Self::WaitingForDeps | Self::Publishing
        )
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
    /// Built, and being put in the repository by the server.
    pub const PUBLISHING: i32 = BuildState::Publishing.as_i32();
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
            (BuildStates::PUBLISHING, BuildState::Publishing),
        ] {
            assert_eq!(BuildState::from_i32(value), Some(state));
            assert_eq!(state.as_i32(), value);
        }
    }

    /// Keys are what people type, so every state has one, they round-trip,
    /// and a list of them parses with or without spaces.
    #[test]
    fn keys_round_trip_and_lists_of_them_parse() {
        for state in BuildState::ALL {
            assert_eq!(BuildState::from_key(state.key()), Some(state));
            assert!(!state.key().contains(' '), "{state:?}");
        }
        assert_eq!(
            BuildState::parse_keys("active, publishing"),
            Ok(vec![BuildState::Active, BuildState::Publishing])
        );
        assert_eq!(BuildState::parse_keys(""), Ok(vec![]));
        let error = BuildState::parse_keys("active,running").unwrap_err();
        assert!(error.contains("\"running\""), "{error}");
        assert!(error.contains("waiting-for-deps"), "{error}");
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
