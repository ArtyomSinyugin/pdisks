#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

//! Narrow Stabby export boundary. No raw pointers, core models or ordinary Rust strings cross it.
//! Stabby 72 exports use edition-2021 attributes; this module's crate deliberately retains that edition.

use stabby::string::String as StableString;

/// Contains all ordinary Rust unwinding before returning an ABI-owned allocation.
pub fn boundary(action: impl FnOnce() -> String) -> StableString {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        StableString::from(action().as_str())
    })) {
        Ok(result) => result,
        Err(payload) => {
            std::mem::forget(payload);
            StableString::from("{\"protocol_version\":1,\"error\":{\"code\":\"panicked\"}}")
        }
    }
}

/// Reports the checked signature contract independently of the application JSON version.
#[stabby::export]
pub extern "C" fn disksgraph_api_version() -> u32 {
    crate::ABI_VERSION
}

/// Opens one owner from bounded trusted configuration; its integer handle is process-local.
#[stabby::export]
pub extern "C" fn disksgraph_open(config: StableString) -> StableString {
    boundary(|| crate::registry::reply(crate::registry::open(config.as_str())))
}

/// Opens an explicitly write-capable native host with a checked application policy callback.
/// The callback and its cookie remain owned by the caller until close joins the worker.
#[stabby::export]
pub extern "C" fn disksgraph_open_with_policy(
    config: StableString,
    callback: crate::policy::PolicyCallback,
    cookie: u64,
) -> StableString {
    boundary(|| {
        crate::registry::reply(crate::registry::open_with_host_policy(
            config.as_str(),
            move || crate::policy::CallbackPolicy { callback, cookie },
        ))
    })
}

/// Dispatches versioned application JSON; background jobs expose passive polling without facade transfer.
#[stabby::export]
pub extern "C" fn disksgraph_call(context: u64, request: StableString) -> StableString {
    boundary(|| match crate::registry::call(context, request.as_str()) {
        Ok(result) => result,
        Err(error) => crate::registry::reply(Err(error)),
    })
}

/// Joins the owner before returning; the caller must also drop returned strings before unloading the library.
#[stabby::export]
pub extern "C" fn disksgraph_close(context: u64) -> StableString {
    boundary(|| crate::registry::reply(crate::registry::close(context)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A panic becomes JSON and cannot unwind across any exported C calling convention.
    #[test]
    fn panic_is_contained_before_crossing_the_abi() -> Result<(), serde_json::Error> {
        let response = boundary(|| panic!("controlled boundary test"));
        let value: serde_json::Value = serde_json::from_str(response.as_str())?;
        assert_eq!(value["error"]["code"], "panicked");
        assert_eq!(disksgraph_api_version(), crate::ABI_VERSION);
        Ok(())
    }
}
