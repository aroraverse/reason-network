//! Lifecycle hooks for Claude Code and Codex.
//!
//! Both agents run `reason-network hook --agent <agent>` on `SessionStart`,
//! `UserPromptSubmit`, `Stop` and `SessionEnd`, passing one JSON object on
//! stdin. The hook keeps the session registry current (status, working
//! directory, repository, branch, conversation id) and, when the session has
//! unread messages, prints a short notice that both agents add to the model's
//! context. The messages themselves are read through the existing
//! `check_inbox` MCP tool, so read state stays in one place.
//!
//! A hook never fails the agent: every error is swallowed after a short
//! timeout and the process exits successfully.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::agents::{self, Agent};
use crate::config::{Config, Endpoint};
use crate::error::{Error, Result};
use crate::ipc;
use crate::protocol::{Method, Request, Response};

/// Upper bound for one hook invocation.
const HOOK_DEADLINE: Duration = Duration::from_secs(3);

/// The parts of a hook payload this integration uses.
#[derive(Debug, Clone, Default)]
pub struct HookEvent {
    /// `SessionStart`, `UserPromptSubmit`, `Stop`, `SessionEnd`, ...
    pub event: String,
    /// The agent's own conversation id.
    pub session_id: Option<String>,
    /// The agent's working directory.
    pub cwd: Option<PathBuf>,
    /// `startup`, `resume`, `clear`, `compact` (SessionStart only).
    pub source: Option<String>,
}

impl HookEvent {
    /// Parse the JSON payload an agent writes to the hook's stdin.
    pub fn parse(input: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(input.trim()).ok()?;
        let text = |field: &str| {
            value
                .get(field)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        Some(Self {
            event: text("hook_event_name")?,
            session_id: text("session_id"),
            cwd: text("cwd").map(PathBuf::from),
            source: text("source"),
        })
    }
}

/// Run one hook. Returns the text to print on stdout (possibly empty).
///
/// Never returns an error: a daemon that is down or slow must not break the
/// agent session.
pub async fn run(config: &Config, agent: Agent, input: &str) -> String {
    let Some(event) = HookEvent::parse(input) else {
        return String::new();
    };
    match tokio::time::timeout(HOOK_DEADLINE, handle(&config.endpoint, agent, &event)).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            eprintln!("[reason-network hook] {}: {error}", event.event);
            String::new()
        }
        Err(_) => {
            eprintln!(
                "[reason-network hook] {}: daemon did not answer",
                event.event
            );
            String::new()
        }
    }
}

/// Apply one lifecycle event to the session registry.
pub async fn handle(endpoint: &Endpoint, agent: Agent, event: &HookEvent) -> Result<String> {
    let workspace = agents::workspace(Some(agent), event.cwd.as_deref());
    let key = agents::session_key(agent, &workspace);

    match event.event.as_str() {
        "SessionStart" => {
            register(
                endpoint,
                agent,
                &key,
                &workspace,
                event,
                "idle",
                "session started",
            )
            .await?;
            let unread = unread_notice(endpoint, &key).await?;
            let mut output = format!(
                "Reason Network: this session is registered as `{key}`. Use the reason-network \
MCP tools: list_team to discover other agent sessions, send_message to message one, \
check_inbox to read messages sent to you, ask_user to ask the human operator."
            );
            if let Some(unread) = unread {
                output.push('\n');
                output.push_str(&unread);
            }
            Ok(output)
        }
        "UserPromptSubmit" => {
            update_or_register(endpoint, agent, &key, &workspace, event, "busy", "working").await?;
            Ok(unread_notice(endpoint, &key).await?.unwrap_or_default())
        }
        "Stop" => {
            update_or_register(endpoint, agent, &key, &workspace, event, "idle", "").await?;
            Ok(String::new())
        }
        "SessionEnd" => {
            update_or_register(endpoint, agent, &key, &workspace, event, "offline", "").await?;
            Ok(String::new())
        }
        _ => Ok(String::new()),
    }
}

async fn register(
    endpoint: &Endpoint,
    agent: Agent,
    key: &str,
    workspace: &Path,
    event: &HookEvent,
    status: &str,
    activity: &str,
) -> Result<()> {
    let (repo, branch) = git_info(workspace);
    call(
        endpoint,
        Method::Register,
        json!({
            "session_key": key,
            "name": agents::session_name(agent, workspace),
            "agent_type": agent.session_agent_type(),
            "agent_ref": event.session_id,
            "working_dir": workspace.display().to_string(),
            "repo": repo,
            "branch": branch,
            "status": status,
            "activity": activity,
        }),
    )
    .await
    .map(|_| ())
}

