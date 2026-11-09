//! The committed schema is what the transport was built against: codex-cli 0.154.0. These tests
//! fail when the transport names a method the schema does not have, or when the tested version
//! constant and the schema directory drift apart. To move to a new Codex: run
//! `codex app-server generate-json-schema --out <dir> --experimental`, replace
//! `schema/codex-<version>/` with the files the tests use (compact JSON), and run the suite.

mod codex_support;

use std::collections::HashSet;

use codex_support::{SCHEMA_VERSION, schema_dir};
use harness_delegate::detect::CODEX_TESTED;
use serde_json::Value;

fn methods(file: &str) -> HashSet<String> {
    let text = std::fs::read_to_string(schema_dir().join(file)).unwrap();
    let schema: Value = serde_json::from_str(&text).unwrap();
    schema["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|o| {
            o["properties"]["method"]["enum"][0]
                .as_str()
                .map(String::from)
        })
        .collect()
}

#[test]
fn the_schema_directory_is_the_version_the_transport_was_tested_with() {
    assert_eq!(SCHEMA_VERSION, CODEX_TESTED);
    assert!(schema_dir().ends_with(format!("codex-{CODEX_TESTED}")));
}

#[test]
fn every_method_the_transport_writes_exists_in_the_schema() {
    let client = methods("ClientRequest.json");
    for m in [
        "initialize",
        "thread/start",
        "thread/resume",
        "turn/start",
        "turn/interrupt",
        "account/rateLimits/read",
    ] {
        assert!(client.contains(m), "ClientRequest has no `{m}`");
    }
    assert!(methods("ClientNotification.json").contains("initialized"));
}

#[test]
fn every_notification_and_server_request_the_transport_reads_exists_in_the_schema() {
    let notes = methods("ServerNotification.json");
    for m in [
        "item/agentMessage/delta",
        "item/reasoning/textDelta",
        "item/reasoning/summaryTextDelta",
        "item/started",
        "item/completed",
        "item/fileChange/patchUpdated",
        "thread/tokenUsage/updated",
        "account/rateLimits/updated",
        "serverRequest/resolved",
        "turn/completed",
        "error",
        "warning",
        "model/rerouted",
        // Opted out of at initialize (see the protocol):
        "item/commandExecution/outputDelta",
        "item/fileChange/outputDelta",
        "item/mcpToolCall/progress",
        "item/commandExecution/terminalInteraction",
    ] {
        assert!(notes.contains(m), "ServerNotification has no `{m}`");
    }
    let requests = methods("ServerRequest.json");
    for m in [
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
    ] {
        assert!(requests.contains(m), "ServerRequest has no `{m}`");
    }
}

#[test]
fn the_decisions_the_transport_sends_are_in_the_schema() {
    for (file, decisions) in [
        (
            "CommandExecutionRequestApprovalResponse.json",
            vec!["accept", "acceptForSession", "decline", "cancel"],
        ),
        (
            "FileChangeRequestApprovalResponse.json",
            vec!["accept", "acceptForSession", "decline", "cancel"],
        ),
    ] {
        let text = std::fs::read_to_string(schema_dir().join(file)).unwrap();
        for d in decisions {
            assert!(
                text.contains(&format!("\"{d}\"")),
                "{file} has no decision `{d}`"
            );
        }
    }
}
