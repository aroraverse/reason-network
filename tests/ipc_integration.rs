//! Integration tests that drive the daemon through the real local IPC
//! interface (Windows named pipes / Unix domain sockets).

mod common;

use reason_network::ipc;
use reason_network::protocol::{Method, Request, Response};
use serde_json::json;

use common::TestDaemon;

#[tokio::test]
async fn daemon_answers_status_over_ipc() {
    let daemon = TestDaemon::start().await;
    let status = daemon.call(Method::Status, json!({})).await.unwrap();
    assert_eq!(status["daemon"], "reason-network");
    assert!(status["pid"].as_u64().is_some());
    // A named pipe on Windows, `<data dir>/daemon.sock` on Unix.
    assert_eq!(
        status["endpoint"].as_str().unwrap(),
        daemon.config.endpoint.display()
    );
}

#[tokio::test]
async fn sessions_register_and_are_discoverable() {
    let daemon = TestDaemon::start().await;

    let alice = daemon
        .call(
            Method::Register,
            json!({
                "session_key": "alice",
                "name": "alice",
                "agent_type": "codex",
                "working_dir": "C:/work/alice",
                "repo": "reason-network",
                "branch": "main",
                "status": "busy",
                "activity": "writing tests",
            }),
        )
        .await
        .unwrap();
    assert_eq!(alice["session_key"], "alice");
    assert_eq!(alice["name"], "alice");
    assert_eq!(alice["status"], "busy");

    let bob = daemon
        .call(
            Method::Register,
            json!({ "session_key": "bob", "name": "bob" }),
        )
        .await
        .unwrap();
    assert_eq!(bob["session_key"], "bob");

    let sessions = daemon.call(Method::ListSessions, json!({})).await.unwrap();
    let sessions = sessions["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);

    let found = daemon
        .call(Method::GetSession, json!({ "session_key": "alice" }))
        .await
        .unwrap();
    assert_eq!(found["activity"], "writing tests");
    assert_eq!(found["branch"], "main");
}

#[tokio::test]
async fn duplicate_session_names_are_disambiguated() {
    let daemon = TestDaemon::start().await;
    daemon
        .call(
            Method::Register,
            json!({ "session_key": "a", "name": "worker" }),
        )
        .await
        .unwrap();
    let second = daemon
        .call(
            Method::Register,
            json!({ "session_key": "b", "name": "worker" }),
        )
        .await
        .unwrap();
    assert_eq!(second["name"], "worker-2");
}

#[tokio::test]
async fn messages_are_delivered_and_marked_read() {
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

    let sent = daemon
        .call(
            Method::SendMessage,
            json!({ "sender_key": "alice", "recipient_key": "bob", "body": "hello bob" }),
        )
        .await
        .unwrap();
    assert_eq!(sent["sender_key"], "alice");
    assert_eq!(sent["recipient_key"], "bob");
    assert_eq!(sent["state"], "unread");
    assert!(sent["id"].as_str().is_some());

    let unread = daemon
        .call(Method::UnreadCount, json!({ "session_key": "bob" }))
        .await
        .unwrap();
    assert_eq!(unread["unread"], 1);

    let inbox = daemon
        .call(
            Method::Inbox,
            json!({ "session_key": "bob", "unread_only": true }),
        )
        .await
        .unwrap();
    let messages = inbox["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["body"], "hello bob");

    let message_id = messages[0]["id"].as_str().unwrap().to_string();
    let marked = daemon
        .call(
            Method::MarkRead,
            json!({ "session_key": "bob", "message_ids": [message_id] }),
        )
        .await
        .unwrap();
    assert_eq!(marked["updated"], 1);

    let unread = daemon
        .call(Method::UnreadCount, json!({ "session_key": "bob" }))
        .await
        .unwrap();
    assert_eq!(unread["unread"], 0);
}

#[tokio::test]
async fn unknown_recipient_is_rejected() {
    let daemon = TestDaemon::start().await;
    daemon
        .call(
            Method::Register,
            json!({ "session_key": "alice", "name": "alice" }),
        )
        .await
        .unwrap();

    let error = daemon
        .call(
            Method::SendMessage,
            json!({ "sender_key": "alice", "recipient_key": "ghost", "body": "hi" }),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not_found"), "{error}");
}

#[tokio::test]
async fn invalid_request_is_rejected() {
    let daemon = TestDaemon::start().await;
    let error = daemon
        .call(Method::SendMessage, json!({ "sender_key": "alice" }))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("invalid_request"), "{error}");
}

#[tokio::test]
async fn state_survives_a_daemon_restart() {
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
            json!({ "sender_key": "alice", "recipient_key": "bob", "body": "persisted" }),
        )
        .await
        .unwrap();

    let restarted = daemon.restart().await;
    let sessions = restarted
        .call(Method::ListSessions, json!({}))
        .await
        .unwrap();
    assert_eq!(sessions["sessions"].as_array().unwrap().len(), 2);

    let inbox = restarted
        .call(Method::Inbox, json!({ "session_key": "bob" }))
        .await
        .unwrap();
    let messages = inbox["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["body"], "persisted");
}

#[tokio::test]
async fn several_clients_are_served_at_the_same_time() {
    let daemon = TestDaemon::start().await;
    daemon
        .call(
            Method::Register,
            json!({ "session_key": "alice", "name": "alice" }),
        )
        .await
        .unwrap();

    let mut tasks = Vec::new();
    for index in 0..8 {
        let endpoint = daemon.config.endpoint.clone();
        tasks.push(tokio::spawn(async move {
            let request = Request {
                id: Some(format!("client-{index}")),
                method: Method::Register,
                params: json!({ "session_key": format!("client-{index}"), "name": format!("client-{index}") }),
            };
            let response: Response = ipc::request_once(&endpoint, &request).await.unwrap();
            assert!(response.ok);
            assert_eq!(response.id.as_deref(), Some(format!("client-{index}").as_str()));
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }

    let sessions = daemon.call(Method::ListSessions, json!({})).await.unwrap();
    assert_eq!(sessions["sessions"].as_array().unwrap().len(), 9);
}

#[tokio::test]
async fn malformed_input_does_not_stop_the_daemon() {
    let daemon = TestDaemon::start().await;

    {
        let mut connection = ipc::connect(&daemon.config.endpoint).await.unwrap();
        connection.write_raw(b"this is not json\n").await.unwrap();
        let response: Response = connection.read_frame().await.unwrap().unwrap();
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "serialization_error");
    }

    // The daemon still answers afterwards.
    let status = daemon.call(Method::Status, json!({})).await.unwrap();
    assert_eq!(status["daemon"], "reason-network");
}

#[tokio::test]
async fn a_session_can_be_removed() {
    let daemon = TestDaemon::start().await;
    daemon
        .call(
            Method::Register,
            json!({ "session_key": "alice", "name": "alice" }),
        )
        .await
        .unwrap();
    let removed = daemon
        .call(Method::RemoveSession, json!({ "session_key": "alice" }))
        .await
        .unwrap();
    assert_eq!(removed["removed"], true);

    let sessions = daemon.call(Method::ListSessions, json!({})).await.unwrap();
    assert!(sessions["sessions"].as_array().unwrap().is_empty());
}
