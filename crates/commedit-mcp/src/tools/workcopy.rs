//! Tools over the uncommitted changes (the engine's working-copy commit `@`)
//! and the session-wide review diff.

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::{tool, tool_router, ErrorData};

use crate::convert::{file_change_dto, wc_entry_dto};
use crate::dto::{
    DiscardWorkingCopyReq, OkResp, SaveResultDto, SessionDiffResp, SquashWorkingCopyReq,
    WorkingCopyStatusResp,
};
use crate::error::{internal, invalid};
use crate::server::CommeditServer;
use crate::session::{ensure_not_pending, find_commit, full_history, save_result};

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

    #[tool(
        description = "Fold the uncommitted changes into a commit as a fixup (the commit's message is kept). The working tree ends up clean; an overlap with the commit's content reports conflicts like any rewrite."
    )]
    pub async fn squash_working_copy(
        &self,
        Parameters(req): Parameters<SquashWorkingCopyReq>,
    ) -> Result<Json<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            repo.snapshot_working_copy().map_err(internal)?;
            if repo.working_copy_chain().is_empty() {
                return Err(invalid("the working copy is clean — nothing to fold"));
            }
            let (_, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &req.dest_sha)?;
            let outcome = repo
                .squash_working_copy_into(None, &commits[idx].id)
                .map_err(internal)?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "Discard ALL uncommitted changes, resetting the working tree to the branch tip. Requires confirm=true: this is the one action whose data this server cannot bring back (undo restores recorded states, none of which contain the discarded edits)."
    )]
    pub async fn discard_working_copy(
        &self,
        Parameters(req): Parameters<DiscardWorkingCopyReq>,
    ) -> Result<Json<OkResp>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            if !req.confirm {
                return Err(invalid(
                    "set confirm=true to discard the uncommitted changes; they cannot \
                     be recovered afterwards",
                ));
            }
            repo.drop_working_copy(None).map_err(internal)?;
            Ok(OkResp { ok: true })
        })
        .await
        .map(Json)
    }
}
