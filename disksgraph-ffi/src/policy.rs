//! Checked host callback channel, separate from all ordinary application request JSON.

use crate::registry::Error;
use disksgraph::{
    native::{NativeHostPolicy, PolicyRequest},
    LiveCheckStatus,
};
use stabby::string::String as StableString;

/// Callback executes on the context owner thread; cookie and code must remain valid through close.
/// It must not unwind, unload its library, or recursively wait for its own busy context.
pub type PolicyCallback = extern "C" fn(u64, StableString) -> StableString;

/// Version of policy JSON, independent of existing application requests and export signatures.
pub const POLICY_VERSION: u32 = 1;
/// Bound policy context independently of application input; it contains reviewed plus fresh graphs.
const MAX_POLICY_REQUEST_BYTES: usize = 64 * 1024 * 1024;
/// Results need only a typed status and bounded explanation.
const MAX_POLICY_REPLY_BYTES: usize = 64 * 1024;

/// The embedding process owns the callback; no callback address is parsed from JSON.
pub struct CallbackPolicy {
    /// Checked stable function pointer supplied as an ABI argument.
    pub callback: PolicyCallback,
    /// Opaque caller value; the library never dereferences it as a pointer.
    pub cookie: u64,
}
impl NativeHostPolicy for CallbackPolicy {
    /// Any serialization, size, version or response error leaves evidence Unknown.
    fn check(&mut self, request: PolicyRequest<'_>) -> LiveCheckStatus {
        let request = match serde_json::to_value(request) {
            Ok(request) => request,
            Err(_) => return unknown(),
        };
        let input =
            serde_json::json!({"policy_version":POLICY_VERSION,"request":request}).to_string();
        if input.len() > MAX_POLICY_REQUEST_BYTES {
            return unknown();
        }
        let response = (self.callback)(self.cookie, StableString::from(input.as_str()));
        decode_reply(response.as_str()).unwrap_or_else(|_| unknown())
    }
}

/// Validates the version and strict response shape before accepting a host attestation.
fn decode_reply(input: &str) -> Result<LiveCheckStatus, Error> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    /// Exactly one versioned typed decision from the trusted callback.
    struct Reply {
        /// Independent policy schema, never guessed from the outer application protocol.
        policy_version: u32,
        /// Satisfied/Violated/Unknown supplied by the embedding application.
        status: LiveCheckStatus,
    }
    if input.len() > MAX_POLICY_REPLY_BYTES {
        return Err(Error::ResourceLimit);
    }
    let reply: Reply =
        serde_json::from_str(input).map_err(|error| Error::InvalidConfig(error.to_string()))?;
    if reply.policy_version != POLICY_VERSION {
        return Err(Error::UnsupportedVersion(u64::from(reply.policy_version)));
    }
    Ok(reply.status)
}

/// Malformed host decisions cannot grant access or masquerade as missing policy checks.
fn unknown() -> LiveCheckStatus {
    LiveCheckStatus::Unknown {
        required_evidence: "valid versioned host policy decision".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Only the exact versioned typed response can authorize a policy check.
    #[test]
    fn callback_decisions_require_version_and_strict_shape() {
        assert_eq!(
            decode_reply(r#"{"policy_version":1,"status":"satisfied"}"#).ok(),
            Some(LiveCheckStatus::Satisfied)
        );
        for input in [
            r#"true"#,
            r#"{"status":"satisfied"}"#,
            r#"{"policy_version":2,"status":"satisfied"}"#,
            r#"{"policy_version":1,"status":"satisfied","approve":true}"#,
            r#"{"policy_version":1,"status":"unknown"}"#,
        ] {
            assert!(decode_reply(input).is_err(), "{input}");
        }
        assert!(decode_reply(&" ".repeat(MAX_POLICY_REPLY_BYTES + 1)).is_err());
    }
}
