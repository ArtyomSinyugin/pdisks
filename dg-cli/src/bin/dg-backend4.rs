#![forbid(unsafe_code)]
//! Persistent Altera backend4 executable; all storage behavior belongs to disksgraph.

use dg_cli::{Error, backend4, files};
use disksgraph::wire::Protocol;
use serde::Deserialize;
use std::{
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

/// Trusted startup configuration, never accepted in forwarded client messages.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// Existing private root shared with the standalone CLI and other hosts.
    root: PathBuf,
    /// Optional actual provider registrations; absence selects saved-input operation.
    #[serde(default)]
    manifest: Option<PathBuf>,
}

/// Altera launches modules without arguments; deployments supply this file or an explicit override.
fn run() -> Result<(), Error> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let path = match args.as_slice() {
        [] => std::env::var_os("DISKSGRAPH_BACKEND4_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/etc/disksgraph/backend4.json".into()),
        [flag, path] if flag == "--config" => path.into(),
        _ => return Err(Error::Usage("dg-backend4 [--config FILE]".into())),
    };
    let config: Config = serde_json::from_str(&files::read_document(&path)?)?;
    let app = disksgraph::native::open(&config.root, config.manifest.as_deref())
        .map_err(|error| Error::Configuration(error.to_string()))?;
    let mut protocol = Protocol::new(app, config.manifest.is_some());
    backend4::serve(
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
        &mut |request| protocol.call(request),
    )?;
    Ok(())
}

/// Emits only JSON lines, including startup failures; broken output returns an I/O exit failure.
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let exit = error.exit_code();
            let mut output = io::stdout().lock();
            let reply = serde_json::json!({"error":error});
            if writeln!(output, "{reply}")
                .and_then(|_| output.flush())
                .is_err()
            {
                return ExitCode::FAILURE;
            }
            ExitCode::from(exit)
        }
    }
}
