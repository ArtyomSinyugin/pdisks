//! Standalone root command host over the same native facade used by library embeddings.

use crate::{
    Command, Error, Invocation,
    confirmation::{RootPolicy, read_reply},
};

/// Keeps configuration errors distinct while using the same application constructor as other transports.
pub fn run(invocation: &Invocation) -> Result<serde_json::Value, Error> {
    if matches!(invocation.command, Command::Apply { .. }) {
        if !rustix::process::geteuid().is_root() || !rustix::process::getuid().is_root() {
            return Err(Error::Configuration(
                "apply requires a root invocation".into(),
            ));
        }
        let manifest = invocation
            .manifest
            .as_deref()
            .ok_or_else(|| Error::Usage("apply requires --manifest".into()))?;
        let policy =
            disksgraph::native::HostPolicy::new(RootPolicy::new(|review: &serde_json::Value| {
                crate::commands::write_event(&mut std::io::stderr(), review)
                    .map_err(|error| error.to_string())?;
                read_reply(&mut std::io::stdin().lock()).map_err(|error| error.to_string())
            }));
        let mut app =
            disksgraph::native::open_with_policy(&invocation.root, manifest, policy.clone())
                .map_err(|error| Error::Configuration(error.to_string()))?;
        // Apply's stdin belongs to the confirmation callback, not the editor/incident reader.
        return crate::execute_with_io(
            &mut app,
            &invocation.command,
            &mut &b""[..],
            &mut std::io::stderr(),
            &mut policy.clone(),
        );
    }
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
    crate::execute(&mut app, &invocation.command, &mut std::io::stderr().lock())
}
