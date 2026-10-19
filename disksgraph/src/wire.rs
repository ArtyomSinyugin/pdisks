//! Versioned application JSON, separate from provider messages and historical document schemas.

use crate::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use storage_runtime::{
    authorization::AuthorizationContext, capability::BackendCapabilityCheck, host::LinuxLiveHost,
};
use uuid::Uuid;

/// Application protocol version; owner transports may implement the job lifecycle methods.
pub const PROTOCOL_VERSION: u32 = 1;
/// Bound checked before parsing an entire application request, including nested documents.
pub const MAX_REQUEST_BYTES: usize = MAX_PLAN_JSON_BYTES;

/// One method call, with no secrets, executable strings or serialized execution permission.
#[derive(Debug, Deserialize)]
#[serde(
    tag = "method",
    content = "params",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Request {
    /// Collect configured fresh provider evidence.
    Probe {},
    /// Start editing from a fresh same-pass baseline.
    SessionCreate {},
    /// Import bounded editable historical inputs without undo history.
    SessionImport { draft: Box<DraftDocument> },
    /// Apply an existing atomic editing operation at the expected revision.
    SessionEdit {
        session: SessionId,
        revision: DraftRevision,
        edit: Box<PlanEdit>,
    },
    /// Undo requests in this living context only.
    SessionUndo {
        session: SessionId,
        revision: DraftRevision,
    },
    /// Redo requests in this living context only.
    SessionRedo {
        session: SessionId,
        revision: DraftRevision,
    },
    /// Obtain this revision's forecast.
    SessionPreview { session: SessionId },
    /// Export inputs, without replayable execution permission.
    SessionExport { session: SessionId },
    /// Export one frozen historical document; imported plans never fabricate a trusted source map.
    SessionFreeze {
        session: SessionId,
        revision: DraftRevision,
    },
    /// Prepare fresh translated inputs without modifying the active session.
    SessionPrepareRebase {
        session: SessionId,
        revision: DraftRevision,
    },
    /// Consume an instance-owned reviewed candidate at its expected source revision.
    SessionAcceptRebase {
        session: SessionId,
        revision: DraftRevision,
        review: Uuid,
    },
    /// Discard one process-owned review without changing drafts, runs or checkpoints.
    ReviewClose { review: Uuid },
    /// Release only the owning facade's editor and history.
    SessionClose { session: SessionId },
    /// Reanalyse and retain a bounded immutable plan.
    PlanImport { document: Box<PlanDocument> },
    /// Export the retained immutable description.
    PlanExport { plan: PlanId },
    /// Release only the retained description.
    PlanClose { plan: PlanId },
    /// Query exact routes without authorizing them.
    Capabilities {
        operations: Vec<host::ProviderAction>,
    },
    /// Execute synchronously through the existing coordinator and retain failed attempts.
    ApplyBlocking { plan: PlanId },
    /// Queue one application attempt on a transport that owns a background worker.
    ApplyStart { plan: PlanId },
    /// Read a passive job snapshot and bounded events without waiting for the worker.
    JobPoll { job: u64, since: u64 },
    /// Release a terminal job mailbox, independently of its retained facade run.
    JobClose { job: u64 },
    /// Explicit cancellation request; transports must reject unsupported interruption.
    JobCancel { job: u64 },
    /// Obtain complete retained evidence, independently of event eviction.
    RunSnapshot { run: RunId },
    /// Replay using the last-consumed exclusive cursor.
    RunEvents { run: RunId, since: u64 },
    /// Release only retained evidence, without clearing a checkpoint.
    RunClose { run: RunId },
    /// Inspect under the configured host's ordinary incident lock.
    IncidentInspect {},
    /// Retain the runtime's immutable incident inspection and explicit confirmation challenge.
    IncidentReview {},
    /// Consume a retained review only after exact conscious confirmation; safety remains host-owned.
    IncidentAcknowledge {
        review: Uuid,
        run_id: Uuid,
        token: Uuid,
    },
}

