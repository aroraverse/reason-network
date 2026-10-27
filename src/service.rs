//! Optional user-level background service for the daemon (M2).
//!
//! | OS | Mechanism | Definition |
//! | --- | --- | --- |
//! | Windows | Task Scheduler task with a logon trigger, run as the current user with least privilege | `schtasks /Create /XML` |
//! | Linux | systemd user unit | `~/.config/systemd/user/<id>.service` |
//! | macOS | launchd user agent | `~/Library/LaunchAgents/<label>.plist` |
//!
//! None of them needs administrator rights. Service management only starts
//! and stops the existing `daemon-start` process; it never touches the
//! database, and uninstalling leaves the data directory in place.
//!
//! The service id is derived from the data directory, so a service for an
//! isolated data directory (tests, demos) never collides with the user's
//! real one.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::json;

use crate::config::{Config, ENV_DATA_DIR, Endpoint};
use crate::error::{Error, Result};
use crate::ipc;
use crate::protocol::{Method, Request, Response};
use crate::util;

/// Environment variables captured into the service definition at install
/// time, when set.
const PASSTHROUGH_ENV: &[&str] = &[
    "REASON_NETWORK_NET",
    "REASON_NETWORK_RELAY",
    "REASON_NETWORK_DISCOVERY",
    "REASON_NETWORK_CLAUDE_BIN",
];

/// How long `start` waits for the daemon to answer.
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// Log files larger than this are rotated when the daemon is launched.
const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;

/// Everything needed to define the service on any platform.
#[derive(Debug, Clone)]
pub struct ServiceSpec {
    /// `reason-network`, or `reason-network-<hash>` for a non-default data dir.
    pub id: String,
    /// The executable the service runs.
    pub exe: PathBuf,
    /// The daemon's data directory.
    pub data_dir: PathBuf,
    /// Environment for the daemon process.
    pub env: Vec<(String, String)>,
    /// Where the daemon's output goes (Windows and macOS).
    pub log_file: PathBuf,
}

impl ServiceSpec {
    /// Describe the service for a configuration.
    ///
    /// `pinned` is true when the data directory was chosen explicitly
    /// (`--data-dir` or `REASON_NETWORK_DATA_DIR`); it is then written into the
    /// definition and gives the service its own id.
    pub fn new(config: &Config, exe: PathBuf, pinned: bool) -> Self {
        let mut env = Vec::new();
        let id = if pinned {
            env.push((
                ENV_DATA_DIR.to_string(),
                config.data_dir.display().to_string(),
            ));
            format!(
                "reason-network-{}",
                util::short_hash(&util::normalize_path(&config.data_dir))
            )
        } else {
            "reason-network".to_string()
        };
        for name in PASSTHROUGH_ENV {
            if let Ok(value) = std::env::var(name)
                && !value.is_empty()
            {
                env.push((name.to_string(), value));
            }
        }
        Self {
            id,
            exe,
            data_dir: config.data_dir.clone(),
            env,
            log_file: config.data_dir.join("logs").join("daemon.log"),
        }
    }

    /// Windows Task Scheduler task name.
    pub fn windows_task_name(&self) -> String {
        match self.id.strip_prefix("reason-network") {
            Some(suffix) => format!("ReasonNetwork{suffix}"),
            None => self.id.clone(),
        }
    }

    /// launchd label.
    pub fn launchd_label(&self) -> String {
        format!("com.reason-network.{}", self.id)
    }

    /// systemd unit file name.
    pub fn systemd_unit_name(&self) -> String {
        format!("{}.service", self.id)
    }

    /// Arguments of the hidden `service launch` command the Windows task runs.
    pub fn launch_args(&self) -> Vec<String> {
        let mut args = vec!["service".to_string(), "launch".to_string()];
        for (key, value) in &self.env {
            if key == ENV_DATA_DIR {
                args.push("--data-dir".into());
                args.push(value.clone());
            } else {
                args.push("--env".into());
                args.push(format!("{key}={value}"));
            }
        }
        args
    }

