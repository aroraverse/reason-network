//! The MCP interface.
//!
//! This layer is intentionally thin: every tool turns into one protocol
//! request to the local daemon. No database access and no messaging logic
//! live here.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::schemars::JsonSchema;
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::{Config, ENV_SESSION_NAME};
use crate::error::Error;
use crate::ipc;
use crate::protocol::{Method, Request, Response};

/// Arguments of the `send_message` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SendMessageArgs {
    /// Session key of the recipient.
    pub to: String,
    /// The message text.
    pub message: String,
}

/// Arguments of the `check_inbox` tool.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct CheckInboxArgs {
    /// Maximum number of messages to return.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Mark the returned messages as read. Defaults to `true`.
    #[serde(default)]
    pub mark_read: Option<bool>,
}

/// Arguments of the `ask_user` tool.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AskUserArgs {
    /// The question for the human operator.
    pub question: String,
}

/// The MCP server. Every tool is a thin wrapper over one daemon request.
#[derive(Clone)]
pub struct ReasonMcpServer {
    endpoint: crate::config::Endpoint,
    session_key: String,
}

impl ReasonMcpServer {
    /// Create a server bound to an endpoint and a session key.
    pub fn new(endpoint: crate::config::Endpoint, session_key: String) -> Self {
        Self {
            endpoint,
            session_key,
        }
    }

    /// The session key this server acts as.
    pub fn session_key(&self) -> &str {
        &self.session_key
    }

    /// Send one request to the daemon and return the raw JSON result.
    pub async fn call(&self, method: Method, params: Value) -> Result<Value, ErrorData> {
        let request = Request {
            id: Some(format!("mcp-{method:?}")),
            method,
            params,
        };
        let response: Response = ipc::request_once(&self.endpoint, &request)
            .await
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        match (response.ok, response.result, response.error) {
            (true, Some(result), _) => Ok(result),
            (true, None, _) => Ok(Value::Null),
            (false, _, Some(error)) => Err(ErrorData::internal_error(
                format!("{}: {}", error.code, error.message),
                None,
            )),
            (false, _, None) => Err(ErrorData::internal_error("daemon returned no result", None)),
        }
    }

    /// List the sessions registered on this computer and on team peers.
    pub async fn list_team(&self) -> Result<String, ErrorData> {
        let result = self.call(Method::Directory, json!({})).await?;
        Ok(render(result))
    }

    /// Send a message to one specific session.
    pub async fn send_message(
        &self,
        Parameters(args): Parameters<SendMessageArgs>,
    ) -> Result<String, ErrorData> {
        let result = self
            .call(
                Method::SendMessage,
                json!({
                    "sender_key": self.session_key,
                    "recipient_key": args.to,
                    "body": args.message,
                }),
            )
            .await?;
        Ok(render(result))
    }

    /// Retrieve pending messages for the current session.
    pub async fn check_inbox(
        &self,
        Parameters(args): Parameters<CheckInboxArgs>,
    ) -> Result<String, ErrorData> {
        let mark_read = args.mark_read.unwrap_or(true);
        let result = self
            .call(
                Method::Inbox,
                json!({
                    "session_key": self.session_key,
                    "limit": args.limit,
                    "unread_only": true,
                }),
            )
            .await?;

        let mut rendered = result.clone();
        if mark_read {
            let messages = result
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let ids: Vec<String> = messages
                .iter()
                .filter_map(|message| {
                    message
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect();
            if !ids.is_empty() {
                let marked = self
                    .call(
                        Method::MarkRead,
                        json!({ "session_key": self.session_key, "message_ids": ids }),
                    )
                    .await?;
                if let Some(object) = rendered.as_object_mut() {
                    object.insert("marked_read".to_string(), marked);
                }
            }
        }
        Ok(render(rendered))
    }

    /// Ask a human operator a question.
    ///
    /// The answer is delivered to this session's inbox as a normal message
    /// from `human`.
    pub async fn ask_user(
        &self,
        Parameters(args): Parameters<AskUserArgs>,
    ) -> Result<String, ErrorData> {
        let result = self
            .call(
                Method::AskUser,
                json!({ "session_key": self.session_key, "question": args.question }),
            )
            .await?;
        Ok(render(result))
    }
}

/// Render a JSON value as an indented text block.
fn render(value: Value) -> String {
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
}

#[tool_router]
impl ReasonMcpServer {
    /// List the agent sessions that are registered on this machine and on the
    /// team peers this machine is connected to.
    #[tool(
        name = "list_team",
        description = "List the agent sessions in the team directory: local sessions and sessions synchronised from team peers, with session key, name, status, activity and owning machine."
    )]
    async fn list_team_tool(&self) -> Result<String, ErrorData> {
        self.list_team().await
    }

