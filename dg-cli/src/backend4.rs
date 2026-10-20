//! Altera backend4 framing over the same application protocol used by the ABI.

use disksgraph::wire::{MAX_REQUEST_BYTES, ProtocolError, Request, encode_reply, parse_request};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{self, BufRead, Read, Write};

/// Outer framing overhead is separate from the unchanged application's request bound.
pub const MAX_LINE_BYTES: usize = MAX_REQUEST_BYTES + 4096;

/// Exactly one Altera-authorized action, with no caller-controlled host configuration.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    /// Standard backend4 action object forwarded by altr-server.
    action: Action,
}

/// Externally tagged enums reject multiple action keys instead of choosing the first one.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    /// Describe the module without invoking the application.
    List(Empty),
    /// Query application state without admitting storage changes or editor mutations.
    Read(Value),
    /// Invoke an application method after Altera authorizes write access.
    Write(Value),
}

/// List accepts an empty object so typos cannot silently become ignored configuration.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

/// Dispatches one outer frame; the caller supplies the existing sole application owner.
pub fn call(
    input: &[u8],
    dispatch: &mut impl FnMut(Request) -> Result<Value, ProtocolError>,
) -> String {
    let result = (|| {
        if input.len() > MAX_LINE_BYTES {
            return Err(ProtocolError::ResourceLimit);
        }
        let envelope: Envelope = serde_json::from_slice(input)
            .map_err(|error| ProtocolError::InvalidRequest(error.to_string()))?;
        let (payload, write) = match envelope.action {
            Action::List(_) => {
                return Ok(json!({
                    "module":"disksgraph", "actions":["list","read","write"],
                    "protocol_version":disksgraph::wire::PROTOCOL_VERSION,
                }));
            }
            Action::Read(payload) => (payload, false),
            Action::Write(payload) => (payload, true),
        };
        let request = parse_request(&payload.to_string())?;
        if !write && !request.is_read_only() {
            return Err(ProtocolError::InvalidRequest(
                "method requires backend4 write action".into(),
            ));
        }
        dispatch(request)
    })();
    encode_reply(result)
}

/// Runs the one-line JSON contract until EOF, preserving the owner after malformed bounded requests.
/// Oversized frames receive one error then close; their unread tail is never a second command.
pub fn serve(
    input: &mut impl BufRead,
    output: &mut impl Write,
    dispatch: &mut impl FnMut(Request) -> Result<Value, ProtocolError>,
) -> io::Result<()> {
    loop {
        let mut line = Vec::new();
        let count = (&mut *input)
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)?;
        if count == 0 {
            return Ok(());
        }
        if line.len() <= MAX_LINE_BYTES && line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let reply = call(&line, dispatch);
        writeln!(output, "{reply}")?;
        output.flush()?;
        if line.len() > MAX_LINE_BYTES {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The outer action must match the method's effects before the shared facade sees the request.
    #[test]
    fn framing_rejects_ambiguous_and_misclassified_requests()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut calls = 0;
        let mut dispatch = |_: Request| {
            calls += 1;
            Ok(json!({"called":true}))
        };
        for input in [
            r#"{"action":{"read":{},"write":{}}}"#,
            r#"{"action":{"list":{}},"root":"/elsewhere"}"#,
            r#"{"action":{"read":{"protocol_version":1,"request":{"method":"session_create","params":{}}}}}"#,
            r#"{"action":{"write":{"protocol_version":2,"request":{"method":"probe","params":{}}}}}"#,
        ] {
            let reply: Value = serde_json::from_str(&call(input.as_bytes(), &mut dispatch))?;
            assert!(reply.get("error").is_some(), "{reply}");
        }
        let listed: Value =
            serde_json::from_str(&call(br#"{"action":{"list":{}}}"#, &mut dispatch))?;
        assert_eq!(listed["result"]["module"], "disksgraph");
        let read: Value = serde_json::from_str(&call(br#"{"action":{"read":{"protocol_version":1,"request":{"method":"probe","params":{}}}}}"#, &mut dispatch))?;
        assert_eq!(read["result"]["called"], true);
        let write: Value = serde_json::from_str(&call(br#"{"action":{"write":{"protocol_version":1,"request":{"method":"session_create","params":{}}}}}"#, &mut dispatch))?;
        assert_eq!(write["result"]["called"], true);
        assert_eq!(calls, 2);
        Ok(())
    }

    /// A bad line leaves the stream usable, while an oversized line closes before its tail executes.
    #[test]
    fn stream_recovers_bounded_errors_and_closes_oversized_frames()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut input = &b"broken\n\n{\"action\":{\"list\":{}}}\n"[..];
        let mut output = Vec::new();
        serve(&mut input, &mut output, &mut |_| {
            panic!("list cannot dispatch")
        })?;
        let replies: Vec<Value> = String::from_utf8(output)?
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        assert_eq!(replies.len(), 2);
        assert!(replies[0].get("error").is_some());
        assert_eq!(replies[1]["result"]["module"], "disksgraph");
        let mut oversized = vec![b' '; MAX_LINE_BYTES + 1];
        oversized.extend_from_slice(b"{\"action\":{\"list\":{}}}\n");
        let mut output = Vec::new();
        serve(&mut &oversized[..], &mut output, &mut |_| {
            panic!("oversized cannot dispatch")
        })?;
        let reply: Value = serde_json::from_slice(&output)?;
        assert_eq!(reply["error"]["code"], "resource_limit");
        Ok(())
    }
}