    /// The systemd user unit.
    pub fn systemd_unit(&self) -> String {
        let mut unit = String::new();
        unit.push_str(
            "# Managed by `reason-network service`. Changes are overwritten on install.\n",
        );
        unit.push_str("[Unit]\n");
        unit.push_str("Description=Reason Network daemon\n");
        unit.push_str("After=network-online.target\n\n");
        unit.push_str("[Service]\n");
        unit.push_str("Type=simple\n");
        unit.push_str(&format!(
            "ExecStart={} daemon-start\n",
            systemd_quote(&self.exe.display().to_string())
        ));
        for (key, value) in &self.env {
            unit.push_str(&format!(
                "Environment={}\n",
                systemd_quote(&format!("{key}={value}"))
            ));
        }
        unit.push_str("Restart=on-failure\n");
        unit.push_str("RestartSec=5\n\n");
        unit.push_str("[Install]\n");
        unit.push_str("WantedBy=default.target\n");
        unit
    }

    /// The launchd user agent property list.
    pub fn launchd_plist(&self) -> String {
        let mut env = String::new();
        for (key, value) in &self.env {
            env.push_str(&format!(
                "    <key>{}</key>\n    <string>{}</string>\n",
                xml_escape(key),
                xml_escape(value)
            ));
        }
        let log = xml_escape(&self.log_file.display().to_string());
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Managed by `reason-network service`. Changes are overwritten on install. -->
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>daemon-start</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
{env}  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ProcessType</key>
  <string>Background</string>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
            label = xml_escape(&self.launchd_label()),
            exe = xml_escape(&self.exe.display().to_string()),
        )
    }

    /// The Task Scheduler definition for `user` (`DOMAIN\name`).
    ///
    /// The task starts at logon with the user's own interactive token and
    /// least privilege. Its action runs `service launch`, which starts the
    /// daemon detached and without a console window, then exits.
    pub fn windows_task_xml(&self, user: &str) -> String {
        let arguments = self
            .launch_args()
            .iter()
            .map(|arg| windows_quote(arg))
            .collect::<Vec<_>>()
            .join(" ");
        let user = xml_escape(user);
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Reason Network daemon (managed by reason-network service)</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Hidden>true</Hidden>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{arguments}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
            command = xml_escape(&self.exe.display().to_string()),
            arguments = xml_escape(&arguments),
        )
    }
}

/// Where the systemd user unit lives.
pub fn systemd_unit_path(spec: &ServiceSpec) -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| util::home_dir().join(".config"));
    base.join("systemd")
        .join("user")
        .join(spec.systemd_unit_name())
}

/// Where the launchd user agent lives.
pub fn launchd_plist_path(spec: &ServiceSpec) -> PathBuf {
    util::home_dir()
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{}.plist", spec.launchd_label()))
}

fn systemd_quote(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn windows_quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    // Windows argument rules: backslashes are literal unless they precede a
    // quote; double them there and escape the quote.
    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.push_str(&"\\".repeat(backslashes));
                quoted.push(c);
                backslashes = 0;
            }
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// The observed state of the service.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ServiceStatus {
    /// Service id.
    pub id: String,
    /// Mechanism used on this platform.
    pub mechanism: &'static str,
    /// Whether the service definition is registered.
    pub installed: bool,
    /// Whether a daemon answers on the local endpoint.
    pub running: bool,
    /// The daemon's process id, when it is running.
    pub pid: Option<u64>,
    /// Where the definition lives.
    pub definition: String,
    /// Where the daemon's output goes.
    pub log: String,
}

/// The service mechanism of this platform.
pub fn mechanism() -> &'static str {
    if cfg!(windows) {
        "Task Scheduler (user logon task)"
    } else if cfg!(target_os = "macos") {
        "launchd user agent"
    } else if cfg!(target_os = "linux") {
        "systemd user service"
    } else {
        "unsupported"
    }
}

