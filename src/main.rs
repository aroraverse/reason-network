//! `reason-network` command line interface.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use reason_network::agents::{self, Agent};
use reason_network::config::{Config, ENV_DATA_DIR, ENV_SESSION_KEY};
use reason_network::daemon::{Daemon, VERSION};
use reason_network::error::{Error, Result};
use reason_network::ipc;
use reason_network::protocol::{Method, Request};
use serde_json::{Value, json};

#[derive(Parser, Debug)]
#[command(
    name = "reason-network",
    version,
    about = "Peer-to-peer messaging for AI coding agents",
    long_about = "reason-network lets AI coding agents on the same computer register themselves, \
discover each other and exchange messages through a local daemon.\n\n\
SESSION IDENTITY\n\
  Commands that act as an agent need a session key. Pass --session (or --session-key for the \
MCP server) or set REASON_NETWORK_SESSION_KEY in the environment.\n\n\
STORAGE\n\
  The database lives in the application data directory. Set REASON_NETWORK_DATA_DIR or pass \
--data-dir to override it (for example to isolate tests).\n\n\
GETTING STARTED\n\
  reason-network setup --agent all     configure Claude Code and Codex\n\
  reason-network service install       start the daemon at login\n\
  reason-network service start         start it now\n\
  reason-network doctor                check everything"
)]
struct Cli {
    /// Print the raw JSON result instead of a human readable summary.
    #[arg(long, global = true)]
    json: bool,

    /// Application data directory. Overrides REASON_NETWORK_DATA_DIR.
    #[arg(long, global = true, value_name = "DIR")]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the local daemon in the foreground.
    DaemonStart,

    /// Ask a running daemon to stop.
    DaemonStop,

    /// Show whether the daemon is running.
    DaemonStatus,

    /// Register an agent session.
    Register {
        /// Human readable session name.
        #[arg(long)]
        name: String,
        /// Stable session key. Defaults to REASON_NETWORK_SESSION_KEY or a generated key.
        #[arg(long)]
        session_key: Option<String>,
        /// Agent type, for example `claude` or `codex`.
        #[arg(long)]
        agent_type: Option<String>,
        /// Provider specific handle used for push delivery, e.g. a Claude session id.
        #[arg(long)]
        agent_ref: Option<String>,
        /// Working directory of the agent.
        #[arg(long)]
        working_dir: Option<String>,
        /// Repository the agent works in.
        #[arg(long)]
        repo: Option<String>,
        /// Branch the agent works on.
        #[arg(long)]
        branch: Option<String>,
        /// Initial status.
        #[arg(long)]
        status: Option<String>,
        /// Short activity description.
        #[arg(long)]
        activity: Option<String>,
    },

    /// List the registered sessions.
    SessionList,

    /// Show one session.
    SessionShow {
        /// Session key.
        session_key: String,
    },

    /// Remove a session.
    SessionRemove {
        /// Session key.
        session_key: String,
    },

    /// Update the status or activity of a session.
    SessionUpdate {
        /// Session key.
        session_key: String,
        /// New status.
        #[arg(long)]
        status: Option<String>,
        /// New activity description.
        #[arg(long)]
        activity: Option<String>,
        /// New provider handle used for push delivery.
        #[arg(long)]
        agent_ref: Option<String>,
    },

    /// Send a message to another session.
    SendMessage {
        /// Sending session key.
        #[arg(long)]
        from: String,
        /// Recipient session key.
        #[arg(long)]
        to: String,
        /// Message body.
        body: String,
    },

    /// Show the inbox of a session.
    Inbox {
        /// Session key. Defaults to REASON_NETWORK_SESSION_KEY.
        #[arg(long)]
        session: Option<String>,
        /// Maximum number of messages.
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Only show unread messages.
        #[arg(long)]
        unread_only: bool,
    },

    /// Count the unread messages of a session.
    Unread {
        /// Session key. Defaults to REASON_NETWORK_SESSION_KEY.
        #[arg(long)]
        session: Option<String>,
    },

