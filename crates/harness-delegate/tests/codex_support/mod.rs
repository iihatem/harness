#![allow(dead_code)]
//! Checks of what the Codex transport reads and writes against the JSON schema generated from
//! `codex app-server generate-json-schema` (codex-cli 0.154.0, in `schema/codex-0.154.0/`).

use std::{collections::HashMap, path::PathBuf, sync::Mutex};

use jsonschema::Validator;
use serde_json::Value;

pub const SCHEMA_VERSION: &str = "0.154.0";

pub fn schema_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schema/codex-0.154.0")
}

fn validator(file: &str) -> &'static Validator {
    static CACHE: Mutex<Option<HashMap<String, &'static Validator>>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap();
    let cache = cache.get_or_insert_with(HashMap::new);
    cache.entry(file.to_string()).or_insert_with(|| {
        let text = std::fs::read_to_string(schema_dir().join(file))
            .unwrap_or_else(|e| panic!("schema {file}: {e}"));
        let schema: Value = serde_json::from_str(&text).unwrap();
        Box::leak(Box::new(
            jsonschema::validator_for(&schema).unwrap_or_else(|e| panic!("{file}: {e}")),
        ))
    })
}

pub fn check(file: &str, instance: &Value) {
    let v = validator(file);
    let errors: Vec<String> = v
        .iter_errors(instance)
        .map(|e| format!("{e} at {}", e.instance_path()))
        .collect();
    assert!(
        errors.is_empty(),
        "{file} rejects {instance}:\n{}",
        errors.join("\n")
    );
}

fn parse(line: &str) -> Value {
    serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON: {line}: {e}"))
}

/// A line harness writes to the child: a request, the `initialized` notification, a response to
/// a server request (checked against `response_schema` when given) or a JSON-RPC error.
pub fn check_client_line(line: &str, response_schema: Option<&str>) -> Value {
    let v = parse(line);
    assert!(
        v.get("jsonrpc").is_none(),
        "app-server messages have no jsonrpc field: {line}"
    );
    if v.get("method").is_some() && v.get("id").is_some() {
        check("ClientRequest.json", &v);
    } else if v.get("method").is_some() {
        check("ClientNotification.json", &v);
    } else if v.get("error").is_some() {
        check("JSONRPCError.json", &v);
    } else if let (Some(result), Some(file)) = (v.get("result"), response_schema) {
        check(file, result);
        assert!(v.get("id").is_some());
    } else {
        panic!("not a client message: {line}");
    }
    v
}

/// A line the child writes: a notification or a request of the server, or a response
/// (`result_schema` checks its result when given).
pub fn check_server_line(line: &str, result_schema: Option<&str>) {
    let v = parse(line);
    if v.get("method").is_some() && v.get("id").is_some() {
        check("ServerRequest.json", &v);
    } else if v.get("method").is_some() {
        check("ServerNotification.json", &v);
    } else if let (Some(result), Some(file)) = (v.get("result"), result_schema) {
        check(file, result);
    } else if v.get("error").is_some() {
        check("JSONRPCError.json", &v);
    }
}
