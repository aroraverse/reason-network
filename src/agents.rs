//! First-class coding-agent integrations (M2).
//!
//! `reason-network setup` configures two agents:
//!
//! * **Claude Code** — the MCP server goes into the user-scope `mcpServers`
//!   of `~/.claude.json` (the file `claude mcp add --scope user` writes), and
//!   lifecycle hooks go into `~/.claude/settings.json`.
//! * **OpenAI Codex CLI** — the MCP server goes into `[mcp_servers]` of
//!   `~/.codex/config.toml` and lifecycle hooks into its `[hooks]` tables.
//!
//! Both honour the agents' own overrides (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`).
//! Only the `reason-network` entries are added or replaced; everything else
//! in those files is preserved, a backup is written before any change, and a
//! repeated run is a no-op.
//!
//! The session identity is derived from the agent and its workspace
//! directory, so the MCP server (started by the agent) and the lifecycle hooks
//! (run by the agent) independently arrive at the same session key without
//! sharing any state.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::error::{Error, Result};
use crate::util;

/// Name of the MCP server entry in every agent configuration.
pub const SERVER_NAME: &str = "reason-network";

/// Hook timeout written into agent configurations, in seconds.
const HOOK_TIMEOUT_SECS: i64 = 10;
/// Codex caps `SessionEnd` hooks at three seconds.
const CODEX_SESSION_END_TIMEOUT_SECS: i64 = 3;

/// A coding agent with a first-class integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    /// Anthropic Claude Code.
    Claude,
    /// OpenAI Codex CLI.
    Codex,
}

impl Agent {
    /// Every supported agent, in setup order.
    pub const ALL: [Agent; 2] = [Agent::Claude, Agent::Codex];

    /// Parse the `--agent` value. `all` expands to every supported agent.
    pub fn parse_selection(text: &str) -> Result<Vec<Agent>> {
        match text.trim().to_ascii_lowercase().as_str() {
            "all" => Ok(Self::ALL.to_vec()),
            other => Self::parse(other).map(|agent| vec![agent]),
        }
    }

    /// Parse a single agent name.
    pub fn parse(text: &str) -> Result<Agent> {
        match text.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Ok(Agent::Claude),
            "codex" | "codex-cli" => Ok(Agent::Codex),
            other => Err(Error::InvalidRequest(format!(
                "unsupported agent `{other}`: supported values are `claude`, `codex` and `all`. \
Any other MCP-compatible agent can run `reason-network mcp --session-key <key>` as a stdio MCP server."
            ))),
        }
    }

    /// Short identifier used on the command line and in session keys.
    pub fn id(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    /// Human readable product name.
    pub fn display_name(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "OpenAI Codex CLI",
        }
    }

    /// The `agent_type` recorded for sessions registered by this integration.
    ///
    /// Claude Code sessions use `claude-code`, not `claude`: `claude` is the
    /// M0 type for daemon-hosted headless sessions that receive push delivery,
    /// and an interactive session must never be resumed by the daemon behind
    /// the user's back.
    pub fn session_agent_type(self) -> &'static str {
        match self {
            Agent::Claude => "claude-code",
            Agent::Codex => "codex",
        }
    }

    /// Executable name looked up on `PATH`.
    pub fn executable_name(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    /// What to do when the agent is not installed.
    pub fn install_hint(self) -> &'static str {
        match self {
            Agent::Claude => {
                "install Claude Code (https://docs.claude.com/en/docs/claude-code/setup) and make sure `claude` is on PATH, then re-run setup"
            }
            Agent::Codex => {
                "install the Codex CLI (https://developers.openai.com/codex/cli, e.g. `npm install -g @openai/codex`) and make sure `codex` is on PATH, then re-run setup"
            }
        }
    }

    /// Lifecycle events this integration hooks into.
    pub fn hook_events(self) -> &'static [&'static str] {
        &["SessionStart", "UserPromptSubmit", "Stop", "SessionEnd"]
    }

    fn hook_timeout(self, event: &str) -> i64 {
        match (self, event) {
            (Agent::Codex, "SessionEnd") => CODEX_SESSION_END_TIMEOUT_SECS,
            _ => HOOK_TIMEOUT_SECS,
        }
    }
}

/// The user environment setup operates on.
///
/// Everything is explicit so tests can point setup at temporary directories
/// instead of the real home directory.
#[derive(Debug, Clone)]
pub struct Host {
    /// The user's home directory.
    pub home: PathBuf,
    /// Directories searched for agent executables.
    pub path: Vec<PathBuf>,
    /// `CLAUDE_CONFIG_DIR`, when set.
    pub claude_config_dir: Option<PathBuf>,
    /// `CODEX_HOME`, when set.
    pub codex_home: Option<PathBuf>,
    /// The `reason-network` executable agents should launch.
    pub exe: PathBuf,
    /// A non-default data directory to pin into the agent configuration.
    pub data_dir: Option<PathBuf>,
}

