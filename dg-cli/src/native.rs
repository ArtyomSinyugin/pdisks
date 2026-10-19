//! CLI configuration reuses the facade's owned native read stack and disabled write policies.

use dg_cli::{Error, Invocation};

/// Keeps configuration errors distinct while using the same application constructor as other transports.
pub(super) fn run(invocation: &Invocation) -> Result<serde_json::Value, Error> {
    let manifest = if invocation.command.needs_probe() {
        invocation.manifest.as_deref()
    } else {
        None
    };
    let mut app =
        disksgraph::native::open(&invocation.root, manifest).map_err(|error| match error {
            disksgraph::native::OpenError::Configuration(message) => Error::Configuration(message),
            disksgraph::native::OpenError::Application(error) => Error::Application(error),
        })?;
    dg_cli::execute(&mut app, &invocation.command, &mut std::io::stderr().lock())
}
