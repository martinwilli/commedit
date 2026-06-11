//! Tools over the uncommitted changes (the engine's working-copy commit `@`)
//! and the session-wide review diff.

use std::collections::HashSet;

use commedit_engine::workcopy::PartialSelection;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, ErrorData};

use crate::convert::{file_change_dto, wc_entry_dto};
use crate::dto::{
    CommitWorkingCopyReq, DiscardWorkingCopyReq, OkResp, SaveResultDto, SessionDiffResp,
    SquashWorkingCopyReq, WorkingCopyStatusResp,
};
use crate::error::{internal, invalid};
use crate::server::CommeditServer;
use crate::session::{
    ensure_not_pending, find_commit, full_history, new_commit_identity, save_result,
};
use crate::wrapper::Yaml;

#[tool_router(router = router_workcopy, vis = "pub")]
impl CommeditServer {
    #[tool(
        description = "Show the uncommitted changes (working copy). They are first-class: every rewrite carries them along automatically. The entry sha can be fed to show_commit for the full diff; it churns on every disk edit."
    )]
    pub async fn working_copy_status(&self) -> Result<Yaml<WorkingCopyStatusResp>, ErrorData> {
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
        .map(Yaml)
    }

    #[tool(
        description = "Diff everything this session changed so far — the current tree (uncommitted changes included) against the tree at session start. Message/identity-only edits don't show up (they change no tree)."
    )]
    pub async fn session_diff(&self) -> Result<Yaml<SessionDiffResp>, ErrorData> {
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
        .map(Yaml)
    }

    #[tool(
        description = "Fold the uncommitted changes into a commit as a fixup (the commit's message is kept). The working tree ends up clean; an overlap with the commit's content reports conflicts like any rewrite."
    )]
    pub async fn squash_working_copy(
        &self,
        Parameters(req): Parameters<SquashWorkingCopyReq>,
    ) -> Result<Yaml<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            repo.snapshot_working_copy().map_err(internal)?;
            if repo.working_copy_chain().is_empty() {
                return Err(invalid("the working copy is clean — nothing to fold"));
            }
            let (_, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &req.dest)?;
            let outcome = repo
                .squash_working_copy_into(None, &commits[idx].id)
                .map_err(internal)?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Commit the uncommitted changes as a new commit on top of HEAD (like `git commit -a`), leaving the working tree clean. Only edits and deletions to git-tracked files are committed; brand-new untracked files are ignored and stay in the working tree (use create_commit to add those). \
\
Pass `paths`, `hunks` and/or `patches` to commit only PART of the changes (the in-process `git add -p`), leaving the rest uncommitted — call show_commit on the working-copy entry first to read each file's numbered `hunks`. Omit all three to commit everything. Refuses when there is nothing tracked to commit, or when the selection commits nothing. To insert a commit from explicit contents elsewhere in history instead, use create_commit."
    )]
    pub async fn commit_working_copy(
        &self,
        Parameters(req): Parameters<CommitWorkingCopyReq>,
    ) -> Result<Yaml<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            repo.snapshot_working_copy().map_err(internal)?;
            if repo.working_copy_chain().is_empty() {
                return Err(invalid("the working copy is clean — nothing to commit"));
            }
            let identity = new_commit_identity(repo, req.identity);

            // A partial commit is requested when any selection tier is present.
            let partial = req.paths.is_some() || req.hunks.is_some() || req.patches.is_some();
            let outcome = if partial {
                let paths = req.paths.unwrap_or_default();
                let hunks: Vec<(String, Vec<usize>)> = req
                    .hunks
                    .unwrap_or_default()
                    .into_iter()
                    .map(|h| (h.path, h.hunks))
                    .collect();
                let patches: Vec<(String, String)> = req
                    .patches
                    .unwrap_or_default()
                    .into_iter()
                    .map(|p| (p.path, p.patch))
                    .collect();

                for (path, indices) in &hunks {
                    if indices.is_empty() {
                        return Err(invalid(format!(
                            "'{path}' is listed in `hunks` but selects no hunk indices"
                        )));
                    }
                }
                if paths.is_empty() && hunks.is_empty() && patches.is_empty() {
                    return Err(invalid(
                        "a partial commit needs at least one path, hunk or patch; omit \
                         paths/hunks/patches entirely to commit the whole working copy",
                    ));
                }
                // A path may appear in at most one tier.
                let mut seen = HashSet::new();
                for path in paths
                    .iter()
                    .chain(hunks.iter().map(|(p, _)| p))
                    .chain(patches.iter().map(|(p, _)| p))
                {
                    if !seen.insert(path.as_str()) {
                        return Err(invalid(format!(
                            "path '{path}' is selected in more than one tier; list it once"
                        )));
                    }
                }

                let sel = PartialSelection {
                    paths: &paths,
                    hunks: &hunks,
                    patches: &patches,
                };
                repo.commit_working_copy_partial(sel, &req.message, identity.as_ref())
                    .map_err(internal)?
            } else {
                repo.commit_working_copy(&req.message, identity.as_ref())
                    .map_err(internal)?
            };
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Discard ALL uncommitted changes, resetting the working tree to the branch tip. Requires confirm=true: this is the one action whose data this server cannot bring back (undo restores recorded states, none of which contain the discarded edits)."
    )]
    pub async fn discard_working_copy(
        &self,
        Parameters(req): Parameters<DiscardWorkingCopyReq>,
    ) -> Result<Yaml<OkResp>, ErrorData> {
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
        .map(Yaml)
    }
}