    /// Send a text message to one specific session.
    #[tool(
        name = "send_message",
        description = "Send a text message to one specific agent session. The recipient must already be registered; use list_team to discover session keys."
    )]
    async fn send_message_tool(
        &self,
        Parameters(args): Parameters<SendMessageArgs>,
    ) -> Result<String, ErrorData> {
        self.send_message(Parameters(args)).await
    }

    /// Retrieve pending messages for the current session.
    #[tool(
        name = "check_inbox",
        description = "Retrieve unread messages addressed to the current session. Messages are marked as read by default."
    )]
    async fn check_inbox_tool(
        &self,
        Parameters(args): Parameters<CheckInboxArgs>,
    ) -> Result<String, ErrorData> {
        self.check_inbox(Parameters(args)).await
    }

    /// Ask a human operator for a decision.
    #[tool(
        name = "ask_user",
        description = "Ask the human operator a question. The answer is delivered to this session's inbox as a message from 'human'."
    )]
    async fn ask_user_tool(
        &self,
        Parameters(args): Parameters<AskUserArgs>,
    ) -> Result<String, ErrorData> {
        self.ask_user(Parameters(args)).await
    }
}

#[tool_handler(
    router = ReasonMcpServer::tool_router(),
    name = "reason-network",
    instructions = "Local agent messaging: discover sessions with list_team, exchange messages with send_message and check_inbox, and ask the human operator with ask_user."
)]
impl ServerHandler for ReasonMcpServer {}

/// Run the MCP server over stdio.
///
/// `agent` names the coding agent that launched the server. It is recorded as
/// the session's agent type when the server has to register the session
/// itself.
pub async fn serve(
    config: Config,
    session_key: String,
    agent: Option<crate::agents::Agent>,
) -> crate::error::Result<()> {
    let server = ReasonMcpServer::new(config.endpoint.clone(), session_key.clone());

    // Make sure the session exists so other agents can address it.
    if let Err(error) = ensure_registered(&server, agent).await {
        eprintln!("[mcp] warning: could not register session {session_key}: {error}");
    }

    let service = rmcp::serve_server(server, rmcp::transport::io::stdio())
        .await
        .map_err(|error| Error::Other(format!("cannot start MCP server: {error}")))?;
    service
        .waiting()
        .await
        .map_err(|error| Error::Other(format!("MCP server stopped: {error}")))?;
    Ok(())
}

/// Resolve the session key of the MCP server.
///
/// An explicit key (flag or `REASON_NETWORK_SESSION_KEY`) always wins, which
/// keeps M0 configurations working. Without one, an agent integration
/// derives the stable key of its workspace, the same key its lifecycle hooks
/// use.
pub fn resolve_session_key(
    explicit: Option<String>,
    agent: Option<crate::agents::Agent>,
) -> crate::error::Result<String> {
    if let Some(key) = explicit
        .or_else(|| std::env::var(crate::config::ENV_SESSION_KEY).ok())
        .filter(|key| !key.trim().is_empty())
    {
        return Ok(key);
    }
    match agent {
        Some(agent) => Ok(crate::agents::session_key(
            agent,
            &crate::agents::workspace(Some(agent), None),
        )),
        None => Err(Error::InvalidRequest(
            "no session key: pass --session-key, set REASON_NETWORK_SESSION_KEY, or pass --agent <claude|codex> to derive one from the working directory"
                .into(),
        )),
    }
}

/// Register the session if the daemon does not know it yet.
///
/// An existing session is left untouched: its lifecycle hooks may already
/// have recorded richer state (status, branch, conversation id).
async fn ensure_registered(
    server: &ReasonMcpServer,
    agent: Option<crate::agents::Agent>,
) -> crate::error::Result<()> {
    let existing = server
        .call(
            Method::GetSession,
            json!({ "session_key": server.session_key() }),
        )
        .await;
    if existing.is_ok() {
        return Ok(());
    }
    let workspace = agent.map(|agent| crate::agents::workspace(Some(agent), None));
    let name = std::env::var(ENV_SESSION_NAME)
        .ok()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| {
            agent
                .zip(workspace.as_ref())
                .map(|(agent, dir)| crate::agents::session_name(agent, dir))
        })
        .unwrap_or_else(|| format!("mcp-{}", server.session_key()));
    let agent_type = agent.map_or("mcp", |agent| agent.session_agent_type());
    server
        .call(
            Method::Register,
            json!({
                "session_key": server.session_key(),
                "name": name,
                "agent_type": agent_type,
                "working_dir": workspace.map(|dir| dir.display().to_string()),
                "status": "idle",
            }),
        )
        .await
        .map(|_| ())
        .map_err(|error| Error::Other(error.message.to_string()))
}
