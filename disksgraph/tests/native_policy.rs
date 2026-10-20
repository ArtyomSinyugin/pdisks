//! Typed native policy side-channel tests use saved observations and never load write backends.

mod support;
use disksgraph::{
    LiveCheckStatus,
    host::{ExecutionContext, authorization::ApprovalRequest},
    native::{NativeHostPolicy, PolicyRequest, ScopeCheck},
};
use std::error::Error;

/// Callback JSON preserves the exact reviewed context and separates destructive from general approval.
#[test]
fn policy_requests_preserve_execution_context_and_confirmation_kind() -> Result<(), Box<dyn Error>>
{
    let (_scratch, mut app, _, _) = support::application()?;
    let session = app.create_session()?;
    let frozen = app.freeze(session, app.revision(session)?)?;
    let run = storage_engine::ExecutionRun::new(frozen.document())?;
    let context: ExecutionContext<'_> = run.context();
    let observed = app.probe()?;
    let request = PolicyRequest::Scope {
        kind: ScopeCheck::FinalScope,
        context: (&context).into(),
        action: None,
        observed: &observed,
    };
    let value = serde_json::to_value(request)?;
    assert_eq!(value["kind"], "final_scope");
    assert_eq!(
        value["context"]["plan_id"],
        serde_json::to_value(context.plan_id)?
    );
    assert_eq!(
        value["context"]["baseline"],
        serde_json::to_value(context.historical_baseline)?
    );
    assert_eq!(value["observed"], serde_json::to_value(&observed)?);
    let approval = ApprovalRequest {
        run_id: run.run_id(),
        plan_id: context.plan_id,
        action_id: disksgraph::ActionId::new(0),
        plan_digest: storage_runtime::checkpoint::plan_digest(run.document())?,
        backend: disksgraph::host::BackendId::new("policy-fixture").ok_or("backend")?,
        targets: Vec::new(),
        observation: observed.reference,
    };
    let target = disksgraph::model::NodeId::new();
    let mut policy = |request: PolicyRequest<'_>| match request {
        PolicyRequest::Authorize {
            request,
            destructive_target,
        } => {
            assert_eq!(request.plan_digest, approval.plan_digest);
            assert_eq!(destructive_target, Some(target));
            LiveCheckStatus::Satisfied
        }
        _ => panic!("unexpected request"),
    };
    assert_eq!(
        policy.check(PolicyRequest::Authorize {
            request: &approval,
            destructive_target: Some(target)
        }),
        LiveCheckStatus::Satisfied
    );
    let general = serde_json::to_value(PolicyRequest::Authorize {
        request: &approval,
        destructive_target: None,
    })?;
    assert_eq!(general["request"], serde_json::to_value(&approval)?);
    assert!(general["destructive_target"].is_null());
    Ok(())
}

/// Construction failures happen before consulting an embedding application's positive policy.
#[test]
fn native_write_constructor_requires_explicit_complete_configuration() -> Result<(), Box<dyn Error>>
{
    use std::{cell::Cell, fs, os::unix::fs::PermissionsExt, rc::Rc};
    let scratch = support::Scratch::new()?;
    let calls = Rc::new(Cell::new(0));
    let captured = calls.clone();
    let policy = disksgraph::native::HostPolicy::new(move |_: PolicyRequest<'_>| {
        captured.set(captured.get() + 1);
        LiveCheckStatus::Satisfied
    });
    let manifest = scratch.0.join("providers.json");
    fs::write(&manifest, r#"{"manifest_version":1,"providers":[]}"#)?;
    assert!(matches!(
        disksgraph::native::open_with_policy(&scratch.0, &manifest, policy.clone()),
        Err(disksgraph::native::OpenError::Configuration(_))
    ));
    fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o755))?;
    assert!(matches!(
        disksgraph::native::open_with_policy(&scratch.0, &manifest, policy),
        Err(disksgraph::native::OpenError::Application(_))
    ));
    assert_eq!(calls.get(), 0);
    assert!(!scratch.0.join("active-attempt.json").exists());
    fs::set_permissions(&scratch.0, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
