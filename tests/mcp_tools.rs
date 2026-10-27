//! Tests for the four MCP tools. The tools are thin wrappers over the local
//! daemon, so these tests run them against a real daemon.

mod common;

use reason_network::mcp::{AskUserArgs, CheckInboxArgs, ReasonMcpServer, SendMessageArgs};
use reason_network::protocol::Method;
use rmcp::handler::server::wrapper::Parameters;
use serde_json::json;

use common::TestDaemon;

fn server(daemon: &TestDaemon, session_key: &str) -> ReasonMcpServer {
    ReasonMcpServer::new(daemon.config.endpoint.clone(), session_key.to_string())
}

#[tokio::test]
async fn list_team_returns_the_registered_sessions() {
    let daemon = TestDaemon::start().await;
    daemon
        .call(
            Method::Register,
            json!({ "session_key": "alice", "name": "alice", "agent_type": "codex" }),
        )
        .await
        .unwrap();

    let server = server(&daemon, "alice");
    let output = server.list_team().await.unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
    let sessions = parsed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["session_key"], "alice");
    assert_eq!(sessions[0]["agent_type"], "codex");
}

#[tokio::test]
async fn send_message_delivers_to_the_recipient() {
    let daemon = TestDaemon::start().await;
    daemon
        .call(
            Method::Register,
            json!({ "session_key": "alice", "name": "alice" }),
        )
        .await
        .unwrap();
    daemon
        .call(
            Method::Register,
            json!({ "session_key": "bob", "name": "bob" }),
        )
        .await
        .unwrap();

    let server = server(&daemon, "alice");
    let output = server
        .send_message(Parameters(SendMessageArgs {
            to: "bob".into(),
            message: "ping from the MCP tool".into(),
        }))
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(parsed["recipient_key"], "bob");
    assert_eq!(parsed["state"], "unread");

    let inbox = daemon
        .call(Method::Inbox, json!({ "session_key": "bob" }))
        .await
        .unwrap();
    let messages = inbox["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["body"], "ping from the MCP tool");
}

#[tokio::test]
async fn check_inbox_returns_messages_and_marks_them_read() {
    let daemon = TestDaemon::start().await;
    daemon
        .call(
            Method::Register,
            json!({ "session_key": "alice", "name": "alice" }),
        )
        .await
        .unwrap();
    daemon
        .call(
            Method::Register,
            json!({ "session_key": "bob", "name": "bob" }),
        )
        .await
        .unwrap();
    daemon
        .call(
            Method::SendMessage,
            json!({ "sender_key": "alice", "recipient_key": "bob", "body": "hello" }),
        )
        .await
        .unwrap();

    let server = server(&daemon, "bob");
    let output = server
        .check_inbox(Parameters(CheckInboxArgs {
            limit: Some(10),
            mark_read: Some(true),
        }))
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
    let messages = parsed["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["body"], "hello");

    let unread = daemon
        .call(Method::UnreadCount, json!({ "session_key": "bob" }))
        .await
        .unwrap();
    assert_eq!(unread["unread"], 0);
}

#[tokio::test]
async fn ask_user_creates_a_question_and_the_answer_reaches_the_inbox() {
    let daemon = TestDaemon::start().await;
    daemon
        .call(
            Method::Register,
            json!({ "session_key": "alice", "name": "alice" }),
        )
        .await
        .unwrap();

    let server = server(&daemon, "alice");
    let output = server
        .ask_user(Parameters(AskUserArgs {
            question: "shall I continue?".into(),
        }))
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
    let question_id = parsed["id"].as_str().unwrap().to_string();
    assert_eq!(parsed["status"], "pending");

    daemon
        .call(
            Method::AnswerQuestion,
            json!({ "question_id": question_id, "answer": "yes, continue" }),
        )
        .await
        .unwrap();

    let inbox = daemon
        .call(Method::Inbox, json!({ "session_key": "alice" }))
        .await
        .unwrap();
    let messages = inbox["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["sender_key"], "human");
    assert_eq!(messages[0]["body"], "yes, continue");
}

#[tokio::test]
async fn sending_to_an_unknown_session_fails() {
    let daemon = TestDaemon::start().await;
    daemon
        .call(
            Method::Register,
            json!({ "session_key": "alice", "name": "alice" }),
        )
        .await
        .unwrap();

    let server = server(&daemon, "alice");
    let error = server
        .send_message(Parameters(SendMessageArgs {
            to: "nobody".into(),
            message: "hello".into(),
        }))
        .await
        .unwrap_err();
    assert!(
        error.message.contains("not_found"),
        "unexpected error: {error:?}"
    );
}
