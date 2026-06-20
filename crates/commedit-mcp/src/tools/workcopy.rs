//! Tools over the uncommitted changes (the engine's working-copy commit `@`)
//! and the session-wide review diff.

use std::collections::HashSet;

use commedit_engine::history::IdAbbrev;
use commedit_engine::workcopy::PartialSelection;
use jj_lib::object_id::ObjectId as _;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, ErrorData};

use crate::convert::{commit_dto, file_change_dto, DetailFields};
use crate::dto::{
    CommitWorkingCopyReq, CommitWorkingCopyResp, DiscardWorkingCopyReq, HunkSelectionDto, OkResp,
    PatchSelectionDto, SaveResultDto, SessionDiffResp, SessionSel, SquashWorkingCopyReq,
    SquashWorkingCopyResp, WorkingCopyStatusResp,
};
use crate::error::{internal, invalid};
use crate::server::CommeditServer;
use crate::session::{
    change_id_set, ensure_not_pending, ensure_worktree_bound, find_commit, full_history,
    new_commit_identity, save_result, save_result_topo, working_copy_status_resp,
};
use crate::wrapper::Yaml;

#[tool_router(router = router_workcopy, vis = "pub")]
impl CommeditServer {
    #[tool(
        description = "Show the uncommitted changes (working copy). They are first-class: every rewrite carries them along automatically. The entry sha can be fed to show_commit for the full diff; it churns on every disk edit."
    )]
    pub async fn working_copy_status(
        &self,
        Parameters(req): Parameters<SessionSel>,
    ) -> Result<Yaml<WorkingCopyStatusResp>, ErrorData> {
        self.with_session(req.session, |repo, _| working_copy_status_resp(repo))
            .await
            .map(Yaml)
    }

    #[tool(
        description = "Diff everything this session changed so far — the current tree (uncommitted changes included) against the tree at session start. Message/identity-only edits don't show up (they change no tree)."
    )]
    pub async fn session_diff(
        &self,
        Parameters(req): Parameters<SessionSel>,
    ) -> Result<Yaml<SessionDiffResp>, ErrorData> {
        self.with_session(req.session, |repo, _| {
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
        description = "Fold the uncommitted changes into a commit as a fixup (the commit's message is kept by default). Pass `message` to reword the destination in the same call. Only edits/deletions to already-tracked files are folded — a brand-new (untracked) file is SILENTLY skipped unless you name it in `add_paths`; in a partial fold a new file must be listed under BOTH `add_paths` (to track it) and `paths` (to select it). \
\
Pass `paths`, `hunks` and/or `patches` to fold only PART of the changes (the in-process `git add -p` for a fixup), leaving the rest uncommitted — call show_commit on the working-copy entry first to read each file's numbered `hunks`. Omit all three to fold everything. The working tree stays byte-identical; an overlap with the commit's content reports conflicts like any rewrite. A clean fold returns the `topology` slice (the destination after the fold) and the remaining `working_copy` — clean for a whole fold, the unselected remainder for a partial one — so it is verifiable without a follow-up read."
    )]
    pub async fn squash_working_copy(
        &self,
        Parameters(req): Parameters<SquashWorkingCopyReq>,
    ) -> Result<Yaml<SquashWorkingCopyResp>, ErrorData> {
        self.with_session(req.session.session.clone(), move |repo, _| {
            ensure_not_pending(repo)?;
            ensure_worktree_bound(repo)?;
            let SquashWorkingCopyReq {
                session: _,
                dest,
                message,
                paths,
                hunks,
                patches,
                add_paths,
            } = req;
            // Track any named new files before checking for changes, so folding a
            // brand-new file alone (no tracked edits) isn't seen as a clean tree.
            repo.snapshot_working_copy_tracking(&add_paths.unwrap_or_default())
                .map_err(internal)?;
            if repo.working_copy_chain().is_empty() {
                return Err(invalid("the working copy is clean — nothing to fold"));
            }
            let (_, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &dest)?;
            let pre = change_id_set(&commits);
            let anchors = vec![commits[idx].change_id_hex()];
            let dest_id = commits[idx].id.clone();

            // A partial fold is requested when any selection tier is present.
            let partial = paths.is_some() || hunks.is_some() || patches.is_some();
            let outcome = if partial {
                let (paths, hunks, patches) = parse_partial_selection(paths, hunks, patches)?;
                let sel = PartialSelection {
                    paths: &paths,
                    hunks: &hunks,
                    patches: &patches,
                };
                repo.squash_working_copy_partial_into(sel, &dest_id, message.as_deref())
                    .map_err(internal)?
            } else {
                repo.squash_working_copy_into(None, &dest_id, message.as_deref())
                    .map_err(internal)?
            };
            let result = save_result_topo(repo, &outcome, &pre, &anchors)?;
            // On a clean fold, report what's left uncommitted (clean for a whole
            // fold, the unselected remainder for a partial one); on conflicts the
            // working copy is held with the rewrite, so there's nothing to report.
            let working_copy = match &result {
                SaveResultDto::Clean { .. } => Some(working_copy_status_resp(repo)?),
                SaveResultDto::Conflicts { .. } => None,
            };
            Ok(SquashWorkingCopyResp {
                result,
                working_copy,
            })
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Commit the uncommitted changes as a new commit on top of HEAD (like `git commit -a`), leaving the working tree clean. Only edits and deletions to already-tracked files are committed — a brand-new (untracked) file is SILENTLY skipped unless you name it in `add_paths`; in a partial commit a new file must be listed under BOTH `add_paths` (to track it) and `paths` (to select it). Or use create_commit to author files from explicit contents. \
\
Pass `paths`, `hunks` and/or `patches` to commit only PART of the changes (the in-process `git add -p`), leaving the rest uncommitted — call show_commit on the working-copy entry first to read each file's numbered `hunks`. Omit all three to commit everything. Refuses when there is nothing tracked to commit, or when the selection commits nothing. To insert a commit from explicit contents elsewhere in history instead, use create_commit. Returns the new `committed` commit (its sha and stable change_id, ready to chain) and the remaining `working_copy` — clean for a whole commit, the unselected remainder for a partial one — so it is verifiable without a follow-up read."
    )]
    pub async fn commit_working_copy(
        &self,
        Parameters(req): Parameters<CommitWorkingCopyReq>,
    ) -> Result<Yaml<CommitWorkingCopyResp>, ErrorData> {
        self.with_session(req.session.session.clone(), move |repo, _| {
            ensure_not_pending(repo)?;
            ensure_worktree_bound(repo)?;
            // Track any named new files before checking for changes, so committing a
            // brand-new file alone (no tracked edits) isn't seen as a clean tree.
            repo.snapshot_working_copy_tracking(&req.add_paths.clone().unwrap_or_default())
                .map_err(internal)?;
            if repo.working_copy_chain().is_empty() {
                return Err(invalid("the working copy is clean — nothing to commit"));
            }
            let identity = new_commit_identity(repo, req.identity);

            // A partial commit is requested when any selection tier is present.
            let partial = req.paths.is_some() || req.hunks.is_some() || req.patches.is_some();
            let outcome = if partial {
                let (paths, hunks, patches) =
                    parse_partial_selection(req.paths, req.hunks, req.patches)?;
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
            let result = save_result(repo, &outcome);
            // On a clean commit, hand back the new commit (its sha + stable
            // change_id, ready to chain) and the remaining working copy — clean for
            // a whole commit, the unselected remainder for a partial one.
            let (committed, working_copy) = match &result {
                SaveResultDto::Clean { .. } => {
                    let (_, commits) = full_history(repo)?;
                    let committed = commits.first().map(|info| {
                        let refs = repo.commit_refs();
                        let root = repo.root_commit_id().hex();
                        let abbrev = IdAbbrev::new(&repo.repo);
                        commit_dto(info, &root, &refs, &abbrev, DetailFields::ALL)
                    });
                    (committed, Some(working_copy_status_resp(repo)?))
                }
                SaveResultDto::Conflicts { .. } => (None, None),
            };
            Ok(CommitWorkingCopyResp {
                result,
                committed,
                working_copy,
            })
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
        self.with_session(req.session.session.clone(), move |repo, _| {
            ensure_not_pending(repo)?;
            ensure_worktree_bound(repo)?;
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

/// The three normalized partial-selection tiers, owned and ready to borrow into a
/// [`PartialSelection`]: whole `paths`, `(path, hunk-indices)` and `(path, patch)`.
type PartialTiers = (
    Vec<String>,
    Vec<(String, Vec<usize>)>,
    Vec<(String, String)>,
);

/// Validate and normalize the three partial-selection tiers shared by
/// `commit_working_copy` and `squash_working_copy`. Returns the owned
/// `(paths, hunks, patches)` ready for a [`PartialSelection`], or an error for an
/// empty hunk list, a path appearing in more than one tier, or an all-empty
/// selection. The caller decides a partial op is wanted (any tier `Some`) before
/// calling; an all-empty selection here means every tier was an empty list.
fn parse_partial_selection(
    paths: Option<Vec<String>>,
    hunks: Option<Vec<HunkSelectionDto>>,
    patches: Option<Vec<PatchSelectionDto>>,
) -> Result<PartialTiers, ErrorData> {
    let paths = paths.unwrap_or_default();
    let hunks: Vec<(String, Vec<usize>)> = hunks
        .unwrap_or_default()
        .into_iter()
        .map(|h| (h.path, h.hunks))
        .collect();
    let patches: Vec<(String, String)> = patches
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
            "a partial selection needs at least one path, hunk or patch; omit \
             paths/hunks/patches entirely for the whole working copy",
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
    Ok((paths, hunks, patches))
}
