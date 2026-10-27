//! Lifecycle hooks, agent-derived MCP session identity and `doctor`, against
//! a real daemon on a temporary data directory.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use reason_network::agents::{self, Agent, Host};
use reason_network::doctor::{self, Level};
use reason_network::hook::{self, HookEvent};
use reason_network::protocol::Method;
use reason_network::service::ServiceSpec;
use serde_json::{Value, json};

use common::TestDaemon;

fn event(name: &str, cwd: &Path) -> HookEvent {
    HookEvent::parse(
        &json!({
            "hook_event_name": name,
            "session_id": "conv-123",
            "cwd": cwd.display().to_string(),
            "source": "startup",
        })
        .to_string(),
    )
    .unwrap()
}

/// The key the hook derives for `cwd` (Claude prefers CLAUDE_PROJECT_DIR).
fn expected_key(agent: Agent, cwd: &Path) -> String {
    agents::session_key(agent, &agents::workspace(Some(agent), Some(cwd)))
}

async fn session(daemon: &TestDaemon, key: &str) -> Value {
    daemon
        .call(Method::GetSession, json!({ "session_key": key }))
        .await
        .unwrap()
}

fn workspace() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("demo-repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
    (dir, repo)
}

#[tokio::test]
async fn hooks_track_the_session_lifecycle() {
    let daemon = TestDaemon::start().await;
    let (_dir, repo) = workspace();
    let endpoint = &daemon.config.endpoint;
    let key = expected_key(Agent::Codex, &repo);

    let output = hook::handle(endpoint, Agent::Codex, &event("SessionStart", &repo))
        .await
        .unwrap();
    assert!(output.contains(&key), "{output}");
    assert!(output.contains("check_inbox"), "{output}");
    let registered = session(&daemon, &key).await;
    assert_eq!(registered["agent_type"], "codex");
    assert_eq!(registered["agent_ref"], "conv-123");
    assert_eq!(registered["repo"], "demo-repo");
    assert_eq!(registered["branch"], "main");
    assert_eq!(registered["status"], "idle");
    assert_eq!(registered["name"], "codex-demo-repo");

    let output = hook::handle(endpoint, Agent::Codex, &event("UserPromptSubmit", &repo))
        .await
        .unwrap();
    assert!(output.is_empty(), "no unread messages, no notice: {output}");
    assert_eq!(session(&daemon, &key).await["status"], "busy");

    hook::handle(endpoint, Agent::Codex, &event("Stop", &repo))
        .await
        .unwrap();
    assert_eq!(session(&daemon, &key).await["status"], "idle");

    hook::handle(endpoint, Agent::Codex, &event("SessionEnd", &repo))
        .await
        .unwrap();
    assert_eq!(session(&daemon, &key).await["status"], "offline");

    // Unknown events are ignored.
    let output = hook::handle(endpoint, Agent::Codex, &event("PreToolUse", &repo))
        .await
        .unwrap();
    assert!(output.is_empty());
}

#[tokio::test]
async fn hooks_announce_unread_messages_without_consuming_them() {
    let daemon = TestDaemon::start().await;
    let (_dir, repo) = workspace();
    let endpoint = &daemon.config.endpoint;
    let key = expected_key(Agent::Claude, &repo);
    hook::handle(endpoint, Agent::Claude, &event("SessionStart", &repo))
        .await
        .unwrap();
    // Interactive Claude Code sessions are not eligible for headless push.
    assert_eq!(session(&daemon, &key).await["agent_type"], "claude-code");

    daemon
        .call(
            Method::Register,
            json!({ "session_key": "alice", "name": "alice" }),
        )
        .await
        .unwrap();
    daemon
        .call(
            Method::SendMessage,
            json!({ "sender_key": "alice", "recipient_key": key, "body": "please review" }),
        )
        .await
        .unwrap();

    let output = hook::handle(endpoint, Agent::Claude, &event("UserPromptSubmit", &repo))
        .await
        .unwrap();
    assert!(output.contains("1 unread message"), "{output}");
    assert!(output.contains("from alice"), "{output}");
    assert!(
        !output.contains("please review"),
        "the body is read via check_inbox"
    );

    // Still unread: the hook only announces.
    let unread = daemon
        .call(Method::UnreadCount, json!({ "session_key": key }))
        .await
        .unwrap();
    assert_eq!(unread["unread"], 1);
}

