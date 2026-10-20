#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

//! Independent checked-import boundary; the consumer never calls the linked Rust export functions.

use libloading::Library;
use serde_json::Value;
use stabby::{
    libloading::{StabbyLibrary, Symbol},
    string::String as StableString,
    IStable,
};
use std::{cell::RefCell, path::Path};

/// Test/consumer errors preserve checked-import diagnostics without matching human text.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Keeps the library alive until all owned responses and contexts have been released.
pub struct Api {
    /// None is used only when a failed close requires intentionally retaining the mapped library.
    library: Option<Library>,
    /// Open handles owned by this consumer, including during unwinding after an assertion failure.
    contexts: RefCell<Vec<u64>>,
    /// Explicit trusted symbol namespace: production or separately built guarded VM artifact.
    namespace: String,
}

impl Api {
    /// Loads only the locally built test cdylib selected by the integration harness.
    pub fn load(path: &Path, namespace: &str) -> Result<Self, Error> {
        // SAFETY: the harness supplies its own trusted build output. This owner keeps the mapping
        // alive through every checked call, response destructor and joined context shutdown.
        let library = unsafe { Library::new(path)? };
        Ok(Self {
            library: Some(library),
            contexts: RefCell::new(Vec::new()),
            namespace: namespace.into(),
        })
    }

    /// Stabby reflection checks every requested signature; returned symbols borrow this mapping.
    fn symbol<T: IStable>(&self, name: &str) -> Result<Symbol<'_, T>, Error> {
        let library = self.library.as_ref().ok_or("library is closed")?;
        // SAFETY: only the fixed signatures below are requested from the trusted test build.
        // get_stabbied rejects incompatible reports before returning a callable symbol.
        unsafe { library.get_stabbied(format!("{}_{}", self.namespace, name).as_bytes()) }
    }

    /// Proves reflection rejects a same-name symbol with an incompatible return representation.
    pub fn rejects_wrong_signature(&self) -> bool {
        self.symbol::<extern "C" fn() -> u64>("api_version")
            .is_err()
    }

    /// Reads the independent ABI signature version through the checked import.
    pub fn version(&self) -> Result<u32, Error> {
        Ok(self.symbol::<extern "C" fn() -> u32>("api_version")?())
    }

    /// Converts and drops every library-allocated response before returning ordinary consumer JSON.
    fn value(response: StableString) -> Result<Value, Error> {
        let value = serde_json::from_str(response.as_str())?;
        drop(response);
        Ok(value)
    }

    /// Records successfully opened owners for panic-safe cleanup before unloading.
    pub fn open(&self, config: &Value) -> Result<Value, Error> {
        let function = self.symbol::<extern "C" fn(StableString) -> StableString>("open")?;
        let value = Self::value(function(StableString::from(config.to_string().as_str())))?;
        if let Some(id) = value["result"]["context"].as_u64() {
            self.contexts.borrow_mut().push(id);
        }
        Ok(value)
    }

    /// Supplies policy through a checked function pointer, never through ordinary request JSON.
    /// The callback and its cookie remain valid until this context is closed and joined.
    pub fn open_with_policy(
        &self,
        config: &Value,
        callback: extern "C" fn(u64, StableString) -> StableString,
        cookie: u64,
    ) -> Result<Value, Error> {
        let function = self.symbol::<extern "C" fn(
            StableString,
            extern "C" fn(u64, StableString) -> StableString,
            u64,
        ) -> StableString>("open_with_policy")?;
        let value = Self::value(function(
            StableString::from(config.to_string().as_str()),
            callback,
            cookie,
        ))?;
        if let Some(id) = value["result"]["context"].as_u64() {
            self.contexts.borrow_mut().push(id);
        }
        Ok(value)
    }

    /// Sends bounded application JSON using stable owned strings in both directions.
    pub fn call(&self, context: u64, request: &Value) -> Result<Value, Error> {
        let function = self.symbol::<extern "C" fn(u64, StableString) -> StableString>("call")?;
        Self::value(function(
            context,
            StableString::from(request.to_string().as_str()),
        ))
    }

    /// Executes one version-1 method through the same imported function as malformed-version tests.
    pub fn method(&self, context: u64, method: &str, params: Value) -> Result<Value, Error> {
        self.call(
            context,
            &serde_json::json!({"protocol_version":1,"request":{"method":method,"params":params}}),
        )
    }

    /// Joins the owner and removes only successfully closed handles from this consumer's cleanup list.
    pub fn close(&self, context: u64) -> Result<Value, Error> {
        let value = Self::value(self
            .symbol::<extern "C" fn(u64) -> StableString>("close")?(
            context
        ))?;
        if value.get("error").is_none() {
            self.contexts.borrow_mut().retain(|id| *id != context);
        }
        Ok(value)
    }
}

impl Drop for Api {
    /// Assertion unwinding must not unload code while its worker or allocation callbacks remain live.
    fn drop(&mut self) {
        let contexts = std::mem::take(self.contexts.get_mut());
        let mut closed = true;
        for id in contexts {
            closed &= self
                .close(id)
                .is_ok_and(|reply| reply.get("error").is_none());
        }
        if !closed {
            // Retaining a failed library mapping is safer than unloading a potentially active owner.
            if let Some(library) = self.library.take() {
                std::mem::forget(library);
            }
        }
    }
}
