//! Bounded process-local context ownership; no application objects cross the dynamic-library boundary.

use crate::jobs::Job;
use disksgraph::wire::{
    encode_reply, parse_request, Protocol, ProtocolError, Request, MAX_REQUEST_BYTES,
    PROTOCOL_VERSION,
};
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
pub enum Error {
    /// Invalid or unknown configuration fields.
    InvalidConfig(#[serde(serialize_with = "disksgraph::diagnostics::serialize_message")] String),
    /// The checked signature version does not match the caller's intended API contract.
    UnsupportedAbiVersion(u64),
    /// Future JSON versions cannot silently become old method calls.
    UnsupportedVersion(u64),
    /// Request size or context/sequence capacity was exhausted.
    ResourceLimit,
    /// Handle is closed, unknown or was never successfully opened.
    UnknownContext,
    /// Job handle is absent from this context, including after explicit mailbox release.
    UnknownJob,
    /// Cursor refers to events the job has not published.
    InvalidEventCursor,
    /// A concurrent call owns this context; there is no unbounded request queue.
    Busy,
    /// Provider/root configuration or thread startup failed.
    OpenFailed(#[serde(serialize_with = "disksgraph::diagnostics::serialize_message")] String),
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
    Call(Box<Request>, SyncSender<String>),
    /// Execute on the same owner and publish passive evidence without holding the context lock.
    Apply(disksgraph::PlanId, Arc<Job>),
    /// Drop facade/native registrations on their owner thread and exit.
    Stop,
}

/// Keeps the worker and every registration alive until explicit close or failed-open cleanup.
struct Context {
    /// Channel never queues more than one pending message.
    sender: SyncSender<Message>,
    /// Joined before the context's reservation can be released.
    worker: Option<JoinHandle<()>>,
    /// Explicit configuration allows saved-only start rejection before allocating a job.
    live: bool,
    /// Exactly one retained mailbox; terminal jobs must be explicitly released before another start.
    job: Option<Arc<Job>>,
    /// Counts live resources even after removal from the public lookup.
    _slot: Slot,
}
impl Context {
    /// Enforces instance ownership before reading or releasing any mailbox.
    fn job(&self, id: u64) -> Result<&Arc<Job>, Error> {
        self.job
            .as_ref()
            .filter(|job| job.id == id)
            .ok_or(Error::UnknownJob)
    }
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
pub fn reply(result: Result<Value, Error>) -> String {
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

/// Owns the bounded transport endpoints; a host factory serves its non-Send facade on this thread.
/// This Rust object never crosses the ABI; the factory must keep registrations alive through serve.
pub struct Worker {
    /// Exactly one pending command, independent of passive job polling.
    requests: mpsc::Receiver<Message>,
    /// Handshake succeeds only once the host facade is constructed.
    ready: SyncSender<Result<(), Error>>,
    /// Explicit configuration, never proof of native readiness or authorization.
    live: bool,
}
impl Worker {
    /// Runs the shared protocol/job loop with a trusted configured host, borrowing its native owners.
    /// All facade objects drop before this returns; close additionally joins the enclosing factory.
    pub fn serve<C, A, D>(
        self,
        app: disksgraph::DisksGraph<disksgraph::host::host::LinuxLiveHost<'_, C, A>, D>,
    ) where
        C: disksgraph::host::capability::BackendCapabilityCheck,
        A: disksgraph::host::authorization::AuthorizationContext,
        D: disksgraph::host::dispatcher::BackendDispatcher,
    {
        self.serve_with_policy(app, disksgraph::native::ReadOnlyPolicy);
    }

    /// Keeps incident safety on the same host-owned policy channel as native admission.
    pub fn serve_with_policy<C, A, D, P>(
        self,
        app: disksgraph::DisksGraph<disksgraph::host::host::LinuxLiveHost<'_, C, A>, D>,
        mut policy: P,
    ) where
        C: disksgraph::host::capability::BackendCapabilityCheck,
        A: disksgraph::host::authorization::AuthorizationContext,
        D: disksgraph::host::dispatcher::BackendDispatcher,
        P: disksgraph::IncidentAcknowledgmentPolicy,
    {
        let mut protocol = Protocol::new(app, self.live);
        if self.ready.send(Ok(())).is_err() {
            return;
        }
        while let Ok(message) = self.requests.recv() {
            match message {
                Message::Stop => break,
                Message::Call(input, response) => {
                    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        encode_reply(protocol.call_with_policy(*input, &mut policy))
                    })) {
                        Ok(result) => {
                            let _ = response.send(result);
                        }
                        Err(payload) => {
                            std::mem::forget(payload);
                            let _ = response.send(reply(Err(Error::Panicked)));
                            break;
                        }
                    }
                }
                Message::Apply(plan, job) => {
                    if !execute_job(&job, || {
                        protocol.apply_plan_with_updates(plan, &mut |snapshot, events| {
                            job.update(snapshot, events)
                        })
                    }) {
                        break;
                    }
                }
            }
        }
    }
}

/// Contains host initialization failures on the owner thread, including before the ready handshake.
fn worker(
    config: Config,
    requests: mpsc::Receiver<Message>,
    ready: SyncSender<Result<(), Error>>,
    initialize: impl FnOnce(PathBuf, Option<PathBuf>, Worker) -> Result<(), Error>,
) {
    let live = config.manifest.is_some();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        initialize(
            config.root,
            config.manifest,
            Worker {
                requests,
                ready: ready.clone(),
                live,
            },
        )
    }));
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            let _ = ready.send(Err(error));
        }
        Err(payload) => {
            std::mem::forget(payload);
            let _ = ready.send(Err(Error::Panicked));
        }
    }
}

