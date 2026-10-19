//! Bounded process-local context ownership; no application objects cross the dynamic-library boundary.

use disksgraph::wire::{Protocol, MAX_REQUEST_BYTES, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
        Arc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
};

/// Bound includes contexts still opening or closing, not just entries visible to callers.
const MAX_CONTEXTS: usize = 32;
/// Native configuration is far smaller than a graph document and has its own pre-parse bound.
const MAX_CONFIG_BYTES: usize = 64 * 1024;
/// Never reused, including after failed initialization; zero is always invalid.
static NEXT_CONTEXT: AtomicU64 = AtomicU64::new(1);
/// Counts resources outside the registry lock while providers initialize or workers join.
static SLOTS: AtomicUsize = AtomicUsize::new(0);
/// Shared lookup contains channels and joins only; the non-Send facade stays on its owning worker.
static CONTEXTS: OnceLock<Mutex<HashMap<u64, Arc<Mutex<Context>>>>> = OnceLock::new();

/// Stable transport errors, separate from the application's own diagnostic codes.
#[derive(Debug, Serialize)]
#[serde(tag = "code", content = "detail", rename_all = "snake_case")]
pub(crate) enum Error {
    /// Invalid or unknown configuration fields.
    InvalidConfig(String),
    /// The checked signature version does not match the caller's intended API contract.
    UnsupportedAbiVersion(u64),
    /// Future JSON versions cannot silently become old method calls.
    UnsupportedVersion(u64),
    /// Request size or context/sequence capacity was exhausted.
    ResourceLimit,
    /// Handle is closed, unknown or was never successfully opened.
    UnknownContext,
    /// A concurrent call owns this context; there is no unbounded request queue.
    Busy,
    /// Provider/root configuration or thread startup failed.
    OpenFailed(String),
    /// The owner stopped and cannot safely continue processing requests.
    Stopped,
    /// Panic contained inside this library; the failed owner is never resumed automatically.
    Panicked,
}

/// Open inputs select trusted reads and an existing root, never principal approval or a write backend.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    /// Required independent signature version.
    abi_version: u32,
    /// Required application message version.
    protocol_version: u32,
    /// Existing owner-only directory shared with cooperating applications.
    root: PathBuf,
    /// None explicitly selects saved-input-only operation.
    #[serde(default)]
    manifest: Option<PathBuf>,
}