    /// Mark messages as read.
    MarkRead {
        /// Session key. Defaults to REASON_NETWORK_SESSION_KEY.
        #[arg(long)]
        session: Option<String>,
        /// Message id. Repeat for several messages. Marks everything when omitted.
        #[arg(long = "id")]
        ids: Vec<String>,
    },

    /// Ask a human a question on behalf of a session.
    AskQuestion {
        /// Session asking the question. Defaults to REASON_NETWORK_SESSION_KEY.
        #[arg(long)]
        session: Option<String>,
        /// The question text.
        question: String,
    },

    /// List the questions agents asked a human.
    AskList {
        /// Only show questions with this status.
        #[arg(long)]
        status: Option<String>,
    },

    /// Answer a question. The answer reaches the agent as an inbox message.
    AskReply {
        /// Question id.
        question_id: String,
        /// The answer.
        answer: String,
    },

    /// Show diagnostic information about the daemon.
    Diagnostics,

    /// Show this machine's peer id and dialable address.
    MachineId,

    /// Create a team administered by this machine.
    TeamCreate {
        /// Human readable team name.
        #[arg(long)]
        name: String,
    },

    /// Issue an invitation token for a team.
    TeamInvite {
        /// Team id.
        #[arg(long)]
        team: String,
        /// How long the invitation stays valid, in minutes.
        #[arg(long, default_value_t = 60)]
        ttl_minutes: i64,
    },

    /// Join a team using an invitation token.
    TeamJoin {
        /// The token printed by `team-invite` on the other machine.
        token: String,
    },

    /// List the teams this machine belongs to.
    TeamList,

    /// List the members of a team.
    TeamMembers {
        /// Team id.
        #[arg(long)]
        team: String,
    },

    /// Leave a team.
    TeamLeave {
        /// Team id.
        #[arg(long)]
        team: String,
    },

    /// Revoke a member of a team (admin only).
    TeamRevoke {
        /// Team id.
        #[arg(long)]
        team: String,
        /// Peer id of the member to revoke, as shown by `team-members`.
        #[arg(long)]
        peer: String,
    },

    /// List known peer machines and their connection state.
    PeerList,

    /// Connect to a peer explicitly using its address JSON.
    PeerConnect {
        /// Address JSON printed by `machine-id --json`.
        #[arg(long)]
        addr: String,
    },

    /// List local and remote sessions in one directory.
    Directory,

    /// Show recent outbound messages and their delivery state.
    Outbox {
        /// Maximum number of messages.
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },

    /// Run the MCP server on stdio.
    Mcp {
        /// Session key of this agent. Defaults to REASON_NETWORK_SESSION_KEY.
        #[arg(long)]
        session_key: Option<String>,
        /// The agent that launches the server (`claude` or `codex`). Without a
        /// session key, a stable key is derived from the agent and the working
        /// directory.
        #[arg(long)]
        agent: Option<String>,
    },

    /// Configure a coding agent to use Reason Network (MCP server and hooks).
    Setup {
        /// `claude`, `codex` or `all`.
        #[arg(long)]
        agent: String,
        /// Show the proposed changes without modifying any file.
        #[arg(long)]
        dry_run: bool,
    },

    /// Check the installation, daemon, network, agents and service.
    Doctor,

    /// Manage the optional user-level background service for the daemon.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },

    /// Check for, or explicitly install, a newer release of this executable.
    Update {
        /// Only report whether an update is available; change nothing.
        #[arg(long)]
        check: bool,
        /// Select this exact version instead of the newest (allows downgrades).
        #[arg(long = "version", value_name = "VERSION")]
        target_version: Option<String>,
        /// Release source: GitHub `owner/name`, a URL, or a local directory
        /// with SHA256SUMS. Defaults to REASON_NETWORK_RELEASE_URL /
        /// REASON_NETWORK_REPO.
        #[arg(long)]
        source: Option<String>,
    },

    /// Lifecycle hook entry point run by Claude Code and Codex (JSON on stdin).
    #[command(hide = true)]
    Hook {
        /// `claude` or `codex`.
        #[arg(long)]
        agent: String,
    },
}

