//! Milestone 1 end-to-end tests.
//!
//! Every test starts two (or three) real daemons, each with its own data
//! directory and its own iroh identity, and connects them through the actual
//! QUIC transport. Relay and discovery are disabled so the suite stays fully
//! local and deterministic.

mod common;

use std::time::Duration;

use reason_network::protocol::Method;
use serde_json::{Value, json};

use common::TestDaemon;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Create a team on `admin`, invite `member` and join it.
async fn join_team(admin: &TestDaemon, member: &TestDaemon) -> String {
    let team = admin
        .call(Method::TeamCreate, json!({ "name": "core" }))
        .await
        .expect("team create");
    let team_id = team["team_id"].as_str().unwrap().to_string();

    let invitation = admin
        .call(
            Method::TeamInvite,
            json!({ "team_id": team_id, "ttl_seconds": 600 }),
        )
        .await
        .expect("team invite");
    let token = invitation["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("rn1:"), "unexpected token: {token}");

    let joined = member
        .call(Method::TeamJoin, json!({ "token": token }))
        .await
        .expect("team join");
    assert_eq!(joined["team_id"], team_id.as_str());

    team_id
}

async fn wait_connected(a: &TestDaemon, b: &TestDaemon) {
    let b_peer = b.peer_id().await;
    let a_peer = a.peer_id().await;
    a.wait_for("A to see B connected", CONNECT_TIMEOUT, || async {
        a.net_info().await["connected"]
            .as_array()
            .map(|peers| peers.iter().any(|peer| peer == &b_peer))
            .unwrap_or(false)
    })
    .await;
    b.wait_for("B to see A connected", CONNECT_TIMEOUT, || async {
        b.net_info().await["connected"]
            .as_array()
            .map(|peers| peers.iter().any(|peer| peer == &a_peer))
            .unwrap_or(false)
    })
    .await;
}

async fn wait_for_session(daemon: &TestDaemon, session_key: &str, local: bool) {
    daemon
        .wait_for(
            &format!("session {session_key} in the directory"),
            CONNECT_TIMEOUT,
            || async {
                let directory = daemon
                    .call(Method::Directory, json!({}))
                    .await
                    .expect("directory");
                directory["sessions"]
                    .as_array()
                    .map(|sessions| {
                        sessions.iter().any(|session| {
                            session["session_key"] == session_key
                                && session["local"].as_bool() == Some(local)
                        })
                    })
                    .unwrap_or(false)
            },
        )
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_invitation_joins_a_team_and_peers_connect() {
    let alice = TestDaemon::start().await;
    let bob = TestDaemon::start().await;

    let team_id = join_team(&alice, &bob).await;
    wait_connected(&alice, &bob).await;

    let members = alice
        .call(Method::TeamMembers, json!({ "team_id": team_id }))
        .await
        .expect("members");
    let members = members["members"].as_array().unwrap();
    assert_eq!(members.len(), 2, "both machines must be in the roster");
    assert!(members.iter().any(|member| {
        member["member_name"]
            .as_str()
            .is_some_and(|name| !name.is_empty())
    }));

    // The joiner has the same roster.
    let bob_teams = bob.call(Method::TeamList, json!({})).await.unwrap();
    assert_eq!(bob_teams["teams"][0]["team_id"], team_id.as_str());
    assert_eq!(bob_teams["teams"][0]["is_admin"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_use_of_the_same_invitation_is_refused() {
    let alice = TestDaemon::start().await;
    let bob = TestDaemon::start().await;
    let mallory = TestDaemon::start().await;

    let team = alice
        .call(Method::TeamCreate, json!({ "name": "core" }))
        .await
        .unwrap();
    let team_id = team["team_id"].as_str().unwrap().to_string();
    let invitation = alice
        .call(
            Method::TeamInvite,
            json!({ "team_id": team_id, "ttl_seconds": 600 }),
        )
        .await
        .unwrap();
    let token = invitation["token"].as_str().unwrap().to_string();

    bob.call(Method::TeamJoin, json!({ "token": token.clone() }))
        .await
        .expect("first use of the invitation must succeed");

    let error = mallory
        .call(Method::TeamJoin, json!({ "token": token }))
        .await
        .expect_err("a reused invitation must be refused");
    let message = error.to_string();
    assert!(
        message.contains("refused") || message.contains("not_a_member"),
        "unexpected error: {message}"
    );

    let teams_of_mallory = mallory.call(Method::TeamList, json!({})).await.unwrap();
    assert!(
        teams_of_mallory["teams"].as_array().unwrap().is_empty(),
        "a refused join must not leave a team behind"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unauthorised_peer_is_refused() {
    let alice = TestDaemon::start().await;
    let bob = TestDaemon::start().await;
    let stranger = TestDaemon::start().await;

    join_team(&alice, &bob).await;
    wait_connected(&alice, &bob).await;

    let addr = alice.addr_json().await;
    let error = stranger
        .call(Method::NetConnect, json!({ "addr": addr }))
        .await
        .expect_err("a machine without membership must be refused");
    let message = error.to_string();
    assert!(
        message.contains("refused") || message.contains("not_a_member"),
        "unexpected error: {message}"
    );
    let stranger_peer = stranger.peer_id().await;
    assert!(
        !alice.net_info().await["connected"]
            .as_array()
            .unwrap()
            .iter()
            .any(|peer| peer == &stranger_peer),
        "the refused peer must not appear as connected"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_are_synchronised_between_machines() {
    let alice = TestDaemon::start().await;
    let bob = TestDaemon::start().await;
    join_team(&alice, &bob).await;
    wait_connected(&alice, &bob).await;

    bob.call(
        Method::Register,
        json!({
            "session_key": "bob-agent",
            "name": "bob-agent",
            "agent_type": "claude",
            "status": "busy",
            "activity": "reviewing",
        }),
    )
    .await
    .unwrap();

    wait_for_session(&alice, "bob-agent", false).await;

    let directory = alice.call(Method::Directory, json!({})).await.unwrap();
    let remote = directory["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["session_key"] == "bob-agent")
        .expect("remote session");
    assert_eq!(remote["local"], false);
    assert_eq!(remote["status"], "busy");
    assert_eq!(remote["machine"], Value::String(bob.peer_id().await));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_crosses_machines_and_is_acknowledged() {
    let alice = TestDaemon::start().await;
    let bob = TestDaemon::start().await;
    join_team(&alice, &bob).await;
    wait_connected(&alice, &bob).await;

    alice
        .call(
            Method::Register,
            json!({ "session_key": "alice-agent", "name": "alice-agent" }),
        )
        .await
        .unwrap();
    bob.call(
        Method::Register,
        json!({ "session_key": "bob-agent", "name": "bob-agent" }),
    )
    .await
    .unwrap();

    wait_for_session(&alice, "bob-agent", false).await;

    let sent = alice
        .call(
            Method::SendMessage,
            json!({
                "sender_key": "alice-agent",
                "recipient_key": "bob-agent",
                "body": "hello from the other machine",
            }),
        )
        .await
        .expect("remote send");
    assert_eq!(sent["delivery_state"], "sent");
    let message_id = sent["id"].as_str().unwrap().to_string();

    bob.wait_for("the message to arrive", DELIVERY_TIMEOUT, || async {
        let inbox = bob
            .call(Method::Inbox, json!({ "session_key": "bob-agent" }))
            .await
            .expect("inbox");
        !inbox["messages"].as_array().unwrap().is_empty()
    })
    .await;

    let inbox = bob
        .call(Method::Inbox, json!({ "session_key": "bob-agent" }))
        .await
        .unwrap();
    let messages = inbox["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["body"], "hello from the other machine");
    assert_eq!(messages[0]["sender_key"], "alice-agent");

    alice
        .wait_for("the acknowledgement", DELIVERY_TIMEOUT, || async {
            let outbox = alice.call(Method::Outbox, json!({})).await.expect("outbox");
            outbox["messages"]
                .as_array()
                .map(|messages| {
                    messages.iter().any(|message| {
                        message["id"] == message_id.as_str() && message["delivery_state"] == "acked"
                    })
                })
                .unwrap_or(false)
        })
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_connected_team_is_stable_without_connection_churn() {
    let alice = TestDaemon::start().await;
    let bob = TestDaemon::start().await;
    join_team(&alice, &bob).await;
    wait_connected(&alice, &bob).await;

    // Let several reconnect ticks pass; a healthy link must not be replaced.
    tokio::time::sleep(Duration::from_secs(12)).await;

    for daemon in [&alice, &bob] {
        let info = daemon.net_info().await;
        assert_eq!(
            info["connected"].as_array().unwrap().len(),
            1,
            "exactly one peer connection is expected"
        );
        assert_eq!(
            info["replacements"].as_u64().unwrap(),
            0,
            "an established connection must not be replaced"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_dials_converge_to_one_connection() {
    let alice = TestDaemon::start().await;
    let bob = TestDaemon::start().await;
    join_team(&alice, &bob).await;
    wait_connected(&alice, &bob).await;

    // Restart both machines so neither holds a link, then dial each other at
    // the same moment. That is the case that used to replace connections.
    alice.stop().await;
    bob.stop().await;
    let alice = alice.restart().await;
    let bob = bob.restart().await;

    let alice_addr = alice.addr_json().await;
    let bob_addr = bob.addr_json().await;
    let (from_alice, from_bob) = tokio::join!(
        alice.call(Method::NetConnect, json!({ "addr": bob_addr })),
        bob.call(Method::NetConnect, json!({ "addr": alice_addr })),
    );
    assert!(from_alice.is_ok(), "alice dial failed: {from_alice:?}");
    assert!(from_bob.is_ok(), "bob dial failed: {from_bob:?}");

    wait_connected(&alice, &bob).await;

    // Wait until the duplicate connections have converged and the counters
    // stop moving. Each machine counts its own replacements, so the two
    // snapshots are tracked separately.
    let mut settled = (u64::MAX, u64::MAX);
    for _ in 0..40 {
        let alice_info = alice.net_info().await;
        let bob_info = bob.net_info().await;
        let current = (
            alice_info["replacements"].as_u64().unwrap(),
            bob_info["replacements"].as_u64().unwrap(),
        );
        let both_single = alice_info["connected"].as_array().unwrap().len() == 1
            && bob_info["connected"].as_array().unwrap().len() == 1;
        if both_single && current == settled {
            break;
        }
        settled = current;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    assert!(
        settled.0 <= 2 && settled.1 <= 2,
        "simultaneous dials must converge, saw {settled:?} replacements"
    );

    // No ping-pong afterwards: several reconnect ticks must change nothing.
    tokio::time::sleep(Duration::from_secs(12)).await;
    let alice_info = alice.net_info().await;
    let bob_info = bob.net_info().await;
    assert_eq!(
        alice_info["replacements"].as_u64().unwrap(),
        settled.0,
        "alice must not keep replacing her connection"
    );
    assert_eq!(
        bob_info["replacements"].as_u64().unwrap(),
        settled.1,
        "bob must not keep replacing his connection"
    );
    assert_eq!(alice_info["connected"].as_array().unwrap().len(), 1);
    assert_eq!(bob_info["connected"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreachable_peer_never_has_two_dials_in_flight() {
    let alice = TestDaemon::start().await;
    let bob = TestDaemon::start().await;
    join_team(&alice, &bob).await;
    wait_connected(&alice, &bob).await;

    // Bob leaves; Alice keeps trying to reach it while it is away.
    bob.stop().await;

    let mut max_in_flight = 0;
    for _ in 0..40 {
        let info = alice.net_info().await;
        let in_flight = info["reconnecting"].as_u64().unwrap_or(0);
        max_in_flight = max_in_flight.max(in_flight);
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    assert!(
        max_in_flight <= 1,
        "at most one dial per peer may run at a time, saw {max_in_flight}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_can_revoke_a_member() {
    let alice = TestDaemon::start().await;
    let bob = TestDaemon::start().await;
    let team_id = join_team(&alice, &bob).await;
    wait_connected(&alice, &bob).await;

    let bob_peer = bob.peer_id().await;
    let revoked = alice
        .call(
            Method::TeamRevoke,
            json!({ "team_id": team_id, "peer_id": bob_peer }),
        )
        .await
        .expect("admin revoke");
    assert_eq!(revoked["revoked"], true);
    assert_eq!(revoked["peer_id"], bob_peer.as_str());

    // The roster drops the member and the revocation is visible.
    let members = alice
        .call(Method::TeamMembers, json!({ "team_id": team_id }))
        .await
        .unwrap();
    assert!(
        members["members"]
            .as_array()
            .unwrap()
            .iter()
            .all(|member| member["peer_id"] != bob_peer.as_str()),
        "a revoked peer must disappear from the roster"
    );
    assert!(
        members["revoked"]
            .as_array()
            .unwrap()
            .iter()
            .any(|peer| peer == bob_peer.as_str())
    );

    // The live connection is closed.
    alice
        .wait_for(
            "alice to drop the revoked peer",
            CONNECT_TIMEOUT,
            || async {
                alice.net_info().await["connected"]
                    .as_array()
                    .map(|peers| peers.is_empty())
                    .unwrap_or(false)
            },
        )
        .await;

    // And a reconnect is refused, even though bob still holds a valid record.
    // Wait until bob has processed the closed connection, otherwise the
    // explicit connect just reports the stale link as connected.
    bob.wait_for(
        "bob to notice the closed connection",
        CONNECT_TIMEOUT,
        || async {
            bob.net_info().await["connected"]
                .as_array()
                .map(|peers| peers.is_empty())
                .unwrap_or(false)
        },
    )
    .await;

    let alice_addr = alice.addr_json().await;
    let error = bob
        .call(Method::NetConnect, json!({ "addr": alice_addr }))
        .await
        .expect_err("a revoked peer must be refused");
    assert!(
        error.to_string().contains("revoked"),
        "unexpected error: {error}"
    );

    // A fresh invitation does not re-admit a revoked peer in M1.
    let invitation = alice
        .call(
            Method::TeamInvite,
            json!({ "team_id": team_id, "ttl_seconds": 600 }),
        )
        .await
        .unwrap();
    let token = invitation["token"].as_str().unwrap().to_string();
    let error = bob
        .call(Method::TeamJoin, json!({ "token": token }))
        .await
        .expect_err("revocation wins over a new invitation");
    assert!(
        error.to_string().contains("revoked"),
        "unexpected error: {error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_admin_can_revoke() {
    let alice = TestDaemon::start().await;
    let bob = TestDaemon::start().await;
    let team_id = join_team(&alice, &bob).await;
    wait_connected(&alice, &bob).await;

    let alice_peer = alice.peer_id().await;
    let error = bob
        .call(
            Method::TeamRevoke,
            json!({ "team_id": team_id, "peer_id": alice_peer }),
        )
        .await
        .expect_err("a member must not revoke anyone");
    assert!(
        error.to_string().contains("only the team admin"),
        "unexpected error: {error}"
    );

    // Nothing changed.
    let members = alice
        .call(Method::TeamMembers, json!({ "team_id": team_id }))
        .await
        .unwrap();
    assert_eq!(members["members"].as_array().unwrap().len(), 2);
    assert!(members["revoked"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revocation_propagates_to_other_members() {
    let alice = TestDaemon::start().await;
    let bob = TestDaemon::start().await;
    let carol = TestDaemon::start().await;

    let team_id = join_team(&alice, &bob).await;
    wait_connected(&alice, &bob).await;

    let invitation = alice
        .call(
            Method::TeamInvite,
            json!({ "team_id": team_id, "ttl_seconds": 600 }),
        )
        .await
        .unwrap();
    let token = invitation["token"].as_str().unwrap().to_string();
    carol
        .call(Method::TeamJoin, json!({ "token": token }))
        .await
        .expect("carol joins");
    wait_connected(&alice, &carol).await;

    let carol_peer = carol.peer_id().await;
    alice
        .call(
            Method::TeamRevoke,
            json!({ "team_id": team_id, "peer_id": carol_peer }),
        )
        .await
        .expect("admin revoke");

    // Bob learns about the revocation through the pushed membership state.
    bob.wait_for(
        "bob to receive the revocation",
        DELIVERY_TIMEOUT,
        || async {
            let members = bob
                .call(Method::TeamMembers, json!({ "team_id": team_id }))
                .await
                .expect("members");
            members["revoked"]
                .as_array()
                .map(|revoked| revoked.iter().any(|peer| peer == carol_peer.as_str()))
                .unwrap_or(false)
        },
    )
    .await;

    // Carol can no longer connect to Bob either.
    let bob_addr = bob.addr_json().await;
    let error = carol
        .call(Method::NetConnect, json!({ "addr": bob_addr }))
        .await
        .expect_err("carol must be refused by bob");
    assert!(
        error.to_string().contains("revoked"),
        "unexpected error: {error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pending_message_is_retried_after_the_peer_returns() {
    let alice = TestDaemon::start().await;
    let bob = TestDaemon::start().await;
    join_team(&alice, &bob).await;
    wait_connected(&alice, &bob).await;

    alice
        .call(
            Method::Register,
            json!({ "session_key": "alice-agent", "name": "alice-agent" }),
        )
        .await
        .unwrap();
    bob.call(
        Method::Register,
        json!({ "session_key": "bob-agent", "name": "bob-agent" }),
    )
    .await
    .unwrap();
    wait_for_session(&alice, "bob-agent", false).await;

    // Bob goes away.
    bob.stop().await;
    alice
        .wait_for("A to notice the disconnect", CONNECT_TIMEOUT, || async {
            alice.net_info().await["connected"]
                .as_array()
                .map(|peers| peers.is_empty())
                .unwrap_or(false)
        })
        .await;

    let sent = alice
        .call(
            Method::SendMessage,
            json!({
                "sender_key": "alice-agent",
                "recipient_key": "bob-agent",
                "body": "please pick this up when you are back",
            }),
        )
        .await
        .expect("the message must be stored even while the peer is away");
    assert_eq!(sent["delivery_state"], "pending");
    let message_id = sent["id"].as_str().unwrap().to_string();

    // Bob comes back on the same data directory, so identity and membership persist.
    let bob = bob.restart().await;

    // Direct addresses change on restart; refresh the address out of band.
    let addr = bob.addr_json().await;
    alice
        .call(Method::NetConnect, json!({ "addr": addr }))
        .await
        .expect("reconnect");
    wait_connected(&alice, &bob).await;

    bob.wait_for(
        "the queued message to be delivered",
        DELIVERY_TIMEOUT,
        || async {
            let inbox = bob
                .call(Method::Inbox, json!({ "session_key": "bob-agent" }))
                .await
                .expect("inbox");
            inbox["messages"]
                .as_array()
                .map(|messages| messages.len() == 1)
                .unwrap_or(false)
        },
    )
    .await;

    // Retries must not duplicate the message.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let inbox = bob
        .call(Method::Inbox, json!({ "session_key": "bob-agent" }))
        .await
        .unwrap();
    assert_eq!(
        inbox["messages"].as_array().unwrap().len(),
        1,
        "a retried message must be stored exactly once"
    );

    alice
        .wait_for(
            "the retried message to be acknowledged",
            DELIVERY_TIMEOUT,
            || async {
                let outbox = alice.call(Method::Outbox, json!({})).await.expect("outbox");
                outbox["messages"]
                    .as_array()
                    .map(|messages| {
                        messages.iter().any(|message| {
                            message["id"] == message_id.as_str()
                                && message["delivery_state"] == "acked"
                        })
                    })
                    .unwrap_or(false)
            },
        )
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identity_team_and_messages_survive_a_restart() {
    let alice = TestDaemon::start().await;
    let bob = TestDaemon::start().await;
    let team_id = join_team(&alice, &bob).await;
    wait_connected(&alice, &bob).await;

    alice
        .call(
            Method::Register,
            json!({ "session_key": "alice-agent", "name": "alice-agent" }),
        )
        .await
        .unwrap();
    bob.call(
        Method::Register,
        json!({ "session_key": "bob-agent", "name": "bob-agent" }),
    )
    .await
    .unwrap();
    wait_for_session(&alice, "bob-agent", false).await;

    let peer_before = alice.peer_id().await;
    alice
        .call(
            Method::SendMessage,
            json!({
                "sender_key": "alice-agent",
                "recipient_key": "bob-agent",
                "body": "persist me",
            }),
        )
        .await
        .unwrap();
    bob.wait_for("delivery before restart", DELIVERY_TIMEOUT, || async {
        let inbox = bob
            .call(Method::Inbox, json!({ "session_key": "bob-agent" }))
            .await
            .unwrap();
        !inbox["messages"].as_array().unwrap().is_empty()
    })
    .await;

    let alice = alice.restart().await;

    // The machine identity is the same key, so the peer id is unchanged.
    assert_eq!(alice.peer_id().await, peer_before);

    let teams = alice.call(Method::TeamList, json!({})).await.unwrap();
    assert_eq!(teams["teams"][0]["team_id"], team_id.as_str());
    let members = alice
        .call(Method::TeamMembers, json!({ "team_id": team_id }))
        .await
        .unwrap();
    assert_eq!(members["members"].as_array().unwrap().len(), 2);

    let sessions = alice.call(Method::ListSessions, json!({})).await.unwrap();
    assert_eq!(sessions["sessions"][0]["session_key"], "alice-agent");
}
