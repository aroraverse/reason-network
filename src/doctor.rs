//! `reason-network doctor`: concise, actionable diagnostics (M2).
//!
//! Every check reports `ok`, `warn` or `fail` with a one-line detail and,
//! when something needs doing, the command that fixes it. Doctor only reads:
//! it never writes configuration, and it never prints private keys,
//! invitation tokens or the environment of agent configurations.

use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

use crate::agents::{self, Agent, Host};
use crate::config::Config;
use crate::daemon::VERSION;
use crate::ipc;
use crate::protocol::{Method, Request, Response};
use crate::service::{self, ServiceSpec};

/// Outcome of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Working as expected.
    Ok,
    /// Works, but something is missing or suboptimal.
    Warn,
    /// Broken.
    Fail,
}

/// One diagnostic line.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    /// What was checked.
    pub name: String,
    /// The outcome.
    pub level: Level,
    /// What was found.
    pub detail: String,
    /// How to fix it, when there is something to do.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl Check {
    fn new(name: impl Into<String>, level: Level, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            level,
            detail: detail.into(),
            fix: None,
        }
    }

    fn fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }
}

/// Run every check.
pub async fn run(config: &Config, host: &Host, service: &ServiceSpec) -> Vec<Check> {
    let mut checks = Vec::new();
    checks.push(check_executable(host));
    checks.push(check_data_dir(config));

    let status = request(config, Method::Status).await;
    let diagnostics = match &status {
        Some(_) => request(config, Method::Diagnostics).await,
        None => None,
    };
    let net_info = match &status {
        Some(_) => request(config, Method::NetInfo).await,
        None => None,
    };

    checks.push(check_database(config, diagnostics.as_ref()));
    checks.push(check_identity(config, net_info.as_ref()));
    checks.push(check_daemon(config, status.as_ref()));
    checks.push(check_network(config, net_info.as_ref()));

    for agent in Agent::ALL {
        let installed = host.find_agent(agent);
        checks.push(check_agent_cli(agent, installed.as_deref()).await);
        if let Some(check) = check_agent_config(agent, host, installed.is_some()) {
            checks.push(check);
        }
    }
    checks.push(check_service(config, service).await);
    checks
}

async fn request(config: &Config, method: Method) -> Option<Value> {
    let request = Request {
        id: Some("doctor".into()),
        method,
        params: json!({}),
    };
    let response: Response = tokio::time::timeout(
        Duration::from_secs(3),
        ipc::request_once(&config.endpoint, &request),
    )
    .await
    .ok()?
    .ok()?;
    if response.ok { response.result } else { None }
}

fn check_executable(host: &Host) -> Check {
    let detail = format!(
        "reason-network {VERSION} ({}/{}) at {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        host.exe.display()
    );
    let dir = host.exe.parent().map(Path::to_path_buf);
    let on_path = dir.as_ref().is_some_and(|dir| {
        host.path
            .iter()
            .any(|entry| crate::util::normalize_path(entry) == crate::util::normalize_path(dir))
    });
    if on_path {
        Check::new("version", Level::Ok, detail)
    } else {
        Check::new(
            "version",
            Level::Warn,
            format!("{detail}; its directory is not on PATH"),
        )
        .fix(format!(
            "add {} to PATH to run `reason-network` from any shell (agent configurations use the absolute path and work regardless)",
            dir.map(|dir| dir.display().to_string()).unwrap_or_default()
        ))
    }
}

fn check_data_dir(config: &Config) -> Check {
    let dir = &config.data_dir;
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() && !meta.permissions().readonly() => {
            Check::new("data directory", Level::Ok, dir.display().to_string())
        }
        Ok(meta) if meta.is_dir() => Check::new(
            "data directory",
            Level::Fail,
            format!("{} is read-only", dir.display()),
        )
        .fix("make the directory writable, or set REASON_NETWORK_DATA_DIR to a writable one"),
        Ok(_) => Check::new(
            "data directory",
            Level::Fail,
            format!("{} exists but is not a directory", dir.display()),
        )
        .fix("move the file away, or set REASON_NETWORK_DATA_DIR"),
        Err(_) => Check::new(
            "data directory",
            Level::Warn,
            format!("{} does not exist yet", dir.display()),
        )
        .fix("it is created when the daemon first starts: `reason-network service start` or `reason-network daemon-start`"),
    }
}

