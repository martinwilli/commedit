//! History mutations. Every tool here follows the engine's mutation pipeline:
//! resolve the target against a fresh history read, run the rewrite, and
//! report the [`SaveResultDto`] — `clean` (exported to git) or `conflicts`
//! (held back until the conflict tools settle it).

use std::collections::BTreeMap;

use commedit_engine::history::IdAbbrev;
use commedit_engine::rewrite::{BatchEdit, Identity};
use commedit_engine::tree::FileEdit;
use jj_lib::object_id::ObjectId as _;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, ErrorData};

use crate::convert::{commit_dto, resolve_squash_mode, DetailFields};
use crate::dto::{
    CherryPickCommitReq, CreateCommitReq, DropCommitReq, DropCommitResp, EditCommitsReq,
    EditIdentityReq, EditMessageReq, FileContentDto, ReorderCommitReq, ReplaceFilesReq,
    RestoreCommitReq, RevertCommitReq, SaveResultDto, SplitCommitReq, SquashCommitReq,
};
use crate::error::{internal, invalid};
use crate::server::CommeditServer;
use crate::session::{
    ensure_not_pending, find_commit, find_trashed, full_history, lookup_ref, new_commit_identity,
    plan_splice, resolve_ref, save_result, PendingTrashOp, RefEntry, SpliceTarget, TrashState,
};
use crate::wrapper::Yaml;

/// Run a mutation whose trash effect is staged: on an engine error the staged
/// op must not linger (the mutation never happened), on success it lands only
/// once the outcome settles clean.
fn run_staged<F>(
    repo: &mut commedit_engine::repo::Repo,
    trash: &mut TrashState,
    staged: PendingTrashOp,
    mutate: F,
) -> Result<commedit_engine::conflict::SaveOutcome, ErrorData>
where
    F: FnOnce(&mut commedit_engine::repo::Repo) -> anyhow::Result<commedit_engine::conflict::SaveOutcome>,
{
    trash.staged = Some(staged);
    match mutate(repo) {
        Ok(outcome) => {
            trash.settle(&outcome);
            Ok(outcome)
        }
        Err(e) => {
            trash.staged = None;
            Err(internal(e))
        }
    }
}

/// Lower a request's file list to the engine's `(path, content)` pairs,
/// refusing an empty list up front (the engine's message names "the diff",
/// which means nothing to an MCP caller).
fn file_pairs(files: Vec<FileContentDto>) -> Result<Vec<(String, String)>, ErrorData> {
    if files.is_empty() {
        return Err(invalid("files must not be empty"));
    }
    Ok(files.into_iter().map(|f| (f.path, f.content)).collect())
}

/// Lower a request's writes and deletions to the engine's [`FileEdit`] list.
/// Unlike [`file_pairs`] an empty result is allowed (a `create_commit` with no
/// edits is an empty commit); callers that require content check it themselves.
fn file_edits(files: Vec<FileContentDto>, delete_paths: Option<Vec<String>>) -> Vec<FileEdit> {
    files
        .into_iter()
        .map(|f| FileEdit::write(f.path, f.content))
        .chain(delete_paths.into_iter().flatten().map(FileEdit::delete))
        .collect()
}

