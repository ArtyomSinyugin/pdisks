#![forbid(unsafe_code)]
//! Persistent Altera backend4 executable; all storage behavior belongs to disksgraph.

use dg_cli::{Error, backend4, confirmation::RootPolicy, files};
use disksgraph::wire::Protocol;
use disksgraph::{
    DisksGraph, IncidentAcknowledgmentPolicy,
    host::{
        authorization::AuthorizationContext, capability::BackendCapabilityCheck,
        dispatcher::BackendDispatcher, host::LinuxLiveHost,
    },
};
use serde::Deserialize;
use std::{
    cell::RefCell,
    io::{self, BufReader, Write},
    path::PathBuf,
    process::ExitCode,
    rc::Rc,
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
    /// Enables the root application policy; each nonempty apply still requires live confirmation.
    #[serde(default)]
    write: bool,
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
    let input = Rc::new(RefCell::new(BufReader::new(io::stdin())));
    let output = Rc::new(RefCell::new(io::stdout()));
    if config.write {
        if !rustix::process::getuid().is_root() || !rustix::process::geteuid().is_root() {
            return Err(Error::Configuration(
                "backend4 write host requires root".into(),
            ));
        }
        let manifest = config
            .manifest
            .as_deref()
            .ok_or_else(|| Error::Configuration("write host requires a manifest".into()))?;
        let confirmation_input = input.clone();
        let confirmation_output = output.clone();
        let policy = disksgraph::native::HostPolicy::new(RootPolicy::new(
            move |review: &serde_json::Value| {
                let mut output = confirmation_output.borrow_mut();
                writeln!(
                    output,
                    "{}",
                    serde_json::json!({"protocol_version":1,"result":review})
                )
                .and_then(|_| output.flush())
                .map_err(|error| error.to_string())?;
                drop(output);
                backend4::read_confirmation(&mut *confirmation_input.borrow_mut())
                    .map_err(|error| error.to_string())
            },
        ));
        let app = disksgraph::native::open_with_policy(&config.root, manifest, policy.clone())
            .map_err(|error| Error::Configuration(error.to_string()))?;
        serve(app, true, input, output, policy)
    } else {
        let app = disksgraph::native::open(&config.root, config.manifest.as_deref())
            .map_err(|error| Error::Configuration(error.to_string()))?;
        serve(
            app,
            config.manifest.is_some(),
            input,
            output,
            disksgraph::native::ReadOnlyPolicy,
        )
    }
}

/// Shares one reader with the confirmation channel and releases borrows before calling the facade.
fn serve<
    C: BackendCapabilityCheck,
    A: AuthorizationContext,
    D: BackendDispatcher,
    P: IncidentAcknowledgmentPolicy,
>(
    app: DisksGraph<LinuxLiveHost<'_, C, A>, D>,
    live: bool,
    input: Rc<RefCell<BufReader<io::Stdin>>>,
    output: Rc<RefCell<io::Stdout>>,
    mut policy: P,
) -> Result<(), Error> {
    let mut protocol = Protocol::new(app, live);
    loop {
        let line = backend4::read_frame(&mut *input.borrow_mut())?;
        let Some(line) = line else {
            return Ok(());
        };
        let reply = backend4::call(&line, &mut |request| {
            protocol.call_with_policy(request, &mut policy)
        });
        let mut output = output.borrow_mut();
        writeln!(output, "{reply}")?;
        output.flush()?;
        if line.len() > backend4::MAX_LINE_BYTES {
            return Ok(());
        }
    }
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
