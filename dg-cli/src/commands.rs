//! Strict command parsing and facade calls shared with command/API parity tests.

use crate::{Error, files};
use disksgraph::{
    DisksGraph, DraftDocument, PlanEdit, PlanRequest,
    host::{dispatcher::BackendDispatcher, host::RuntimeHost},
};
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};

/// Supported foreground commands; all durable state is an explicit user-selected document.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Collect one fresh native publication.
    Probe,
    /// Start a draft from one fresh same-pass baseline.
    New {
        /// Atomic destination for the editable document.
        output: PathBuf,
    },
    /// Append one existing typed recipe from a JSON request file.
    Add {
        /// Saved inputs, including the historical baseline.
        draft: PathBuf,
        /// One serialized PlanRequest, not backend actions or permission.
        request: PathBuf,
        /// Atomic destination, which may equal the input draft.
        output: PathBuf,
    },
    /// Recompute the current forecast without any new native observation.
    Preview {
        /// Editable inputs to recompute.
        draft: PathBuf,
    },
    /// Export one immutable, historically reanalysed execution description.
    Freeze {
        /// Editable revision to freeze.
        draft: PathBuf,
        /// Atomic destination for canonical PlanDocument JSON.
        output: PathBuf,
    },
}

impl Command {
    /// Only commands explicitly requesting actual state need native read registrations.
    pub fn needs_probe(&self) -> bool {
        matches!(self, Self::Probe | Self::New { .. })
    }
}

/// Explicit trusted host configuration kept separate from saved draft/plan contents.
#[derive(Debug, PartialEq, Eq)]
pub struct Invocation {
    /// Native provider registrations chosen by the invoking host.
    pub manifest: Option<PathBuf>,
    /// Existing owner-only execution directory, shared by all cooperating callers.
    pub root: PathBuf,
    /// Validated operation and file paths.
    pub command: Command,
}

impl Invocation {
    /// Rejects duplicate, unknown and missing flags before native configuration or disk access.
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, Error> {
        let mut args = args.into_iter().peekable();
        let mut words = Vec::new();
        while args
            .peek()
            .is_some_and(|arg| !arg.to_string_lossy().starts_with('-'))
        {
            if let Some(word) = args.next() {
                words.push(word);
            }
        }
        let mut flags = BTreeMap::new();
        while let Some(flag) = args.next() {
            let key = flag
                .to_str()
                .ok_or_else(|| Error::Usage("flag must be UTF-8".into()))?;
            if !key.starts_with("--") {
                return Err(Error::Usage("expected a named flag".into()));
            }
            let value = args
                .next()
                .filter(|value| !value.to_string_lossy().starts_with("--"))
                .ok_or_else(|| Error::Usage(format!("missing value for {key}")))?;
            if flags.insert(key.to_owned(), value).is_some() {
                return Err(Error::Usage(format!("duplicate flag {key}")));
            }
        }
        let manifest = flags.remove("--manifest").map(PathBuf::from);
        let root = take(&mut flags, "--root")?;
        let words: Vec<_> = words
            .iter()
            .map(|word| word.to_str().unwrap_or(""))
            .collect();
        let command = match words.as_slice() {
            ["probe"] => Command::Probe,
            ["plan", "new"] => Command::New {
                output: take(&mut flags, "--output")?,
            },
            ["plan", "add"] => Command::Add {
                draft: take(&mut flags, "--draft")?,
                request: take(&mut flags, "--request")?,
                output: take(&mut flags, "--output")?,
            },
            ["plan", "preview"] => Command::Preview {
                draft: take(&mut flags, "--draft")?,
            },
            ["plan", "freeze"] => Command::Freeze {
                draft: take(&mut flags, "--draft")?,
                output: take(&mut flags, "--output")?,
            },
            _ => return Err(Error::Usage("unknown command; use --help".into())),
        };
        if command.needs_probe()
            && manifest
                .as_ref()
                .is_none_or(|path| path.as_os_str().is_empty())
        {
            return Err(Error::Usage("required flag --manifest".into()));
        }
        if let Some(flag) = flags.keys().next() {
            return Err(Error::Usage(format!("unknown flag {flag}")));
        }
        Ok(Self {
            manifest,
            root,
            command,
        })
    }
}

/// Takes one mandatory file option without interpreting paths as executable content.
fn take(flags: &mut BTreeMap<String, OsString>, key: &str) -> Result<PathBuf, Error> {
    flags
        .remove(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Error::Usage(format!("required flag {key}")))
}

/// Runs commands only through the supplied facade; test hosts use this exact command path.
pub fn execute<H: RuntimeHost, D: BackendDispatcher>(
    app: &mut DisksGraph<H, D>,
    command: &Command,
) -> Result<serde_json::Value, Error> {
    match command {
        Command::Probe => Ok(serde_json::to_value(app.probe()?)?),
        Command::New { output } => {
            let session = app.create_session()?;
            let document = app.export_draft(session)?;
            let json = document
                .to_json()
                .map_err(disksgraph::DisksGraphError::from)?;
            files::save_document(output, &json)?;
            Ok(serde_json::json!({"revision": app.revision(session)?}))
        }
        Command::Add {
            draft,
            request,
            output,
        } => {
            let document = DraftDocument::from_json(&files::read_document(draft)?)
                .map_err(disksgraph::DisksGraphError::from)?;
            let request: PlanRequest = serde_json::from_str(&files::read_document(request)?)?;
            let session = app.import_draft(document)?;
            let receipt = app.edit(session, app.revision(session)?, PlanEdit::Add(request))?;
            let document = app.export_draft(session)?;
            files::save_document(
                output,
                &document
                    .to_json()
                    .map_err(disksgraph::DisksGraphError::from)?,
            )?;
            Ok(serde_json::json!({
                "revision": receipt.revision, "operation": receipt.added_operation,
                "created_node": receipt.created_node, "status": app.preview(session)?.status(),
            }))
        }
        Command::Preview { draft } | Command::Freeze { draft, .. } => {
            let document = DraftDocument::from_json(&files::read_document(draft)?)
                .map_err(disksgraph::DisksGraphError::from)?;
            let session = app.import_draft(document)?;
            if let Command::Freeze { output, .. } = command {
                let frozen = app.freeze(session, app.revision(session)?)?;
                let json = frozen
                    .document()
                    .canonical_json()
                    .map_err(disksgraph::DisksGraphError::from)?;
                files::save_document(output, &json)?;
                Ok(
                    serde_json::json!({"plan_id": frozen.document().plan_id, "revision": frozen.revision()}),
                )
            } else {
                Ok(serde_json::to_value(app.preview_document(session)?)?)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Typos and repeated host/file options must never silently select another configuration.
    #[test]
    fn parser_rejects_ambiguous_inputs() {
        for args in [
            vec!["probe"],
            vec!["probe", "--manifest", "m", "--root", "r", "--root", "other"],
            vec!["probe", "--manifest", "m", "--root", "r", "--yes", "true"],
            vec!["plan", "new", "--manifest", "m", "--root", "r", "--output"],
            vec!["probe", "--manifest", "--root", "r"],
        ] {
            assert!(Invocation::parse(args.into_iter().map(OsString::from)).is_err());
        }
        let invocation = Invocation::parse(
            [
                "plan",
                "new",
                "--manifest",
                "m",
                "--root",
                "r",
                "--output",
                "draft.json",
            ]
            .map(OsString::from),
        );
        assert!(matches!(
            invocation,
            Ok(Invocation {
                command: Command::New { .. },
                ..
            })
        ));
    }
}
