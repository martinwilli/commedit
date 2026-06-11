//! Tools over the uncommitted changes (the engine's working-copy commit `@`)
//! and the session-wide review diff.

use rmcp::handler::server::wrapper::Json;
use rmcp::{tool, tool_router, ErrorData};

use crate::convert::{file_change_dto, wc_entry_dto};
use crate::dto::{SessionDiffResp, WorkingCopyStatusResp};
use crate::error::internal;
use crate::server::CommeditServer;

#[tool_router(router = router_workcopy, vis = "pub")]
impl CommeditServer {
    #[tool(
        description = "Show the uncommitted changes (working copy). They are first-class: every rewrite carries them along automatically. The entry sha can be fed to show_commit for the full diff; it churns on every disk edit."
    )]
    pub async fn working_copy_status(&self) -> Result<Json<WorkingCopyStatusResp>, ErrorData> {
        self.with_session(|repo, _| {
            // A fresh read wants the latest on-disk state folded in.
            repo.snapshot_working_copy().map_err(internal)?;
            let entries = repo.working_copy_chain();
            Ok(WorkingCopyStatusResp {
                clean: entries.is_empty(),
                entries: entries.iter().map(wc_entry_dto).collect(),
                session_start_head_sha: repo.session_start_head_hex(),
            })
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "Diff everything this session changed so far — the current tree (uncommitted changes included) against the tree at session start. Message/identity-only edits don't show up (they change no tree)."
    )]
    pub async fn session_diff(&self) -> Result<Json<SessionDiffResp>, ErrorData> {
        self.with_session(|repo, _| {
            let files = repo
                .session_changes()
                .map_err(internal)?
                .iter()
                .map(|fc| file_change_dto(fc, false))
                .collect();
            Ok(SessionDiffResp { files })
        })
        .await
        .map(Json)
    }
}
