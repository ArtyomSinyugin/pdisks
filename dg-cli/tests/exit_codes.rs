//! Executable exit categories use saved fixtures and never configure native storage providers.

#[path = "../../disksgraph/tests/support/mod.rs"]
mod support;

use std::{
    error::Error,
    fs,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
};

/// Checks process codes together with the single structured JSON error on stdout.
#[test]
fn binary_reports_input_io_and_root_failures_consistently() -> Result<(), Box<dyn Error>> {
    let (scratch, mut app, _, writes) = support::application()?;
    let draft = scratch.0.join("draft.json");
    dg_cli::execute(
        &mut app,
        &dg_cli::Command::New {
            output: draft.clone(),
        },
        &mut Vec::new(),
    )?;
    let malformed = scratch.0.join("malformed.json");
    fs::write(&malformed, b"{broken")?;
    let missing = scratch.0.join("missing.json");
    for (path, mode, expected, kind) in [
        (&malformed, 0o700, 2, "application"),
        (&missing, 0o700, 1, "io"),
        (&draft, 0o710, 1, "application"),
    ] {
        fs::set_permissions(&scratch.0, fs::Permissions::from_mode(mode))?;
        let result = Command::new(env!("CARGO_BIN_EXE_dg-cli"))
            .args(["plan", "preview", "--root"])
            .arg(&scratch.0)
            .arg("--draft")
            .arg(path)
            .output()?;
        assert_eq!(result.status.code(), Some(expected));
        let value: serde_json::Value = serde_json::from_slice(&result.stdout)?;
        assert_eq!(value["error"]["kind"], kind);
        assert!(result.stderr.is_empty());
        if path == &malformed {
            assert_eq!(value["error"]["detail"]["code"], "invalid_draft");
        }
    }
    fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o700))?;
    let result = Command::new(env!("CARGO_BIN_EXE_dg-cli")).output()?;
    assert_eq!(result.status.code(), Some(2));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&result.stdout)?["error"]["kind"],
        "usage"
    );
    let result = Command::new(env!("CARGO_BIN_EXE_dg-cli"))
        .args(["plan", "preview", "--root"])
        .arg(&scratch.0)
        .arg("--draft")
        .arg(&draft)
        .output()?;
    assert_eq!(result.status.code(), Some(0));
    let _: serde_json::Value = serde_json::from_slice(&result.stdout)?;
    assert_eq!(writes.get(), 0);
    assert!(!scratch.0.join("active-attempt.json").exists());
    Ok(())
}

/// Broken final stdout is an I/O failure even when the saved preview itself succeeds.
#[test]
fn binary_returns_io_failure_when_stdout_is_closed() -> Result<(), Box<dyn Error>> {
    let (scratch, mut app, _, writes) = support::application()?;
    let draft = scratch.0.join("draft.json");
    dg_cli::execute(
        &mut app,
        &dg_cli::Command::New {
            output: draft.clone(),
        },
        &mut Vec::new(),
    )?;
    let (reader, writer) = rustix::pipe::pipe()?;
    drop(reader);
    let result = Command::new(env!("CARGO_BIN_EXE_dg-cli"))
        .args(["plan", "preview", "--root"])
        .arg(&scratch.0)
        .arg("--draft")
        .arg(&draft)
        .stdout(Stdio::from(writer))
        .output()?;
    assert_eq!(result.status.code(), Some(1));
    assert!(result.stderr.is_empty());
    assert_eq!(writes.get(), 0);
    Ok(())
}

/// Help output follows the same I/O exit contract and does not panic on a closed pipe.
#[test]
fn binary_help_reports_broken_stdout_without_panicking() -> Result<(), Box<dyn Error>> {
    let (reader, writer) = rustix::pipe::pipe()?;
    drop(reader);
    let result = Command::new(env!("CARGO_BIN_EXE_dg-cli"))
        .arg("--help")
        .stdout(Stdio::from(writer))
        .output()?;
    assert_eq!(result.status.code(), Some(1));
    assert!(result.stderr.is_empty());
    let success = Command::new(env!("CARGO_BIN_EXE_dg-cli"))
        .arg("--help")
        .output()?;
    assert_eq!(success.status.code(), Some(0));
    assert!(String::from_utf8(success.stdout)?.contains("Native writes are unconfigured"));
    assert!(success.stderr.is_empty());
    Ok(())
}

/// Saved preview ignores native selection, and live reads never fall back to an environment manifest.
#[test]
fn binary_keeps_native_configuration_explicit() -> Result<(), Box<dyn Error>> {
    let (scratch, mut app, _, writes) = support::application()?;
    let draft = scratch.0.join("draft.json");
    dg_cli::execute(
        &mut app,
        &dg_cli::Command::New {
            output: draft.clone(),
        },
        &mut Vec::new(),
    )?;
    let ignored = scratch.0.join("unused-manifest.json");
    fs::write(&ignored, b"invalid native configuration")?;
    let preview = Command::new(env!("CARGO_BIN_EXE_dg-cli"))
        .args(["plan", "preview", "--root"])
        .arg(&scratch.0)
        .arg("--draft")
        .arg(&draft)
        .arg("--manifest")
        .arg(&ignored)
        .env("DISKSGRAPH_PROVIDER_MANIFEST", &ignored)
        .output()?;
    assert_eq!(preview.status.code(), Some(0));
    let _: serde_json::Value = serde_json::from_slice(&preview.stdout)?;
    assert!(preview.stderr.is_empty());
    let probe = Command::new(env!("CARGO_BIN_EXE_dg-cli"))
        .args(["probe", "--root"])
        .arg(&scratch.0)
        .env("DISKSGRAPH_PROVIDER_MANIFEST", &ignored)
        .output()?;
    assert_eq!(probe.status.code(), Some(2));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&probe.stdout)?["error"]["kind"],
        "usage"
    );
    assert!(probe.stderr.is_empty());
    assert_eq!(writes.get(), 0);
    assert!(!scratch.0.join("execution.lock").exists());
    assert!(!scratch.0.join("active-attempt.json").exists());
    Ok(())
}