/// Query the service and the daemon.
pub async fn status(spec: &ServiceSpec, endpoint: &Endpoint) -> ServiceStatus {
    let pid = daemon_pid(endpoint).await;
    ServiceStatus {
        id: spec.id.clone(),
        mechanism: mechanism(),
        installed: is_installed(spec),
        running: pid.is_some(),
        pid,
        definition: definition_location(spec),
        log: if cfg!(target_os = "linux") {
            format!("journalctl --user -u {}", spec.systemd_unit_name())
        } else {
            spec.log_file.display().to_string()
        },
    }
}

fn definition_location(spec: &ServiceSpec) -> String {
    if cfg!(windows) {
        format!("Task Scheduler: \\{}", spec.windows_task_name())
    } else if cfg!(target_os = "macos") {
        launchd_plist_path(spec).display().to_string()
    } else {
        systemd_unit_path(spec).display().to_string()
    }
}

/// Whether the service definition is registered with the OS.
pub fn is_installed(spec: &ServiceSpec) -> bool {
    if cfg!(windows) {
        run_quiet("schtasks", &["/Query", "/TN", &spec.windows_task_name()]).is_ok()
    } else if cfg!(target_os = "macos") {
        launchd_plist_path(spec).is_file()
    } else {
        systemd_unit_path(spec).is_file()
    }
}

/// The pid of the daemon answering on `endpoint`, if any.
pub async fn daemon_pid(endpoint: &Endpoint) -> Option<u64> {
    let request = Request {
        id: Some("service-status".into()),
        method: Method::Status,
        params: json!({}),
    };
    let response: Response = tokio::time::timeout(
        Duration::from_secs(3),
        ipc::request_once(endpoint, &request),
    )
    .await
    .ok()?
    .ok()?;
    response.result?.get("pid")?.as_u64()
}