#[derive(Subcommand, Debug)]
enum ServiceAction {
    /// Register the daemon to start at login (does not start it).
    Install,
    /// Start the daemon through the service.
    Start,
    /// Stop the daemon.
    Stop,
    /// Show whether the service is installed and the daemon is running.
    Status,
    /// Stop the daemon and remove the service. The data directory is kept.
    Uninstall,
    /// Start the daemon detached (run by the Windows logon task).
    #[command(hide = true)]
    Launch {
        /// Extra environment for the daemon, `KEY=VALUE`.
        #[arg(long = "env")]
        env: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    // An explicitly chosen data directory is pinned into agent and service
    // configuration; the default one is not.
    let pinned_dir = cli
        .data_dir
        .clone()
        .or_else(|| {
            std::env::var_os(ENV_DATA_DIR)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .map(|dir| std::path::absolute(&dir).unwrap_or(dir));
    let config = match &pinned_dir {
        Some(dir) => Config::with_data_dir(dir),
        None => Config::from_env(),
    };

    match cli.command {
        Command::DaemonStart => daemon_start(config).await,
        Command::DaemonStop => {
            let result = call(&config, Method::Shutdown, json!({})).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("daemon is shutting down");
            }
            Ok(())
        }
        Command::DaemonStatus => {
            let result = call(&config, Method::Status, json!({})).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("daemon:    running");
                println!("version:   {VERSION}");
                println!("pid:       {}", value(&result, "pid"));
                println!("endpoint:  {}", value(&result, "endpoint"));
                println!("uptime:    {}s", value(&result, "uptime_seconds"));
                println!("sessions:  {}", value(&result, "sessions"));
                println!("messages:  {}", value(&result, "messages"));
            }
            Ok(())
        }
        Command::Register {
            name,
            session_key,
            agent_type,
            agent_ref,
            working_dir,
            repo,
            branch,
            status,
            activity,
        } => {
            let session_key = session_key
                .or_else(|| std::env::var(ENV_SESSION_KEY).ok())
                .filter(|key| !key.is_empty());
            let result = call(
                &config,
                Method::Register,
                json!({
                    "session_key": session_key,
                    "name": name,
                    "agent_type": agent_type,
                    "agent_ref": agent_ref,
                    "working_dir": working_dir,
                    "repo": repo,
                    "branch": branch,
                    "status": status,
                    "activity": activity,
                }),
            )
            .await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("registered session {}", value(&result, "session_key"));
                println!("  name:    {}", value(&result, "name"));
                println!("  status:  {}", value(&result, "status"));
                if let Some(kind) = result.get("agent_type").and_then(Value::as_str) {
                    println!("  agent:   {kind}");
                }
            }
            Ok(())
        }
        Command::SessionList => {
            let result = call(&config, Method::ListSessions, json!({})).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }
            let sessions = result
                .get("sessions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if sessions.is_empty() {
                println!("no sessions registered");
                return Ok(());
            }
            println!(
                "{:<24} {:<20} {:<10} {:<10} ACTIVITY",
                "SESSION KEY", "NAME", "AGENT", "STATUS"
            );
            for session in sessions {
                println!(
                    "{:<24} {:<20} {:<10} {:<10} {}",
                    text(&session, "session_key"),
                    text(&session, "name"),
                    session
                        .get("agent_type")
                        .and_then(Value::as_str)
                        .unwrap_or("-"),
                    text(&session, "status"),
                    text(&session, "activity"),
                );
            }
            Ok(())
        }
        Command::SessionShow { session_key } => {
            let result = call(
                &config,
                Method::GetSession,
                json!({ "session_key": session_key }),
            )
            .await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("session key:  {}", value(&result, "session_key"));
                println!("name:         {}", value(&result, "name"));
                println!("agent type:   {}", value(&result, "agent_type"));
                println!("status:       {}", value(&result, "status"));
                println!("activity:     {}", value(&result, "activity"));
                println!("working dir:  {}", value(&result, "working_dir"));
                println!(
                    "repo/branch:  {}/{}",
                    value(&result, "repo"),
                    value(&result, "branch")
                );
                println!("last seen:    {}", value(&result, "last_seen"));
            }
            Ok(())
        }
        Command::SessionRemove { session_key } => {
            let result = call(
                &config,
                Method::RemoveSession,
                json!({ "session_key": session_key }),
            )
            .await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else if result.get("removed").and_then(Value::as_bool) == Some(true) {
                println!("removed session {session_key}");
            } else {
                println!("no session {session_key}");
            }
            Ok(())
        }
        Command::SessionUpdate {
            session_key,
            status,
            activity,
            agent_ref,
        } => {
            let result = call(
                &config,
                Method::Update,
                json!({
                    "session_key": session_key,
                    "status": status,
                    "activity": activity,
                    "agent_ref": agent_ref,
                }),
            )
            .await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("updated session {}", value(&result, "session_key"));
            }
            Ok(())
        }
        Command::SendMessage { from, to, body } => {
            let result = call(
                &config,
                Method::SendMessage,
                json!({ "sender_key": from, "recipient_key": to, "body": body }),
            )
            .await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("sent message {}", value(&result, "id"));
                println!("  from:      {}", value(&result, "sender_key"));
                println!("  to:        {}", value(&result, "recipient_key"));
                let delivery = value(&result, "delivery_state");
                if delivery == "local" {
                    println!("  delivery:  local");
                } else {
                    println!("  delivery:  {delivery}");
                    if let Some(peer) = result.get("peer_id").and_then(Value::as_str) {
                        println!("  peer:      {peer}");
                    }
                    if delivery == "pending" {
                        println!("  (queued; it will be retried when the peer is reachable)");
                    }
                }
            }
            Ok(())
        }
        Command::Inbox {
            session,
            limit,
            unread_only,
        } => {
            let session = resolve_session(session)?;
            let result = call(
                &config,
                Method::Inbox,
                json!({
                    "session_key": session,
                    "limit": limit,
                    "unread_only": unread_only,
                }),
            )
            .await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }
            let messages = result
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if messages.is_empty() {
                println!("inbox is empty");
                return Ok(());
            }
            for message in messages {
                println!(
                    "[{}] {} -> {} ({})",
                    text(&message, "id"),
                    text(&message, "sender_key"),
                    text(&message, "recipient_key"),
                    text(&message, "state"),
                );
                println!("{}", text(&message, "body"));
                println!();
            }
            Ok(())
        }
        Command::Unread { session } => {
            let session = resolve_session(session)?;
            let result = call(
                &config,
                Method::UnreadCount,
                json!({ "session_key": session }),
            )
            .await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", value(&result, "unread"));
            }
            Ok(())
        }
        Command::MarkRead { session, ids } => {
            let session = resolve_session(session)?;
            let result = call(
                &config,
                Method::MarkRead,
                json!({ "session_key": session, "message_ids": ids }),
            )
            .await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("marked {} message(s) as read", value(&result, "updated"));
            }
            Ok(())
        }
        Command::AskQuestion { session, question } => {
            let session = resolve_session(session)?;
            let result = call(
                &config,
                Method::AskUser,
                json!({ "session_key": session, "question": question }),
            )
            .await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("asked question {}", value(&result, "id"));
                println!(
                    "answer it with: reason-network ask-reply {} \"<answer>\"",
                    value(&result, "id")
                );
            }
            Ok(())
        }
        Command::AskList { status } => {
            let result = call(&config, Method::ListQuestions, json!({ "status": status })).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }
            let questions = result
                .get("questions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if questions.is_empty() {
                println!("no questions");
                return Ok(());
            }
            for question in questions {
                println!(
                    "[{}] {} ({}): {}",
                    text(&question, "id"),
                    text(&question, "session_key"),
                    text(&question, "status"),
                    text(&question, "question"),
                );
                if let Some(answer) = question.get("answer").and_then(Value::as_str) {
                    println!("  answer: {answer}");
                }
            }
            Ok(())
        }
        Command::AskReply {
            question_id,
            answer,
        } => {
            let result = call(
                &config,
                Method::AnswerQuestion,
                json!({ "question_id": question_id, "answer": answer }),
            )
            .await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("answered question {}", value(&result, "id"));
            }
            Ok(())
        }
        Command::Diagnostics => {
            let result = call(&config, Method::Diagnostics, json!({})).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }
            println!("version:     {VERSION}");
            println!("pid:         {}", value(&result, "pid"));
            println!("endpoint:    {}", value(&result, "endpoint"));
            println!("data dir:    {}", value(&result, "data_dir"));
            println!("database:    {}", value(&result, "db_path"));
            println!("uptime:      {}s", value(&result, "uptime_seconds"));
            println!("sessions:    {}", value(&result, "sessions"));
            println!("messages:    {}", value(&result, "messages"));
            if let Some(delivery) = result.get("delivery") {
                println!(
                    "delivery:    {} via {}",
                    value(delivery, "provider"),
                    value(delivery, "binary")
                );
                println!("live agents: {}", value(delivery, "live_sessions"));
            }
            if let Some(network) = result.get("network") {
                println!("peer id:     {}", value(network, "peer_id"));
                println!("networking:  {}", value(network, "running"));
                println!("relay:       {}", value(network, "relay"));
                println!("discovery:   {}", value(network, "discovery"));
                let connected = network
                    .get("connected")
                    .and_then(Value::as_array)
                    .map(|peers| peers.len())
                    .unwrap_or(0);
                println!("peers:       {connected} connected");
            }
            Ok(())
        }
        Command::MachineId => {
            let result = call(&config, Method::NetInfo, json!({})).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }
            println!("peer id:   {}", value(&result, "peer_id"));
            if let Some(addr) = result.get("addr") {
                println!("address:   {addr}");
            }
            println!("networking:{}", value(&result, "running"));
            println!("teams:     {}", value(&result, "teams"));
            Ok(())
        }
        Command::TeamCreate { name } => {
            let result = call(&config, Method::TeamCreate, json!({ "name": name })).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }
            println!("created team {}", value(&result, "team_id"));
            println!("  name:  {}", value(&result, "name"));
            println!("  admin: {}", value(&result, "admin_peer"));
            println!();
            println!(
                "invite another machine with: reason-network team-invite --team {}",
                value(&result, "team_id")
            );
            Ok(())
        }
        Command::TeamInvite { team, ttl_minutes } => {
            let result = call(
                &config,
                Method::TeamInvite,
                json!({ "team_id": team, "ttl_seconds": ttl_minutes * 60 }),
            )
            .await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }
            println!("{}", value(&result, "token"));
            println!();
            println!("team:     {}", value(&result, "team_id"));
            println!("expires:  {}", value(&result, "expires_at"));
            println!("copy the token line to the other machine and run:");
            println!("  reason-network team-join <token>");
            Ok(())
        }
        Command::TeamJoin { token } => {
            let result = call(&config, Method::TeamJoin, json!({ "token": token })).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }
            println!("joined team {}", value(&result, "team_id"));
            println!("  name:  {}", value(&result, "name"));
            println!("  admin: {}", value(&result, "admin_peer"));
            Ok(())
        }
        Command::TeamList => {
            let result = call(&config, Method::TeamList, json!({})).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }
            let teams = result
                .get("teams")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if teams.is_empty() {
                println!("no teams; create one with: reason-network team-create --name <name>");
                return Ok(());
            }
            println!("{:<34} {:<20} {:<8} MEMBERS", "TEAM ID", "NAME", "ROLE");
            for team in teams {
                let role = if team.get("is_admin").and_then(Value::as_bool) == Some(true) {
                    "admin"
                } else {
                    "member"
                };
                println!(
                    "{:<34} {:<20} {:<8} {}",
                    text(&team, "team_id"),
                    text(&team, "name"),
                    role,
                    value(&team, "members"),
                );
            }
            Ok(())
        }
        Command::TeamMembers { team } => {
            let result = call(&config, Method::TeamMembers, json!({ "team_id": team })).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }
            let members = result
                .get("members")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            println!("{:<52} {:<20} STATE", "PEER ID", "MACHINE");
            for member in members {
                let state = if member.get("self").and_then(Value::as_bool) == Some(true) {
                    "this machine"
                } else if member.get("connected").and_then(Value::as_bool) == Some(true) {
                    "connected"
                } else {
                    "offline"
                };
                println!(
                    "{:<52} {:<20} {}",
                    text(&member, "peer_id"),
                    text(&member, "member_name"),
                    state
                );
            }
            Ok(())
        }
        Command::TeamLeave { team } => {
            let result = call(&config, Method::TeamLeave, json!({ "team_id": team })).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else if result.get("removed").and_then(Value::as_bool) == Some(true) {
                println!("left team {team}");
            } else {
                println!("no team {team}");
            }
            Ok(())
        }
        Command::TeamRevoke { team, peer } => {
            let result = call(
                &config,
                Method::TeamRevoke,
                json!({ "team_id": team, "peer_id": peer }),
            )
            .await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("revoked {} from team {team}", value(&result, "peer_id"));
                println!(
                    "the connection was closed and the revocation was pushed to connected members"
                );
            }
            Ok(())
        }
        Command::PeerList => {
            let result = call(&config, Method::PeerList, json!({})).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }
            let peers = result
                .get("peers")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if peers.is_empty() {
                println!("no known peers yet");
                return Ok(());
            }
            println!("{:<52} {:<20} STATE", "PEER ID", "MACHINE");
            for peer in peers {
                println!(
                    "{:<52} {:<20} {}",
                    text(&peer, "peer_id"),
                    text(&peer, "label"),
                    text(&peer, "status"),
                );
            }
            Ok(())
        }
        Command::PeerConnect { addr } => {
            let result = call(&config, Method::NetConnect, json!({ "addr": addr })).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("connected to {}", value(&result, "peer_id"));
            }
            Ok(())
        }
        Command::Outbox { limit } => {
            let result = call(&config, Method::Outbox, json!({})).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }
            let messages = result
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if messages.is_empty() {
                println!("no outbound messages");
                return Ok(());
            }
            println!(
                "{:<24} {:<20} {:<10} PEER",
                "MESSAGE ID", "RECIPIENT", "STATE"
            );
            for message in messages.into_iter().take(limit as usize) {
                println!(
                    "{:<24} {:<20} {:<10} {}",
                    short(&text(&message, "id")),
                    short(&text(&message, "recipient_key")),
                    text(&message, "delivery_state"),
                    short(&text(&message, "peer_id")),
                );
            }
            Ok(())
        }
        Command::Directory => {
            let result = call(&config, Method::Directory, json!({})).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&result)?);
                return Ok(());
            }
            let sessions = result
                .get("sessions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if sessions.is_empty() {
                println!("no sessions registered");
                return Ok(());
            }
            println!(
                "{:<24} {:<18} {:<9} {:<9} {:<6} MACHINE",
                "SESSION KEY", "NAME", "AGENT", "STATUS", "LOCAL"
            );
            for session in sessions {
                let local = session.get("local").and_then(Value::as_bool) == Some(true);
                println!(
                    "{:<24} {:<18} {:<9} {:<9} {:<6} {}",
                    short(&text(&session, "session_key")),
                    short(&text(&session, "name")),
                    session
                        .get("agent_type")
                        .and_then(Value::as_str)
                        .unwrap_or("-"),
                    text(&session, "status"),
                    if local { "yes" } else { "no" },
                    short(&text(&session, "machine")),
                );
            }
            Ok(())
        }
        Command::Mcp { session_key, agent } => {
            let agent = agent.as_deref().map(Agent::parse).transpose()?;
            let session_key = reason_network::mcp::resolve_session_key(session_key, agent)?;
            reason_network::mcp::serve(config, session_key, agent).await
        }
        Command::Setup { agent, dry_run } => setup(&agent, dry_run, pinned_dir, cli.json),
        Command::Doctor => {
            let host = agents::Host::from_env(pinned_dir.clone())?;
            let spec = service_spec(&config, &pinned_dir)?;
            let checks = reason_network::doctor::run(&config, &host, &spec).await;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&checks)?);
            } else {
                print!("{}", reason_network::doctor::render(&checks));
            }
            if checks
                .iter()
                .any(|check| check.level == reason_network::doctor::Level::Fail)
            {
                return Err(Error::Other("doctor found failures".into()));
            }
            Ok(())
        }
        Command::Service { action } => {
            service_command(&config, &pinned_dir, action, cli.json).await
        }
        Command::Update {
            check,
            target_version,
            source,
        } => update_command(&config, check, target_version, source, cli.json).await,
        Command::Hook { agent } => {
            // Hooks must never break the agent: any problem is reported on
            // stderr and the hook still exits successfully.
            let Ok(agent) = Agent::parse(&agent) else {
                eprintln!("[reason-network hook] unknown agent {agent}");
                return Ok(());
            };
            let mut input = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
            let output = reason_network::hook::run(&config, agent, &input).await;
            if !output.is_empty() {
                println!("{output}");
            }
            Ok(())
        }
    }
}

