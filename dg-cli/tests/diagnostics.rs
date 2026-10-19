//! Public CLI errors omit opaque text without changing failure codes or retained execution evidence.

#[path = "../../disksgraph/tests/support/mod.rs"]
mod support;
use disksgraph::diagnostics::REDACTED;
use std::{
    error::Error,
    io::{self, Write},
    process::Command,
};

/// Even usage diagnostics must not echo attacker-controlled flags into captured JSON output.
#[test]
fn executable_does_not_echo_unknown_flags() -> Result<(), Box<dyn Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_dg-cli"))
        .args([
            "plan",
            "preview",
            "--root",
            "/unused",
            "--draft",
            "/unused",
            "--SECRET-private-flag",
            "value",
        ])
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["error"]["kind"], "usage");
    assert_eq!(value["error"]["detail"], REDACTED);
    assert!(!String::from_utf8(output.stdout)?.contains("SECRET"));
    assert!(output.stderr.is_empty());
    Ok(())
}

/// Covers direct serde use by embedding hosts as well as ordinary executable envelopes.
#[test]
fn typed_cli_errors_keep_codes_and_raw_rust_context() -> Result<(), Box<dyn Error>> {
    let raw = "SECRET /private/file\nprovider echo";
    for (error, code) in [
        (dg_cli::Error::Usage(raw.into()), 2),
        (dg_cli::Error::Document(raw.into()), 2),
        (dg_cli::Error::Io(raw.into()), 1),
        (dg_cli::Error::Configuration(raw.into()), 1),
    ] {
        assert_eq!(error.exit_code(), code);
        assert_eq!(error.to_string(), raw);
        assert_eq!(serde_json::to_value(error)?["detail"], REDACTED);
    }
    Ok(())
}

/// Synthetic writer models an error implementation that itself contains sensitive context.
struct PrivateWriter;
impl Write for PrivateWriter {
    /// Only the public result category, never this arbitrary message, should escape the adapter.
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("SECRET-output-context"))
    }
    /// The synthetic error happens on write, rather than flush.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Redacting stream failure must not erase the actual verified run or its nonzero I/O exit.
#[test]
fn event_output_failure_is_redacted_without_changing_run_success() -> Result<(), Box<dyn Error>> {
    let (scratch, mut app, _, writes) = support::application()?;
    let session = app.create_session()?;
    let frozen = app.freeze(session, app.revision(session)?)?;
    let path = scratch.0.join("plan.json");
    dg_cli::files::save_document(&path, &frozen.document().canonical_json()?)?;
    let result = dg_cli::execute(
        &mut app,
        &dg_cli::Command::Apply {
            file: path,
            events: true,
        },
        &mut PrivateWriter,
    )?;
    assert_eq!(result["run"]["verified"], true);
    assert_eq!(result["event_output_error"], REDACTED);
    assert_eq!(dg_cli::result_exit_code(&result), 1);
    assert!(!result.to_string().contains("SECRET"));
    assert_eq!(writes.get(), 0);
    Ok(())
}