/// One counted owner; all error and close paths release the reservation after joining its worker.
struct Slot;
impl Slot {
    /// Reserves before loading libraries or creating another worker.
    fn acquire() -> Result<Self, Error> {
        SLOTS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_CONTEXTS).then_some(count + 1)
            })
            .map(|_| Self)
            .map_err(|_| Error::ResourceLimit)
    }
}
impl Drop for Slot {
    /// Closing the last handle frees capacity, but never clears an incident checkpoint.
    fn drop(&mut self) {
        SLOTS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Bounded worker messages; response channels belong to one synchronous call.
enum Message {
    /// Process one bounded application JSON document.
    Call(String, SyncSender<String>),
    /// Drop facade/native registrations on their owner thread and exit.
    Stop,
}

/// Keeps the worker and every registration alive until explicit close or failed-open cleanup.
struct Context {
    /// Channel never queues more than one pending message.
    sender: SyncSender<Message>,
    /// Joined before the context's reservation can be released.
    worker: Option<JoinHandle<()>>,
    /// Counts live resources even after removal from the public lookup.
    _slot: Slot,
}
impl Context {
    /// Stops and joins exactly once, even if another caller temporarily owns an Arc to this entry.
    fn shutdown(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = self.sender.send(Message::Stop);
            let _ = worker.join();
        }
    }
}
impl Drop for Context {
    /// A synchronous operation completes before shutdown; there is no unsafe cancellation or detached work.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Wraps transport results with the same protocol version as ordinary application responses.
pub(crate) fn reply(result: Result<Value, Error>) -> String {
    let value = match result {
        Ok(result) => json!({"protocol_version":PROTOCOL_VERSION,"result":result}),
        Err(error) => json!({"protocol_version":PROTOCOL_VERSION,"error":error}),
    };
    value.to_string()
}

/// Validates both versions before native configuration, thread creation or private-root access.
fn config(input: &str) -> Result<Config, Error> {
    if input.len() > MAX_CONFIG_BYTES {
        return Err(Error::ResourceLimit);
    }
    let value: Value =
        serde_json::from_str(input).map_err(|error| Error::InvalidConfig(error.to_string()))?;
    for (field, expected) in [
        ("abi_version", crate::ABI_VERSION),
        ("protocol_version", PROTOCOL_VERSION),
    ] {
        let version = value
            .get(field)
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::InvalidConfig(format!("{field} must be an unsigned integer")))?;
        if version != u64::from(expected) {
            return Err(if field == "abi_version" {
                Error::UnsupportedAbiVersion(version)
            } else {
                Error::UnsupportedVersion(version)
            });
        }
    }
    let config: Config =
        serde_json::from_value(value).map_err(|error| Error::InvalidConfig(error.to_string()))?;
    debug_assert_eq!(
        (config.abi_version, config.protocol_version),
        (crate::ABI_VERSION, PROTOCOL_VERSION)
    );
    Ok(config)
}

/// Creates and owns the facade entirely on one thread, avoiding Send assertions and self-referential data.
fn worker(config: Config, requests: mpsc::Receiver<Message>, ready: SyncSender<Result<(), Error>>) {
    let initialized = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let live = config.manifest.is_some();
        disksgraph::native::open(&config.root, config.manifest.as_deref())
            .map(|app| Protocol::new(app, live))
            .map_err(|error| Error::OpenFailed(error.to_string()))
    }));
    let mut protocol = match initialized {
        Ok(Ok(protocol)) => protocol,
        Ok(Err(error)) => {
            let _ = ready.send(Err(error));
            return;
        }
        Err(payload) => {
            std::mem::forget(payload);
            let _ = ready.send(Err(Error::Panicked));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        return;
    }
    while let Ok(Message::Call(input, response)) = requests.recv() {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| protocol.call_json(&input)))
        {
            Ok(result) => {
                let _ = response.send(result);
            }
            Err(payload) => {
                // A payload's destructor is user code too; never permit a second panic across FFI.
                std::mem::forget(payload);
                let _ = response.send(reply(Err(Error::Panicked)));
                break;
            }
        }
    }
}

/// Opens at most MAX_CONTEXTS owners, with no global lock held during native loading or probing.
pub(crate) fn open(input: &str) -> Result<Value, Error> {
    let config = config(input)?;
    let live = config.manifest.is_some();
    let slot = Slot::acquire()?;
    let id = NEXT_CONTEXT
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .map_err(|_| Error::ResourceLimit)?;
    let (sender, requests) = mpsc::sync_channel(1);
    let (ready, initialized) = mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name(format!("disksgraph-{id}"))
        .spawn(move || worker(config, requests, ready))
        .map_err(|error| Error::OpenFailed(error.to_string()))?;
    let context = Context {
        sender,
        worker: Some(worker),
        _slot: slot,
    };
    initialized.recv().map_err(|_| Error::Stopped)??;
    let mut contexts = CONTEXTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| Error::Stopped)?;
    contexts.insert(id, Arc::new(Mutex::new(context)));
    Ok(json!({"context":id,"abi_version":crate::ABI_VERSION,"mode":"synchronous","live_host":live}))
}

/// Forwards to the sole owner; try_lock bounds concurrent demand without blocking unrelated contexts.
pub(crate) fn call(id: u64, input: &str) -> Result<String, Error> {
    if input.len() > MAX_REQUEST_BYTES {
        return Err(Error::ResourceLimit);
    }
    let context = CONTEXTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| Error::Stopped)?
        .get(&id)
        .cloned()
        .ok_or(Error::UnknownContext)?;
    let context = context.try_lock().map_err(|error| match error {
        std::sync::TryLockError::WouldBlock => Error::Busy,
        std::sync::TryLockError::Poisoned(_) => Error::Stopped,
    })?;
    if context.worker.is_none() {
        return Err(Error::UnknownContext);
    }
    let (response, result) = mpsc::sync_channel(1);
    context
        .sender
        .send(Message::Call(input.into(), response))
        .map_err(|_| Error::Stopped)?;
    result.recv().map_err(|_| Error::Stopped)
}