/// `reason-network setup --agent <agent> [--dry-run]`.
fn setup(selection: &str, dry_run: bool, pinned_dir: Option<PathBuf>, json: bool) -> Result<()> {
    let selected = Agent::parse_selection(selection)?;
    let host = agents::Host::from_env(pinned_dir)?;
    let report = agents::run_setup(&host, &selected, dry_run);
    if json {
        println!("{}", serde_json::to_string_pretty(&report.to_json())?);
    } else {
        print!("{}", report.render());
    }
    report.into_result()
}

fn service_spec(
    config: &Config,
    pinned_dir: &Option<PathBuf>,
) -> Result<reason_network::service::ServiceSpec> {
    let exe = agents::Host::from_env(None)?.exe;
    Ok(reason_network::service::ServiceSpec::new(
        config,
        exe,
        pinned_dir.is_some(),
    ))
}

/// `reason-network update [--check] [--version V] [--source S]`.
async fn update_command(
    config: &Config,
    check: bool,
    target_version: Option<String>,
    source: Option<String>,
    json: bool,
) -> Result<()> {
    use reason_network::update::{self, Source, Version};
    let source = match source {
        Some(source) => Source::parse(&source)?,
        None => Source::from_env()?,
    };
    let wanted = target_version.as_deref().map(Version::parse).transpose()?;
    let target = update::current_target().ok_or_else(|| {
        Error::Other(format!(
            "no release is built for {}/{}; build from source instead",
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
    })?;

    if check {
        let found = update::check(&source, wanted.as_ref(), target).await?;
        if json {
            println!("{}", serde_json::to_string_pretty(&found)?);
        } else {
            println!("current:   {}", found.current);
            println!("available: {} ({})", found.available, found.target);
            println!("source:    {}", found.source);
            if found.update_available {
                println!("an update is available: run `reason-network update`");
            } else if wanted.is_some() {
                println!(
                    "install it with `reason-network update --version {}`",
                    found.available
                );
            } else {
                println!("up to date");
            }
        }
        return Ok(());
    }

    let exe = agents::Host::from_env(None)?.exe;
    let result = update::update(&source, wanted.as_ref(), target, &exe).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    if !result.changed {
        println!("already up to date ({})", result.from);
        return Ok(());
    }
    println!(
        "updated {} -> {}: {}",
        result.from, result.to, result.executable
    );
    if let Some(previous) = &result.previous {
        println!("previous executable kept at {previous}");
    }
    println!("configuration and data were not modified");
    if reason_network::service::daemon_pid(&config.endpoint)
        .await
        .is_some()
    {
        println!(
            "the running daemon still uses the old version; restart it: `reason-network service stop` then `reason-network service start` (or restart `daemon-start`)"
        );
    }
    Ok(())
}

/// `reason-network service <action>`.
async fn service_command(
    config: &Config,
    pinned_dir: &Option<PathBuf>,
    action: ServiceAction,
    json: bool,
) -> Result<()> {
    use reason_network::service;
    let mut spec = service_spec(config, pinned_dir)?;
    let message = match action {
        ServiceAction::Install => service::install(&spec)?,
        ServiceAction::Start => service::start(&spec, &config.endpoint).await?,
        ServiceAction::Stop => service::stop(&spec, &config.endpoint).await?,
        ServiceAction::Uninstall => service::uninstall(&spec, &config.endpoint).await?,
        ServiceAction::Launch { env } => {
            for pair in env {
                let (key, value) = pair.split_once('=').ok_or_else(|| {
                    Error::InvalidRequest(format!("--env expects KEY=VALUE, got {pair}"))
                })?;
                spec.env.push((key.to_string(), value.to_string()));
            }
            service::launch(&spec, &config.endpoint).await?
        }
        ServiceAction::Status => {
            let status = service::status(&spec, &config.endpoint).await;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("service:    {} ({})", status.id, status.mechanism);
                println!(
                    "installed:  {}",
                    if status.installed { "yes" } else { "no" }
                );
                match status.pid {
                    Some(pid) => println!("daemon:     running (pid {pid})"),
                    None => println!("daemon:     not running"),
                }
                println!("definition: {}", status.definition);
                println!("log:        {}", status.log);
                println!("data dir:   {}", config.data_dir.display());
            }
            return Ok(());
        }
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "result": message }))?
        );
    } else {
        println!("{message}");
    }
    Ok(())
}

