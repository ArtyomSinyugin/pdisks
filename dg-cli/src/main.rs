#![forbid(unsafe_code)]

//! Native CLI entry point; its manifest configures reads, never execution authorization.

use dg_cli::{Invocation, result_exit_code};
use std::{
    io::{self, Write},
    process::ExitCode,
};

/// Commands produce one JSON result on stdout; diagnostics retain typed error categories.
fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && args[0] == "--help" {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        let result = writeln!(
            output,
            "dg-cli probe --manifest FILE --root PRIVATE_DIR\ndg-cli plan new --manifest FILE --root PRIVATE_DIR --output DRAFT\ndg-cli plan add --root PRIVATE_DIR --draft DRAFT --request REQUEST --output DRAFT\ndg-cli plan preview --root PRIVATE_DIR --draft DRAFT\ndg-cli plan freeze --root PRIVATE_DIR --draft DRAFT --output PLAN\ndg-cli plan edit --root PRIVATE_DIR --draft DRAFT [--manifest FILE]\ndg-cli plan apply --manifest FILE --root PRIVATE_DIR --file PLAN [--events jsonl]\ndg-cli incident show --manifest FILE --root PRIVATE_DIR\ndg-cli incident acknowledge --manifest FILE --root PRIVATE_DIR\nAll results are JSON. The execution root must already exist with mode 0700.\nSaved-draft commands do not probe or refresh the baseline.\nNative apply requires root, execute-enabled backends and exact plan confirmation. Events are flushed live during blocking execution."
        );
        return if result.and_then(|_| output.flush()).is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    let result = Invocation::parse(args).and_then(|invocation| dg_cli::native::run(&invocation));
    let (value, exit) = match result {
        Ok(value) => {
            let exit = result_exit_code(&value);
            (value, exit)
        }
        Err(error) => {
            let exit = error.exit_code();
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