#[tokio::test]
async fn hooks_never_fail_when_the_daemon_is_down() {
    let daemon = TestDaemon::start().await;
    daemon.stop().await;
    let (_dir, repo) = workspace();
    let input = json!({ "hook_event_name": "SessionStart", "cwd": repo.display().to_string() });
    let started = std::time::Instant::now();
    let output = hook::run(&daemon.config, Agent::Claude, &input.to_string()).await;
    assert!(output.is_empty());
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        hook::run(&daemon.config, Agent::Claude, "garbage")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn hook_recovers_a_session_lost_by_the_daemon() {
    let daemon = TestDaemon::start().await;
    let (_dir, repo) = workspace();
    let key = expected_key(Agent::Codex, &repo);
    // No SessionStart (e.g. the daemon started on a fresh database mid-session).
    hook::handle(
        &daemon.config.endpoint,
        Agent::Codex,
        &event("UserPromptSubmit", &repo),
    )
    .await
    .unwrap();
    assert_eq!(session(&daemon, &key).await["status"], "busy");
}

/// The MCP server launched by an agent derives the same session key as the
/// agent's hooks, and does not clobber what the hooks recorded.
#[tokio::test]
async fn mcp_server_and_hooks_share_one_identity() {
    let daemon = TestDaemon::start().await;
    let (_dir, repo) = workspace();
    let key = expected_key(Agent::Codex, &repo);
    hook::handle(
        &daemon.config.endpoint,
        Agent::Codex,
        &event("UserPromptSubmit", &repo),
    )
    .await
    .unwrap();

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_reason-network"))
        .args(["mcp", "--agent", "codex"])
        .current_dir(&repo)
        .env("REASON_NETWORK_DATA_DIR", daemon.data_dir())
        .env("REASON_NETWORK_NET", "0")
        .env_remove("REASON_NETWORK_SESSION_KEY")
        .env_remove("REASON_NETWORK_SESSION_NAME")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    // Give the server time to run its registration check.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let sessions = daemon.call(Method::ListSessions, json!({})).await.unwrap();
    let sessions = sessions["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "one session, not two: {sessions:?}");
    assert_eq!(sessions[0]["session_key"], key);
    assert_eq!(sessions[0]["status"], "busy", "hook state preserved");
    assert_eq!(sessions[0]["branch"], "main");
    let _ = child.kill().await;

    // Without hooks, the MCP server registers the derived session itself.
    let other = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_reason-network"))
        .args(["mcp", "--agent", "codex"])
        .current_dir(other.path())
        .env("REASON_NETWORK_DATA_DIR", daemon.data_dir())
        .env_remove("REASON_NETWORK_SESSION_KEY")
        .env_remove("REASON_NETWORK_SESSION_NAME")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let derived = agents::session_key(Agent::Codex, other.path());
    let mut found = Value::Null;
    for _ in 0..50 {
        if let Ok(session) = daemon
            .call(Method::GetSession, json!({ "session_key": derived }))
            .await
        {
            found = session;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(found["agent_type"], "codex", "derived session registered");
    let _ = child.kill().await;
}

#[test]
fn explicit_session_keys_still_win() {
    assert_eq!(
        reason_network::mcp::resolve_session_key(Some("agent-mcp".into()), Some(Agent::Claude))
            .unwrap(),
        "agent-mcp"
    );
}

fn fake_host(data_dir: &Path, with_claude: bool) -> (tempfile::TempDir, Host) {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    if with_claude {
        let name = if cfg!(windows) {
            "claude.cmd"
        } else {
            "claude"
        };
        std::fs::write(bin.join(name), "").unwrap();
    }
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_reason-network"));
    let host = Host {
        home: dir.path().join("home"),
        path: vec![bin, exe.parent().unwrap().to_path_buf()],
        claude_config_dir: None,
        codex_home: None,
        exe,
        data_dir: Some(data_dir.to_path_buf()),
    };
    (dir, host)
}

fn find<'a>(checks: &'a [doctor::Check], name: &str) -> &'a doctor::Check {
    checks
        .iter()
        .find(|check| check.name == name)
        .unwrap_or_else(|| panic!("no check {name}: {checks:?}"))
}

#[tokio::test]
async fn doctor_reports_daemon_and_configuration_state() {
    let daemon = TestDaemon::start().await;
    let (_home, host) = fake_host(daemon.data_dir(), true);
    let spec = ServiceSpec::new(&daemon.config, host.exe.clone(), true);

    // Create secrets that must never be printed.
    let team = daemon
        .call(Method::TeamCreate, json!({ "name": "core" }))
        .await
        .unwrap();
    let invite = daemon
        .call(
            Method::TeamInvite,
            json!({ "team_id": team["team_id"], "ttl_seconds": 600 }),
        )
        .await
        .unwrap();
    let token = invite["token"].as_str().unwrap().to_string();
    let key = std::fs::read_to_string(daemon.data_dir().join("machine.key")).unwrap();

    let checks = doctor::run(&daemon.config, &host, &spec).await;
    assert_eq!(find(&checks, "daemon").level, Level::Ok);
    assert_eq!(find(&checks, "database").level, Level::Ok);
    assert_eq!(find(&checks, "machine identity").level, Level::Ok);
    assert!(find(&checks, "network").detail.contains("1 team(s)"));
    assert_eq!(find(&checks, "claude CLI").level, Level::Ok);
    let config = find(&checks, "claude MCP config");
    assert_eq!(config.level, Level::Warn, "{config:?}");
    assert_eq!(
        config.fix.as_deref(),
        Some("reason-network setup --agent claude")
    );
    assert_eq!(find(&checks, "service").level, Level::Ok);

    let rendered = doctor::render(&checks) + &serde_json::to_string(&checks).unwrap();
    assert!(!rendered.contains(&token), "invitation token leaked");
    assert!(!rendered.contains(key.trim()), "machine key leaked");

    // After setup the configuration check passes.
    agents::run_setup(&host, &[Agent::Claude], false)
        .into_result()
        .unwrap();
    let checks = doctor::run(&daemon.config, &host, &spec).await;
    assert_eq!(find(&checks, "claude MCP config").level, Level::Ok);

    // A project-scoped entry shadows the user-scope server in that project.
    let path = host.claude_json();
    let mut claude: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    claude["projects"] = json!({
        "/work/app": { "mcpServers": { "reason-network": { "command": "old" } } }
    });
    std::fs::write(&path, claude.to_string()).unwrap();
    let checks = doctor::run(&daemon.config, &host, &spec).await;
    let check = find(&checks, "claude MCP config");
    assert_eq!(check.level, Level::Warn, "{check:?}");
    assert!(check.detail.contains("/work/app"), "{check:?}");
}

#[tokio::test]
async fn doctor_reports_a_stopped_daemon_as_a_failure() {
    let daemon = TestDaemon::start().await;
    daemon.stop().await;
    let (_home, host) = fake_host(daemon.data_dir(), false);
    let spec = ServiceSpec::new(&daemon.config, host.exe.clone(), true);
    let checks = doctor::run(&daemon.config, &host, &spec).await;
    let check = find(&checks, "daemon");
    assert_eq!(check.level, Level::Fail);
    assert!(check.fix.as_deref().unwrap().contains("service start"));
    // The database is still checked, read-only.
    assert_eq!(find(&checks, "database").level, Level::Ok);
    assert_eq!(find(&checks, "claude CLI").level, Level::Warn);
    assert!(
        checks.iter().all(|check| check.name != "claude MCP config"),
        "no config check for an agent that is neither installed nor configured"
    );
}

#[tokio::test]
async fn doctor_flags_a_configuration_pointing_at_a_missing_executable() {
    let daemon = TestDaemon::start().await;
    let (_home, mut host) = fake_host(daemon.data_dir(), true);
    let real = host.exe.clone();
    host.exe = host.home.join("gone").join("reason-network");
    agents::run_setup(&host, &[Agent::Claude], false)
        .into_result()
        .unwrap();
    host.exe = real;
    let spec = ServiceSpec::new(&daemon.config, host.exe.clone(), true);
    let checks = doctor::run(&daemon.config, &host, &spec).await;
    let check = find(&checks, "claude MCP config");
    assert_eq!(check.level, Level::Fail, "{check:?}");
    assert!(check.detail.contains("does not exist"));
}
