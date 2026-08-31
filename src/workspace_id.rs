//! Logical workspace identifier (v0.2 M1 registry foundation).
//!
//! A [`WorkspaceId`] is a pure logical name chosen by the operator in the
//! registry configuration — never a filesystem path. Future milestones will
//! route MCP requests by this id through an exact registry lookup, so the
//! type is hashable, orderable, and serializable, with no case folding and
//! no aliasing: two ids are the same only if their strings are equal.
//!
//! Grammar: `[a-z0-9][a-z0-9._-]{0,63}` — 1–64 ASCII characters, lowercase
//! letters/digits first, then `.`/`_`/`-` allowed. Anything else (uppercase,
//! leading `.`, leading `-`, path separators, traversal segments, non-ASCII)
//! is rejected.

use serde::de::{Deserializer, Visitor};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Maximum length of a workspace id in characters.
pub const WORKSPACE_ID_MAX_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    /// Validate `raw` against the workspace-id grammar and wrap it.
    ///
    /// The error is a plain, operator-readable string: configuration
    /// problems must be actionable at startup, not buried in typed variants.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let reject = |reason: String| format!("invalid workspace id '{raw}': {reason}");
        if raw.is_empty() {
            return Err(reject("must not be empty".to_string()));
        }
        if raw.chars().count() > WORKSPACE_ID_MAX_LEN {
            return Err(reject(format!(
                "must be at most {WORKSPACE_ID_MAX_LEN} characters"
            )));
        }
        let mut chars = raw.chars();
        let first = chars.next().expect("non-empty string has a first char");
        if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
            return Err(reject(
                "must start with a lowercase ASCII letter or digit".to_string(),
            ));
        }
        for c in chars {
            if !(c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')) {
                return Err(reject(format!(
                    "may only contain lowercase ASCII letters, digits, '.', '_', '-' (found '{c}')"
                )));
            }
        }
        Ok(Self(raw.to_string()))
    }

    /// The validated id string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for WorkspaceId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// MCP tool-argument schema for workspace selectors.
///
/// Exposing the id grammar directly in the advertised schema lets clients
/// validate before sending, while deserialization still enforces the same
/// grammar server-side: a selector that violates it fails strictly at the
/// boundary, before any lookup or path handling is attempted.
impl rmcp::schemars::JsonSchema for WorkspaceId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "WorkspaceId".into()
    }

    fn json_schema(_gen: &mut rmcp::schemars::SchemaGenerator) -> rmcp::schemars::Schema {
        rmcp::schemars::json_schema!({
            "type": "string",
            "pattern": "^[a-z0-9][a-z0-9._-]{0,63}$",
            "description": "Logical workspace id ([a-z0-9][a-z0-9._-]{0,63}), exactly as configured by the operator at server startup. Not a filesystem path."
        })
    }
}

impl Serialize for WorkspaceId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for WorkspaceId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct WorkspaceIdVisitor;

        impl Visitor<'_> for WorkspaceIdVisitor {
            type Value = WorkspaceId;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a valid workspace id ([a-z0-9][a-z0-9._-]{0,63})")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                WorkspaceId::parse(v).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(WorkspaceIdVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_ids() {
        for valid in [
            "nian-vision",
            "nian_home",
            "project.v2",
            "a",
            "w0rk-5pace._-",
        ] {
            let id =
                WorkspaceId::parse(valid).unwrap_or_else(|e| panic!("'{valid}' rejected: {e}"));
            assert_eq!(id.as_str(), valid);
        }
    }

    #[test]
    fn rejects_uppercase() {
        assert!(WorkspaceId::parse("Nian-Vision").is_err());
        assert!(WorkspaceId::parse("nian-Vision").is_err());
    }

    #[test]
    fn rejects_traversal_like_ids() {
        assert!(WorkspaceId::parse("../foo").is_err());
        assert!(WorkspaceId::parse("..").is_err());
        assert!(WorkspaceId::parse("foo/../../bar").is_err());
        // Interior dots are legal grammar ('project.v2'), only leading ones
        // (and leading hyphens/underscores) are not.
        assert!(WorkspaceId::parse("a..b").is_ok());
    }

    #[test]
    fn rejects_path_separators() {
        assert!(WorkspaceId::parse("/tmp/foo").is_err());
        assert!(WorkspaceId::parse("foo/bar").is_err());
        assert!(WorkspaceId::parse(r"foo\bar").is_err());
    }

    #[test]
    fn rejects_empty() {
        assert!(WorkspaceId::parse("").is_err());
    }

    #[test]
    fn rejects_leading_dot_and_hyphen() {
        assert!(WorkspaceId::parse(".foo").is_err());
        assert!(WorkspaceId::parse("-foo").is_err());
        assert!(WorkspaceId::parse("_foo").is_err());
    }

    #[test]
    fn rejects_non_ascii_and_whitespace() {
        assert!(WorkspaceId::parse("nian-vïsion").is_err());
        assert!(WorkspaceId::parse("nian vision").is_err());
        assert!(WorkspaceId::parse(" nian").is_err());
        assert!(WorkspaceId::parse("nian\tvision").is_err());
    }

    #[test]
    fn accepts_max_length() {
        let max = format!("a{}", "-".repeat(WORKSPACE_ID_MAX_LEN - 1));
        assert_eq!(max.chars().count(), WORKSPACE_ID_MAX_LEN);
        assert!(WorkspaceId::parse(&max).is_ok());
    }

    #[test]
    fn rejects_over_max_length() {
        let over = format!("a{}", "-".repeat(WORKSPACE_ID_MAX_LEN));
        assert_eq!(over.chars().count(), WORKSPACE_ID_MAX_LEN + 1);
        let err = WorkspaceId::parse(&over).unwrap_err();
        assert!(err.contains("at most 64"), "unexpected error: {err}");
    }

    #[test]
    fn error_message_quotes_the_raw_id() {
        let err = WorkspaceId::parse("Nian").unwrap_err();
        assert!(err.contains("'Nian'"), "unexpected error: {err}");
    }

    #[test]
    fn serde_round_trip_and_rejection() {
        let id: WorkspaceId = serde_json::from_str("\"nian-vision\"").unwrap();
        assert_eq!(id.as_str(), "nian-vision");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"nian-vision\"");

        let err = serde_json::from_str::<WorkspaceId>("\"../evil\"").unwrap_err();
        assert!(err.to_string().contains("invalid workspace id"));
    }
}
