//! Conscious same-process incident confirmation; JSON challenges never grant safety authorization.

use crate::{Error, commands::write_event, files::MAX_INPUT_BYTES};
use disksgraph::{
    DisksGraph, IncidentAcknowledgmentPolicy, IncidentInspection, LiveCheckStatus,
    host::{
        authorization::AuthorizationContext, capability::BackendCapabilityCheck,
        dispatcher::BackendDispatcher, host::LinuxLiveHost,
    },
};
use serde::Deserialize;
use std::io::{BufRead, Write};

/// Explicitly absent production authorization/coverage/quiescence configuration.
pub(crate) struct UnconfiguredPolicy;
impl IncidentAcknowledgmentPolicy for UnconfiguredPolicy {
    /// A token, root privilege and an available lock cannot establish native backend quiescence.
    fn check(&mut self, _: &IncidentInspection, _: &IncidentInspection) -> LiveCheckStatus {
        LiveCheckStatus::Unknown {
            required_evidence:
                "trusted principal authorization, complete scope and native backend quiescence"
                    .into(),
        }
    }
}

/// One reply to the exact review shown by this foreground invocation.
#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
enum Confirmation {
    /// Confirms intent to acknowledge; independent host safety policy still decides whether clearing is safe.
    Acknowledge {
        run_id: uuid::Uuid,
        token: uuid::Uuid,
    },
    /// Leaves the checkpoint untouched.
    Cancel {},
}

/// Shows the runtime's complete immutable review before reading one bounded confirmation line.
/// EOF/cancel preserve the marker; wrong tokens, changed state and missing safety evidence fail closed.
pub fn acknowledge<C: BackendCapabilityCheck, A: AuthorizationContext, D: BackendDispatcher>(
    app: &mut DisksGraph<LinuxLiveHost<'_, C, A>, D>,
    input: &mut impl BufRead,
    output: &mut dyn Write,
    policy: &mut impl IncidentAcknowledgmentPolicy,
) -> Result<serde_json::Value, Error> {
    let Some(review) = app.review_incident()? else {
        return Ok(serde_json::json!({"incident": null, "acknowledged": false}));
    };
    write_event(
        output,
        &serde_json::json!({
            "type": "incident_review", "incident": review.inspection(), "token": review.confirmation_token(),
        }),
    )?;
    let mut line = Vec::new();
    let count =
        std::io::Read::take(input, MAX_INPUT_BYTES as u64 + 1).read_until(b'\n', &mut line)?;
    if line.len() > MAX_INPUT_BYTES {
        return Err(Error::Document(
            "incident confirmation exceeds the input size limit".into(),
        ));
    }
    if count == 0 {
        return Ok(serde_json::json!({"acknowledged": false}));
    }
    match serde_json::from_slice::<Confirmation>(&line)? {
        Confirmation::Cancel {} => Ok(serde_json::json!({"acknowledged": false})),
        Confirmation::Acknowledge { run_id, token } => {
            if run_id != review.inspection().attempt.run_id {
                return Err(Error::Usage(
                    "run ID differs from the reviewed incident".into(),
                ));
            }
            let inspection = app.acknowledge_incident(review, token, policy)?;
            Ok(serde_json::json!({"incident": inspection, "acknowledged": true}))
        }
    }
}
