//! The `agentux` MCP server: the bus tools for one session, built with the
//! official Rust MCP SDK (`rmcp`).

use std::sync::Arc;

use agentux_config::BusTool;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};

use crate::bridge::BusEndpoint;
use crate::prompt::session_prompt;
use crate::tools::descriptions::*;
use crate::tools::{
    AskHuman, BusCall, BusReply, Handoff, HumanReply, Inbox, PostMessage, Posted, ReadMessages,
    RequestReview, RunStateReply,
};

/// Name the server reports, and the name harnesses show its tools under.
pub const SERVER_NAME: &str = "agentux";

/// An MCP server scoped to one session: every call is made as the endpoint's
/// identity, and only the tools `agentux.yaml` allows are listed.
#[derive(Clone)]
pub struct BusServer {
    endpoint: Arc<dyn BusEndpoint>,
    tool_router: ToolRouter<Self>,
}

type ToolResult<T> = Result<Json<T>, String>;

impl BusServer {
    pub fn new(endpoint: Arc<dyn BusEndpoint>) -> Self {
        let mut tool_router = Self::tool_router();
        for tool in BusTool::ALL {
            if !endpoint.allowed_tools().contains(&tool) {
                tool_router.remove_route(tool.as_str());
            }
        }
        Self {
            endpoint,
            tool_router,
        }
    }

    /// Serves the server over this process's stdin and stdout until the
    /// client disconnects.
    pub async fn serve_stdio(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let service = self.serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;
        Ok(())
    }

    async fn call(&self, call: BusCall) -> Result<BusReply, String> {
        self.endpoint.call(call).await.map_err(|e| e.to_string())
    }
}

fn unexpected(reply: &BusReply) -> String {
    format!("internal error: unexpected bus reply {reply:?}")
}

#[tool_router]
impl BusServer {
    #[tool(name = "post_message", description = POST_MESSAGE)]
    async fn post_message(&self, Parameters(args): Parameters<PostMessage>) -> ToolResult<Posted> {
        match self.call(BusCall::PostMessage(args)).await? {
            BusReply::Posted(posted) => Ok(Json(posted)),
            other => Err(unexpected(&other)),
        }
    }

    #[tool(name = "read_messages", description = READ_MESSAGES)]
    async fn read_messages(&self, Parameters(args): Parameters<ReadMessages>) -> ToolResult<Inbox> {
        match self.call(BusCall::ReadMessages(args)).await? {
            BusReply::Inbox(inbox) => Ok(Json(inbox)),
            other => Err(unexpected(&other)),
        }
    }

    #[tool(name = "request_review", description = REQUEST_REVIEW)]
    async fn request_review(
        &self,
        Parameters(args): Parameters<RequestReview>,
    ) -> ToolResult<Posted> {
        match self.call(BusCall::RequestReview(args)).await? {
            BusReply::Posted(posted) => Ok(Json(posted)),
            other => Err(unexpected(&other)),
        }
    }

    #[tool(name = "handoff", description = HANDOFF)]
    async fn handoff(&self, Parameters(args): Parameters<Handoff>) -> ToolResult<Posted> {
        match self.call(BusCall::Handoff(args)).await? {
            BusReply::Posted(posted) => Ok(Json(posted)),
            other => Err(unexpected(&other)),
        }
    }

    #[tool(name = "get_run_state", description = GET_RUN_STATE)]
    async fn get_run_state(&self) -> ToolResult<RunStateReply> {
        match self.call(BusCall::GetRunState).await? {
            BusReply::RunState(state) => Ok(Json(*state)),
            other => Err(unexpected(&other)),
        }
    }

    #[tool(name = "ask_human", description = ASK_HUMAN)]
    async fn ask_human(&self, Parameters(args): Parameters<AskHuman>) -> ToolResult<HumanReply> {
        match self.call(BusCall::AskHuman(args)).await? {
            BusReply::Human(reply) => Ok(Json(reply)),
            other => Err(unexpected(&other)),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BusServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION")))
            .with_instructions(session_prompt(
                self.endpoint.identity(),
                self.endpoint.allowed_tools(),
                self.endpoint.max_turns_per_exchange(),
            ))
    }
}
