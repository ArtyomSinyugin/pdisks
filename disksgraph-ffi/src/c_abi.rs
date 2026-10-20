#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
//! Narrow C memory bridge over the same owned registry, jobs, policy and coordinator as Stabby.

use crate::{
    policy,
    registry::{self, Error},
};
use disksgraph::{
    native::{NativeHostPolicy, PolicyRequest},
    LiveCheckStatus,
};
use std::{
    ffi::{c_char, CString},
    slice, str,
};

/// Host-owned function remains valid until close joins the owner; input is borrowed for this call only.
type Callback = unsafe extern "C" fn(u64, *const u8, usize) -> i32;

/// Returns a caller-freed UTF-8 JSON allocation after ordinary panic containment.
fn output(action: impl FnOnce() -> String) -> *mut c_char {
    let reply = crate::abi::boundary(action);
    CString::new(reply.as_str())
        .map(CString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}

/// Validates lengths before borrowing external memory; no borrowed data escapes the exported call.
/// # Safety
/// A non-null pointer must reference `length` readable bytes for the duration of this call.
unsafe fn input<'a>(pointer: *const u8, length: usize, limit: usize) -> Result<&'a str, Error> {
    if length > limit {
        return Err(Error::ResourceLimit);
    }
    if pointer.is_null() {
        return Err(Error::InvalidConfig("null JSON input".into()));
    }
    // SAFETY: the exported function's caller guarantees a readable region of exactly this length.
    let bytes = unsafe { slice::from_raw_parts(pointer, length) };
    str::from_utf8(bytes).map_err(|error| Error::InvalidConfig(error.to_string()))
}

/// Reports the C memory-ownership contract version; application JSON has its own version.
#[unsafe(no_mangle)]
pub extern "C" fn disksgraph_json_api_version() -> u32 {
    1
}

/// Opens the ordinary read-only/saved-input native context.
/// # Safety
/// `json` must reference `length` readable bytes. Free the result only with disksgraph_json_free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn disksgraph_json_open(json: *const u8, length: usize) -> *mut c_char {
    output(|| {
        // SAFETY: caller guarantees readable input; the 64 KiB bound is checked before dereference.
        registry::reply(unsafe { input(json, length, 64 * 1024) }.and_then(registry::open))
    })
}

/// Calls the same bounded owner transport, including background jobs and passive evidence polling.
/// # Safety
/// `json` must reference `length` readable bytes. The returned allocation belongs to this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn disksgraph_json_call(
    context: u64,
    json: *const u8,
    length: usize,
) -> *mut c_char {
    output(|| {
        // SAFETY: caller guarantees input validity for the exported call; no reference is retained.
        match unsafe { input(json, length, disksgraph::wire::MAX_REQUEST_BYTES) }
            .and_then(|input| registry::call(context, input))
        {
            Ok(reply) => reply,
            Err(error) => registry::reply(Err(error)),
        }
    })
}

/// Closes and joins the owner before its host callback may be released.
#[unsafe(no_mangle)]
pub extern "C" fn disksgraph_json_close(context: u64) -> *mut c_char {
    output(|| registry::reply(registry::close(context)))
}

/// Frees exactly one returned string, including strings containing errors; NULL is accepted.
/// # Safety
/// A non-null pointer must be an unfreed allocation returned by this library's JSON exports.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn disksgraph_json_free(pointer: *mut c_char) {
    if !pointer.is_null() {
        // SAFETY: caller transfers back the exact allocation from CString::into_raw, once only.
        drop(unsafe { CString::from_raw(pointer) });
    }
}

/// Host callback supplied as code, never as a JSON address or request-level approval flag.
struct CPolicy {
    /// Retained caller function, valid throughout the context owner's lifetime.
    callback: Callback,
    /// Opaque caller value; the library does not dereference it.
    cookie: u64,
}
impl NativeHostPolicy for CPolicy {
    /// Converts only the fixed decision codes; malformed or exceptional host responses fail closed.
    fn check(&mut self, request: PolicyRequest<'_>) -> LiveCheckStatus {
        let Some(input) = policy::encode_request(request) else {
            return policy::unknown();
        };
        // SAFETY: the open caller retains callback code through close; input remains live for this call.
        match unsafe { (self.callback)(self.cookie, input.as_ptr(), input.len()) } {
            1 => LiveCheckStatus::Satisfied,
            0 => LiveCheckStatus::Violated {
                reason: "embedding host refused this policy request".into(),
            },
            _ => policy::unknown(),
        }
    }
}

