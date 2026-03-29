use rmcp::{
    handler::server::router::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ServerHandler,
};
use serde::Deserialize;
use std::sync::{Arc, Mutex};

use crate::gateway::GatewayClient;

// ── Parameter structs ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SetIdentityParams {
    #[schemars(
        description = "Git remote URL (e.g. github.com/org/repo.git) or directory name identifying this project"
    )]
    project_ident: String,
    #[schemars(
        description = "Channel plugin to use: 'discord', 'slack', 'email', etc. Omit to use the gateway's default."
    )]
    channel: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SendMessageParams {
    #[schemars(description = "The message content to send to the user")]
    content: String,
}

// ── Session state ─────────────────────────────────────────────────────────────

#[derive(Default)]
struct Session {
    ident: Option<String>,
    channel_name: Option<String>,
}

// ── Server handler ────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct MailServer {
    tool_router: ToolRouter<Self>,
    session: Arc<Mutex<Session>>,
    gateway: GatewayClient,
}

#[tool_router]
impl MailServer {
    pub fn new(gateway: GatewayClient) -> Self {
        Self {
            tool_router: Self::tool_router(),
            session: Arc::new(Mutex::new(Session::default())),
            gateway,
        }
    }

    /// Set the project identity for this session.
    #[tool(
        description = "Set the project identity for this agent session. Pass a git remote URL (e.g. github.com/org/repo.git) or a directory name. Optionally specify a channel plugin (discord, slack, email). Must be called before send_message or get_messages."
    )]
    async fn set_identity(
        &self,
        Parameters(SetIdentityParams {
            project_ident,
            channel,
        }): Parameters<SetIdentityParams>,
    ) -> String {
        match self
            .gateway
            .register_project(&project_ident, channel.as_deref())
            .await
        {
            Ok(resp) => {
                let mut s = self.session.lock().unwrap();
                s.ident = Some(resp.ident.clone());
                s.channel_name = Some(resp.channel_name.clone());
                format!(
                    "Identity set to '{}' via {} channel.",
                    resp.ident, resp.channel_name
                )
            }
            Err(e) => format!("Error registering project: {}", e),
        }
    }

    /// Send a message to the user via the project's configured channel.
    #[tool(
        description = "Send a message to the user via the project's configured channel. set_identity must be called first."
    )]
    async fn send_message(
        &self,
        Parameters(SendMessageParams { content }): Parameters<SendMessageParams>,
    ) -> String {
        let ident = {
            let s = self.session.lock().unwrap();
            s.ident.clone()
        };

        let Some(ident) = ident else {
            return "Error: identity not set. Call set_identity first.".to_string();
        };

        match self.gateway.send_message(&ident, &content).await {
            Ok(resp) => format!("Message sent (id={}).", resp.message_id),
            Err(e) => format!("Error sending message: {}", e),
        }
    }

    /// Get unread messages from the project's channel since the last call.
    #[tool(
        description = "Get unread messages from the project's channel since the last call. Returns '[AGENT]' and '[USER]' prefixed lines, or 'no messages'. set_identity must be called first."
    )]
    async fn get_messages(&self) -> String {
        let ident = {
            let s = self.session.lock().unwrap();
            s.ident.clone()
        };

        let Some(ident) = ident else {
            return "Error: identity not set. Call set_identity first.".to_string();
        };

        match self.gateway.get_unread(&ident).await {
            Ok(resp) => {
                if resp.messages.is_empty() {
                    return "no messages".to_string();
                }
                resp.messages
                    .iter()
                    .map(|m| {
                        let prefix = if m.source == "agent" {
                            "[AGENT]"
                        } else {
                            "[USER]"
                        };
                        format!("{} {}", prefix, m.content)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Err(e) => format!("Error fetching messages: {}", e),
        }
    }
}

impl MailServer {
    /// Pre-set the project identity (used by main when DEFAULT_PROJECT_IDENT is configured).
    pub fn set_default_ident(&self, ident: String, channel_name: String) {
        let mut s = self.session.lock().unwrap();
        s.ident = Some(ident);
        s.channel_name = Some(channel_name);
    }
}

#[tool_handler]
impl ServerHandler for MailServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "claude-mail: communicate with the user via a configured channel (Discord, Slack, \
                 email, etc.). Call set_identity first (once per session), then use send_message \
                 to notify the user and get_messages to poll for replies."
                .to_string(),
        )
    }
}
