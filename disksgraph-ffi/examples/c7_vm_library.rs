#![deny(unsafe_code, unsafe_op_in_unsafe_fn)]
//! Separate guarded acceptance library; the production cdylib keeps its read-only constructor.

#[path = "../../dg-cli/examples/support/guest.rs"]
pub mod guest;

/// Narrow Stabby macro boundary, matching the production owned-string signatures.
#[allow(unsafe_code)]
mod abi {
    use disksgraph_ffi::transport::{boundary, call, close, open_with, reply, Error};
    use stabby::string::String as StableString;

    /// Same checked signature version as the production transport.
    #[stabby::export]
    pub extern "C" fn disksgraph_vm_api_version() -> u32 {
        disksgraph_ffi::ABI_VERSION
    }

    /// Only this separately built artifact supplies a guarded VM host; request JSON supplies no policies.
    #[stabby::export]
    pub extern "C" fn disksgraph_vm_open(config: StableString) -> StableString {
        boundary(|| {
            reply(open_with(config.as_str(), |root, manifest, worker| {
                crate::guest::guard().map_err(|error| Error::OpenFailed(error.to_string()))?;
                if root != std::path::Path::new("/run/c4-execution")
                    || manifest.as_deref() != Some(std::path::Path::new("/run/providers.json"))
                {
                    return Err(Error::OpenFailed(
                        "fixed guest configuration required".into(),
                    ));
                }
                let fail_mount = match std::fs::read_to_string("/run/c7-case")
                    .map_err(|error| Error::OpenFailed(error.to_string()))?
                    .as_str()
                {
                    "success" => false,
                    "mount-target-change" => true,
                    _ => return Err(Error::OpenFailed("unknown guest case".into())),
                };
                let reviewed = std::rc::Rc::new(std::cell::RefCell::new(None));
                crate::guest::with_host(
                    fail_mount,
                    reviewed,
                    Some("/run/c7-review.json"),
                    |app, calls| {
                        worker.serve(app);
                        std::fs::write("/run/c7-native-calls", calls.get().to_string())?;
                        Ok(())
                    },
                )
                .map_err(|error| Error::OpenFailed(error.to_string()))
            }))
        })
    }

    /// The exact production protocol/job transport, with a separate test-artifact symbol namespace.
    #[stabby::export]
    pub extern "C" fn disksgraph_vm_call(context: u64, request: StableString) -> StableString {
        boundary(|| match call(context, request.as_str()) {
            Ok(response) => response,
            Err(error) => reply(Err(error)),
        })
    }

    /// Joins the same owner, including borrowed native registrations and final guest counter publication.
    #[stabby::export]
    pub extern "C" fn disksgraph_vm_close(context: u64) -> StableString {
        boundary(|| reply(close(context)))
    }
}
