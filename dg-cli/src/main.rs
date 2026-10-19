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
            "dg-cli probe --manifest FILE --root PRIVATE_DIR\ndg-cli plan new --manifest FILE --root PRIVATE_DIR --output DRAFT\nAll results are JSON. The execution root must already exist with mode 0700."
        );
        return ExitCode::SUCCESS;
    }
    let result = Invocation::parse(args).and_then(|invocation| native::run(&invocation));
    let (value, exit) = match result {
        Ok(value) => (value, 0),
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
