//! The MCP server type: shared session state, router assembly and the
//! `ServerHandler` implementation with the agent-facing instructions.

use std::sync::{Arc, Mutex};

use commedit_engine::repo::Repo;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool_handler, ServerHandler};

use crate::session::TrashState;

/// One editing session over one repository. Cheap to clone — all state is
/// behind `Arc`s; the `std::sync::Mutex` serializes every tool body (each runs
/// in `spawn_blocking` and takes the lock inside, never across an `.await`).
#[derive(Clone)]
pub struct CommeditServer {
    pub(crate) repo: Arc<Mutex<Repo>>,
    pub(crate) trash: Arc<Mutex<TrashState>>,
    tool_router: ToolRouter<Self>,
}

impl CommeditServer {
    pub fn new(repo: Repo) -> Self {
        Self {
            repo: Arc::new(Mutex::new(repo)),
            trash: Arc::new(Mutex::new(TrashState::default())),
            tool_router: Self::router_read()
                + Self::router_workcopy()
                + Self::router_conflict()
                + Self::router_ops(),
        }
    }
}

/// The agent's manual, served as the MCP `instructions` field.
const INSTRUCTIONS: &str = "\
commedit edits the history of the checked-out git branch in place: any commit \
reachable from HEAD can be edited, and its descendants are rebased \
automatically. The repository stays a plain git repo throughout.";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CommeditServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("commedit", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}