fn check_database(config: &Config, diagnostics: Option<&Value>) -> Check {
    if let Some(diagnostics) = diagnostics {
        return Check::new(
            "database",
            Level::Ok,
            format!(
                "{} (open by the daemon: {} sessions, {} messages)",
                config.db_path.display(),
                diagnostics["sessions"],
                diagnostics["messages"]
            ),
        );
    }
    if !config.db_path.is_file() {
        return Check::new(
            "database",
            Level::Warn,
            format!("{} does not exist yet", config.db_path.display()),
        )
        .fix("it is created when the daemon first starts");
    }
    // The daemon is not running: open read-only to prove the file is usable.
    let opened = rusqlite::Connection::open_with_flags(
        &config.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .and_then(|conn| {
        conn.query_row("SELECT count(*) FROM sessions", [], |row| {
            row.get::<_, i64>(0)
        })
    });
    match opened {
        Ok(sessions) => Check::new(
            "database",
            Level::Ok,
            format!(
                "{} readable ({sessions} sessions)",
                config.db_path.display()
            ),
        ),
        Err(error) => Check::new(
            "database",
            Level::Fail,
            format!("{} cannot be read: {error}", config.db_path.display()),
        )
        .fix("check the file permissions; if it is corrupt, move it aside (this discards local state)"),
    }
}

fn check_identity(config: &Config, net_info: Option<&Value>) -> Check {
    let key = config.data_dir.join(crate::identity::KEY_FILE);
    let peer = net_info
        .and_then(|info| info.get("peer_id"))
        .and_then(Value::as_str)
        .map(|peer| format!(", peer id {peer}"))
        .unwrap_or_default();
    if key.is_file() {
        Check::new(
            "machine identity",
            Level::Ok,
            format!("key present (private key not shown){peer}"),
        )
    } else {
        Check::new("machine identity", Level::Warn, "no machine key yet")
            .fix("a key is generated when the daemon first starts")
    }
}

fn check_daemon(config: &Config, status: Option<&Value>) -> Check {
    match status {
        Some(status) => Check::new(
            "daemon",
            Level::Ok,
            format!(
                "running (pid {}, version {}, up {} s) on {}",
                status["pid"],
                status["version"].as_str().unwrap_or("?"),
                status["uptime_seconds"],
                config.endpoint.display()
            ),
        ),
        None => Check::new(
            "daemon",
            Level::Fail,
            format!("not reachable on {}", config.endpoint.display()),
        )
        .fix("start it: `reason-network service start` (after `service install`) or `reason-network daemon-start`"),
    }
}

fn check_network(config: &Config, net_info: Option<&Value>) -> Check {
    if !config.net.enabled {
        return Check::new(
            "network",
            Level::Ok,
            "peer-to-peer networking disabled (REASON_NETWORK_NET=0); local messaging only",
        );
    }
    let Some(info) = net_info else {
        return Check::new("network", Level::Warn, "unknown (daemon not running)");
    };
    if info["running"].as_bool() != Some(true) {
        return Check::new(
            "network",
            Level::Fail,
            "networking enabled but the endpoint is not running",
        )
        .fix("check the daemon log for endpoint errors (firewall, UDP blocked)");
    }
    let teams = info["teams"].as_u64().unwrap_or(0);
    let connected = info["connected"].as_array().map_or(0, Vec::len);
    let relay = if info["relay"].as_bool() == Some(true) {
        "on"
    } else {
        "off"
    };
    let detail =
        format!("endpoint up, relay {relay}, {teams} team(s), {connected} peer(s) connected");
    if teams == 0 {
        Check::new(
            "network",
            Level::Ok,
            format!("{detail}; no team joined (local messaging only)"),
        )
    } else if connected == 0 {
        Check::new(
            "network",
            Level::Warn,
            format!("{detail}; no team peer is connected"),
        )
        .fix("start the daemon on the other machines; `reason-network peer-list` shows their state")
    } else {
        Check::new("network", Level::Ok, detail)
    }
}

async fn check_agent_cli(agent: Agent, installed: Option<&Path>) -> Check {
    let name = format!("{} CLI", agent.id());
    let Some(exe) = installed else {
        return Check::new(
            name,
            Level::Warn,
            format!("{} not found on PATH", agent.display_name()),
        )
        .fix(agent.install_hint());
    };
    let version = agent_version(exe).await;
    Check::new(
        name,
        Level::Ok,
        format!(
            "{} at {}",
            version.unwrap_or_else(|| "version unknown".into()),
            exe.display()
        ),
    )
}

/// `<agent> --version`, bounded by a timeout.
pub async fn agent_version(exe: &Path) -> Option<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(exe)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    text.lines()
        .next()
        .map(str::to_string)
        .filter(|line| !line.is_empty())
}