/// Contains a job panic before publishing terminal failure; a panicked owner must stop processing.
fn execute_job(job: &Job, action: impl FnOnce() -> Result<Value, ProtocolError>) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(action)) {
        Ok(result) => {
            job.finish(result.err().map(|error| json!(error)));
            true
        }
        Err(payload) => {
            std::mem::forget(payload);
            job.finish(Some(json!({"code":"panicked"})));
            false
        }
    }
}

/// Opens at most MAX_CONTEXTS owners, with no global lock held during native loading or probing.
pub(crate) fn open(input: &str) -> Result<Value, Error> {
    open_with(input, |root, manifest, worker| {
        let app = disksgraph::native::open(&root, manifest.as_deref())
            .map_err(|error| Error::OpenFailed(error.to_string()))?;
        worker.serve(app);
        Ok(())
    })
}

/// Shares native owner construction between Stabby and C-compatible policy entry points.
pub(crate) fn open_with_host_policy<P: disksgraph::native::NativeHostPolicy + 'static>(
    input: &str,
    make_policy: impl FnOnce() -> P + Send + 'static,
) -> Result<Value, Error> {
    open_with(input, move |root, manifest, worker| {
        let manifest = manifest.ok_or_else(|| {
            Error::InvalidConfig("native manifest required for host policy".into())
        })?;
        let policy = disksgraph::native::HostPolicy::new(make_policy());
        let app = disksgraph::native::open_with_policy(&root, &manifest, policy.clone())
            .map_err(|error| Error::OpenFailed(error.to_string()))?;
        worker.serve_with_policy(app, policy);
        Ok(())
    })
}

/// Opens a context using an explicit trusted Rust host factory, never selected from request JSON.
/// The factory runs on its worker; it must call serve before returning and keep its host owners scoped.
/// Dropped factories, failed initialization and panics release the reservation after joining.
pub fn open_with(
    input: &str,
    initialize: impl FnOnce(PathBuf, Option<PathBuf>, Worker) -> Result<(), Error> + Send + 'static,
) -> Result<Value, Error> {
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
        .spawn(move || worker(config, requests, ready, initialize))
        .map_err(|error| Error::OpenFailed(error.to_string()))?;
    let context = Context {
        sender,
        worker: Some(worker),
        _slot: slot,
        live,
        job: None,
    };
    initialized.recv().map_err(|_| Error::Stopped)??;
    let mut contexts = CONTEXTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| Error::Stopped)?;
    contexts.insert(id, Arc::new(Mutex::new(context)));
    Ok(
        json!({"context":id,"abi_version":crate::ABI_VERSION,"mode":"background_jobs","live_host":live}),
    )
}