impl Request {
    /// Saved-input contexts cannot accidentally publish an empty probe as native evidence.
    fn needs_live_host(&self) -> bool {
        matches!(
            self,
            Self::Probe {}
                | Self::SessionCreate {}
                | Self::SessionPrepareRebase { .. }
                | Self::Capabilities { .. }
                | Self::ApplyBlocking { .. }
                | Self::ApplyStart { .. }
                | Self::IncidentInspect {}
                | Self::IncidentReview {}
                | Self::IncidentAcknowledge { .. }
        )
    }
}

/// Rejects unknown envelope fields independently of the method's nested parameters.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    /// Version validated before interpreting the method vocabulary.
    protocol_version: u32,
    /// Typed method, never a provider transport request.
    request: Request,
}

/// Stable protocol categories retain application diagnostics without parsing human messages.
#[derive(Debug, Serialize, thiserror::Error)]
#[serde(tag = "code", content = "detail", rename_all = "snake_case")]
pub enum ProtocolError {
    /// The envelope or method parameters are malformed.
    #[error("invalid application request: {0}")]
    InvalidRequest(#[serde(serialize_with = "crate::diagnostics::serialize_message")] String),
    /// A future version cannot silently be interpreted as the current protocol.
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u64),
    /// A request exceeds the transport's pre-parse bound.
    #[error("application request exceeds the size limit")]
    ResourceLimit,
    /// The context explicitly supports saved inputs only.
    #[error("native provider configuration is required for this method")]
    LiveUnavailable,
    /// The synchronous adapter has no background owner, or the requested interruption is unsupported.
    #[error("operation requires an owner transport or is unsupported: {0}")]
    UnsupportedOperation(#[serde(serialize_with = "crate::diagnostics::serialize_message")] String),
    /// Review was closed, has another kind or belongs to another protocol owner.
    #[error("review is not owned by this context")]
    UnknownReview,
    /// An existing review cannot confirm another session, revision, run or challenge.
    #[error("request does not match the retained review")]
    ReviewMismatch,
    /// The sole facade supplies detailed stable codes and instance ownership checks.
    #[error(transparent)]
    Application(#[from] DisksGraphError),
    /// A serializable application result could not be encoded.
    #[error("application serialization failed: {0}")]
    Serialization(#[serde(serialize_with = "crate::diagnostics::serialize_message")] String),
}

/// Validates size and protocol version before interpreting potentially unsupported method fields.
pub fn parse_request(input: &str) -> Result<Request, ProtocolError> {
    if input.len() > MAX_REQUEST_BYTES {
        return Err(ProtocolError::ResourceLimit);
    }
    let value: Value = serde_json::from_str(input)
        .map_err(|error| ProtocolError::InvalidRequest(error.to_string()))?;
    let version = value
        .get("protocol_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            ProtocolError::InvalidRequest("protocol_version must be an unsigned integer".into())
        })?;
    if version != u64::from(PROTOCOL_VERSION) {
        return Err(ProtocolError::UnsupportedVersion(version));
    }
    let envelope: Envelope = serde_json::from_value(value)
        .map_err(|error| ProtocolError::InvalidRequest(error.to_string()))?;
    debug_assert_eq!(envelope.protocol_version, PROTOCOL_VERSION);
    Ok(envelope.request)
}

/// Produces one unambiguous result/error envelope, including errors before an application call.
pub fn encode_reply(result: Result<Value, ProtocolError>) -> String {
    let reply = match result {
        Ok(value) => json!({"protocol_version":PROTOCOL_VERSION, "result":value}),
        Err(error) => json!({"protocol_version":PROTOCOL_VERSION, "error":error}),
    };
    serde_json::to_string(&reply).unwrap_or_else(|_| {
        "{\"protocol_version\":1,\"error\":{\"code\":\"serialization\"}}".into()
    })
}

/// Immutable process-owned candidates; returned JSON is display data, never the trusted object.
enum Review {
    /// Only the exact owning facade session may accept this translation.
    Rebase {
        /// Facade handle, independent of the imported document's source session UUID.
        session: SessionId,
        /// Existing engine candidate retains its private source context checks.
        candidate: Box<RebasePreview>,
    },
    /// The existing runtime owner binds exact marker, fresh observations and private root inode.
    Incident(Box<IncidentReview>),
}

/// Owns exactly one ordinary facade; JSON cannot select another host or reconstruct a live run.
pub struct Protocol<H, D> {
    /// The same editor, plan registry, coordinator and evidence retention as direct Rust callers.
    app: DisksGraph<H, D>,
    /// Explicit host configuration, not an attestation of coverage or authorization.
    live_host: bool,
    /// Combined bounded registry; releasing a context discards candidates without accepting them.
    reviews: HashMap<Uuid, Review>,
}

impl<C: BackendCapabilityCheck, A: AuthorizationContext, D: BackendDispatcher>
    Protocol<LinuxLiveHost<'_, C, A>, D>
{
    /// Takes ownership of a configured application; false restricts calls to saved-input methods.
    pub fn new(
        app: DisksGraph<LinuxLiveHost<'_, C, A>, D>,
        live_host: bool,
    ) -> Protocol<LinuxLiveHost<'_, C, A>, D> {
        Protocol {
            app,
            live_host,
            reviews: HashMap::new(),
        }
    }
    /// Decodes and dispatches one bounded message; neither imported JSON nor a success envelope grants permission.
    pub fn call_json(&mut self, input: &str) -> String {
        encode_reply(parse_request(input).and_then(|request| self.call(request)))
    }
    /// Executes the same application path with passive updates for a transport-owned background job.
    /// The transport must serialize facade access and retain its owner until this returns.
    pub fn apply_plan_with_updates(
        &mut self,
        plan: PlanId,
        observer: &mut dyn FnMut(RunSnapshot, EventsPage),
    ) -> Result<Value, ProtocolError> {
        if !self.live_host {
            return Err(ProtocolError::LiveUnavailable);
        }
        let run = match self.app.apply_plan_with_updates(plan, observer) {
            Ok(run) => run,
            Err(error) => error.run_id.ok_or(error)?,
        };
        serde_json::to_value(self.app.run_snapshot(run)?)
            .map_err(|error| ProtocolError::Serialization(error.to_string()))
    }
    /// Bounds pending reviews before native work; failed allocation cannot consume a provider pass.
    fn reserve_review(&mut self) -> Result<(), ProtocolError> {
        if self.reviews.len() >= MAX_APPLICATION_OBJECTS {
            return Err(ProtocolError::ResourceLimit);
        }
        self.reviews
            .try_reserve(1)
            .map_err(|_| ProtocolError::ResourceLimit)
    }
    /// Delegates every implemented method to the instance-owned facade.
    pub fn call(&mut self, request: Request) -> Result<Value, ProtocolError> {
        self.call_with_policy(request, &mut native::ReadOnlyPolicy)
    }
    /// Accepts independent incident safety evidence only from a trusted Rust host, never request JSON.
    /// The default ABI/JSON entrypoint uses ReadOnlyPolicy; no confirmation token grants authorization.
    pub fn call_with_policy(
        &mut self,
        request: Request,
        policy: &mut impl IncidentAcknowledgmentPolicy,
    ) -> Result<Value, ProtocolError> {
        if request.needs_live_host() && !self.live_host {
            return Err(ProtocolError::LiveUnavailable);
        }
        let value = match request {
            Request::Probe {} => serde_json::to_value(self.app.probe()?),
            Request::SessionCreate {} => Ok(json!({"session": self.app.create_session()?})),
            Request::SessionImport { draft } => {
                Ok(json!({"session":self.app.import_draft(*draft)?}))
            }
            Request::SessionEdit {
                session,
                revision,
                edit,
            } => serde_json::to_value(self.app.edit(session, revision, *edit)?),
            Request::SessionUndo { session, revision } => {
                Ok(json!({"revision":self.app.undo(session, revision)?}))
            }
            Request::SessionRedo { session, revision } => {
                Ok(json!({"revision":self.app.redo(session, revision)?}))
            }
            Request::SessionPreview { session } => {
                serde_json::to_value(self.app.preview_document(session)?)
            }
            Request::SessionExport { session } => {
                serde_json::to_value(self.app.export_draft(session)?)
            }
            Request::SessionFreeze { session, revision } => {
                serde_json::to_value(self.app.freeze(session, revision)?.document())
            }
            Request::SessionPrepareRebase { session, revision } => {
                self.reserve_review()?;
                let candidate = self.app.prepare_rebase(session, revision)?;
                let review = Uuid::now_v7();
                let result = json!({
                    "review":review, "source_revision":candidate.source_revision(),
                    "baseline":candidate.target_baseline(), "draft":candidate.draft(),
                    "correlations":candidate.correlations(), "status":candidate.preview().status(),
                    "diagnostics":candidate.preview().diagnostics(),
                });
                self.reviews.insert(
                    review,
                    Review::Rebase {
                        session,
                        candidate: Box::new(candidate),
                    },
                );
                Ok(result)
            }
            Request::SessionAcceptRebase {
                session,
                revision,
                review,
            } => {
                let Some(Review::Rebase {
                    session: owner,
                    candidate,
                }) = self.reviews.get(&review)
                else {
                    return Err(ProtocolError::UnknownReview);
                };
                if *owner != session || candidate.source_revision() != revision {
                    return Err(ProtocolError::ReviewMismatch);
                }
                let Some(Review::Rebase { candidate, .. }) = self.reviews.remove(&review) else {
                    return Err(ProtocolError::UnknownReview);
                };
                Ok(json!({"revision":self.app.accept_rebase(session, revision, *candidate)?}))
            }
            Request::ReviewClose { review } => {
                self.reviews
                    .remove(&review)
                    .ok_or(ProtocolError::UnknownReview)?;
                Ok(Value::Null)
            }
            Request::SessionClose { session } => {
                self.app.close_session(session)?;
                self.reviews.retain(|_, review| !matches!(review, Review::Rebase { session: owner, .. } if *owner == session));
                Ok(Value::Null)
            }
            Request::PlanImport { document } => {
                Ok(json!({"plan":self.app.import_plan(*document)?}))
            }
            Request::PlanExport { plan } => serde_json::to_value(self.app.export_plan(plan)?),
            Request::PlanClose { plan } => {
                self.app.close_plan(plan)?;
                Ok(Value::Null)
            }
            Request::Capabilities { operations } => {
                serde_json::to_value(self.app.capabilities(&operations)?)
            }
            Request::ApplyBlocking { plan } => {
                let run = match self.app.apply_plan(plan) {
                    Ok(run) => run,
                    Err(error) => error.run_id.ok_or(error)?,
                };
                serde_json::to_value(self.app.run_snapshot(run)?)
            }
            Request::ApplyStart { .. }
            | Request::JobPoll { .. }
            | Request::JobClose { .. }
            | Request::JobCancel { .. } => {
                return Err(ProtocolError::UnsupportedOperation(
                    "job lifecycle requires an owning transport".into(),
                ));
            }
            Request::RunSnapshot { run } => serde_json::to_value(self.app.run_snapshot(run)?),
            Request::RunEvents { run, since } => serde_json::to_value(self.app.events(run, since)?),
            Request::RunClose { run } => {
                self.app.close_run(run)?;
                Ok(Value::Null)
            }
            Request::IncidentInspect {} => serde_json::to_value(self.app.incident()?),
            Request::IncidentReview {} => {
                self.reserve_review()?;
                if let Some(candidate) = self.app.review_incident()? {
                    let review = Uuid::now_v7();
                    let result = json!({"review":review,"incident":candidate.inspection(),"token":candidate.confirmation_token()});
                    self.reviews
                        .insert(review, Review::Incident(Box::new(candidate)));
                    Ok(result)
                } else {
                    Ok(json!({"review":null,"incident":null}))
                }
            }
            Request::IncidentAcknowledge {
                review,
                run_id,
                token,
            } => {
                let Some(Review::Incident(candidate)) = self.reviews.get(&review) else {
                    return Err(ProtocolError::UnknownReview);
                };
                if candidate.inspection().attempt.run_id != run_id
                    || candidate.confirmation_token() != token
                {
                    return Err(ProtocolError::ReviewMismatch);
                }
                let Some(Review::Incident(candidate)) = self.reviews.remove(&review) else {
                    return Err(ProtocolError::UnknownReview);
                };
                let incident = self.app.acknowledge_incident(*candidate, token, policy)?;
                Ok(json!({"incident":incident,"acknowledged":true}))
            }
        };
        value.map_err(|error| ProtocolError::Serialization(error.to_string()))
    }
}
