//! The session op-log: listing recorded operations and (in later commits)
//! time-travelling between them.

use rmcp::handler::server::wrapper::Json;
use rmcp::{tool, tool_router, ErrorData};

use crate::convert::op_entry_dto;
use crate::dto::ListOperationsResp;
use crate::server::CommeditServer;

#[tool_router(router = router_ops, vis = "pub")]
impl CommeditServer {
    #[tool(
        description = "List this session's recorded operations (every landed mutation), oldest first, with the undo cursor. Index 0 is the session start; an entry's index is the state right after it — both are jump_to_operation targets."
    )]
    pub async fn list_operations(&self) -> Result<Json<ListOperationsResp>, ErrorData> {
        self.with_session(|repo, _| {
            Ok(ListOperationsResp {
                ops: repo
                    .session_ops()
                    .iter()
                    .enumerate()
                    .map(|(i, e)| op_entry_dto(i + 1, e))
                    .collect(),
                cursor: repo.op_cursor(),
                can_undo: repo.can_undo(),
                can_redo: repo.can_redo(),
                pending: repo.is_pending(),
            })
        })
        .await
        .map(Json)
    }
}
