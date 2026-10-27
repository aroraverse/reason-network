//! `reason-network setup` against isolated configuration fixtures.
//!
//! Every test builds a fake home directory with pre-existing agent
//! configuration and fake agent executables. The real user configuration is
//! never read or written.

use std::path::{Path, PathBuf};

use reason_network::agents::{self, Agent, Host, Outcome};
use serde_json::{Value, json};
use tempfile::TempDir;

const CLAUDE_JSON: &str = r#"{
  "numStartups": 12,
  "theme": "dark",
  "mcpServers": {
    "github": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "keep-me" }
    }
  },
  "projects": {
    "/work/app": { "allowedTools": ["Bash"] }
  }
}
"#;

const CLAUDE_SETTINGS: &str = r#"{
  "model": "opus",
  "env": { "MY_FLAG": "1" },
  "permissions": { "allow": ["Bash(ls:*)"] },
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [ { "type": "command", "command": "echo pre-tool" } ] }
    ],
    "SessionStart": [
      { "hooks": [ { "type": "command", "command": "echo user-session-start" } ] }
    ]
  }
}
"#;

const CODEX_CONFIG: &str = r#"# my codex settings
model = "gpt-5-codex"

[mcp_servers.docs]
command = "docs-mcp" # keep this comment
args = ["--port", "1"]

[mcp_servers.docs.env]
DOCS_TOKEN = "keep-me"

[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = "echo user-stop"
"#;

/// A fake user environment.
struct Fixture {
    dir: TempDir,
    host: Host,
}

impl Fixture {
    /// Both agents installed, with existing configuration.
    fn new() -> Self {
        let fixture = Self::empty(&[Agent::Claude, Agent::Codex]);
        fixture.write(&fixture.host.claude_json(), CLAUDE_JSON);
        fixture.write(&fixture.host.claude_settings(), CLAUDE_SETTINGS);
        fixture.write(&fixture.host.codex_config(), CODEX_CONFIG);
        fixture
    }

    /// The given agents installed, no configuration files.
    fn empty(installed: &[Agent]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        for agent in installed {
            let name = if cfg!(windows) {
                format!("{}.cmd", agent.executable_name())
            } else {
                agent.executable_name().to_string()
            };
            std::fs::write(bin.join(name), "fake agent").unwrap();
        }
        let exe = dir.path().join("install").join(if cfg!(windows) {
            "reason-network.exe"
        } else {
            "reason-network"
        });
        let host = Host {
            home,
            path: vec![bin],
            claude_config_dir: None,
            codex_home: None,
            exe,
            data_dir: None,
        };
        Self { dir, host }
    }

    fn write(&self, path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn read(&self, path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    fn json(&self, path: &Path) -> Value {
        serde_json::from_str(&self.read(path)).unwrap()
    }

    /// Every file under the fixture, with contents, for change detection.
    fn snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let mut files = Vec::new();
        let mut stack = vec![self.dir.path().to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let bytes = std::fs::read(&path).unwrap();
                    files.push((path, bytes));
                }
            }
        }
        files.sort();
        files
    }

    fn backups(&self) -> Vec<PathBuf> {
        self.snapshot()
            .into_iter()
            .map(|(path, _)| path)
            .filter(|path| path.to_string_lossy().contains(".reason-network-backup-"))
            .collect()
    }
}

fn setup(fixture: &Fixture, agents: &[Agent]) -> agents::SetupReport {
    agents::run_setup(&fixture.host, agents, false)
}

fn our_handlers(settings: &Value, event: &str) -> usize {
    settings["hooks"][event]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|group| group["hooks"].as_array().cloned().unwrap_or_default())
        .filter(|handler| {
            handler["command"]
                .as_str()
                .is_some_and(|c| c.contains("hook --agent claude"))
        })
        .count()
}

#[test]
fn dry_run_makes_no_changes() {
    let fixture = Fixture::new();
    // A missing settings directory must not be created by a dry run either.
    let empty = Fixture::empty(&[Agent::Claude, Agent::Codex]);
    for fixture in [&fixture, &empty] {
        let before = fixture.snapshot();
        let report = agents::run_setup(&fixture.host, &Agent::ALL, true);
        assert_eq!(before, fixture.snapshot(), "a dry run must not touch files");
        for (_, outcome) in &report.entries {
            let Outcome::Planned(plan) = outcome else {
                panic!("expected a plan, got {outcome:?}");
            };
            assert!(!plan.is_noop(), "the plan should propose changes");
        }
        let rendered = report.render();
        assert!(
            rendered.contains("dry run: no files were changed"),
            "{rendered}"
        );
        assert!(
            rendered.contains("add MCP server `reason-network`"),
            "{rendered}"
        );
    }
}