fn check_agent_config(agent: Agent, host: &Host, installed: bool) -> Option<Check> {
    let name = format!("{} MCP config", agent.id());
    let setup = format!("reason-network setup --agent {}", agent.id());
    let status = agents::inspect(agent, host);
    if let Some(error) = status.error {
        return Some(Check::new(name, Level::Fail, error));
    }
    let Some(command) = status.mcp_command else {
        if !installed {
            return None;
        }
        let detail = if status.project_overrides.is_empty() {
            "reason-network MCP server not configured".to_string()
        } else {
            format!(
                "not configured at user scope; only project-scoped entries exist, in: {}",
                status.project_overrides.join(", ")
            )
        };
        return Some(Check::new(name, Level::Warn, detail).fix(setup));
    };
    let files = host
        .config_files(agent)
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if !Path::new(&command).is_file() {
        return Some(
            Check::new(
                name,
                Level::Fail,
                format!("configured command {command} does not exist ({files})"),
            )
            .fix(setup),
        );
    }
    if crate::util::normalize_path(Path::new(&command)) != crate::util::normalize_path(&host.exe) {
        return Some(
            Check::new(
                name,
                Level::Warn,
                format!("configured for {command}, not this executable"),
            )
            .fix(format!(
                "{setup} (run it with the executable you want agents to use)"
            )),
        );
    }
    if !status.project_overrides.is_empty() {
        return Some(
            Check::new(
                name,
                Level::Warn,
                format!(
                    "configured, but project-scoped `reason-network` entries override it in: {}",
                    status.project_overrides.join(", ")
                ),
            )
            .fix("remove the project entries with `claude mcp remove reason-network --scope local` in those directories if they are stale"),
        );
    }
    if !status.up_to_date {
        return Some(
            Check::new(
                name,
                Level::Warn,
                format!("MCP server or hooks out of date ({files})"),
            )
            .fix(setup),
        );
    }
    let note = match agent {
        Agent::Codex => "; trust the hooks once with `/hooks` inside codex",
        Agent::Claude => "",
    };
    Some(Check::new(
        name,
        Level::Ok,
        format!("MCP server and lifecycle hooks configured ({files}){note}"),
    ))
}

async fn check_service(config: &Config, spec: &ServiceSpec) -> Check {
    let status = service::status(spec, &config.endpoint).await;
    let what = format!("{} `{}`", status.mechanism, spec.id);
    match (status.installed, status.running) {
        (true, true) => Check::new(
            "service",
            Level::Ok,
            format!("{what} installed, daemon running"),
        ),
        (true, false) => Check::new(
            "service",
            Level::Warn,
            format!("{what} installed, daemon not running"),
        )
        .fix(format!(
            "reason-network service start (log: {})",
            status.log
        )),
        (false, _) => Check::new(
            "service",
            Level::Ok,
            format!("{what} not installed (optional)"),
        )
        .fix("reason-network service install && reason-network service start"),
    }
}

/// Render checks for a terminal.
pub fn render(checks: &[Check]) -> String {
    let width = checks
        .iter()
        .map(|check| check.name.len())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for check in checks {
        let tag = match check.level {
            Level::Ok => "[ ok ]",
            Level::Warn => "[warn]",
            Level::Fail => "[FAIL]",
        };
        out.push_str(&format!("{tag} {:<width$}  {}\n", check.name, check.detail));
        if let Some(fix) = &check.fix
            && check.level != Level::Ok
        {
            out.push_str(&format!("       {:<width$}  fix: {fix}\n", ""));
        }
    }
    let fails = checks.iter().filter(|c| c.level == Level::Fail).count();
    let warns = checks.iter().filter(|c| c.level == Level::Warn).count();
    out.push_str(&format!(
        "\n{} ok, {warns} warning(s), {fails} failure(s)\n",
        checks.len() - fails - warns
    ));
    out
}
