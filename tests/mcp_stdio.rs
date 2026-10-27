//! End-to-end test of the MCP server over stdio.
//!
//! The test speaks raw JSON-RPC to the `reason-network mcp` binary, exactly
//! like an MCP client would.

mod common;

use std::process::Stdio;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin};

use common::TestDaemon;

/// Send one JSON-RPC message.
async fn send(stdin: &mut ChildStdin, message: Value) {
    let mut line = serde_json::to_string(&message).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();
}

/// Read messages until the response with the given id shows up.
async fn read_response(
    reader: &mut Lines<BufReader<tokio::process::ChildStdout>>,
    id: u64,
) -> Value {
    for _ in 0..50 {
        let line = reader.next_line().await.unwrap().unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        if value.get("id").and_then(Value::as_u64) == Some(id) {
            return value;
        }
    }
    panic!("no response with id {id}");
}

fn start_mcp_server(
    daemon: &TestDaemon,
) -> (
    Child,
    ChildStdin,
    Lines<BufReader<tokio::process::ChildStdout>>,
) {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_reason-network"))
        .arg("mcp")
        .arg("--session-key")
        .arg("mcp-agent")
        .env("REASON_NETWORK_DATA_DIR", daemon.data_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("mcp server");
    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
    (child, stdin, stdout)
}

#[tokio::test]
async fn the_mcp_server_exposes_the_four_tools_over_stdio() {
    let daemon = TestDaemon::start().await;
    let (mut child, mut stdin, mut stdout) = start_mcp_server(&daemon);

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "reason-network-test", "version": "1.0.0" }
            }
        }),
    )
    .await;
    let initialized = read_response(&mut stdout, 1).await;
    assert_eq!(
        initialized["result"]["serverInfo"]["name"],
        "reason-network"
    );

    send(
        &mut stdin,
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    send(
        &mut stdin,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    let tools = read_response(&mut stdout, 2).await;
    let tools = tools["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["ask_user", "check_inbox", "list_team", "send_message"]
    );

    let _ = child.kill().await;
}

#[tokio::test]
async fn the_mcp_server_calls_the_daemon() {
    let daemon = TestDaemon::start().await;
    daemon
        .call(
            reason_network::protocol::Method::Register,
            json!({ "session_key": "peer", "name": "peer", "agent_type": "codex" }),
        )
        .await
        .unwrap();

    let (mut child, mut stdin, mut stdout) = start_mcp_server(&daemon);

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "reason-network-test", "version": "1.0.0" }
            }
        }),
    )
    .await;
    read_response(&mut stdout, 1).await;
    send(
        &mut stdin,
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "list_team", "arguments": {} }
        }),
    )
    .await;
    let response = read_response(&mut stdout, 2).await;
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    let payload: Value = serde_json::from_str(text).unwrap();
    let sessions = payload["sessions"].as_array().unwrap();
    assert!(
        sessions
            .iter()
            .any(|session| session["session_key"] == "mcp-agent"),
        "the MCP server should have registered its own session: {payload}"
    );
    assert!(
        sessions
            .iter()
            .any(|session| session["session_key"] == "peer"),
        "the peer session should be listed: {payload}"
    );

    let _ = child.kill().await;
}