/// Removes the handle before joining; stale or racing calls can never acquire a replacement owner.
pub(crate) fn close(id: u64) -> Result<Value, Error> {
    let context = {
        let mut contexts = CONTEXTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .map_err(|_| Error::Stopped)?;
        let context = contexts.remove(&id).ok_or(Error::UnknownContext)?;
        if contexts.is_empty() {
            *contexts = HashMap::new();
        }
        context
    };
    // Poison cannot permit more work, but still requires joining before the .so may be unloaded.
    context
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .shutdown();
    Ok(json!({"closed":id}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{error::Error as StdError, fs, os::unix::fs::DirBuilderExt};

    /// Registry counters are process-wide; keep tests from consuming each other's bounded slots.
    static TESTS: Mutex<()> = Mutex::new(());

    /// Private test root, never a block endpoint or a native write target.
    struct Scratch(PathBuf);
    impl Scratch {
        /// Creates only a new owner-only directory for this test process.
        fn new() -> std::io::Result<Self> {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            let path = std::env::temp_dir().join(format!(
                "disksgraph-ffi-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&path)?;
            Ok(Self(path))
        }
        /// Uses saved-input-only mode so native provider availability cannot affect unit tests.
        fn config(&self) -> String {
            json!({"abi_version":1,"protocol_version":1,"root":self.0}).to_string()
        }
    }
    impl Drop for Scratch {
        /// Removes only this test's new private directory after its workers have been joined.
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Failed opens release capacity; close joins even while another caller still owns the entry's Arc.
    #[test]
    fn context_lifecycle_never_reuses_handles_or_detaches_workers() -> Result<(), Box<dyn StdError>>
    {
        let _serial = TESTS.lock().unwrap_or_else(|error| error.into_inner());
        let scratch = Scratch::new()?;
        let invalid = json!({"abi_version":1,"protocol_version":1,"root":scratch.0.join("absent")})
            .to_string();
        assert!(matches!(open(&invalid), Err(Error::OpenFailed(_))));
        assert_eq!(SLOTS.load(Ordering::Acquire), 0);
        let id = open(&scratch.config()).map_err(|error| format!("{error:?}"))?["context"]
            .as_u64()
            .ok_or("context")?;
        let retained = CONTEXTS
            .get()
            .ok_or("registry")?
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&id)
            .cloned()
            .ok_or("owner")?;
        close(id).map_err(|error| format!("{error:?}"))?;
        assert!(retained
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .worker
            .is_none());
        assert!(matches!(call(id, "{}"), Err(Error::UnknownContext)));
        assert!(matches!(close(id), Err(Error::UnknownContext)));
        drop(retained);
        assert_eq!(SLOTS.load(Ordering::Acquire), 0);
        let next = open(&scratch.config()).map_err(|error| format!("{error:?}"))?["context"]
            .as_u64()
            .ok_or("next")?;
        assert!(next > id);
        close(next).map_err(|error| format!("{error:?}"))?;
        Ok(())
    }

    /// Bound checks and version negotiation precede thread creation and any native configuration.
    #[test]
    fn context_capacity_and_versions_fail_closed() -> Result<(), Box<dyn StdError>> {
        let _serial = TESTS.lock().unwrap_or_else(|error| error.into_inner());
        let scratch = Scratch::new()?;
        assert!(matches!(
            open(&json!({"abi_version":99}).to_string()),
            Err(Error::UnsupportedAbiVersion(99))
        ));
        assert!(matches!(
            open(&json!({"abi_version":1,"protocol_version":99}).to_string()),
            Err(Error::UnsupportedVersion(99))
        ));
        assert!(matches!(
            open(&" ".repeat(MAX_CONFIG_BYTES + 1)),
            Err(Error::ResourceLimit)
        ));
        let mut ids = Vec::new();
        for _ in 0..MAX_CONTEXTS {
            ids.push(
                open(&scratch.config()).map_err(|error| format!("{error:?}"))?["context"]
                    .as_u64()
                    .ok_or("context")?,
            );
        }
        assert!(matches!(open(&scratch.config()), Err(Error::ResourceLimit)));
        assert!(matches!(
            call(ids[0], &" ".repeat(MAX_REQUEST_BYTES + 1)),
            Err(Error::ResourceLimit)
        ));
        for id in ids {
            close(id).map_err(|error| format!("{error:?}"))?;
        }
        assert_eq!(SLOTS.load(Ordering::Acquire), 0);
        Ok(())
    }
}