#[test]
fn existing_agent_configuration_is_preserved() {
    let fixture = Fixture::new();
    setup(&fixture, &Agent::ALL).into_result().unwrap();

    // Claude: other servers, settings, projects and hooks untouched.
    let claude = fixture.json(&fixture.host.claude_json());
    assert_eq!(claude["numStartups"], 12);
    assert_eq!(claude["theme"], "dark");
    assert_eq!(claude["projects"]["/work/app"]["allowedTools"][0], "Bash");
    assert_eq!(
        claude["mcpServers"]["github"]["env"]["GITHUB_TOKEN"],
        "keep-me"
    );
    let ours = &claude["mcpServers"]["reason-network"];
    assert_eq!(ours["type"], "stdio");
    assert_eq!(ours["command"], fixture.host.exe.display().to_string());
    assert_eq!(ours["args"], json!(["mcp", "--agent", "claude"]));
    // Key order of the user's file is kept.
    let keys: Vec<&String> = claude.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["numStartups", "theme", "mcpServers", "projects"]);

    let settings = fixture.json(&fixture.host.claude_settings());
    assert_eq!(settings["model"], "opus");
    assert_eq!(settings["env"]["MY_FLAG"], "1");
    assert_eq!(settings["permissions"]["allow"][0], "Bash(ls:*)");
    assert_eq!(
        settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
        "echo pre-tool"
    );
    assert_eq!(
        settings["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "echo user-session-start"
    );
    for event in Agent::Claude.hook_events() {
        assert_eq!(our_handlers(&settings, event), 1, "{event}");
    }

    // Codex: comments, other servers and user hooks untouched.
    let codex = fixture.read(&fixture.host.codex_config());
    assert!(codex.starts_with("# my codex settings\n"), "{codex}");
    assert!(
        codex.contains("command = \"docs-mcp\" # keep this comment"),
        "{codex}"
    );
    assert!(codex.contains("DOCS_TOKEN = \"keep-me\""), "{codex}");
    assert!(codex.contains("command = \"echo user-stop\""), "{codex}");
    let doc: toml_edit::DocumentMut = codex.parse().unwrap();
    let server = &doc["mcp_servers"]["reason-network"];
    assert_eq!(
        server["command"].as_str(),
        Some(fixture.host.exe.display().to_string().as_str())
    );
    let args: Vec<&str> = server["args"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(args, ["mcp", "--agent", "codex"]);
    for event in Agent::Codex.hook_events() {
        let groups = doc["hooks"][event].as_array_of_tables().unwrap();
        let ours = groups
            .iter()
            .flat_map(|group| group["hooks"].as_array_of_tables().unwrap().iter())
            .filter(|handler| {
                handler["command"]
                    .as_str()
                    .is_some_and(|c| c.contains("hook --agent codex"))
            })
            .count();
        assert_eq!(ours, 1, "{event}");
    }
    assert_eq!(doc["hooks"]["Stop"].as_array_of_tables().unwrap().len(), 2);
    assert_eq!(
        doc["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"].as_integer(),
        Some(3),
        "Codex caps SessionEnd hooks at 3 s"
    );
}

#[test]
fn repeated_setup_does_not_duplicate_entries() {
    let fixture = Fixture::new();
    setup(&fixture, &Agent::ALL).into_result().unwrap();
    let after_first = fixture.snapshot();
    let backups = fixture.backups().len();

    let second = setup(&fixture, &Agent::ALL);
    for (_, outcome) in &second.entries {
        let Outcome::Applied(plan, applied) = outcome else {
            panic!("expected applied, got {outcome:?}");
        };
        assert!(plan.is_noop(), "second run must be a no-op: {plan:?}");
        assert!(applied.iter().all(|applied| !applied.changed));
    }
    let rendered = second.render();
    assert!(rendered.contains("already configured"), "{rendered}");
    assert!(
        !rendered.contains("next:"),
        "no follow-up steps on a no-op: {rendered}"
    );
    assert_eq!(after_first, fixture.snapshot(), "nothing rewritten");
    assert_eq!(backups, fixture.backups().len(), "no extra backups");

    let settings = fixture.json(&fixture.host.claude_settings());
    for event in Agent::Claude.hook_events() {
        assert_eq!(our_handlers(&settings, event), 1);
    }
}

#[test]
fn setup_replaces_a_stale_entry_instead_of_adding_another() {
    let fixture = Fixture::new();
    setup(&fixture, &Agent::ALL).into_result().unwrap();

    // The executable moved (for example after reinstalling elsewhere).
    let mut moved = fixture.host.clone();
    moved.exe = fixture.dir.path().join("elsewhere").join("reason-network");
    moved.data_dir = Some(fixture.dir.path().join("data"));
    agents::run_setup(&moved, &Agent::ALL, false)
        .into_result()
        .unwrap();

    let claude = fixture.json(&fixture.host.claude_json());
    assert_eq!(
        claude["mcpServers"]["reason-network"]["command"],
        moved.exe.display().to_string()
    );
    assert_eq!(
        claude["mcpServers"]["reason-network"]["env"]["REASON_NETWORK_DATA_DIR"],
        fixture.dir.path().join("data").display().to_string()
    );
    assert_eq!(claude["mcpServers"].as_object().unwrap().len(), 2);

    let settings = fixture.json(&fixture.host.claude_settings());
    for event in Agent::Claude.hook_events() {
        assert_eq!(our_handlers(&settings, event), 1, "{event}");
    }
    let start = settings["hooks"]["SessionStart"].to_string();
    assert!(start.contains("elsewhere"), "{start}");
    assert!(start.contains("--data-dir"), "{start}");

    let codex: toml_edit::DocumentMut = fixture.read(&fixture.host.codex_config()).parse().unwrap();
    let start = codex["hooks"]["SessionStart"].as_array_of_tables().unwrap();
    assert_eq!(start.len(), 1);
    assert_eq!(
        codex["mcp_servers"]["reason-network"]["env"]["REASON_NETWORK_DATA_DIR"].as_str(),
        Some(
            fixture
                .dir
                .path()
                .join("data")
                .display()
                .to_string()
                .as_str()
        )
    );
}

#[test]
fn backups_are_created_before_changes() {
    let fixture = Fixture::new();
    let report = setup(&fixture, &Agent::ALL);
    let mut backed_up = 0;
    for (_, outcome) in &report.entries {
        let Outcome::Applied(_, applied) = outcome else {
            panic!("expected applied");
        };
        for applied in applied {
            let backup = applied
                .backup
                .as_ref()
                .expect("existing files are backed up");
            let original = std::fs::read_to_string(backup).unwrap();
            let expected = match applied.path.file_name().unwrap().to_str().unwrap() {
                ".claude.json" => CLAUDE_JSON,
                "settings.json" => CLAUDE_SETTINGS,
                "config.toml" => CODEX_CONFIG,
                other => panic!("unexpected file {other}"),
            };
            assert_eq!(original, expected, "backup holds the previous contents");
            backed_up += 1;
        }
    }
    assert_eq!(backed_up, 3);

    // Files that did not exist are created, with nothing to back up.
    let empty = Fixture::empty(&[Agent::Claude]);
    let report = setup(&empty, &[Agent::Claude]);
    let Outcome::Applied(_, applied) = &report.entries[0].1 else {
        panic!("expected applied");
    };
    assert!(applied.iter().all(|a| a.changed && a.backup.is_none()));
    assert!(empty.host.claude_json().is_file());
    assert!(empty.host.claude_settings().is_file());
    assert!(empty.backups().is_empty());
}

#[test]
fn missing_agents_produce_clear_errors() {
    let fixture = Fixture::empty(&[]);
    let report = setup(&fixture, &[Agent::Codex]);
    let Outcome::Missing(reason) = &report.entries[0].1 else {
        panic!("expected missing");
    };
    assert!(
        reason.contains("OpenAI Codex CLI is not installed"),
        "{reason}"
    );
    assert!(reason.contains("npm install -g @openai/codex"), "{reason}");
    assert!(report.render().contains("skipped"));
    assert!(report.into_result().is_err());
    assert!(fixture.snapshot().is_empty(), "nothing written");

    // `all` with only Claude installed configures Claude and skips Codex.
    let partial = Fixture::empty(&[Agent::Claude]);
    let report = setup(&partial, &Agent::ALL);
    assert!(matches!(report.entries[0].1, Outcome::Applied(..)));
    assert!(matches!(report.entries[1].1, Outcome::Missing(_)));
    report.into_result().unwrap();
    assert!(!partial.host.codex_config().exists());

    // `all` with nothing installed is an error.
    let none = Fixture::empty(&[]);
    let error = setup(&none, &Agent::ALL).into_result().unwrap_err();
    assert!(error.to_string().contains("no supported agent"), "{error}");
}

#[test]
fn invalid_existing_configuration_is_reported_and_left_alone() {
    let fixture = Fixture::empty(&[Agent::Claude, Agent::Codex]);
    fixture.write(&fixture.host.claude_json(), "{ not json");
    fixture.write(&fixture.host.codex_config(), "model = = 1");
    let before = fixture.snapshot();
    let report = setup(&fixture, &Agent::ALL);
    for (_, outcome) in &report.entries {
        let Outcome::Failed(reason) = outcome else {
            panic!("expected failure, got {outcome:?}");
        };
        assert!(reason.contains("nothing was changed"), "{reason}");
    }
    assert_eq!(before, fixture.snapshot());
    assert!(report.into_result().is_err());
}

#[test]
fn agent_config_directory_overrides_are_honoured() {
    let mut fixture = Fixture::empty(&[Agent::Claude, Agent::Codex]);
    let claude_dir = fixture.dir.path().join("claude-config");
    let codex_dir = fixture.dir.path().join("codex-home");
    fixture.host.claude_config_dir = Some(claude_dir.clone());
    fixture.host.codex_home = Some(codex_dir.clone());
    setup(&fixture, &Agent::ALL).into_result().unwrap();
    assert!(claude_dir.join(".claude.json").is_file());
    assert!(claude_dir.join("settings.json").is_file());
    assert!(codex_dir.join("config.toml").is_file());
    assert!(!fixture.host.home.join(".claude.json").exists());
    assert!(!fixture.host.home.join(".codex").exists());
}

#[test]
fn inline_codex_hook_arrays_are_supported() {
    let fixture = Fixture::empty(&[Agent::Codex]);
    fixture.write(
        &fixture.host.codex_config(),
        "[hooks]\nStop = [{ hooks = [{ type = \"command\", command = \"echo mine\" }] }]\n",
    );
    setup(&fixture, &[Agent::Codex]).into_result().unwrap();
    setup(&fixture, &[Agent::Codex]).into_result().unwrap();
    let text = fixture.read(&fixture.host.codex_config());
    assert!(text.contains("echo mine"), "{text}");
    assert_eq!(text.matches("hook --agent codex").count(), 4, "{text}");
}

#[test]
fn inspect_reports_configuration_state() {
    let fixture = Fixture::new();
    let before = agents::inspect(Agent::Claude, &fixture.host);
    assert!(before.mcp_command.is_none());
    assert!(!before.up_to_date);

    setup(&fixture, &Agent::ALL).into_result().unwrap();
    for agent in Agent::ALL {
        let status = agents::inspect(agent, &fixture.host);
        assert!(status.error.is_none(), "{status:?}");
        assert!(status.up_to_date, "{agent:?}");
        assert_eq!(
            status.mcp_command.as_deref(),
            Some(fixture.host.exe.display().to_string().as_str())
        );
    }
}

/// The real CLI with a fake home: dry run writes nothing, a missing agent
/// fails with instructions.
#[test]
fn cli_setup_dry_run_and_missing_agent() {
    let fixture = Fixture::empty(&[Agent::Claude]);
    let exe = env!("CARGO_BIN_EXE_reason-network");
    let run = |args: &[&str], path: &Path| {
        std::process::Command::new(exe)
            .args(args)
            .env("HOME", &fixture.host.home)
            .env("USERPROFILE", &fixture.host.home)
            .env("PATH", path)
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env("REASON_NETWORK_DATA_DIR", fixture.dir.path().join("data"))
            .output()
            .unwrap()
    };

    let output = run(
        &["setup", "--agent", "claude", "--dry-run"],
        &fixture.host.path[0],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("dry run: no files were changed"),
        "{stdout}"
    );
    assert!(stdout.contains("would create"), "{stdout}");
    assert!(!fixture.host.home.join(".claude.json").exists());
    assert!(!fixture.host.home.join(".claude").exists());

    let output = run(&["setup", "--agent", "codex"], &fixture.host.path[0]);
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("OpenAI Codex CLI is not installed"),
        "{stdout}"
    );

    let output = run(&["setup", "--agent", "gemini"], &fixture.host.path[0]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unsupported agent `gemini`"), "{stderr}");
}
