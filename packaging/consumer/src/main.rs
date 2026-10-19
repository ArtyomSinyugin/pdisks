#![deny(unsafe_code, unsafe_op_in_unsafe_fn)]
//! Standalone checked Stabby client: print a saved draft's preview without a native provider or write path.

mod api;
use api::{Api, Error};
use serde_json::{json, Value};
use std::{fs::File, io::Read, path::PathBuf};

/// Version-1 request bound also limits this example's file read before JSON parsing.
const MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;

/// Rejects protocol/operation errors without inferring success from the transport's return alone.
fn result(reply: Value) -> Result<Value, Error> {
    if reply["protocol_version"] != 1 || reply.get("error").is_some() {
        return Err(format!("protocol reply: {reply}").into());
    }
    reply
        .get("result")
        .cloned()
        .ok_or_else(|| "missing result".into())
}

/// Takes an explicit library, existing private root and bounded regular draft file; closes before unload.
fn main() -> Result<(), Error> {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let [library, root, draft] = args.as_slice() else {
        return Err("usage: disksgraph-consumer-example LIBRARY PRIVATE_ROOT DRAFT_JSON".into());
    };
    let file = File::open(draft)?;
    if !file.metadata()?.is_file() {
        return Err("draft must be a regular file".into());
    }
    let mut input = String::new();
    file.take(MAX_INPUT_BYTES as u64 + 1)
        .read_to_string(&mut input)?;
    if input.len() > MAX_INPUT_BYTES {
        return Err("draft exceeds the input size limit".into());
    }
    let draft: Value = serde_json::from_str(&input)?;
    let api = Api::load(library, "disksgraph")?;
    if api.version()? != 1 || !api.rejects_wrong_signature() {
        return Err("incompatible checked ABI".into());
    }
    let opened = result(api.open(&json!({"abi_version":1,"protocol_version":1,"root":root}))?)?;
    let context = opened["context"].as_u64().ok_or("context handle")?;
    let imported = result(api.method(context, "session_import", json!({"draft":draft}))?)?;
    let session = imported["session"].as_str().ok_or("session handle")?;
    let preview = result(api.method(context, "session_preview", json!({"session":session}))?)?;
    result(api.method(context, "session_close", json!({"session":session}))?)?;
    result(api.close(context)?)?;
    println!("{}", serde_json::to_string(&preview)?);
    Ok(())
}