impl Host {
    /// Describe the real environment of the current user.
    pub fn from_env(data_dir: Option<PathBuf>) -> Result<Self> {
        let exe = std::env::current_exe()
            .map_err(|error| Error::Other(format!("cannot locate this executable: {error}")))?;
        let exe = std::fs::canonicalize(&exe)
            .map(strip_verbatim)
            .unwrap_or(exe);
        Ok(Self {
            home: util::home_dir(),
            path: util::path_dirs(),
            claude_config_dir: env_path("CLAUDE_CONFIG_DIR"),
            codex_home: env_path("CODEX_HOME"),
            exe,
            data_dir,
        })
    }

    /// `~/.claude.json`, or `$CLAUDE_CONFIG_DIR/.claude.json`.
    pub fn claude_json(&self) -> PathBuf {
        match &self.claude_config_dir {
            Some(dir) => dir.join(".claude.json"),
            None => self.home.join(".claude.json"),
        }
    }

    /// `~/.claude/settings.json`, or `$CLAUDE_CONFIG_DIR/settings.json`.
    pub fn claude_settings(&self) -> PathBuf {
        self.claude_config_dir
            .clone()
            .unwrap_or_else(|| self.home.join(".claude"))
            .join("settings.json")
    }

    /// `~/.codex/config.toml`, or `$CODEX_HOME/config.toml`.
    pub fn codex_config(&self) -> PathBuf {
        self.codex_home
            .clone()
            .unwrap_or_else(|| self.home.join(".codex"))
            .join("config.toml")
    }

    /// The installed executable of an agent, if any.
    pub fn find_agent(&self, agent: Agent) -> Option<PathBuf> {
        util::find_executable(agent.executable_name(), &self.path)
    }

    /// The configuration files an agent integration touches.
    pub fn config_files(&self, agent: Agent) -> Vec<PathBuf> {
        match agent {
            Agent::Claude => vec![self.claude_json(), self.claude_settings()],
            Agent::Codex => vec![self.codex_config()],
        }
    }

    /// The command line an agent runs for every lifecycle hook.
    pub fn hook_command(&self, agent: Agent) -> String {
        let mut command = format!("{} hook --agent {}", quote(&self.exe), agent.id());
        if let Some(dir) = &self.data_dir {
            command.push_str(&format!(" --data-dir {}", quote(dir)));
        }
        command
    }

    fn mcp_args(&self, agent: Agent) -> Vec<String> {
        vec!["mcp".into(), "--agent".into(), agent.id().into()]
    }

    fn mcp_env(&self) -> Vec<(String, String)> {
        self.data_dir
            .iter()
            .map(|dir| {
                (
                    crate::config::ENV_DATA_DIR.to_string(),
                    dir.display().to_string(),
                )
            })
            .collect()
    }