async fn wait_until(endpoint: &Endpoint, running: bool, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if daemon_pid(endpoint).await.is_some() == running {
            return true;
        }
        if std::time::Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

fn unsupported() -> Error {
    Error::Other(
        "background services are supported on Windows, Linux (systemd) and macOS (launchd); run `reason-network daemon-start` under your own supervisor on this platform".into(),
    )
}

/// Register the service so the daemon starts at login. Does not start it.
pub fn install(spec: &ServiceSpec) -> Result<String> {
    if !spec.exe.is_file() {
        return Err(Error::Other(format!(
            "executable {} does not exist",
            spec.exe.display()
        )));
    }
    std::fs::create_dir_all(spec.log_file.parent().unwrap_or(&spec.data_dir))?;
    if cfg!(windows) {
        install_windows(spec)
    } else if cfg!(target_os = "macos") {
        let path = launchd_plist_path(spec);
        let was_loaded = launchd_loaded(spec);
        util::atomic_write(&path, spec.launchd_plist().as_bytes())?;
        if was_loaded {
            // Reload so a changed definition takes effect.
            let _ = run_quiet("launchctl", &["bootout", &launchd_target(spec)?]);
            run_checked(
                "launchctl",
                &["bootstrap", &launchd_domain()?, &path.display().to_string()],
            )?;
        }
        Ok(format!(
            "installed launchd agent {} ({}); it starts at login, or now with `reason-network service start`",
            spec.launchd_label(),
            path.display()
        ))
    } else if cfg!(target_os = "linux") {
        require_systemd()?;
        let path = systemd_unit_path(spec);
        util::atomic_write(&path, spec.systemd_unit().as_bytes())?;
        run_checked("systemctl", &["--user", "daemon-reload"])?;
        run_checked(
            "systemctl",
            &["--user", "enable", &spec.systemd_unit_name()],
        )?;
        Ok(format!(
            "installed systemd user service {} ({}); it starts at login, or now with `reason-network service start`",
            spec.systemd_unit_name(),
            path.display()
        ))
    } else {
        Err(unsupported())
    }
}

fn install_windows(spec: &ServiceSpec) -> Result<String> {
    let user = match (std::env::var("USERDOMAIN"), std::env::var("USERNAME")) {
        (Ok(domain), Ok(name)) if !domain.is_empty() => format!("{domain}\\{name}"),
        (_, Ok(name)) => name,
        _ => {
            return Err(Error::Other(
                "cannot determine the current user (USERNAME is not set)".into(),
            ));
        }
    };
    // Task Scheduler expects UTF-16 XML.
    let xml = spec.windows_task_xml(&user);
    let mut bytes = vec![0xFF, 0xFE];
    for unit in xml.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let file = std::env::temp_dir().join(format!("{}-task-{}.xml", spec.id, std::process::id()));
    std::fs::write(&file, bytes)?;
    // `/F` replaces an existing task of the same name: no duplicates.
    let result = run_checked(
        "schtasks",
        &[
            "/Create",
            "/TN",
            &spec.windows_task_name(),
            "/XML",
            &file.display().to_string(),
            "/F",
        ],
    );
    let _ = std::fs::remove_file(&file);
    result?;
    Ok(format!(
        "installed Task Scheduler task \\{} for {user}; it starts at logon, or now with `reason-network service start`",
        spec.windows_task_name()
    ))
}

/// Start the daemon through the service mechanism.
pub async fn start(spec: &ServiceSpec, endpoint: &Endpoint) -> Result<String> {
    if let Some(pid) = daemon_pid(endpoint).await {
        return Ok(format!("daemon is already running (pid {pid})"));
    }
    if !is_installed(spec) {
        return Err(Error::Other(
            "the service is not installed: run `reason-network service install` first (or run `reason-network daemon-start` in a terminal)".into(),
        ));
    }
    if cfg!(windows) {
        run_checked("schtasks", &["/Run", "/TN", &spec.windows_task_name()])?;
    } else if cfg!(target_os = "macos") {
        if launchd_loaded(spec) {
            run_checked("launchctl", &["kickstart", &launchd_target(spec)?])?;
        } else {
            run_checked(
                "launchctl",
                &[
                    "bootstrap",
                    &launchd_domain()?,
                    &launchd_plist_path(spec).display().to_string(),
                ],
            )?;
        }
    } else if cfg!(target_os = "linux") {
        run_checked("systemctl", &["--user", "start", &spec.systemd_unit_name()])?;
    } else {
        return Err(unsupported());
    }
    if wait_until(endpoint, true, START_TIMEOUT).await {
        let pid = daemon_pid(endpoint).await.unwrap_or_default();
        Ok(format!("daemon started (pid {pid})"))
    } else {
        Err(Error::Other(format!(
            "the service was started but the daemon did not answer within {} s; check {}",
            START_TIMEOUT.as_secs(),
            status(spec, endpoint).await.log
        )))
    }
}

/// Stop the daemon. The data directory is not touched.
pub async fn stop(spec: &ServiceSpec, endpoint: &Endpoint) -> Result<String> {
    if daemon_pid(endpoint).await.is_none() {
        return Ok("daemon is not running".into());
    }
    if cfg!(target_os = "macos") && launchd_loaded(spec) {
        let _ = run_quiet("launchctl", &["bootout", &launchd_target(spec)?]);
    } else if cfg!(target_os = "linux") && is_installed(spec) {
        let _ = run_quiet("systemctl", &["--user", "stop", &spec.systemd_unit_name()]);
    }
    // Windows (the task only launches the daemon), or a daemon that was
    // started by hand: ask it to shut down over IPC.
    if daemon_pid(endpoint).await.is_some() {
        let request = Request {
            id: Some("service-stop".into()),
            method: Method::Shutdown,
            params: json!({}),
        };
        let _: Result<Response> = ipc::request_once(endpoint, &request).await;
    }
    if wait_until(endpoint, false, Duration::from_secs(15)).await {
        Ok("daemon stopped".into())
    } else {
        Err(Error::Other(
            "the daemon did not stop within 15 s; check `reason-network daemon-status`".into(),
        ))
    }
}

/// Stop the daemon and remove the service definition. Never deletes data.
pub async fn uninstall(spec: &ServiceSpec, endpoint: &Endpoint) -> Result<String> {
    let installed = is_installed(spec);
    stop(spec, endpoint).await?;
    if installed {
        if cfg!(windows) {
            run_checked(
                "schtasks",
                &["/Delete", "/TN", &spec.windows_task_name(), "/F"],
            )?;
        } else if cfg!(target_os = "macos") {
            let _ = run_quiet("launchctl", &["bootout", &launchd_target(spec)?]);
            std::fs::remove_file(launchd_plist_path(spec))?;
        } else if cfg!(target_os = "linux") {
            let _ = run_quiet(
                "systemctl",
                &["--user", "disable", "--now", &spec.systemd_unit_name()],
            );
            std::fs::remove_file(systemd_unit_path(spec))?;
            let _ = run_quiet("systemctl", &["--user", "daemon-reload"]);
        } else {
            return Err(unsupported());
        }
    }
    let head = if installed {
        format!("removed service {}", spec.id)
    } else {
        format!("service {} was not installed", spec.id)
    };
    Ok(format!(
        "{head}; data directory preserved: {}",
        spec.data_dir.display()
    ))
}

/// Start the daemon detached from any console, logging to the log file.
///
/// This is what the Windows logon task runs. It returns once the daemon
/// answers, so a failure shows up as the task's last result.
pub async fn launch(spec: &ServiceSpec, endpoint: &Endpoint) -> Result<String> {
    if let Some(pid) = daemon_pid(endpoint).await {
        return Ok(format!("daemon is already running (pid {pid})"));
    }
    std::fs::create_dir_all(spec.log_file.parent().unwrap_or(&spec.data_dir))?;
    rotate_log(&spec.log_file);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&spec.log_file)?;
    let mut command = std::process::Command::new(&spec.exe);
    command
        .arg("daemon-start")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    spawn_detached(&mut command)?;
    if wait_until(endpoint, true, START_TIMEOUT).await {
        Ok("daemon launched".into())
    } else {
        Err(Error::Other(format!(
            "the daemon did not answer within {} s; see {}",
            START_TIMEOUT.as_secs(),
            spec.log_file.display()
        )))
    }
}

