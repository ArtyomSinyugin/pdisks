#![forbid(unsafe_code)]

//! Native CLI entry point; its manifest configures reads, never execution authorization.

mod native;

use dg_cli::{Error, Invocation};
use std::{
    io::{self, Write},
    process::ExitCode,
};

/// Commands produce one JSON result on stdout; diagnostics retain typed error categories.
fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && args[0] == "--help" {
        println!(
            "dg-cli probe --manifest FILE --root PRIVATE_DIR\ndg-cli plan new --manifest FILE --root PRIVATE_DIR --output DRAFT\ndg-cli plan add --root PRIVATE_DIR --draft DRAFT --request REQUEST --output DRAFT\ndg-cli plan preview --root PRIVATE_DIR --draft DRAFT\ndg-cli plan freeze --root PRIVATE_DIR --draft DRAFT --output PLAN\ndg-cli plan edit --root PRIVATE_DIR --draft DRAFT [--manifest FILE]\ndg-cli plan apply --manifest FILE --root PRIVATE_DIR --file PLAN [--events jsonl]\ndg-cli incident show --manifest FILE --root PRIVATE_DIR\ndg-cli incident acknowledge --manifest FILE --root PRIVATE_DIR\nAll results are JSON. The execution root must already exist with mode 0700.\nSaved-draft commands do not probe or refresh the baseline.\nNative writes are unconfigured and fail closed. Events are flushed live during blocking execution."
        );
        return ExitCode::SUCCESS;
    }
    let result = Invocation::parse(args).and_then(|invocation| native::run(&invocation));
    let (value, exit) = match result {
        Ok(value) => {
            let exit = result_exit_code(&value);
            (value, exit)
        }
        Err(error) => {
            let exit = if matches!(error, Error::Usage(_) | Error::Document(_)) {
                2
            } else {
                1
            };
            (serde_json::json!({"error": error}), exit)
        }
    };
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if serde_json::to_writer(&mut output, &value).is_err()
        || writeln!(output).is_err()
        || output.flush().is_err()
    {
        return ExitCode::FAILURE;
    }
    ExitCode::from(exit)
}

/// Completed attempts succeed only after actual final verification; event I/O has its own failure.
fn result_exit_code(value: &serde_json::Value) -> u8 {
    if !value["event_output_error"].is_null() {
        1
    } else if value.get("run").is_some() && value["run"]["verified"] != true {
        3
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Neither finished=true nor an otherwise successful facade call can masquerade as verification.
    #[test]
    fn only_verified_attempt_has_success_exit() {
        assert_eq!(result_exit_code(&serde_json::json!({"revision": 0})), 0);
        assert_eq!(
            result_exit_code(&serde_json::json!({"run": {"finished": true, "verified": false}})),
            3
        );
        assert_eq!(
            result_exit_code(&serde_json::json!({"run": {"verified": true}})),
            0
        );
        assert_eq!(
            result_exit_code(
                &serde_json::json!({"run": {"verified": true}, "event_output_error": "closed"})
            ),
            1
        );
    }
}
