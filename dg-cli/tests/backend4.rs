//! Actual backend4 subprocesses share application sessions and never configure disk writes.

#[path = "../../disksgraph/tests/support/mod.rs"]
mod support;

use serde_json::{Value, json};
use std::{
    error::Error,
    fs,
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
};

/// Uses the same outer framing as altr-server and reads exactly one flushed response.
fn exchange(
    input: &mut impl Write,
    output: &mut impl BufRead,
    action: &str,
    payload: Value,
) -> Result<Value, Box<dyn Error>> {
    writeln!(input, "{}", json!({"action":{action:payload}}))?;
    input.flush()?;
    let mut line = String::new();
    assert!(output.read_line(&mut line)? > 0);
    Ok(serde_json::from_str(&line)?)
}

/// Keeps imported drafts alive across messages and returns the direct facade's exact preview.
#[test]
fn executable_preserves_live_sessions_and_backend4_framing() -> Result<(), Box<dyn Error>> {
    let (scratch, mut app, _, writes) = support::application()?;
    let session = app.create_session()?;
    let draft = app.export_draft(session)?;
    let expected = serde_json::to_value(app.preview_document(session)?)?;
    let config = scratch.0.join("backend4.json");
    fs::write(&config, json!({"root":scratch.0}).to_string())?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_dg-backend4"))
        .arg("--config")
        .arg(&config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().ok_or("stdin")?;
    let mut output = BufReader::new(child.stdout.take().ok_or("stdout")?);
    assert_eq!(
        exchange(&mut input, &mut output, "list", json!({}))?["result"]["module"],
        "disksgraph"
    );
    let imported = exchange(
        &mut input,
        &mut output,
        "write",
        json!({"protocol_version":1,"request":{"method":"session_import","params":{"draft":draft}}}),
    )?;
    let session = imported["result"]["session"].clone();
    assert!(session.is_string(), "{imported}");
    let preview = exchange(
        &mut input,
        &mut output,
        "read",
        json!({"protocol_version":1,"request":{"method":"session_preview","params":{"session":session}}}),
    )?;
    assert_eq!(preview["result"], expected);
    let rejected = exchange(
        &mut input,
        &mut output,
        "read",
        json!({"protocol_version":1,"request":{"method":"apply_blocking","params":{"plan":"00000000-0000-7000-8000-000000000001"}}}),
    )?;
    assert_eq!(rejected["error"]["code"], "invalid_request");
    let closed = exchange(
        &mut input,
        &mut output,
        "write",
        json!({"protocol_version":1,"request":{"method":"session_close","params":{"session":session}}}),
    )?;
    assert!(closed.get("error").is_none());
    drop(input);
    let final_output = child.wait_with_output()?;
    assert!(final_output.status.success());
    assert!(final_output.stdout.is_empty());
    assert!(final_output.stderr.is_empty());
    assert_eq!(writes.get(), 0);
    assert!(!scratch.0.join("active-attempt.json").exists());
    Ok(())
}