#[cfg(windows)]
fn spawn_detached(command: &mut std::process::Command) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    // Break away from the Task Scheduler job when allowed, so the daemon
    // outlives the launcher; fall back when the job forbids it.
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB);
    if command.spawn().is_ok() {
        return Ok(());
    }
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| Error::Other(format!("cannot start the daemon: {error}")))
}

#[cfg(not(windows))]
fn spawn_detached(command: &mut std::process::Command) -> Result<()> {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| Error::Other(format!("cannot start the daemon: {error}")))
}

fn rotate_log(path: &Path) {
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() > LOG_ROTATE_BYTES) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
}

// ----- helpers -------------------------------------------------------------

fn run_output(program: &str, args: &[&str]) -> Result<std::process::Output> {
    Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| Error::Other(format!("cannot run `{program}`: {error}")))
}

fn run_quiet(program: &str, args: &[&str]) -> Result<()> {
    let output = run_output(program, args)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::Other(format!("`{program}` failed")))
    }
}

fn run_checked(program: &str, args: &[&str]) -> Result<String> {
    let output = run_output(program, args)?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() {
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() { stdout } else { stderr };
        Err(Error::Other(format!(
            "`{program} {}` failed: {detail}",
            args.join(" ")
        )))
    }
}

fn require_systemd() -> Result<()> {
    run_quiet("systemctl", &["--user", "show-environment"]).map_err(|_| {
        Error::Other(
            "systemd user services are not available (no `systemctl --user` session); run `reason-network daemon-start` under your own supervisor, or enable lingering with `loginctl enable-linger $USER`".into(),
        )
    })
}