/// Forwards to the sole owner; try_lock bounds concurrent demand without blocking unrelated contexts.
pub fn call(id: u64, input: &str) -> Result<String, Error> {
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
    let mut context = context.try_lock().map_err(|error| match error {
        std::sync::TryLockError::WouldBlock => Error::Busy,
        std::sync::TryLockError::Poisoned(_) => Error::Stopped,
    })?;
    if context.worker.is_none() {
        return Err(Error::UnknownContext);
    }
    let request = match parse_request(input) {
        Ok(request) => request,
        Err(error) => return Ok(encode_reply(Err(error))),
    };
    match request {
        Request::JobPoll { job, since } => return Ok(reply(context.job(job)?.poll(since))),
        Request::JobClose { job } => {
            if !context.job(job)?.finished()? {
                return Err(Error::Busy);
            }
            context.job = None;
            return Ok(reply(Ok(Value::Null)));
        }
        Request::JobCancel { job } => {
            context.job(job)?;
            return Ok(encode_reply(Err(ProtocolError::UnsupportedOperation(
                "native invocation cancellation is unsupported; close waits for completion".into(),
            ))));
        }
        Request::ApplyStart { plan } => {
            if !context.live {
                return Ok(encode_reply(Err(ProtocolError::LiveUnavailable)));
            }
            if let Some(job) = &context.job {
                return Err(if job.finished()? {
                    Error::ResourceLimit
                } else {
                    Error::Busy
                });
            }
            let job = Arc::new(Job::new()?);
            context
                .sender
                .send(Message::Apply(plan, job.clone()))
                .map_err(|_| Error::Stopped)?;
            let id = job.id;
            context.job = Some(job);
            return Ok(reply(Ok(json!({"job":id}))));
        }
        _ => {
            if let Some(job) = &context.job {
                if !job.finished()? {
                    return Err(Error::Busy);
                }
            }
        }
    }
    let (response, result) = mpsc::sync_channel(1);
    context
        .sender
        .send(Message::Call(Box::new(request), response))
        .map_err(|_| Error::Stopped)?;
    result.recv().map_err(|_| Error::Stopped)
}

