//! The emission surface: one type per kind of event.
//!
//! An event is an ordinary struct with named fields and `#[derive(Serialize,
//! Deserialize)]`. The struct **is** the field set, so serde does the work a
//! schema would otherwise have to: it writes the flat payload on the way out
//! and, with `#[serde(deny_unknown_fields)]`, refuses a missing or unknown key
//! on the way back.
//!
//! ```ignore
//! #[derive(Serialize, Deserialize)]
//! #[serde(deny_unknown_fields)]
//! pub struct DepsReplaced {
//!     pub dependent: PackageRef,
//!     pub old: PackageRef,
//!     pub new: PackageRef,
//! }
//!
//! impl LogEvent for DepsReplaced {
//!     const KIND: &'static str = "deps.replaced";
//!     const SEVERITY: Severity = Severity::Info;
//!     fn message(&self) -> String {
//!         format!("Replaced {} with {} as dependency of {}", self.old, self.new, self.dependent)
//!     }
//! }
//! ```
//!
//! No derive macro and no enum. A struct literal cannot omit or misspell a
//! field, and [`LogEvent::SEVERITY`] is a required associated constant, so a new
//! event type does not compile without choosing its level -- which is the
//! guarantee an exhaustive match over one big enum was going to provide.
//!
//! See `design/structured-logs.md`.

use aurcache_common::api::activity::Severity;
use aurcache_common::api::log::EntityRef;
use serde::Serialize;

/// Something worth recording, as the code that notices it describes it.
pub trait LogEvent: Serialize {
    /// Stable identity, `domain.verb_object`: `deps.replaced`.
    ///
    /// Stored as written, so it outlives any rename of the Rust type.
    const KIND: &'static str;

    /// How much attention this kind of event deserves.
    ///
    /// A property of the kind, never passed by a caller: "publishing failed"
    /// and "package added" are not one event with a field.
    const SEVERITY: Severity;

    /// Which case of this kind, when one kind covers several.
    ///
    /// Kinds are consolidated where an operator would not filter on the
    /// difference -- refreshing a git remote and refreshing a snapshot cache
    /// are both `source.refresh_failed` -- and this is the detail that
    /// consolidation would otherwise throw away. Read from the payload, like
    /// the references are, so the two cannot disagree.
    ///
    /// `None` for a kind with one case, which is most of them.
    fn subkind(&self) -> Option<&'static str> {
        None
    }

    /// The sentence, rendered now.
    ///
    /// Stored alongside the payload so a row still reads when its payload can
    /// no longer be parsed -- a field removed or made required by a later
    /// version. That is the one failure a payload-as-record model cannot
    /// otherwise survive.
    fn message(&self) -> String;
}

/// Every entity a serialized payload refers to, with the role it played.
///
/// Read out of the payload rather than declared a second time by each event:
/// a list that had to be written by hand is a list that can be forgotten, and
/// this is what the entity index -- the whole "everything about package foo"
/// filter -- is built from.
///
/// A role naming several entities (a `Vec<BuildRef>`) yields one entry per
/// element, all under the same role.
///
/// Only values that are *entirely* a known reference are taken, so context
/// values are left alone: `"connection refused: timeout"` has a colon but
/// `"connection refused"` is not a namespace. The one way a context value could
/// be mistaken for a reference is by being exactly `pkg:…`, `worker:…` or
/// `build:…/<number>`; if that ever happens the field wanted to be typed
/// anyway.
#[must_use]
pub fn references(payload: &serde_json::Value) -> Vec<(String, EntityRef)> {
    let Some(fields) = payload.as_object() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for (role, value) in fields {
        match value {
            serde_json::Value::String(raw) => {
                if let Ok(entity) = raw.parse::<EntityRef>() {
                    found.push((role.clone(), entity));
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    if let Some(entity) = item.as_str().and_then(|raw| raw.parse().ok()) {
                        found.push((role.clone(), entity));
                    }
                }
            }
            _ => {}
        }
    }
    found
}

/// The payload of one event, and everything the index needs to file it.
///
/// Built once at emission so the payload is serialized a single time: the
/// references are read from the same JSON that gets stored, which is what keeps
/// the index and the row describing the same thing.
pub struct Rendered {
    pub kind: &'static str,
    pub subkind: Option<&'static str>,
    pub severity: Severity,
    pub message: String,
    pub payload: serde_json::Value,
    pub references: Vec<(String, EntityRef)>,
}