fn launchd_domain() -> Result<String> {
    let uid = run_checked("id", &["-u"])?;
    Ok(format!("gui/{uid}"))
}

fn launchd_target(spec: &ServiceSpec) -> Result<String> {
    Ok(format!("{}/{}", launchd_domain()?, spec.launchd_label()))
}

fn launchd_loaded(spec: &ServiceSpec) -> bool {
    launchd_target(spec)
        .and_then(|target| run_quiet("launchctl", &["print", &target]))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(pinned: bool) -> ServiceSpec {
        let config = Config::with_data_dir("/data/rn & co");
        let mut spec = ServiceSpec::new(&config, PathBuf::from("/opt/rn/reason-network"), pinned);
        // Do not depend on the test runner's environment.
        spec.env.retain(|(key, _)| key == ENV_DATA_DIR);
        spec
    }

    #[test]
    fn default_data_dir_uses_the_plain_id() {
        let spec = spec(false);
        assert_eq!(spec.id, "reason-network");
        assert_eq!(spec.windows_task_name(), "ReasonNetwork");
        assert_eq!(spec.launchd_label(), "com.reason-network.reason-network");
        assert!(spec.env.is_empty());
        assert_eq!(spec.launch_args(), vec!["service", "launch"]);
    }

    #[test]
    fn pinned_data_dir_gets_its_own_id_and_environment() {
        let spec = spec(true);
        assert!(spec.id.starts_with("reason-network-"));
        assert_eq!(spec.id.len(), "reason-network-".len() + 8);
        assert!(spec.windows_task_name().starts_with("ReasonNetwork-"));
        assert_eq!(
            spec.launch_args(),
            vec!["service", "launch", "--data-dir", "/data/rn & co"]
        );
    }

    #[test]
    fn systemd_unit_runs_the_daemon_with_escaped_values() {
        let unit = spec(true).systemd_unit();
        assert!(unit.contains("ExecStart=\"/opt/rn/reason-network\" daemon-start\n"));
        assert!(unit.contains("Environment=\"REASON_NETWORK_DATA_DIR=/data/rn & co\"\n"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
        assert_eq!(systemd_quote("100%"), "\"100%%\"");
    }

    #[test]
    fn launchd_plist_is_escaped_and_restarts_only_on_failure() {
        let plist = spec(true).launchd_plist();
        assert!(plist.contains("<string>/opt/rn/reason-network</string>"));
        assert!(plist.contains("<string>daemon-start</string>"));
        assert!(plist.contains("<string>/data/rn &amp; co</string>"));
        assert!(plist.contains("<key>SuccessfulExit</key>\n    <false/>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>"));
    }

    #[test]
    fn windows_task_runs_least_privilege_at_logon() {
        let xml = spec(true).windows_task_xml("PC\\ada");
        assert!(xml.contains("<LogonTrigger>"));
        assert!(xml.contains("<UserId>PC\\ada</UserId>"));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
        assert!(xml.contains("<Command>/opt/rn/reason-network</Command>"));
        assert!(xml.contains(
            "<Arguments>service launch --data-dir &quot;/data/rn &amp; co&quot;</Arguments>"
        ));
    }

    #[test]
    fn windows_quoting_follows_argv_rules() {
        assert_eq!(windows_quote("plain"), "plain");
        assert_eq!(windows_quote("a b"), "\"a b\"");
        assert_eq!(
            windows_quote(r"C:\dir with space\"),
            r#""C:\dir with space\\""#
        );
        assert_eq!(windows_quote("say \"hi\""), r#""say \"hi\"""#);
    }
}
