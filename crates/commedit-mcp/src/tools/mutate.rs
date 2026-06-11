//! History mutations. Every tool here follows the engine's mutation pipeline:
//! resolve the target against a fresh history read, run the rewrite, and
//! report the [`SaveResultDto`] — `clean` (exported to git) or `conflicts`
//! (held back until the conflict tools settle it).

use commedit_engine::rewrite::Identity;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::{tool, tool_router, ErrorData};

use crate::dto::{
    EditIdentityReq, EditMessageReq, FileContentDto, ReplaceFilesReq, SaveResultDto,
    SplitCommitReq,
};
use crate::error::{internal, invalid};
use crate::server::CommeditServer;
use crate::session::{ensure_not_pending, find_commit, full_history, save_result};

/// Lower a request's file list to the engine's `(path, content)` pairs,
/// refusing an empty list up front (the engine's message names "the diff",
/// which means nothing to an MCP caller).
fn file_pairs(files: Vec<FileContentDto>) -> Result<Vec<(String, String)>, ErrorData> {
    if files.is_empty() {
        return Err(invalid("files must not be empty"));
    }
    Ok(files.into_iter().map(|f| (f.path, f.content)).collect())
}

#[tool_router(router = router_mutate, vis = "pub")]
impl CommeditServer {
    #[tool(
        description = "Replace a commit's message (subject + body). Descendants are rebased; the commit's sha changes."
    )]
    pub async fn edit_message(
        &self,
        Parameters(req): Parameters<EditMessageReq>,
    ) -> Result<Json<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            let (_, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &req.sha)?;
            let outcome = repo
                .rewrite_message(&commits[idx].id, &req.message)
                .map_err(internal)?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "Change a commit's author and/or committer (name, email, date). Omitted fields keep their current value. Unlike other edits this also pins the committer timestamp instead of re-stamping it to now."
    )]
    pub async fn edit_identity(
        &self,
        Parameters(req): Parameters<EditIdentityReq>,
    ) -> Result<Json<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            let (_, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &req.sha)?;
            let c = &commits[idx];
            let identity = Identity {
                author_name: req.author_name.unwrap_or_else(|| c.author_name.clone()),
                author_email: req.author_email.unwrap_or_else(|| c.author_email.clone()),
                author_time: req.author_time.unwrap_or_else(|| c.author_time.clone()),
                committer_name: req
                    .committer_name
                    .unwrap_or_else(|| c.committer_name.clone()),
                committer_email: req
                    .committer_email
                    .unwrap_or_else(|| c.committer_email.clone()),
                committer_time: req
                    .committer_time
                    .unwrap_or_else(|| c.committer_time.clone()),
            };
            let outcome = repo.rewrite_identity(&c.id, &identity).map_err(internal)?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "Replace file contents inside a commit (whole-file replacement, no patch format). A path the commit doesn't have is added; deleting a file from a commit is not supported. Descendants are rebased onto the edited tree and may report conflicts."
    )]
    pub async fn replace_files(
        &self,
        Parameters(req): Parameters<ReplaceFilesReq>,
    ) -> Result<Json<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            let (_, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &req.sha)?;
            let files = file_pairs(req.files)?;
            let outcome = repo.rewrite_files(&commits[idx].id, &files).map_err(internal)?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "Split a commit in two: the commit keeps the given file contents (the subset to retain, as in replace_files), and a new `fixup!` child commit receives the remainder, so both combined reproduce the original change. Descendants are untouched."
    )]
    pub async fn split_commit(
        &self,
        Parameters(req): Parameters<SplitCommitReq>,
    ) -> Result<Json<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            let (_, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &req.sha)?;
            let files = file_pairs(req.files)?;
            let outcome = repo.split_commit(&commits[idx].id, &files).map_err(internal)?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Json)
    }
}