/// Render an event into what the store writes.
///
/// # Errors
///
/// Returns the serialization error if the event's payload cannot be written,
/// which for a plain struct of scalars and references cannot happen.
pub fn render<E: LogEvent>(event: &E) -> Result<Rendered, serde_json::Error> {
    let payload = serde_json::to_value(event)?;
    Ok(Rendered {
        kind: E::KIND,
        subkind: event.subkind(),
        severity: E::SEVERITY,
        message: event.message(),
        references: references(&payload),
        payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurcache_common::api::log::{BuildRef, PackageRef, WorkerRef};
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct DepsReplaced {
        dependent: PackageRef,
        old: PackageRef,
        new: PackageRef,
    }

    impl LogEvent for DepsReplaced {
        const KIND: &'static str = "deps.replaced";
        const SEVERITY: Severity = Severity::Info;
        fn message(&self) -> String {
            format!(
                "Replaced {} with {} as dependency of {}",
                self.old, self.new, self.dependent
            )
        }
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct LeaseExpired {
        worker: WorkerRef,
        builds: Vec<BuildRef>,
        since_contact: u64,
        lease_ttl: u64,
    }

    impl LogEvent for LeaseExpired {
        const KIND: &'static str = "worker.lease_expired";
        const SEVERITY: Severity = Severity::Error;
        fn message(&self) -> String {
            format!(
                "{} unreachable for {}s (> lease {}s); {} build(s) taken back",
                self.worker,
                self.since_contact,
                self.lease_ttl,
                self.builds.len()
            )
        }
    }

    fn deps_replaced() -> DepsReplaced {
        DepsReplaced {
            dependent: "baz".into(),
            old: "foo".into(),
            new: "bar".into(),
        }
    }

    /// The payload is the field set: the flat map out, the same struct back.
    #[test]
    fn an_event_round_trips_through_its_payload() {
        let event = deps_replaced();
        let payload = serde_json::to_value(&event).unwrap();
        assert_eq!(
            payload,
            json!({"dependent": "pkg:baz", "old": "pkg:foo", "new": "pkg:bar"})
        );
        assert_eq!(
            serde_json::from_value::<DepsReplaced>(payload).unwrap(),
            event
        );
    }

    /// A key that should not be there, and one that should, are both refused --
    /// the validation that replaces a hand-written schema at the boundary.
    #[test]
    fn an_unknown_or_missing_key_is_refused() {
        let extra = json!({"dependent":"pkg:baz","old":"pkg:foo","new":"pkg:bar","why":"x"});
        assert!(serde_json::from_value::<DepsReplaced>(extra).is_err());

        let missing = json!({"dependent": "pkg:baz", "old": "pkg:foo"});
        assert!(serde_json::from_value::<DepsReplaced>(missing).is_err());
    }

    /// Every package the row names, whatever role it played. This is the
    /// "everything about foo" filter, and it must not need to know the kind.
    #[test]
    fn every_reference_is_found_with_its_role() {
        let rendered = render(&deps_replaced()).unwrap();
        let mut found = rendered.references;
        found.sort();

        assert_eq!(
            found,
            vec![
                ("dependent".to_string(), PackageRef::from("baz").into()),
                ("new".to_string(), PackageRef::from("bar").into()),
                ("old".to_string(), PackageRef::from("foo").into()),
            ]
        );
    }

    /// A role naming several entities yields one index entry per entity.
    #[test]
    fn a_list_role_yields_an_entry_each() {
        let event = LeaseExpired {
            worker: "builder-01".into(),
            builds: vec![
                BuildRef {
                    pkgbase: "hello".into(),
                    number: 7,
                },
                BuildRef {
                    pkgbase: "yay".into(),
                    number: 3,
                },
            ],
            since_contact: 93,
            lease_ttl: 60,
        };
        let rendered = render(&event).unwrap();

        let builds: Vec<_> = rendered
            .references
            .iter()
            .filter(|(role, _)| role == "builds")
            .map(|(_, entity)| entity.to_string())
            .collect();
        assert_eq!(builds, ["build:hello/7", "build:yay/3"]);

        // And the worker, under its own role.
        assert!(
            rendered
                .references
                .iter()
                .any(|(role, e)| role == "worker" && e.to_string() == "worker:builder-01")
        );
    }

    /// Context values are not references, even when they contain a colon.
    #[test]
    fn context_values_are_left_alone() {
        #[derive(Serialize)]
        struct Failed {
            pkg: PackageRef,
            error: String,
            attempt: u32,
        }
        impl LogEvent for Failed {
            const KIND: &'static str = "test.failed";
            const SEVERITY: Severity = Severity::Warning;
            fn message(&self) -> String {
                String::new()
            }
        }

        let rendered = render(&Failed {
            pkg: "hello".into(),
            error: "connection refused: timed out".to_string(),
            attempt: 2,
        })
        .unwrap();

        assert_eq!(rendered.references.len(), 1);
        assert_eq!(rendered.references[0].0, "pkg");
    }

    /// A kind covering several cases keeps the difference, without splitting
    /// into kinds nobody would filter apart.
    #[test]
    fn a_subkind_distinguishes_cases_of_one_kind() {
        #[derive(Serialize)]
        #[serde(rename_all = "snake_case")]
        enum Which {
            Git,
            Snapshot,
        }

        #[derive(Serialize)]
        struct RefreshFailed {
            pkg: PackageRef,
            source: Which,
        }
        impl LogEvent for RefreshFailed {
            const KIND: &'static str = "source.refresh_failed";
            const SEVERITY: Severity = Severity::Warning;
            fn subkind(&self) -> Option<&'static str> {
                Some(match self.source {
                    Which::Git => "git",
                    Which::Snapshot => "snapshot",
                })
            }
            fn message(&self) -> String {
                String::new()
            }
        }

        let git = render(&RefreshFailed {
            pkg: "hello".into(),
            source: Which::Git,
        })
        .unwrap();
        let snapshot = render(&RefreshFailed {
            pkg: "hello".into(),
            source: Which::Snapshot,
        })
        .unwrap();

        assert_eq!(git.kind, snapshot.kind);
        assert_eq!(git.subkind, Some("git"));
        assert_eq!(snapshot.subkind, Some("snapshot"));
        // And it is in the payload too, so re-rendering does not need the
        // column.
        assert_eq!(git.payload["source"], "git");
    }

    /// The kind and the severity come from the type, never from the call site.
    #[test]
    fn the_type_carries_its_kind_and_severity() {
        let rendered = render(&deps_replaced()).unwrap();
        assert_eq!(rendered.kind, "deps.replaced");
        assert_eq!(rendered.subkind, None, "most kinds have one case");
        assert_eq!(rendered.severity, Severity::Info);
        assert_eq!(
            rendered.message,
            "Replaced pkg:foo with pkg:bar as dependency of pkg:baz"
        );
    }
}
