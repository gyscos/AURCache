//! Turning an event into what the store writes, and back again.
//!
//! The events themselves are [`crate::events::Event`]; this is the pair of
//! functions either side of the database. [`render`] splits the tagged form
//! into the two columns a row holds, and [`decode`] puts them back together --
//! so the storage layout and the wire form are the same arrangement, and serde
//! does the dispatch rather than a table somebody maintains.
//!
//! See `design/structured-logs.md`.

use crate::events::Event;
use aurcache_common::api::activity::Severity;
use aurcache_common::api::log::EntityRef;

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
    pub severity: Severity,
    pub message: String,
    pub payload: serde_json::Value,
    pub references: Vec<(String, EntityRef)>,
}

/// The two columns a row is stored in, as serde writes them.
const TAG: &str = "kind";
const CONTENT: &str = "data";

/// Render an event into what the store writes.
///
/// The enum serializes to `{"kind": .., "data": {..}}` -- the adjacent tag --
/// and the two halves go to their own columns. Splitting here rather than
/// serializing twice is what keeps the stored kind and the stored payload
/// describing the same thing.
///
/// # Errors
///
/// Returns the serialization error if the payload cannot be written, which for
/// a plain struct of scalars and references cannot happen.
pub fn render(event: &Event) -> Result<Rendered, serde_json::Error> {
    let mut tagged = serde_json::to_value(event)?;
    let payload = tagged
        .get_mut(CONTENT)
        .map_or(serde_json::Value::Null, serde_json::Value::take);
    Ok(Rendered {
        kind: event.kind(),
        severity: event.severity(),
        message: event.message(),
        references: references(&payload),
        payload,
    })
}

/// Turn a stored row back into the event it was written from.
///
/// The inverse of [`render`]: the `kind` column is the tag and `data` is the
/// content, so serde dispatches with no table to keep in step. `None` for a
/// kind this build does not know -- a row from a newer server -- or a payload
/// that no longer matches its type, which is the case the stored message
/// exists for.
#[must_use]
pub fn decode(kind: &str, data: &str) -> Option<Event> {
    let content: serde_json::Value = serde_json::from_str(data).ok()?;
    serde_json::from_value(serde_json::json!({ TAG: kind, CONTENT: content })).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{RefreshTarget, SourceinfoPurpose};
    use aurcache_common::api::log::{BuildRef, PackageRef};
    use serde_json::json;

    fn refresh_failed() -> Event {
        Event::SourceRefreshFailed {
            pkg: PackageRef::from("hello"),
            target: RefreshTarget::Git,
            error: "host is unreachable".to_string(),
        }
    }

    /// What the two columns hold: the tag in one, the payload in the other,
    /// with the payload flat.
    #[test]
    fn rendering_splits_the_tag_from_the_payload() {
        let rendered = render(&refresh_failed()).unwrap();
        assert_eq!(rendered.kind, "source.refresh_failed");
        assert_eq!(rendered.severity, Severity::Warning);
        assert_eq!(
            rendered.payload,
            json!({"pkg": "pkg:hello", "target": "git", "error": "host is unreachable"})
        );
        assert!(
            rendered.message.contains("git source"),
            "{}",
            rendered.message
        );
    }

    /// And back again, with no table to keep in step: serde dispatches on the
    /// kind column.
    #[test]
    fn a_stored_row_decodes_to_the_event_it_came_from() {
        let rendered = render(&refresh_failed()).unwrap();
        let back = decode(rendered.kind, &rendered.payload.to_string()).expect("decodes");
        assert_eq!(back.kind(), rendered.kind);
        assert_eq!(back.message(), rendered.message);
    }

    /// A row from a newer server, and a payload that no longer fits its type,
    /// both decode to nothing rather than costing the listing -- which is what
    /// the stored message is there for.
    #[test]
    fn an_unknown_kind_or_a_stale_payload_decodes_to_nothing() {
        assert!(decode("from.the.future", "{}").is_none());
        assert!(decode("source.refresh_failed", r#"{"pkg":"pkg:hello"}"#).is_none());
        assert!(decode("source.refresh_failed", "not json").is_none());
    }

    /// A key the type does not declare is *ignored*, not refused.
    ///
    /// Deliberate, and the right way round for a log: a field added by a later
    /// version makes a row that an older server can still read and render,
    /// rather than one it throws away. A missing field is still an error --
    /// that is the half worth keeping, and the test above covers it.
    #[test]
    fn a_key_from_a_later_version_is_ignored_rather_than_refused() {
        let extra = json!({
            "pkg": "pkg:hello",
            "target": "git",
            "error": "boom",
            "added_later": "x"
        });
        let decoded = decode("source.refresh_failed", &extra.to_string())
            .expect("an unknown key must not cost the row");
        assert_eq!(decoded.kind(), "source.refresh_failed");
    }

    /// Every package the row names, whatever role it played. This is the
    /// "everything about foo" filter, and it must not need to know the kind.
    #[test]
    fn every_reference_is_found_with_its_role() {
        let rendered = render(&Event::SourceinfoFailed {
            pkg: PackageRef::from("hello"),
            purpose: SourceinfoPurpose::Vcs,
            error: "boom".to_string(),
        })
        .unwrap();
        assert_eq!(
            rendered.references,
            vec![("pkg".to_string(), PackageRef::from("hello").into())]
        );
    }

    /// A build reference is one value, so nothing has to reassemble it.
    #[test]
    fn a_build_reference_survives_as_one_value() {
        let rendered = render(&Event::BuildMarkFailed {
            build: BuildRef {
                pkgbase: "hello".to_string(),
                number: 7,
            },
            error: "boom".to_string(),
        })
        .unwrap();
        assert_eq!(rendered.payload["build"], "build:hello/7");
        assert_eq!(rendered.references.len(), 1);
        assert_eq!(rendered.references[0].0, "build");
    }

    /// A role naming several entities yields one index entry per entity.
    #[test]
    fn a_list_role_yields_an_entry_each() {
        let payload = json!({"builds": ["build:hello/7", "build:yay/3"], "count": 2});
        let found = references(&payload);
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|(role, _)| role == "builds"));
    }

    /// Context values are not references, even when they contain a colon.
    #[test]
    fn context_values_are_left_alone() {
        let rendered = render(&Event::VcsSyncFailed {
            pkg: PackageRef::from("hello"),
            error: "connection refused: timed out".to_string(),
        })
        .unwrap();
        assert_eq!(rendered.references.len(), 1);
        assert_eq!(rendered.references[0].0, "pkg");
    }
}
