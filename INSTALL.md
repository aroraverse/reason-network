# Installing Reason Network

Reason Network is one executable, `reason-network`. It installs per user, without administrator
rights, and keeps its data in a separate directory that installs, upgrades and uninstalls never
touch unless you explicitly ask.

| Platform | Release target | Package |
| --- | --- | --- |
| Windows x86-64 | `x86_64-pc-windows-msvc` | `reason-network-<version>-x86_64-pc-windows-msvc.zip` |
| Linux x86-64 (glibc) | `x86_64-unknown-linux-gnu` | `reason-network-<version>-x86_64-unknown-linux-gnu.tar.gz` |
| macOS Apple Silicon | `aarch64-apple-darwin` | `reason-network-<version>-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `x86_64-apple-darwin` | `reason-network-<version>-x86_64-apple-darwin.tar.gz` |

Each release also carries the bare executable for every target (used by `reason-network update`),
the two installer scripts, and `SHA256SUMS` covering all of them.

| | Default install directory | Default data directory |
| --- | --- | --- |
| Windows | `%LOCALAPPDATA%\Programs\reason-network` | `%APPDATA%\reason-network` |
| macOS, Linux | `~/.local/bin` | `$XDG_DATA_HOME/reason-network`, else `~/.local/share/reason-network` |

`REASON_NETWORK_DATA_DIR` (or `--data-dir`) overrides the data directory for every command.

## 1. Install

The installers never execute anything they have not verified. Download the installer and
`SHA256SUMS` from the release page, check the installer, then run it. The installer downloads the
package for your platform and refuses to install it unless its SHA-256 matches `SHA256SUMS`.

**macOS / Linux** (needs `tar`, `sha256sum` or `shasum`, and `curl` or `wget`):

```sh
REPO=OWNER/reason-network            # the GitHub repository that publishes releases
curl -fsSLO "https://github.com/$REPO/releases/latest/download/install.sh"
curl -fsSLO "https://github.com/$REPO/releases/latest/download/SHA256SUMS"
grep ' install.sh$' SHA256SUMS | sha256sum -c -      # macOS: shasum -a 256 -c
sh install.sh --repo "$REPO"                         # add --version 0.2.0 to pin a version
```

**Windows** (PowerShell 5.1 or 7):

```powershell
$Repo = "OWNER/reason-network"
Invoke-WebRequest "https://github.com/$Repo/releases/latest/download/install.ps1" -OutFile install.ps1
Invoke-WebRequest "https://github.com/$Repo/releases/latest/download/SHA256SUMS" -OutFile SHA256SUMS
(Get-FileHash install.ps1 -Algorithm SHA256).Hash.ToLower()
Select-String ' install.ps1$' SHA256SUMS                    # the two hashes must match
powershell -ExecutionPolicy Bypass -File install.ps1 -Repo $Repo -AddToPath
```

`-AddToPath` adds the install directory to your user `PATH`; without it the installer prints the
command to do so. Open a new terminal afterwards.

### Install from local files (no download)

Put `SHA256SUMS` and the package in one directory (for example the `dist/` directory produced by
`scripts/package.py`, or files downloaded by hand) and point the installer at it:

```sh
sh install.sh --from-dir ./dist
```
```powershell
.\install.ps1 -FromDir .\dist
```

The same checksum verification applies.

### Manual install

Extract the package and copy `reason-network` (`reason-network.exe`) to any directory on your
`PATH`. Verify the package first: `sha256sum -c SHA256SUMS --ignore-missing`.

### Build from source

```sh
cargo build --release      # Rust 1.85 or newer
# target/release/reason-network
```

## 2. Set up

```sh
reason-network setup --agent all --dry-run   # show what would change
reason-network setup --agent all             # configure Claude Code and Codex
reason-network service install               # start the daemon at login
reason-network service start                 # start it now
reason-network doctor                        # verify everything
```

`setup` accepts `claude`, `codex` or `all`. It edits only the `reason-network` entries, backs up
every file it changes (`<file>.reason-network-backup-<timestamp>`), and is safe to repeat.

| Agent | MCP server | Lifecycle hooks |
| --- | --- | --- |
| Claude Code | `mcpServers` in `~/.claude.json` (user scope) | `hooks` in `~/.claude/settings.json` |
| Codex CLI | `[mcp_servers.reason-network]` in `~/.codex/config.toml` | `[hooks]` in `~/.codex/config.toml` |

`CLAUDE_CONFIG_DIR` and `CODEX_HOME` are honoured. Restart running agent sessions afterwards.
Codex skips new hooks until you trust them: start `codex`, run `/hooks`, trust the
`reason-network` entries. The MCP tools work without the hooks.

## 3. Background service

| OS | Mechanism | Definition |
| --- | --- | --- |
| Windows | Task Scheduler logon task, current user, least privilege | Task `\ReasonNetwork` |
| Linux | systemd user service | `~/.config/systemd/user/reason-network.service` |
| macOS | launchd user agent | `~/Library/LaunchAgents/com.reason-network.reason-network.plist` |

```sh
reason-network service install     # register (idempotent); starts at the next login
reason-network service start
reason-network service status
reason-network service stop
reason-network service uninstall   # stops and unregisters; the data directory is kept
```

A non-default data directory gets its own service (`reason-network-<hash>`), so a test setup never
replaces the real one. Logs: `<data dir>/logs/daemon.log` on Windows and macOS,
`journalctl --user -u reason-network` on Linux.

On Linux, user services stop at logout unless lingering is enabled:
`loginctl enable-linger $USER`.

## 4. Updates

```sh
reason-network --version
reason-network update --check                 # what is available; changes nothing
reason-network update                         # install the newest release
reason-network update --version 0.2.0         # install exactly this version
reason-network update --check --source ./dist # use a local release directory
```

Updates are always explicit. The new executable is verified against `SHA256SUMS`, test-run, then
swapped in with two renames; the previous one is kept as `reason-network(.exe).old`. Configuration
and data are not touched. Restart the daemon afterwards (`service stop`, `service start`).

Release builds know their repository. A source build needs `--source OWNER/NAME`,
`REASON_NETWORK_REPO=OWNER/NAME`, or `REASON_NETWORK_RELEASE_URL=<url or directory>`.

Re-running the installer is the other upgrade path; it also preserves configuration and data.

## 5. Uninstall

```sh
sh install.sh --uninstall                 # keeps the data directory
sh install.sh --uninstall --purge-data    # also deletes the data directory
```
```powershell
.\install.ps1 -Uninstall                  # keeps the data directory
.\install.ps1 -Uninstall -PurgeData       # also deletes the data directory
```

Uninstalling stops and removes the background service and the executable (and, on Windows, the
`PATH` entry). Remove the agent entries with the agents' own commands:
`claude mcp remove reason-network --scope user` and `codex mcp remove reason-network`, and delete
the `reason-network hook` entries from the hooks if you no longer want them (setup's backups hold
the previous versions of those files).

## Troubleshooting

Start with `reason-network doctor`. Every warning or failure comes with a fix.

| Symptom | Fix |
| --- | --- |
| `daemon ... not reachable` | `reason-network service start`, or `reason-network daemon-start` in a terminal to see errors |
| `service start` times out | read the log shown by `reason-network service status` |
| Agent tools say the daemon is not running | the agent and the daemon use different data directories; run `setup` with the same `--data-dir`/`REASON_NETWORK_DATA_DIR` as the daemon |
| `MCP config ... configured for <other path>` | re-run `reason-network setup --agent <agent>` with the executable you want agents to use |
| `project-scoped reason-network entries override it` | an older per-project Claude Code entry shadows the user-scope one; remove it in that project with `claude mcp remove reason-network --scope local` |
| Codex shows no session status | trust the hooks with `/hooks` in `codex`; the MCP tools work regardless |
| `systemd user services are not available` | no user session bus (containers, some SSH sessions); run `daemon-start` under your own supervisor |
| Windows firewall prompt on first start | allow on private networks for LAN connections; relay connectivity works without it |
| `checksum mismatch` | the download is corrupt or was altered; nothing was installed, download again |
| Windows: "file contains a virus or potentially unwanted software" | the binaries are unsigned, and Defender's machine-learning heuristics can flag a fresh build (observed on a local release build). Verify the SHA-256 against `SHA256SUMS`, then report a false positive to Microsoft or build from source; code signing is not yet in place |