    fn exe_text(&self) -> String {
        self.exe.display().to_string()
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Remove the `\\?\` prefix `canonicalize` adds on Windows.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path,
    }
}

/// Quote a path for a hook command line.
///
/// Hook commands are run by a shell the agent chooses (a POSIX shell, Git
/// Bash or cmd on Windows). Forward slashes and double quotes work in all of
/// them; quotes are only added when the path needs them.
fn quote(path: &Path) -> String {
    let mut text = path.display().to_string();
    if cfg!(windows) {
        text = text.replace('\\', "/");
    }
    let plain = text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-:+@".contains(c));
    if plain { text } else { format!("\"{text}\"") }
}

/// True when a hook command was written by this integration.
fn is_our_hook(command: &str, agent: Agent) -> bool {
    command.contains("reason-network") && command.contains(&format!("hook --agent {}", agent.id()))
}

// ---------------------------------------------------------------------------
// Session identity
// ---------------------------------------------------------------------------

/// The workspace directory that identifies an agent session.
///
/// Claude Code exports `CLAUDE_PROJECT_DIR` to hooks; it is preferred over the
/// current directory because the agent may `cd` during a session.
pub fn workspace(agent: Option<Agent>, hinted: Option<&Path>) -> PathBuf {
    if agent == Some(Agent::Claude)
        && let Some(dir) = env_path("CLAUDE_PROJECT_DIR")
    {
        return dir;
    }
    if let Some(dir) = hinted.filter(|dir| !dir.as_os_str().is_empty()) {
        return dir.to_path_buf();
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Stable session key for an agent working in a directory:
/// `<agent>-<folder>-<hash>`.
///
/// The directory is canonicalised first, so spellings of the same directory
/// agree: `/var/...` and `/private/var/...` on macOS, short and long names or
/// forward and back slashes on Windows. The agent's hooks and the MCP server
/// it launches often see different spellings.
pub fn session_key(agent: Agent, workspace: &Path) -> String {
    format!(
        "{}-{}",
        session_name(agent, workspace),
        util::short_hash(&util::normalize_path(&canonical(workspace)))
    )
}

fn canonical(workspace: &Path) -> PathBuf {
    std::fs::canonicalize(workspace)
        .map(strip_verbatim)
        .unwrap_or_else(|_| workspace.to_path_buf())
}

/// Human readable session name: `<agent>-<folder>`.
pub fn session_name(agent: Agent, workspace: &Path) -> String {
    let folder = canonical(workspace)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut slug = String::new();
    for c in folder.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(32).collect();
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        agent.id().to_string()
    } else {
        format!("{}-{slug}", agent.id())
    }
}

// ---------------------------------------------------------------------------
// Planning and applying configuration changes
// ---------------------------------------------------------------------------

/// A proposed change to one configuration file.
#[derive(Debug, Clone)]
pub struct FileChange {
    /// The file.
    pub path: PathBuf,
    /// Its contents when the plan was made, `None` if it does not exist.
    pub original: Option<String>,
    /// The contents after the change.
    pub updated: String,
    /// What changes, in words.
    pub actions: Vec<String>,
}

impl FileChange {
    /// True when the file already holds the desired configuration.
    pub fn is_noop(&self) -> bool {
        self.original.as_deref() == Some(self.updated.as_str())
    }
}

/// Everything `setup` would do for one agent.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The agent.
    pub agent: Agent,
    /// The agent's executable.
    pub agent_exe: PathBuf,
    /// One entry per configuration file.
    pub changes: Vec<FileChange>,
    /// Follow-up steps the user has to take.
    pub notes: Vec<String>,
}

impl Plan {
    /// True when nothing needs to change.
    pub fn is_noop(&self) -> bool {
        self.changes.iter().all(FileChange::is_noop)
    }
}

/// The result of applying one file change.
#[derive(Debug, Clone)]
pub struct Applied {
    /// The file.
    pub path: PathBuf,
    /// Backup of the previous contents, when the file existed and changed.
    pub backup: Option<PathBuf>,
    /// Whether the file was written.
    pub changed: bool,
}

/// Work out the configuration changes for an agent without touching anything.
pub fn plan(agent: Agent, host: &Host) -> Result<Plan> {
    let agent_exe = host.find_agent(agent).ok_or_else(|| {
        Error::NotFound(format!(
            "{} is not installed: `{}` was not found on PATH; {}",
            agent.display_name(),
            agent.executable_name(),
            agent.install_hint()
        ))
    })?;
    let mut notes = Vec::new();
    let changes = match agent {
        Agent::Claude => {
            let changes = vec![plan_claude_mcp(host)?, plan_claude_hooks(host)?];
            if !changes.iter().all(FileChange::is_noop) {
                notes.push(
                    "restart running Claude Code sessions so they load the MCP server and hooks"
                        .to_string(),
                );
            }
            changes
        }
        Agent::Codex => {
            let change = plan_codex(host)?;
            let hooks_changed = change.actions.iter().any(|action| {
                action.starts_with("add lifecycle") || action.starts_with("update lifecycle")
            });
            if hooks_changed {
                notes.push(
                    "Codex skips new or changed hooks until you trust them: start `codex`, run `/hooks` and trust the reason-network entries (the MCP tools work without them)"
                        .to_string(),
                );
            }
            if !change.is_noop() {
                notes
                    .push("restart running Codex sessions so they load the MCP server".to_string());
            }
            vec![change]
        }
    };
    Ok(Plan {
        agent,
        agent_exe,
        changes,
        notes,
    })
}

/// Apply a plan: back up every file that changes, then write it atomically.
///
/// A file that changed on disk since the plan was made is left alone and
/// reported as an error, so a concurrent edit is never overwritten.
pub fn apply(plan: &Plan) -> Result<Vec<Applied>> {
    let mut applied = Vec::new();
    for change in &plan.changes {
        if change.is_noop() {
            applied.push(Applied {
                path: change.path.clone(),
                backup: None,
                changed: false,
            });
            continue;
        }
        let current = read_optional(&change.path)?;
        if current != change.original {
            return Err(Error::Other(format!(
                "{} changed while setup was running; nothing was written to it, re-run setup",
                change.path.display()
            )));
        }
        let backup = match &change.original {
            Some(_) => Some(util::backup_file(&change.path).map_err(|error| {
                Error::Other(format!(
                    "cannot back up {}: {error}; nothing was changed",
                    change.path.display()
                ))
            })?),
            None => None,
        };
        util::atomic_write(&change.path, change.updated.as_bytes()).map_err(|error| {
            Error::Other(format!("cannot write {}: {error}", change.path.display()))
        })?;
        applied.push(Applied {
            path: change.path.clone(),
            backup,
            changed: true,
        });
    }
    Ok(applied)
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::Other(format!(
            "cannot read {}: {error}",
            path.display()
        ))),
    }
}

// ----- Claude Code ---------------------------------------------------------

fn read_json_object(path: &Path) -> Result<(Option<String>, Map<String, Value>)> {
    let original = read_optional(path)?;
    let object = match original.as_deref().map(str::trim) {
        None | Some("") => Map::new(),
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(object)) => object,
            Ok(_) => {
                return Err(Error::Other(format!(
                    "{} is not a JSON object; fix or move it and re-run setup (nothing was changed)",
                    path.display()
                )));
            }
            Err(error) => {
                return Err(Error::Other(format!(
                    "{} is not valid JSON ({error}); fix or move it and re-run setup (nothing was changed)",
                    path.display()
                )));
            }
        },
    };
    Ok((original, object))
}

fn render_json(original: &Option<String>, object: Map<String, Value>) -> Result<String> {
    let mut text = serde_json::to_string_pretty(&Value::Object(object))?;
    if original.as_deref().is_none_or(|text| text.ends_with('\n')) {
        text.push('\n');
    }
    Ok(text)
}

fn claude_mcp_entry(host: &Host) -> Value {
    let env: Map<String, Value> = host
        .mcp_env()
        .into_iter()
        .map(|(key, value)| (key, Value::String(value)))
        .collect();
    json!({
        "type": "stdio",
        "command": host.exe_text(),
        "args": host.mcp_args(Agent::Claude),
        "env": env,
    })
}

fn plan_claude_mcp(host: &Host) -> Result<FileChange> {
    let path = host.claude_json();
    let (original, mut object) = read_json_object(&path)?;
    let desired = claude_mcp_entry(host);
    let current = object
        .get("mcpServers")
        .and_then(|servers| servers.get(SERVER_NAME))
        .cloned();
    if current.as_ref() == Some(&desired) {
        return Ok(FileChange {
            path,
            updated: original.clone().unwrap_or_default(),
            original,
            actions: vec!["MCP server `reason-network` already configured".into()],
        });
    }
    let action = if current.is_some() {
        "update MCP server `reason-network` (user scope)"
    } else {
        "add MCP server `reason-network` (user scope)"
    };
    let servers = object
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    if !servers.is_object() {
        return Err(Error::Other(format!(
            "`mcpServers` in {} is not an object; fix it and re-run setup (nothing was changed)",
            path.display()
        )));
    }
    if let Some(servers) = servers.as_object_mut() {
        servers.insert(SERVER_NAME.to_string(), desired);
    }
    Ok(FileChange {
        updated: render_json(&original, object)?,
        path,
        original,
        actions: vec![action.to_string()],
    })
}

fn json_handler_command(handler: &Value) -> Option<&str> {
    handler.get("command").and_then(Value::as_str)
}

fn plan_claude_hooks(host: &Host) -> Result<FileChange> {
    let agent = Agent::Claude;
    let path = host.claude_settings();
    let (original, mut object) = read_json_object(&path)?;
    let command = host.hook_command(agent);
    let desired = |event: &str| json!({ "type": "command", "command": command, "timeout": agent.hook_timeout(event) });

    let hooks = object
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(hooks) = hooks.as_object_mut() else {
        return Err(Error::Other(format!(
            "`hooks` in {} is not an object; fix it and re-run setup (nothing was changed)",
            path.display()
        )));
    };

    // Already configured: exactly the desired handler, once, in every event.
    let ours = |event: &str| -> Vec<Value> {
        hooks
            .get(event)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|group| group.get("hooks").and_then(Value::as_array))
            .flatten()
            .filter(|handler| json_handler_command(handler).is_some_and(|c| is_our_hook(c, agent)))
            .cloned()
            .collect()
    };
    let configured = agent
        .hook_events()
        .iter()
        .all(|event| ours(event) == vec![desired(event)])
        && hooks
            .keys()
            .filter(|event| !agent.hook_events().contains(&event.as_str()))
            .all(|event| ours(event).is_empty());
    if configured {
        return Ok(FileChange {
            path,
            updated: original.clone().unwrap_or_default(),
            original,
            actions: vec!["lifecycle hooks already configured".into()],
        });
    }

    let mut replaced = false;
    for groups in hooks.values_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        groups.retain_mut(|group| {
            let Some(handlers) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            let before = handlers.len();
            handlers.retain(|handler| {
                !json_handler_command(handler).is_some_and(|c| is_our_hook(c, agent))
            });
            if handlers.len() != before {
                replaced = true;
                return !handlers.is_empty();
            }
            true
        });
    }
    for event in agent.hook_events() {
        let groups = hooks
            .entry(event.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(groups) = groups.as_array_mut() else {
            return Err(Error::Other(format!(
                "`hooks.{event}` in {} is not an array; fix it and re-run setup (nothing was changed)",
                path.display()
            )));
        };
        groups.push(json!({ "hooks": [desired(event)] }));
    }
    let verb = if replaced { "update" } else { "add" };
    Ok(FileChange {
        updated: render_json(&original, object)?,
        path,
        original,
        actions: vec![format!(
            "{verb} lifecycle hooks ({}) running `{command}`",
            agent.hook_events().join(", ")
        )],
    })
}

// ----- Codex ---------------------------------------------------------------

fn plan_codex(host: &Host) -> Result<FileChange> {
    use toml_edit::{DocumentMut, Item, Table};

    let agent = Agent::Codex;
    let path = host.codex_config();
    let original = read_optional(&path)?;
    let mut doc: DocumentMut = original.as_deref().unwrap_or("").parse().map_err(|error| {
        Error::Other(format!(
            "{} is not valid TOML ({error}); fix it and re-run setup (nothing was changed)",
            path.display()
        ))
    })?;
    let mut actions = Vec::new();

    // MCP server.
    let desired_args = host.mcp_args(agent);
    let desired_env = host.mcp_env();
    let servers = doc
        .entry("mcp_servers")
        .or_insert_with(|| implicit_table().into());
    let Some(servers) = servers.as_table_like_mut() else {
        return Err(Error::Other(format!(
            "`mcp_servers` in {} is not a table; fix it and re-run setup (nothing was changed)",
            path.display()
        )));
    };
    let current = servers.get(SERVER_NAME);
    let matches = current.is_some_and(|item| {
        toml_server_matches(item, &host.exe_text(), &desired_args, &desired_env)
    });
    if matches {
        actions.push("MCP server `reason-network` already configured".to_string());
    } else {
        actions.push(
            if current.is_some() {
                "update MCP server `reason-network`"
            } else {
                "add MCP server `reason-network`"
            }
            .to_string(),
        );
        let mut server = Table::new();
        server.insert("command", toml_edit::value(host.exe_text()));
        let mut args = toml_edit::Array::new();
        for arg in &desired_args {
            args.push(arg.as_str());
        }
        server.insert("args", toml_edit::value(args));
        if !desired_env.is_empty() {
            let mut env = Table::new();
            for (key, value) in &desired_env {
                env.insert(key, toml_edit::value(value.as_str()));
            }
            server.insert("env", Item::Table(env));
        }
        servers.insert(SERVER_NAME, Item::Table(server));
    }

    // Lifecycle hooks.
    let command = host.hook_command(agent);
    let hooks = doc
        .entry("hooks")
        .or_insert_with(|| implicit_table().into());
    let Some(hooks) = hooks.as_table_like_mut() else {
        return Err(Error::Other(format!(
            "`hooks` in {} is not a table; fix it and re-run setup (nothing was changed)",
            path.display()
        )));
    };
    let configured = agent.hook_events().iter().all(|event| {
        hooks.get(event).is_some_and(|item| {
            toml_our_handlers(item, agent) == vec![(command.clone(), agent.hook_timeout(event))]
        })
    }) && hooks
        .iter()
        .filter(|(event, _)| !agent.hook_events().contains(event))
        .all(|(_, item)| toml_our_handlers(item, agent).is_empty());
    if configured {
        actions.push("lifecycle hooks already configured".to_string());
    } else {
        let keys: Vec<String> = hooks.iter().map(|(key, _)| key.to_string()).collect();
        let mut replaced = false;
        for key in keys {
            if let Some(item) = hooks.get_mut(&key) {
                replaced |= toml_remove_our_handlers(item, agent);
            }
        }
        for event in agent.hook_events() {
            let mut handler = Table::new();
            handler.insert("type", toml_edit::value("command"));
            handler.insert("command", toml_edit::value(command.as_str()));
            handler.insert("timeout", toml_edit::value(agent.hook_timeout(event)));
            let mut handlers = toml_edit::ArrayOfTables::new();
            handlers.push(handler);
            let mut group = Table::new();
            group.insert("hooks", Item::ArrayOfTables(handlers));

            let entry = hooks
                .entry(event)
                .or_insert(Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
            match entry {
                Item::ArrayOfTables(groups) => groups.push(group),
                Item::Value(toml_edit::Value::Array(groups)) => {
                    groups.push(toml_edit::Value::InlineTable(group.into_inline_table()))
                }
                _ => {
                    return Err(Error::Other(format!(
                        "`hooks.{event}` in {} is not an array; fix it and re-run setup (nothing was changed)",
                        path.display()
                    )));
                }
            }
        }
        let verb = if replaced { "update" } else { "add" };
        actions.push(format!(
            "{verb} lifecycle hooks ({}) running `{command}`",
            agent.hook_events().join(", ")
        ));
    }

    let unchanged = matches && configured;
    let updated = if unchanged {
        original.clone().unwrap_or_default()
    } else {
        doc.to_string()
    };
    Ok(FileChange {
        path,
        original,
        updated,
        actions,
    })
}

fn implicit_table() -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    table.set_implicit(true);
    table
}

fn toml_server_matches(
    item: &toml_edit::Item,
    command: &str,
    args: &[String],
    env: &[(String, String)],
) -> bool {
    let Some(table) = item.as_table_like() else {
        return false;
    };
    let command_ok = table
        .get("command")
        .and_then(|item| item.as_str())
        .is_some_and(|value| value == command);
    let current_args: Option<Vec<String>> =
        table
            .get("args")
            .and_then(|item| item.as_array())
            .map(|array| {
                array
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect()
            });
    let args_ok = current_args.as_deref() == Some(args);
    let current_env: Vec<(String, String)> = table
        .get("env")
        .and_then(|item| item.as_table_like())
        .map(|env| {
            env.iter()
                .filter_map(|(key, value)| value.as_str().map(|v| (key.to_string(), v.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let only_known_keys = table
        .iter()
        .all(|(key, _)| matches!(key, "command" | "args" | "env"));
    command_ok && args_ok && current_env == env && only_known_keys
}

/// Our handlers inside one `hooks.<Event>` item, as `(command, timeout)`.
fn toml_our_handlers(item: &toml_edit::Item, agent: Agent) -> Vec<(String, i64)> {
    let mut found = Vec::new();
    for group in toml_groups(item) {
        let Some(handlers) = group.get("hooks") else {
            continue;
        };
        for handler in toml_groups(handlers) {
            if let Some(command) = handler.get("command").and_then(|c| c.as_str())
                && is_our_hook(command, agent)
            {
                let timeout = handler
                    .get("timeout")
                    .and_then(|t| t.as_integer())
                    .unwrap_or(0);
                found.push((command.to_string(), timeout));
            }
        }
    }
    found
}

/// The tables inside an array-of-tables or an inline array of tables.
fn toml_groups(item: &toml_edit::Item) -> Vec<&dyn toml_edit::TableLike> {
    match item {
        toml_edit::Item::ArrayOfTables(groups) => groups
            .iter()
            .map(|table| table as &dyn toml_edit::TableLike)
            .collect(),
        toml_edit::Item::Value(toml_edit::Value::Array(groups)) => groups
            .iter()
            .filter_map(|value| value.as_inline_table())
            .map(|table| table as &dyn toml_edit::TableLike)
            .collect(),
        _ => Vec::new(),
    }
}

/// Remove our handlers from one `hooks.<Event>` item, dropping groups that
/// become empty because of it. Returns whether anything was removed.
fn toml_remove_our_handlers(item: &mut toml_edit::Item, agent: Agent) -> bool {
    use toml_edit::{Item, Value};

    let is_ours_table = |table: &dyn toml_edit::TableLike| {
        table
            .get("command")
            .and_then(|c| c.as_str())
            .is_some_and(|c| is_our_hook(c, agent))
    };
    // Returns (removed, now_empty) for one group's `hooks` item.
    let prune = |handlers: &mut Item| -> (bool, bool) {
        match handlers {
            Item::ArrayOfTables(list) => {
                let before = list.len();
                list.retain(|table| !is_ours_table(table));
                (list.len() != before, list.is_empty())
            }
            Item::Value(Value::Array(list)) => {
                let before = list.len();
                list.retain(|value| !value.as_inline_table().is_some_and(|t| is_ours_table(t)));
                (list.len() != before, list.is_empty())
            }
            _ => (false, false),
        }
    };

    let mut removed_any = false;
    match item {
        Item::ArrayOfTables(groups) => {
            let mut drop = Vec::new();
            for group in groups.iter_mut() {
                let (removed, empty) = group.get_mut("hooks").map(prune).unwrap_or((false, false));
                removed_any |= removed;
                drop.push(removed && empty);
            }
            let mut index = 0;
            groups.retain(|_| {
                let keep = !drop[index];
                index += 1;
                keep
            });
        }
        Item::Value(Value::Array(groups)) => {
            let mut drop = Vec::new();
            for group in groups.iter_mut() {
                let (removed, empty) = match group.as_inline_table_mut() {
                    Some(table) => match table.get_mut("hooks") {
                        Some(Value::Array(list)) => {
                            let before = list.len();
                            list.retain(|value| {
                                !value.as_inline_table().is_some_and(|t| is_ours_table(t))
                            });
                            (list.len() != before, list.is_empty())
                        }
                        _ => (false, false),
                    },
                    None => (false, false),
                };
                removed_any |= removed;
                drop.push(removed && empty);
            }
            let mut index = 0;
            groups.retain(|_| {
                let keep = !drop[index];
                index += 1;
                keep
            });
        }
        _ => {}
    }
    removed_any
}

// ---------------------------------------------------------------------------
// Setup command
// ---------------------------------------------------------------------------

/// What setup did, or would do, for one agent.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// The agent is not installed.
    Missing(String),
    /// The configuration could not be planned or written.
    Failed(String),
    /// Dry run: the changes that would be made.
    Planned(Plan),
    /// The changes were made.
    Applied(Plan, Vec<Applied>),
}

/// The result of `reason-network setup`.
#[derive(Debug, Clone)]
pub struct SetupReport {
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// The MCP command line agents will run.
    pub mcp_command: String,
    /// One entry per selected agent.
    pub entries: Vec<(Agent, Outcome)>,
}

/// Configure the selected agents. With `dry_run`, nothing is written.
pub fn run_setup(host: &Host, agents: &[Agent], dry_run: bool) -> SetupReport {
    let entries = agents
        .iter()
        .map(|&agent| {
            let outcome = match plan(agent, host) {
                Err(Error::NotFound(reason)) => Outcome::Missing(reason),
                Err(error) => Outcome::Failed(error.to_string()),
                Ok(plan) if dry_run => Outcome::Planned(plan),
                Ok(plan) => match apply(&plan) {
                    Ok(applied) => Outcome::Applied(plan, applied),
                    Err(error) => Outcome::Failed(error.to_string()),
                },
            };
            (agent, outcome)
        })
        .collect();
    SetupReport {
        dry_run,
        mcp_command: format!("{} mcp --agent <agent>", host.exe.display()),
        entries,
    }
}

impl SetupReport {
    /// Human readable report.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.dry_run {
            out.push_str("dry run: no files were changed\n\n");
        }
        for (agent, outcome) in &self.entries {
            match outcome {
                Outcome::Missing(reason) => {
                    out.push_str(&format!(
                        "{}: skipped\n  {reason}\n\n",
                        agent.display_name()
                    ));
                }
                Outcome::Failed(reason) => {
                    out.push_str(&format!("{}: FAILED\n  {reason}\n\n", agent.display_name()));
                }
                Outcome::Planned(plan) | Outcome::Applied(plan, _) => {
                    let applied = match outcome {
                        Outcome::Applied(_, applied) => Some(applied),
                        _ => None,
                    };
                    let state = if plan.is_noop() {
                        "already configured"
                    } else if applied.is_some() {
                        "configured"
                    } else {
                        "would be configured"
                    };
                    out.push_str(&format!(
                        "{}: {state} ({})\n",
                        agent.display_name(),
                        plan.agent_exe.display()
                    ));
                    for (index, change) in plan.changes.iter().enumerate() {
                        let verdict = if change.is_noop() {
                            "unchanged".to_string()
                        } else {
                            match applied.and_then(|applied| applied.get(index)) {
                                Some(Applied {
                                    backup: Some(backup),
                                    ..
                                }) => format!("updated, backup {}", backup.display()),
                                Some(_) => "created".to_string(),
                                None if change.original.is_some() => {
                                    "would update (a backup is made first)".to_string()
                                }
                                None => "would create".to_string(),
                            }
                        };
                        out.push_str(&format!("  {} [{verdict}]\n", change.path.display()));
                        for action in &change.actions {
                            out.push_str(&format!("    - {action}\n"));
                        }
                    }
                    for note in &plan.notes {
                        out.push_str(&format!("  next: {note}\n"));
                    }
                    out.push('\n');
                }
            }
        }
        out.push_str(&format!("MCP server command: {}\n", self.mcp_command));
        out.push_str(
            "The daemon must be running for the tools to work: `reason-network service install` \
then `reason-network service start`, or `reason-network daemon-start`. Check with `reason-network doctor`.\n",
        );
        out
    }

    /// Machine readable report.
    pub fn to_json(&self) -> Value {
        let entries: Vec<Value> = self
            .entries
            .iter()
            .map(|(agent, outcome)| {
                let (state, detail, files, notes) = match outcome {
                    Outcome::Missing(reason) => ("missing", Some(reason.clone()), vec![], vec![]),
                    Outcome::Failed(reason) => ("failed", Some(reason.clone()), vec![], vec![]),
                    Outcome::Planned(plan) | Outcome::Applied(plan, _) => {
                        let applied = match outcome {
                            Outcome::Applied(_, applied) => Some(applied),
                            _ => None,
                        };
                        let files = plan
                            .changes
                            .iter()
                            .enumerate()
                            .map(|(index, change)| {
                                let backup = applied
                                    .and_then(|applied| applied.get(index))
                                    .and_then(|applied| applied.backup.as_ref())
                                    .map(|backup| backup.display().to_string());
                                json!({
                                    "path": change.path.display().to_string(),
                                    "changed": !change.is_noop(),
                                    "actions": change.actions,
                                    "backup": backup,
                                })
                            })
                            .collect();
                        let state = if plan.is_noop() {
                            "unchanged"
                        } else if applied.is_some() {
                            "configured"
                        } else {
                            "planned"
                        };
                        (state, None, files, plan.notes.clone())
                    }
                };
                json!({
                    "agent": agent.id(),
                    "state": state,
                    "detail": detail,
                    "files": files,
                    "notes": notes,
                })
            })
            .collect();
        json!({ "dry_run": self.dry_run, "agents": entries })
    }

    /// Fail when an agent could not be configured.
    ///
    /// A missing agent is an error when it was requested on its own; with
    /// `--agent all` it is only an error when no agent at all is installed.
    pub fn into_result(self) -> Result<()> {
        let failed: Vec<&str> = self
            .entries
            .iter()
            .filter(|(_, outcome)| matches!(outcome, Outcome::Failed(_)))
            .map(|(agent, _)| agent.id())
            .collect();
        if !failed.is_empty() {
            return Err(Error::Other(format!(
                "setup failed for: {}",
                failed.join(", ")
            )));
        }
        let missing = self
            .entries
            .iter()
            .filter(|(_, outcome)| matches!(outcome, Outcome::Missing(_)))
            .count();
        if missing == self.entries.len() {
            return Err(Error::NotFound(
                "no supported agent is installed; nothing was configured".into(),
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Inspection (used by `doctor`)
// ---------------------------------------------------------------------------

/// The state of one agent's Reason Network configuration.
#[derive(Debug, Clone)]
pub struct ConfigStatus {
    /// The command of the configured MCP server, if any.
    pub mcp_command: Option<String>,
    /// Whether every configuration file already matches what setup writes.
    pub up_to_date: bool,
    /// Project-scoped MCP entries that override the user-scope one.
    pub project_overrides: Vec<String>,
    /// A problem reading the configuration.
    pub error: Option<String>,
}

/// Inspect an agent's configuration without changing it.
///
/// Works even when the agent executable is missing, so doctor can report a
/// configuration left behind by an uninstalled agent.
pub fn inspect(agent: Agent, host: &Host) -> ConfigStatus {
    let mut status = ConfigStatus {
        mcp_command: None,
        up_to_date: false,
        project_overrides: Vec::new(),
        error: None,
    };
    match agent {
        Agent::Claude => match read_json_object(&host.claude_json()) {
            Ok((_, object)) => {
                status.mcp_command = object
                    .get("mcpServers")
                    .and_then(|servers| servers.get(SERVER_NAME))
                    .and_then(|server| server.get("command"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(projects) = object.get("projects").and_then(Value::as_object) {
                    for (project, config) in projects {
                        if config
                            .get("mcpServers")
                            .and_then(|servers| servers.get(SERVER_NAME))
                            .is_some()
                        {
                            status.project_overrides.push(project.clone());
                        }
                    }
                }
            }
            Err(error) => status.error = Some(error.to_string()),
        },
        Agent::Codex => match read_optional(&host.codex_config()) {
            Ok(text) => match text
                .as_deref()
                .unwrap_or("")
                .parse::<toml_edit::DocumentMut>()
            {
                Ok(doc) => {
                    status.mcp_command = doc
                        .get("mcp_servers")
                        .and_then(|servers| servers.get(SERVER_NAME))
                        .and_then(|server| server.get("command"))
                        .and_then(|command| command.as_str())
                        .map(str::to_string);
                }
                Err(error) => status.error = Some(format!("invalid TOML: {error}")),
            },
            Err(error) => status.error = Some(error.to_string()),
        },
    }
    if status.error.is_none() {
        let changes = match agent {
            Agent::Claude => plan_claude_mcp(host).and_then(|mcp| {
                plan_claude_hooks(host).map(|hooks| mcp.is_noop() && hooks.is_noop())
            }),
            Agent::Codex => plan_codex(host).map(|change| change.is_noop()),
        };
        match changes {
            Ok(up_to_date) => status.up_to_date = up_to_date,
            Err(error) => status.error = Some(error.to_string()),
        }
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_keys_are_stable_and_distinct_per_agent_and_folder() {
        let a = Path::new("/work/My Project");
        let b = Path::new("/work/other");
        assert_eq!(session_key(Agent::Claude, a), session_key(Agent::Claude, a));
        assert_ne!(session_key(Agent::Claude, a), session_key(Agent::Codex, a));
        assert_ne!(session_key(Agent::Claude, a), session_key(Agent::Claude, b));
        assert!(session_key(Agent::Claude, a).starts_with("claude-my-project-"));
        assert_eq!(session_name(Agent::Codex, b), "codex-other");
        assert_eq!(
            session_key(Agent::Claude, Path::new("/work/x/")),
            session_key(Agent::Claude, Path::new("/work/x"))
        );
    }

    #[test]
    fn different_spellings_of_one_directory_share_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("Repo");
        std::fs::create_dir_all(&real).unwrap();
        let key = session_key(Agent::Codex, &real);
        let slashed = PathBuf::from(real.display().to_string().replace('\\', "/"));
        assert_eq!(session_key(Agent::Codex, &slashed), key);
        assert_eq!(session_key(Agent::Codex, &real.join(".")), key);
        #[cfg(unix)]
        {
            let link = dir.path().join("repo-link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert_eq!(session_key(Agent::Codex, &link), key);
        }
    }

    #[test]
    fn agent_selection_parses_all_and_rejects_unknown() {
        assert_eq!(Agent::parse_selection("all").unwrap().len(), 2);
        assert_eq!(
            Agent::parse_selection("Claude").unwrap(),
            vec![Agent::Claude]
        );
        let error = Agent::parse_selection("gemini").unwrap_err().to_string();
        assert!(error.contains("unsupported agent `gemini`"), "{error}");
        assert!(error.contains("mcp --session-key"), "{error}");
    }

    #[test]
    fn quoting_only_wraps_paths_that_need_it() {
        assert_eq!(quote(Path::new("/usr/bin/rn")), "/usr/bin/rn");
        assert_eq!(quote(Path::new("/a b/rn")), "\"/a b/rn\"");
    }

    #[test]
    fn our_hooks_are_recognised_by_agent() {
        assert!(is_our_hook(
            "\"/x/reason-network\" hook --agent claude",
            Agent::Claude
        ));
        assert!(!is_our_hook(
            "/x/reason-network hook --agent codex",
            Agent::Claude
        ));
        assert!(!is_our_hook("echo hook --agent claude", Agent::Claude));
    }
}