/// Opens native execution using caller-owned policy code. No callback runs before complete configuration.
/// # Safety
/// JSON must be readable for `length` bytes. Callback code/cookie must remain valid through context close;
/// callbacks must not unwind or recursively wait for this context, and may not retain the input pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn disksgraph_json_open_with_policy(
    json: *const u8,
    length: usize,
    callback: Option<Callback>,
    cookie: u64,
) -> *mut c_char {
    output(|| {
        // SAFETY: open caller guarantees the input region; only owned configuration crosses threads.
        let config = unsafe { input(json, length, 64 * 1024) };
        let result = config.and_then(|input| {
            let callback = callback
                .ok_or_else(|| Error::InvalidConfig("host policy callback required".into()))?;
            registry::open_with_host_policy(input, move || CPolicy { callback, cookie })
        });
        registry::reply(result)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{error::Error as StdError, ffi::CStr, fs, os::unix::fs::DirBuilderExt};
    /// Takes ownership of one known library reply and copies its JSON before freeing it exactly once.
    unsafe fn value(pointer: *mut c_char) -> Result<serde_json::Value, Box<dyn StdError>> {
        assert!(!pointer.is_null());
        // SAFETY: caller supplies the live NUL-terminated allocation just returned by a JSON export.
        let bytes = unsafe { CStr::from_ptr(pointer) }.to_bytes().to_vec();
        // SAFETY: this is the same unique unfreed allocation; bytes have already been copied.
        unsafe { disksgraph_json_free(pointer) };
        Ok(serde_json::from_slice(&bytes)?)
    }
    /// Parses only the borrowed callback input and returns the requested fixed test decision code.
    unsafe extern "C" fn decide(cookie: u64, pointer: *const u8, length: usize) -> i32 {
        // SAFETY: CPolicy supplies a readable slice retained until this callback returns.
        let bytes = unsafe { slice::from_raw_parts(pointer, length) };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            return -1;
        };
        if value["policy_version"] != 1
            || value["request"]["check"] != "authorize"
            || value["request"]["request"]["plan_digest"] != "a".repeat(64)
        {
            return -1;
        }
        cookie as i32
    }
    /// Real callback function pointers receive versioned exact requests and cannot invent decision codes.
    #[test]
    fn c_policy_callbacks_preserve_cookie_context_and_three_valued_status(
    ) -> Result<(), Box<dyn StdError>> {
        let request = disksgraph::host::authorization::ApprovalRequest {
            run_id: uuid::Uuid::now_v7(),
            plan_id: uuid::Uuid::now_v7(),
            action_id: disksgraph::ActionId::new(0),
            plan_digest: "a".repeat(64),
            backend: disksgraph::host::BackendId::new("callback-fixture").ok_or("backend")?,
            targets: Vec::new(),
            observation: disksgraph::ObservationReference {
                probe_instance: uuid::Uuid::now_v7(),
                epoch: disksgraph::model::ProbeEpoch::new(1),
            },
        };
        for code in [0, 1, 2] {
            let mut policy = CPolicy {
                callback: decide,
                cookie: code,
            };
            let status = policy.check(PolicyRequest::Authorize {
                request: &request,
                destructive_target: None,
            });
            assert_eq!(status == LiveCheckStatus::Satisfied, code == 1);
            assert_eq!(
                matches!(status, LiveCheckStatus::Violated { .. }),
                code == 0
            );
            assert_eq!(matches!(status, LiveCheckStatus::Unknown { .. }), code == 2);
        }
        Ok(())
    }

    /// Real C signatures share the registry lifecycle, bounded input and diagnostic contracts.
    #[test]
    fn c_json_exports_own_buffers_and_close_the_same_worker() -> Result<(), Box<dyn StdError>> {
        let root = std::env::temp_dir().join(format!("disksgraph-c-api-{}", uuid::Uuid::now_v7()));
        fs::DirBuilder::new().mode(0o700).create(&root)?;
        let config =
            serde_json::json!({"abi_version":1,"protocol_version":1,"root":root}).to_string();
        // SAFETY: config bytes remain readable throughout open; value consumes its unique returned buffer.
        let opened = unsafe { value(disksgraph_json_open(config.as_ptr(), config.len())) }?;
        let context = opened["result"]["context"].as_u64().ok_or("context")?;
        let request = br#"{"protocol_version":1,"request":{"method":"SECRET-method","params":{}}}"#;
        // SAFETY: request is a readable static byte slice and value owns the sole returned allocation.
        let rejected = unsafe {
            value(disksgraph_json_call(
                context,
                request.as_ptr(),
                request.len(),
            ))
        }?;
        assert_eq!(rejected["error"]["code"], "invalid_request");
        assert!(!rejected.to_string().contains("SECRET"));
        // SAFETY: config is live, None is a deliberately rejected nullable callback, reply is uniquely owned.
        let no_policy = unsafe {
            value(disksgraph_json_open_with_policy(
                config.as_ptr(),
                config.len(),
                None,
                0,
            ))
        }?;
        assert_eq!(no_policy["error"]["code"], "invalid_config");
        // SAFETY: value receives the unique allocation returned by close.
        let closed = unsafe { value(disksgraph_json_close(context)) }?;
        assert!(closed.get("error").is_none());
        assert!(!root.join("active-attempt.json").exists());
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