#[tool_router(router = router_mutate, vis = "pub")]
impl CommeditServer {
    #[tool(
        description = "Replace a commit's message (subject + body). Descendants are rebased; the commit's sha changes."
    )]
    pub async fn edit_message(
        &self,
        Parameters(req): Parameters<EditMessageReq>,
    ) -> Result<Yaml<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            let (_, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &req.commit)?;
            let outcome = repo
                .rewrite_message(&commits[idx].id, &req.message)
                .map_err(internal)?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Change a commit's author and/or committer (name, email, date). Omitted fields keep their current value. Unlike other edits this also pins the committer timestamp instead of re-stamping it to now."
    )]
    pub async fn edit_identity(
        &self,
        Parameters(req): Parameters<EditIdentityReq>,
    ) -> Result<Yaml<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            let (_, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &req.commit)?;
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
        .map(Yaml)
    }

    #[tool(
        description = "Edit several commits' messages and/or identities in ONE transaction with a single rebase — the bulk form of edit_message/edit_identity. Each entry sets a new message and/or identity for its commit (omitted identity fields keep their value; the committer timestamp is pinned, not re-stamped). Applied atomically and ancestors-first, so re-dating a whole parent→child range stays correct; if the rebase conflicts the whole batch is held back like any mutation. Prefer this over many single edits when re-dating or rewording a range. A commit may appear at most once."
    )]
    pub async fn edit_commits(
        &self,
        Parameters(req): Parameters<EditCommitsReq>,
    ) -> Result<Yaml<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            if req.edits.is_empty() {
                return Err(invalid("edits must not be empty"));
            }
            let (_, commits) = full_history(repo)?;
            let mut batch = Vec::with_capacity(req.edits.len());
            for e in req.edits {
                let idx = find_commit(&commits, &e.commit)?;
                let c = &commits[idx];
                let has_identity = e.author_name.is_some()
                    || e.author_email.is_some()
                    || e.author_time.is_some()
                    || e.committer_name.is_some()
                    || e.committer_email.is_some()
                    || e.committer_time.is_some();
                if e.message.is_none() && !has_identity {
                    return Err(invalid(format!(
                        "edit for {} changes nothing: set message or an identity field",
                        e.commit
                    )));
                }
                let identity = if has_identity {
                    Some(Identity {
                        author_name: e.author_name.unwrap_or_else(|| c.author_name.clone()),
                        author_email: e.author_email.unwrap_or_else(|| c.author_email.clone()),
                        author_time: e.author_time.unwrap_or_else(|| c.author_time.clone()),
                        committer_name: e
                            .committer_name
                            .unwrap_or_else(|| c.committer_name.clone()),
                        committer_email: e
                            .committer_email
                            .unwrap_or_else(|| c.committer_email.clone()),
                        committer_time: e
                            .committer_time
                            .unwrap_or_else(|| c.committer_time.clone()),
                    })
                } else {
                    None
                };
                batch.push(BatchEdit {
                    target: c.id.clone(),
                    message: e.message,
                    identity,
                });
            }
            let outcome = repo.rewrite_batch(batch).map_err(internal)?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Replace file contents inside a commit (whole-file replacement, no patch format). A path in `files` the commit doesn't have is added; `delete_paths` removes files. Descendants are rebased onto the edited tree and may report conflicts."
    )]
    pub async fn replace_files(
        &self,
        Parameters(req): Parameters<ReplaceFilesReq>,
    ) -> Result<Yaml<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            let (_, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &req.commit)?;
            let edits = file_edits(req.files, req.delete_paths);
            if edits.is_empty() {
                return Err(invalid("files and delete_paths must not both be empty"));
            }
            let outcome = repo.rewrite_files_edits(&commits[idx].id, &edits).map_err(internal)?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Split a commit in two: the commit keeps the given file contents (the subset to retain, as in replace_files), and a new `fixup!` child commit receives the remainder, so both combined reproduce the original change. Descendants are untouched."
    )]
    pub async fn split_commit(
        &self,
        Parameters(req): Parameters<SplitCommitReq>,
    ) -> Result<Yaml<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            let (_, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &req.commit)?;
            let files = file_pairs(req.files)?;
            let outcome = repo.split_commit(&commits[idx].id, &files).map_err(internal)?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Create a brand-new commit from given file contents and insert it into history. `new_parent` (sha/change id, or `root`; omitted = top of HEAD) sets where it goes; existing descendants rebase onto it (a mid-history insert may report conflicts). Omit `files`/`delete_paths` for an empty commit. Uncommitted changes ride on top untouched — use commit_working_copy to commit those instead."
    )]
    pub async fn create_commit(
        &self,
        Parameters(req): Parameters<CreateCommitReq>,
    ) -> Result<Yaml<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            let (head, commits) = full_history(repo)?;
            let new_parent = req.new_parent.unwrap_or_else(|| head.hex());
            let mv = plan_splice(
                repo,
                &commits,
                SpliceTarget::New,
                &new_parent,
                req.child.as_deref(),
            )?;
            let edits = file_edits(req.files, req.delete_paths);
            let identity = new_commit_identity(repo, req.identity);
            let outcome = repo
                .create_commit(
                    mv.new_parents,
                    mv.new_children,
                    &req.message,
                    identity.as_ref(),
                    &edits,
                )
                .map_err(internal)?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Create a commit that reverts another commit's change (its inverse diff, like `git revert`) and insert it into history. `new_parent` (sha/change id, or `root`; omitted = top of HEAD) sets where it goes. The revert may itself conflict where the insertion point diverged from the reverted commit. Merge commits cannot be reverted."
    )]
    pub async fn revert_commit(
        &self,
        Parameters(req): Parameters<RevertCommitReq>,
    ) -> Result<Yaml<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            let (head, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &req.commit)?;
            if commits[idx].parents.len() > 1 {
                return Err(invalid("cannot revert a merge commit"));
            }
            let target = commits[idx].id.clone();
            let new_parent = req.new_parent.unwrap_or_else(|| head.hex());
            let mv = plan_splice(
                repo,
                &commits,
                SpliceTarget::New,
                &new_parent,
                req.child.as_deref(),
            )?;
            let identity = new_commit_identity(repo, req.identity);
            let outcome = repo
                .revert_commit(&target, mv.new_parents, mv.new_children, identity.as_ref())
                .map_err(internal)?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Create a commit that re-applies another commit's change (its forward diff, like `git cherry-pick`) and insert it into history. The source may live OUTSIDE the current branch — pass a commit on another branch by its full sha (from `git log <branch>`); its branch is never touched. `new_parent` (sha/change id, or `root`; omitted = top of HEAD) sets where the copy goes. By default the source's author is preserved and the committer is stamped afresh (git's `cherry-pick -x`, recording a provenance trailer). The pick may conflict where the insertion point diverged from the source. Merge commits cannot be cherry-picked."
    )]
    pub async fn cherry_pick_commit(
        &self,
        Parameters(req): Parameters<CherryPickCommitReq>,
    ) -> Result<Yaml<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            let (head, commits) = full_history(repo)?;
            // Resolve in-history refs (sha/change id/prefix) as usual; fall back
            // to a direct ODB load for a full sha that names a commit off the
            // branch. Keep find_commit's error otherwise, so an ambiguous or
            // too-short in-history prefix still reports precisely.
            let target = match find_commit(&commits, &req.commit) {
                Ok(idx) => commits[idx].id.clone(),
                Err(e) => repo.lookup_commit_in_store(&req.commit).ok_or(e)?,
            };
            let new_parent = req.new_parent.unwrap_or_else(|| head.hex());
            let mv = plan_splice(
                repo,
                &commits,
                SpliceTarget::New,
                &new_parent,
                req.child.as_deref(),
            )?;
            let identity = new_commit_identity(repo, req.identity);
            let outcome = repo
                .cherry_pick_commit(&target, mv.new_parents, mv.new_children, identity.as_ref())
                .map_err(internal)?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Drop a commit from history: its children rebase onto its parent, and the commit moves to the session trash (restorable via restore_commit or squash_commit). Merge commits and the branch's only commit cannot be dropped."
    )]
    pub async fn drop_commit(
        &self,
        Parameters(req): Parameters<DropCommitReq>,
    ) -> Result<Yaml<DropCommitResp>, ErrorData> {
        self.with_session(move |repo, trash| {
            ensure_not_pending(repo)?;
            let (_, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &req.commit)?;
            let id = repo.plan_drop(&commits, idx).ok_or_else(|| {
                invalid(
                    "this commit cannot be dropped: merge commits and the branch's only \
                     commit stay fixed",
                )
            })?;
            let info = commits[idx].clone();
            let root = repo.root_commit_id().hex();
            let dropped =
                commit_dto(&info, &root, &BTreeMap::new(), &IdAbbrev::new(&repo.repo), DetailFields::ALL);
            let outcome = run_staged(repo, trash, PendingTrashOp::Push(info), |repo| {
                repo.abandon_commit(&id)
            })?;
            Ok(DropCommitResp { result: save_result(repo, &outcome), dropped })
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Move a commit to another place in the history: new_parent names the commit that becomes its parent (or `root` for the very first position). A true rebase — commits that don't commute report conflicts. Merge commits cannot be moved."
    )]
    pub async fn reorder_commit(
        &self,
        Parameters(req): Parameters<ReorderCommitReq>,
    ) -> Result<Yaml<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            let (_, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &req.commit)?;
            let mv = plan_splice(
                repo,
                &commits,
                SpliceTarget::InHistory(idx),
                &req.new_parent,
                req.child.as_deref(),
            )?;
            let outcome = repo
                .reorder_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
                .map_err(internal)?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Graft a trashed commit (see list_trash) back into the history, like reorder_commit: new_parent names the commit that becomes its parent (or `root`). On success it leaves the trash."
    )]
    pub async fn restore_commit(
        &self,
        Parameters(req): Parameters<RestoreCommitReq>,
    ) -> Result<Yaml<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, trash| {
            ensure_not_pending(repo)?;
            let info = find_trashed(trash, &req.commit)?;
            let (_, commits) = full_history(repo)?;
            let mv = plan_splice(
                repo,
                &commits,
                SpliceTarget::Trashed(info.clone()),
                &req.new_parent,
                req.child.as_deref(),
            )?;
            let outcome = run_staged(repo, trash, PendingTrashOp::Remove(info.id), |repo| {
                repo.restore_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
            })?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Fold one commit into another, anywhere in the graph (the source may also be a trashed commit). mode picks the message handling: fixup keeps the destination's, squash appends the source's body, amend replaces it — defaulting to the source's `fixup!`/`squash!`/`amend!` subject prefix, else fixup. A merge can be the destination but not the source."
    )]
    pub async fn squash_commit(
        &self,
        Parameters(req): Parameters<SquashCommitReq>,
    ) -> Result<Yaml<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, trash| {
            ensure_not_pending(repo)?;
            let (_, commits) = full_history(repo)?;
            let dest_entries = commits.iter().enumerate().map(|(i, c)| RefEntry::of(c, i));
            let dest_idx = resolve_ref(&req.dest, dest_entries.collect(), || {
                invalid(format!(
                    "dest {} is not in the current branch history; use a ref from \
                     list_history",
                    req.dest
                ))
            })?;

            // History first, then the trash — so after a drop + undo, where
            // the identical commit sits in both, the ref means the live one.
            // An ambiguity *inside* the history errors immediately rather
            // than falling through to the trash.
            let src_entries = commits.iter().enumerate().map(|(i, c)| RefEntry::of(c, i));
            if let Some(src_idx) = lookup_ref(&req.source, src_entries.collect())? {
                let mode = resolve_squash_mode(req.mode.as_deref(), &commits[src_idx].subject)
                    .map_err(invalid)?;
                let (src, dest) = repo.plan_squash(&commits, src_idx, dest_idx).ok_or_else(|| {
                    invalid(
                        "cannot squash: the source must be a non-merge commit on the branch, \
                         distinct from the destination",
                    )
                })?;
                let outcome = repo.squash_into(&src, &dest, mode).map_err(internal)?;
                Ok(save_result(repo, &outcome))
            } else {
                let trash_entries = trash.entries.iter().map(|c| RefEntry::of(c, c.clone()));
                let info = resolve_ref(&req.source, trash_entries.collect(), || {
                    invalid(format!(
                        "source {} is neither in the branch history nor in the trash \
                         (see list_history / list_trash)",
                        req.source
                    ))
                })?;
                let mode =
                    resolve_squash_mode(req.mode.as_deref(), &info.subject).map_err(invalid)?;
                let (src, dest) =
                    repo.plan_squash_restore(&commits, &info, dest_idx).ok_or_else(|| {
                        invalid("cannot squash the trashed commit onto itself or off-branch")
                    })?;
                let outcome = run_staged(repo, trash, PendingTrashOp::Remove(info.id), |repo| {
                    repo.squash_restore_into(&src, &dest, mode)
                })?;
                Ok(save_result(repo, &outcome))
            }
        })
        .await
        .map(Yaml)
    }
}
