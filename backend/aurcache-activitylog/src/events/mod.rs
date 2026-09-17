//! The catalogue: every kind of structured event this build can record.
//!
//! One type per kind, grouped by the subsystem that emits it. A kind is a
//! stable dotted name, `domain.verb_object`, and it is what a stored row
//! carries -- so renaming a Rust type here is free, and renaming a `KIND` is
//! not.
//!
//! Adding an event is: a struct with its fields, a `LogEvent` impl, and a line
//! in [`KINDS`] below. Nothing else has to learn about it -- not the store, not
//! the entity index, not the filter.
//!
//! See `design/structured-logs.md`.

pub mod source;
pub mod version_check;

/// Every kind this build knows, so they can be checked against each other.
///
/// Not a registry anything dispatches on -- `kind` is stored as text and a
/// server shows a row written by a newer version rather than refusing it. This
/// exists so two events cannot quietly share a name, and so the list is
/// somewhere a person can read.
pub const KINDS: &[&str] = &[
    crate::kinds::SERVER_START,
    source::RefreshFailed::KIND_STR,
    source::SourceinfoFailed::KIND_STR,
    source::VcsSyncFailed::KIND_STR,
    version_check::AurMissing::KIND_STR,
    version_check::StoreFailed::KIND_STR,
    version_check::CompareFallback::KIND_STR,
    version_check::QueueFailed::KIND_STR,
];

#[cfg(test)]
mod tests {
    use super::KINDS;
    use std::collections::HashSet;

    /// Two events sharing a kind would be one filter returning both and no way
    /// to tell them apart.
    #[test]
    fn every_kind_is_unique() {
        let unique: HashSet<_> = KINDS.iter().collect();
        assert_eq!(unique.len(), KINDS.len(), "a kind is declared twice");
    }

    /// `domain.verb_object`, lowercase: the shape a UI catalogue and a
    /// `kind LIKE 'worker.%'` family filter both rely on.
    #[test]
    fn every_kind_is_a_dotted_lowercase_name() {
        for kind in KINDS {
            assert!(
                kind.contains('.'),
                "{kind} has no domain; kinds are `domain.verb_object`"
            );
            assert!(
                kind.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_'),
                "{kind} should be lowercase with `.` and `_` only"
            );
            assert!(!kind.starts_with('.') && !kind.ends_with('.'), "{kind}");
        }
    }
}
