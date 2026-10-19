//! Public serialization of opaque diagnostics; raw Rust evidence is retained for trusted callers.

use serde::{Serialize, Serializer};

/// Fixed output avoids guessing which substring of an untrusted backend message might be secret.
pub const REDACTED: &str = "[diagnostic redacted]";

/// Serializes opaque free text without copying paths, credentials, or provider/request echoes.
/// This is a presentation boundary, not a sanitizer for persisted storage identities or plans.
pub fn serialize_message<S: Serializer>(_: &str, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(REDACTED)
}

/// Keeps absence distinguishable from a present diagnostic while suppressing its contents.
pub fn serialize_optional_message<S: Serializer>(
    value: &Option<String>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    value.as_ref().map(|_| REDACTED).serialize(serializer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Arbitrary encodings and multiline contents never reach serialized diagnostics.
    #[test]
    fn serialization_preserves_presence_and_raw_evidence() -> Result<(), serde_json::Error> {
        #[derive(Serialize)]
        struct Diagnostic {
            /// Arbitrary provider text remains owned by the trusted caller.
            #[serde(serialize_with = "serialize_message")]
            message: String,
            /// A missing reason must remain null rather than look like a present failure.
            #[serde(serialize_with = "serialize_optional_message")]
            reason: Option<String>,
        }
        let raw = "token=SECRET\nпароль=секрет\u{1b}[31m /private/path";
        let mut value = Diagnostic {
            message: raw.into(),
            reason: Some(raw.into()),
        };
        let json = serde_json::to_value(&value)?;
        assert_eq!(json["message"], REDACTED);
        assert_eq!(json["reason"], REDACTED);
        assert_eq!(value.message, raw);
        value.reason = None;
        assert!(serde_json::to_value(&value)?["reason"].is_null());
        Ok(())
    }
}