/// Start the daemon and block until it is stopped.
async fn daemon_start(config: Config) -> Result<()> {
    config.ensure_data_dir()?;
    let daemon = Daemon::new(config)?;
    let stopper = daemon.clone();
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            println!("stopping daemon...");
            stopper.stop();
        }
    });
    // systemd and launchd stop services with SIGTERM; shut down cleanly.
    #[cfg(unix)]
    {
        let stopper = daemon.clone();
        tokio::spawn(async move {
            if let Ok(mut term) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                term.recv().await;
                println!("stopping daemon...");
                stopper.stop();
            }
        });
    }
    daemon.serve().await
}

/// Send one request to the daemon and return the result payload.
async fn call(config: &Config, method: Method, params: Value) -> Result<Value> {
    let request = Request {
        id: Some(format!("cli-{method:?}")),
        method,
        params,
    };
    let response: reason_network::protocol::Response =
        ipc::request_once(&config.endpoint, &request).await?;
    match (response.ok, response.result, response.error) {
        (true, Some(result), _) => Ok(result),
        (true, None, _) => Ok(Value::Null),
        (false, _, Some(error)) => Err(Error::Other(format!("{}: {}", error.code, error.message))),
        (false, _, None) => Err(Error::Other("daemon returned no result".into())),
    }
}

/// Resolve the session key from the flag or the environment.
fn resolve_session(session: Option<String>) -> Result<String> {
    session
        .or_else(|| std::env::var(ENV_SESSION_KEY).ok())
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            Error::InvalidRequest(
                "no session key: pass --session or set REASON_NETWORK_SESSION_KEY".into(),
            )
        })
}

/// Read a string field from a JSON object.
fn text(value: &Value, field: &str) -> String {
    match value.get(field) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => "-".to_string(),
        Some(other) => other.to_string(),
    }
}

/// Read a field from a JSON object and print it.
fn value(value: &Value, field: &str) -> String {
    text(value, field)
}

/// Shorten long identifiers for table output.
fn short(text: &str) -> String {
    if text.chars().count() <= 20 {
        text.to_string()
    } else {
        let head: String = text.chars().take(18).collect();
        format!("{head}..")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn update_accepts_a_version_flag() {
        let cli =
            Cli::try_parse_from(["reason-network", "update", "--check", "--version", "0.2.0"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Command::Update { check: true, target_version: Some(ref v), .. } if v == "0.2.0"
        ));
    }
}
