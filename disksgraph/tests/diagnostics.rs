//! Error serialization omits opaque context while preserving typed behavior and trusted Rust evidence.

use disksgraph::{
    DisksGraphError, ErrorCode,
    diagnostics::REDACTED,
    wire::{ProtocolError, encode_reply, parse_request},
};

/// Raw diagnostics survive in memory, but direct serde and protocol envelopes never echo them.
#[test]
fn application_and_protocol_errors_do_not_serialize_raw_text() -> Result<(), serde_json::Error> {
    let raw = "passphrase=SECRET /private/path\nprovider echo";
    let error = DisksGraphError {
        code: ErrorCode::ProbeFailed,
        message: raw.into(),
        run_id: None,
    };
    assert_eq!(error.message, raw);
    assert!(error.to_string().contains(raw));
    let json = serde_json::to_value(&error)?;
    assert_eq!(json["message"], REDACTED);
    assert_eq!(json["code"], "probe_failed");
    for error in [
        ProtocolError::Application(error),
        ProtocolError::InvalidRequest(raw.into()),
        ProtocolError::Serialization(raw.into()),
        ProtocolError::UnsupportedOperation(raw.into()),
    ] {
        let output = encode_reply(Err(error));
        assert!(!output.contains("SECRET"));
        assert!(output.contains(REDACTED));
    }
    let request = r#"{"protocol_version":1,"request":{"method":"SECRET-user-supplied-method"}}"#;
    let error = parse_request(request)
        .err()
        .ok_or_else(|| serde_json::Error::io(std::io::Error::other("expected rejection")))?;
    assert!(error.to_string().contains("SECRET-user-supplied-method"));
    let value: serde_json::Value = serde_json::from_str(&encode_reply(Err(error)))?;
    assert_eq!(value["error"]["code"], "invalid_request");
    assert_eq!(value["error"]["detail"], REDACTED);
    Ok(())
}