/// Removes the handle before joining; stale or racing calls can never acquire a replacement owner.
pub fn close(id: u64) -> Result<Value, Error> {
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

    /// Transport-owned parse/loader diagnostics are opaque even before the facade exists.
    #[test]
    fn transport_error_json_omits_raw_configuration_context() -> Result<(), serde_json::Error> {
        for error in [
            Error::InvalidConfig("SECRET-request-echo".into()),
            Error::OpenFailed("SECRET-native-path".into()),
        ] {
            let value = serde_json::to_value(&error)?;
            assert_eq!(value["detail"], disksgraph::diagnostics::REDACTED);
            assert!(!value.to_string().contains("SECRET"));
        }
        Ok(())
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
    /// A channel-blocked synthetic owner proves polling is independent and close waits for quiescence.
    #[test]
    fn active_job_polling_and_close_do_not_detach_the_owner() -> Result<(), Box<dyn StdError>> {
        use std::time::{Duration, Instant};
        let _serial = TESTS.lock().unwrap_or_else(|error| error.into_inner());
        let slot = Slot::acquire().map_err(|error| format!("{error:?}"))?;
        let id = NEXT_CONTEXT.fetch_add(1, Ordering::Relaxed);
        let (sender, requests) = mpsc::sync_channel(1);
        let (entered, started) = mpsc::sync_channel(1);
        let (release, blocked) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            if let Ok(Message::Apply(_, job)) = requests.recv() {
                let _ = entered.send(job.clone());
                let _ = blocked.recv_timeout(Duration::from_secs(10));
                execute_job(&job, || Ok(Value::Null));
            }
            let _ = requests.recv();
        });
        CONTEXTS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                id,
                Arc::new(Mutex::new(Context {
                    sender,
                    worker: Some(worker),
                    _slot: slot,
                    live: true,
                    job: None,
                })),
            );
        let request = |method: &str, params: Value| {
            json!({"protocol_version":1,"request":{"method":method,"params":params}}).to_string()
        };
        let plan = json!("00000000-0000-7000-8000-000000000001");
        let response: Value = serde_json::from_str(
            &call(id, &request("apply_start", json!({"plan":plan})))
                .map_err(|error| format!("{error:?}"))?,
        )?;
        let job = started.recv_timeout(Duration::from_secs(10))?;
        assert_eq!(response["result"]["job"], job.id);
        let polled: Value = serde_json::from_str(
            &call(id, &request("job_poll", json!({"job":job.id,"since":0})))
                .map_err(|error| format!("{error:?}"))?,
        )?;
        assert_eq!(polled["result"]["finished"], false);
        assert!(matches!(
            call(id, &request("probe", json!({}))),
            Err(Error::Busy)
        ));
        assert!(matches!(
            call(id, &request("apply_start", json!({"plan":plan}))),
            Err(Error::Busy)
        ));
        assert!(matches!(
            call(id, &request("job_close", json!({"job":job.id}))),
            Err(Error::Busy)
        ));
        assert!(matches!(
            call(id, &request("job_poll", json!({"job":job.id+1,"since":0}))),
            Err(Error::UnknownJob)
        ));
        let cancellation: Value = serde_json::from_str(
            &call(id, &request("job_cancel", json!({"job":job.id})))
                .map_err(|error| format!("{error:?}"))?,
        )?;
        assert_eq!(cancellation["error"]["code"], "unsupported_operation");
        assert!(!job.finished().map_err(|error| format!("{error:?}"))?);
        let (done, completion) = mpsc::sync_channel(1);
        let closer = thread::spawn(move || {
            let _ = done.send(close(id));
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while CONTEXTS
            .get()
            .ok_or("registry")?
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(&id)
        {
            if Instant::now() >= deadline {
                return Err("close did not remove handle".into());
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(matches!(
            completion.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert!(matches!(call(id, "{}"), Err(Error::UnknownContext)));
        release.send(())?;
        completion
            .recv_timeout(Duration::from_secs(10))?
            .map_err(|error| format!("{error:?}"))?;
        closer.join().map_err(|_| "close panic")?;
        assert!(job.finished().map_err(|error| format!("{error:?}"))?);
        assert_eq!(SLOTS.load(Ordering::Acquire), 0);
        Ok(())
    }

    /// Panic ends the background owner rather than leaving a permanently active or successful job.
    #[test]
    fn job_panic_is_terminal_and_pre_run_errors_remain_visible() -> Result<(), String> {
        let job = Job::new().map_err(|error| format!("{error:?}"))?;
        assert!(!execute_job(&job, || panic!("controlled job panic")));
        assert_eq!(
            job.poll(0).map_err(|error| format!("{error:?}"))?["error"]["code"],
            "panicked"
        );
        let failed = Job::new().map_err(|error| format!("{error:?}"))?;
        assert!(execute_job(&failed, || Err(ProtocolError::LiveUnavailable)));
        let result = failed.poll(0).map_err(|error| format!("{error:?}"))?;
        assert_eq!(result["finished"], true);
        assert_eq!(result["snapshot"], Value::Null);
        assert_eq!(result["error"]["code"], "live_unavailable");
        Ok(())
    }
    /// Factories cannot bypass negotiation; close joins their scoped owners and all failure paths release slots.
    #[test]
    fn trusted_factory_lifetime_is_scoped_to_the_joined_owner() -> Result<(), Box<dyn StdError>> {
        use std::sync::atomic::AtomicBool;
        let _serial = TESTS.lock().unwrap_or_else(|error| error.into_inner());
        let scratch = Scratch::new()?;
        assert!(matches!(
            open_with("{\"abi_version\":99}", |_, _, _| panic!(
                "must not initialize"
            )),
            Err(Error::UnsupportedAbiVersion(99))
        ));
        assert!(matches!(
            open_with(&scratch.config(), |_, _, _| Ok(())),
            Err(Error::Stopped)
        ));
        assert!(matches!(
            open_with(&scratch.config(), |_, _, _| panic!(
                "controlled factory failure"
            )),
            Err(Error::Panicked)
        ));
        assert_eq!(SLOTS.load(Ordering::Acquire), 0);
        let dropped = Arc::new(AtomicBool::new(false));
        let after = dropped.clone();
        let opened = open_with(&scratch.config(), move |root, manifest, worker| {
            let app = disksgraph::native::open(&root, manifest.as_deref())
                .map_err(|error| Error::OpenFailed(error.to_string()))?;
            worker.serve(app);
            after.store(true, Ordering::Release);
            Ok(())
        })
        .map_err(|error| format!("{error:?}"))?;
        let id = opened["context"].as_u64().ok_or("context")?;
        assert!(!dropped.load(Ordering::Acquire));
        close(id).map_err(|error| format!("{error:?}"))?;
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(SLOTS.load(Ordering::Acquire), 0);
        Ok(())
    }
}