async fn update_or_register(
    endpoint: &Endpoint,
    agent: Agent,
    key: &str,
    workspace: &Path,
    event: &HookEvent,
    status: &str,
    activity: &str,
) -> Result<()> {
    let updated = call(
        endpoint,
        Method::Update,
        json!({
            "session_key": key,
            "status": status,
            "activity": activity,
            "agent_ref": event.session_id,
        }),
    )
    .await;
    match updated {
        Ok(_) => Ok(()),
        // The daemon may have been started on a fresh database since the
        // session began; register it again rather than losing track of it.
        Err(Error::NotFound(_)) => {
            register(endpoint, agent, key, workspace, event, status, activity).await
        }
        Err(error) => Err(error),
    }
}

/// A one-line notice about unread messages, or `None` when there are none.
async fn unread_notice(endpoint: &Endpoint, key: &str) -> Result<Option<String>> {
    let inbox = call(
        endpoint,
        Method::Inbox,
        json!({ "session_key": key, "unread_only": true, "limit": 50 }),
    )
    .await?;
    let messages = inbox
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if messages.is_empty() {
        return Ok(None);
    }
    let mut senders: Vec<String> = Vec::new();
    for message in &messages {
        if let Some(sender) = message.get("sender_key").and_then(Value::as_str)
            && !senders.iter().any(|known| known == sender)
        {
            senders.push(sender.to_string());
        }
    }
    let count = messages.len();
    let plural = if count == 1 { "" } else { "s" };
    Ok(Some(format!(
        "Reason Network: {count} unread message{plural} for `{key}` from {}. Call the \
reason-network check_inbox tool to read {}.",
        senders.join(", "),
        if count == 1 { "it" } else { "them" }
    )))
}

async fn call(endpoint: &Endpoint, method: Method, params: Value) -> Result<Value> {
    let request = Request {
        id: Some(format!("hook-{method:?}")),
        method,
        params,
    };
    let response: Response = ipc::request_once(endpoint, &request).await?;
    match (response.ok, response.result, response.error) {
        (true, Some(result), _) => Ok(result),
        (true, None, _) => Ok(Value::Null),
        (false, _, Some(error)) if error.code == "not_found" => Err(Error::NotFound(error.message)),
        (false, _, Some(error)) => Err(Error::Other(format!("{}: {}", error.code, error.message))),
        (false, _, None) => Err(Error::Other("daemon returned no result".into())),
    }
}

/// Repository name and current branch of the git checkout containing `dir`.
///
/// Reads `.git/HEAD` directly instead of running `git`, which keeps hooks
/// fast and works without git on PATH.
pub fn git_info(dir: &Path) -> (Option<String>, Option<String>) {
    for candidate in dir.ancestors() {
        let dot_git = candidate.join(".git");
        let git_dir = if dot_git.is_dir() {
            dot_git
        } else if dot_git.is_file() {
            // Worktrees and submodules: `.git` is a file `gitdir: <path>`.
            let Some(target) = std::fs::read_to_string(&dot_git).ok().and_then(|text| {
                text.trim()
                    .strip_prefix("gitdir:")
                    .map(|path| PathBuf::from(path.trim()))
            }) else {
                return (None, None);
            };
            if target.is_absolute() {
                target
            } else {
                candidate.join(target)
            }
        } else {
            continue;
        };
        let repo = candidate
            .file_name()
            .map(|name| name.to_string_lossy().to_string());
        let branch = std::fs::read_to_string(git_dir.join("HEAD"))
            .ok()
            .and_then(|head| {
                let head = head.trim();
                match head.strip_prefix("ref: refs/heads/") {
                    Some(branch) => Some(branch.to_string()),
                    None if head.len() >= 7 => Some(format!("detached@{}", &head[..7])),
                    None => None,
                }
            });
        return (repo, branch);
    }
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_and_codex_payloads() {
        let claude = HookEvent::parse(
            r#"{"session_id":"abc","transcript_path":"/t","cwd":"/work/app","hook_event_name":"SessionStart","source":"startup"}"#,
        )
        .unwrap();
        assert_eq!(claude.event, "SessionStart");
        assert_eq!(claude.session_id.as_deref(), Some("abc"));
        assert_eq!(claude.cwd, Some(PathBuf::from("/work/app")));
        assert_eq!(claude.source.as_deref(), Some("startup"));

        let codex = HookEvent::parse(
            r#"{"session_id":"t-1","cwd":"/w","hook_event_name":"Stop","model":"gpt","turn_id":"x"}"#,
        )
        .unwrap();
        assert_eq!(codex.event, "Stop");

        assert!(HookEvent::parse("not json").is_none());
        assert!(HookEvent::parse(r#"{"cwd":"/w"}"#).is_none());
    }

    #[test]
    fn git_info_reads_branch_and_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("my-repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(
            repo.join(".git").join("HEAD"),
            "ref: refs/heads/feature/x\n",
        )
        .unwrap();
        let (name, branch) = git_info(&repo.join("src"));
        assert_eq!(name.as_deref(), Some("my-repo"));
        assert_eq!(branch.as_deref(), Some("feature/x"));
    }
}
