//! One-process JSONL editing, with explicit atomic save and in-memory undo/redo history.

use crate::{Error, files};
use disksgraph::{
    DisksGraph, DraftDocument, PlanEdit,
    host::{
        authorization::AuthorizationContext, capability::BackendCapabilityCheck,
        dispatcher::BackendDispatcher, host::LinuxLiveHost,
    },
};
use serde::Deserialize;
use std::{
    io::{BufRead, Write},
    path::Path,
};

/// One bounded JSON command, independent of frozen backend actions and execution authorization.
#[derive(Debug, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
enum EditCommand {
    /// Apply one existing atomic editing operation.
    Edit { edit: Box<PlanEdit> },
    /// Restore editable requests only; no disk effect can be undone here.
    Undo {},
    /// Reapply requests undone during this invocation.
    Redo {},
    /// Return the forecast for the current revision.
    Preview {},
    /// Atomically replace the opened draft file.
    Save {},
    /// End without implicitly saving pending changes.
    Quit {},
}

/// Runs until EOF/quit, emitting JSONL replies separately from the caller's final JSON result.
/// Invalid commands leave the session usable; EOF and quit never silently persist unsaved edits.
pub fn edit<C: BackendCapabilityCheck, A: AuthorizationContext, D: BackendDispatcher>(
    app: &mut DisksGraph<LinuxLiveHost<'_, C, A>, D>,
    path: &Path,
    input: &mut impl BufRead,
    output: &mut dyn Write,
) -> Result<serde_json::Value, Error> {
    let document = DraftDocument::from_json(&files::read_document(path)?)
        .map_err(disksgraph::DisksGraphError::from)?;
    let session = app.import_draft(document)?;
    let result = (|| {
        let mut saved = app.revision(session)?;
        loop {
            let mut line = Vec::new();
            // Each command is bounded even for an untrusted producer that never sends a newline.
            let count = std::io::Read::take(&mut *input, files::MAX_INPUT_BYTES as u64 + 1)
                .read_until(b'\n', &mut line)?;
            if line.len() > files::MAX_INPUT_BYTES {
                return Err(Error::Document(
                    "REPL command exceeds the input size limit".into(),
                ));
            }
            if count == 0 {
                break;
            }
            let command = serde_json::from_slice::<EditCommand>(&line);
            if matches!(command, Ok(EditCommand::Quit { .. })) {
                break;
            }
            let reply = (|| -> Result<serde_json::Value, Error> {
                let revision = app.revision(session)?;
                match command? {
                    EditCommand::Edit { edit } => {
                        Ok(serde_json::to_value(app.edit(session, revision, *edit)?)?)
                    }
                    EditCommand::Undo {} => {
                        Ok(serde_json::json!({"revision": app.undo(session, revision)?}))
                    }
                    EditCommand::Redo {} => {
                        Ok(serde_json::json!({"revision": app.redo(session, revision)?}))
                    }
                    EditCommand::Preview {} => {
                        Ok(serde_json::to_value(app.preview_document(session)?)?)
                    }
                    EditCommand::Save {} => {
                        let document = app
                            .export_draft(session)?
                            .to_json()
                            .map_err(disksgraph::DisksGraphError::from)?;
                        files::save_document(path, &document)?;
                        saved = revision;
                        Ok(serde_json::json!({"revision": revision, "saved": true}))
                    }
                    EditCommand::Quit {} => unreachable!("quit exits before editing"),
                }
            })();
            let reply = match reply {
                Ok(value) => serde_json::json!({"result": value}),
                Err(error) => serde_json::json!({"error": error}),
            };
            serde_json::to_writer(&mut *output, &reply)?;
            writeln!(output)?;
            output.flush()?;
        }
        let revision = app.revision(session)?;
        Ok(serde_json::json!({"revision": revision, "saved": revision == saved}))
    })();
    app.close_session(session)?;
    result
}
