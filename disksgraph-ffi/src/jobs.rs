//! One bounded passive mailbox per context; it never owns admission, dispatch or resumable state.

use crate::registry::Error;
use disksgraph::{EventsPage, RunSnapshot};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};

/// Process-wide identities prevent a context from accepting another context's integer job handle.
static NEXT_JOB: AtomicU64 = AtomicU64::new(1);

/// Replaced wholesale at existing coordinator notification boundaries; no additional event journal.
struct Progress {
    /// Latest coherent live/final evidence, absent while queued or after pre-run validation failure.
    snapshot: Option<RunSnapshot>,
    /// The engine's bounded retained ring, copied rather than extended by the transport.
    events: EventsPage,
    /// True only after the owner returns or contains its panic.
    finished: bool,
    /// Protocol-level failure before a retained run, or a contained owner panic.
    error: Option<Value>,
}

/// Shared only as passive evidence; the non-Send application remains on its original worker.
pub(crate) struct Job {
    /// Never reused, even after a mailbox is released.
    pub(crate) id: u64,
    /// Polling borrows this short lock, independently of the worker's provider/native calls.
    progress: Mutex<Progress>,
}
impl Job {
    /// Allocates a mailbox only after the context has reserved its sole job slot.
    pub(crate) fn new() -> Result<Self, Error> {
        let id = NEXT_JOB
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| Error::ResourceLimit)?;
        Ok(Self {
            id,
            progress: Mutex::new(Progress {
                snapshot: None,
                events: EventsPage {
                    events: vec![],
                    next_sequence: 0,
                    gap: false,
                },
                finished: false,
                error: None,
            }),
        })
    }
    /// Publishes a whole coherent replacement; a poisoned mailbox remains inaccessible to pollers.
    pub(crate) fn update(&self, snapshot: RunSnapshot, events: EventsPage) {
        if let Ok(mut progress) = self.progress.lock() {
            progress.snapshot = Some(snapshot);
            progress.events = events;
        }
    }
    /// Marks the owner's return; failed attempts never become successful solely by finishing.
    pub(crate) fn finish(&self, error: Option<Value>) {
        if let Ok(mut progress) = self.progress.lock() {
            progress.error = error;
            progress.finished = true;
        }
    }
    /// Serial operations and mailbox release require the background attempt to have returned.
    pub(crate) fn finished(&self) -> Result<bool, Error> {
        Ok(self.progress.lock().map_err(|_| Error::Stopped)?.finished)
    }
    /// Returns bounded exclusive replay with exact future-cursor rejection and eviction-gap semantics.
    pub(crate) fn poll(&self, since: u64) -> Result<Value, Error> {
        let progress = self.progress.lock().map_err(|_| Error::Stopped)?;
        if since > progress.events.next_sequence {
            return Err(Error::InvalidEventCursor);
        }
        let page = EventsPage {
            events: progress
                .events
                .events
                .iter()
                .filter(|event| event.sequence > since)
                .cloned()
                .collect(),
            next_sequence: progress.events.next_sequence,
            gap: progress
                .events
                .events
                .first()
                .is_some_and(|event| since < event.sequence - 1),
        };
        Ok(
            json!({"job":self.id,"finished":progress.finished,"snapshot":progress.snapshot,"events":page,"error":progress.error}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use disksgraph::RunEventKind;

    /// Polling serializes protected event text while retaining the original provider message in Rust.
    #[test]
    fn mailbox_poll_redacts_progress_without_changing_cursors(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let job = Job::new().map_err(|error| format!("{error:?}"))?;
        let run_id = serde_json::from_value(json!("00000000-0000-7000-8000-000000000001"))?;
        let mut state = job.progress.lock().map_err(|error| error.to_string())?;
        state.events = EventsPage {
            events: vec![disksgraph::RunEvent {
                run_id,
                sequence: 1,
                kind: RunEventKind::Progress {
                    action: disksgraph::ActionId::new(0),
                    detail: "SECRET-native-progress".into(),
                },
            }],
            next_sequence: 1,
            gap: false,
        };
        drop(state);
        let value = job.poll(0).map_err(|error| format!("{error:?}"))?;
        assert_eq!(value["events"]["next_sequence"], 1);
        assert_eq!(
            value["events"]["events"][0]["kind"]["progress"]["detail"],
            disksgraph::diagnostics::REDACTED
        );
        assert!(!value.to_string().contains("SECRET"));
        assert_eq!(
            job.poll(1).map_err(|error| format!("{error:?}"))?["events"]["events"],
            json!([])
        );
        let state = job.progress.lock().map_err(|error| error.to_string())?;
        assert!(
            matches!(&state.events.events[0].kind, RunEventKind::Progress { detail, .. } if detail == "SECRET-native-progress")
        );
        Ok(())
    }

    /// Polling never fabricates pre-run evidence and preserves engine eviction/cursor rules on finish.
    #[test]
    fn mailbox_is_bounded_and_rejects_future_cursors() -> Result<(), String> {
        let job = Job::new().map_err(|error| format!("{error:?}"))?;
        let queued = job.poll(0).map_err(|error| format!("{error:?}"))?;
        assert_eq!(queued["snapshot"], Value::Null);
        assert_eq!(queued["finished"], false);
        assert!(matches!(job.poll(1), Err(Error::InvalidEventCursor)));
        let mut state = job.progress.lock().map_err(|error| error.to_string())?;
        state.events = EventsPage {
            events: vec![disksgraph::RunEvent {
                run_id: serde_json::from_value(json!("00000000-0000-7000-8000-000000000001"))
                    .map_err(|error| error.to_string())?,
                sequence: 5,
                kind: RunEventKind::GoalVerification { verified: false },
            }],
            next_sequence: 5,
            gap: true,
        };
        drop(state);
        assert_eq!(
            job.poll(0).map_err(|error| format!("{error:?}"))?["events"]["gap"],
            true
        );
        assert_eq!(
            job.poll(4).map_err(|error| format!("{error:?}"))?["events"]["gap"],
            false
        );
        assert_eq!(
            job.poll(5).map_err(|error| format!("{error:?}"))?["events"]["events"],
            json!([])
        );
        job.finish(Some(json!({"code":"panicked"})));
        assert!(job.finished().map_err(|error| format!("{error:?}"))?);
        assert_eq!(
            job.poll(5).map_err(|error| format!("{error:?}"))?["error"]["code"],
            "panicked"
        );
        assert!(Job::new().map_err(|error| format!("{error:?}"))?.id > job.id);
        Ok(())
    }
}
