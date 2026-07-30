//! Tools over the uncommitted changes (the engine's working-copy commit `@`)
//! and the session-wide review diff.

use std::collections::HashSet;

use commedit_engine::history::IdAbbrev;
use commedit_engine::workcopy::PartialSelection;
use jj_lib::object_id::ObjectId as _;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, ErrorData};

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::rewrite::Identity;
use commedit_engine::workcopy::CarveEntry;

use crate::convert::{commit_dto, file_change_dto, DetailFields};
use crate::dto::{
    AbsorbFileStatDto, AbsorbPlanEntryDto, AbsorbSkipDto, AbsorbWorkingCopyReq,
    AbsorbWorkingCopyResp, CarveWorkingCopyReq, CarveWorkingCopyResp, CommitWorkingCopyReq,
    CommitWorkingCopyResp, DiscardWorkingCopyReq, HunkSelectionDto, OkResp, PatchSelectionDto,
    SaveResultDto, SessionDiffResp, SessionSel, SquashWorkingCopyReq, SquashWorkingCopyResp,
    WorkingCopyStatusResp,
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
        description = "Diff everything this session changed so far — the current tree (uncommitted changes included) against the tree at session start. Message/identity-only edits don't show up (they change no tree). Each file's diff is capped at a line limit (a cut file is marked `truncated` with its `total_lines`)."
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
        description = "Fold the uncommitted changes into a commit as a fixup (the destination's message is kept; pass `message` to reword it in the same call). Pass `paths`/`hunks`/`patches` to fold only PART of the changes, leaving the rest uncommitted; omit all three to fold everything. Only already-tracked files are folded — an untracked file needs `add_paths` (see the field docs). A clean fold returns the `topology` slice and the remaining `working_copy` (the unselected remainder for a partial fold); an overlap with the commit conflicts like any rewrite. Pass `dry_run: true` to preview the fold — `dest_changes` (what the destination would really gain) and `remaining`, nothing written — then call again to apply. A real `patches`-tier fold echoes `dest_changes` too: that tier is 3-way merged into the destination rather than replayed there, so a patch built against HEAD can land only partly and still report `clean`. Check it."
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
                dry_run,
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
            // The `patches` tier is the one whose result can differ from its
            // intent (it is merged into the destination, not replayed there), so
            // it gets the destination echo even on a real fold.
            let echo_dest = dry_run || patches.as_ref().is_some_and(|p| !p.is_empty());
            let outcome = if partial {
                let (paths, hunks, patches) = parse_partial_selection(paths, hunks, patches)?;
                let sel = PartialSelection {
                    paths: &paths,
                    hunks: &hunks,
                    patches: &patches,
                };
                repo.squash_working_copy_partial_into_ext(
                    sel,
                    &dest_id,
                    message.as_deref(),
                    dry_run,
                )
                .map_err(internal)?
            } else {
                repo.squash_working_copy_into_ext(None, &dest_id, message.as_deref(), dry_run)
                    .map_err(internal)?
            };
            let dest_changes = echo_dest.then(|| {
                outcome
                    .dest_changes
                    .iter()
                    .map(|fc| file_change_dto(fc, false))
                    .collect()
            });

            let Some(applied) = outcome.applied else {
                // A dry run wrote nothing, so there is no outcome and no settled
                // working copy to read — just the preview.
                return Ok(SquashWorkingCopyResp {
                    dry_run: true,
                    result: None,
                    dest_changes,
                    remaining: Some(outcome.remaining),
                    working_copy: None,
                });
            };
            let result = save_result_topo(repo, &applied, &pre, &anchors)?;
            // On a clean fold, report what's left uncommitted (clean for a whole
            // fold, the unselected remainder for a partial one); on conflicts the
            // working copy is held with the rewrite, so there's nothing to report.
            let working_copy = match &result {
                SaveResultDto::Clean { .. } => Some(working_copy_status_resp(repo)?),
                SaveResultDto::Conflicts { .. } => None,
            };
            Ok(SquashWorkingCopyResp {
                dry_run: false,
                result: Some(result),
                dest_changes,
                remaining: None,
                working_copy,
            })
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Commit the uncommitted changes as a new commit on top of HEAD (like `git commit -a`), leaving the tree clean — and the only way to commit a deterministic SUBSET of the tree. Pass `paths`/`hunks`/`patches` to commit only PART of the changes, leaving the rest uncommitted; omit all three to commit everything. Only already-tracked files are committed — an untracked file needs `add_paths` (see the field docs). To author files from explicit contents, or insert a commit elsewhere in history, use create_commit instead. Returns the new `committed` commit (sha + stable change_id, ready to chain) and the remaining `working_copy` (the unselected remainder for a partial commit)."
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
        description = "Carve the uncommitted changes into SEVERAL commits in one call — an ordered (oldest-first) list of {message, selection}, each stacked on the previous on top of HEAD, with whatever no commit selects left uncommitted. This is the batch commit_working_copy: every selection addresses the one working-copy diff you already read, so hunk indices don't shift between commits the way they do across separate commit_working_copy calls. Each commit's `paths`/`hunks`/`patches` tiers work as in commit_working_copy; across the carve a path may be split by `hunks` (disjoint indices) but a whole-file/`patches` selection of a path must be unique. Untracked files need `add_paths`. Returns the new commits (oldest-first) and the remaining working copy."
    )]
    pub async fn carve_working_copy(
        &self,
        Parameters(req): Parameters<CarveWorkingCopyReq>,
    ) -> Result<Yaml<CarveWorkingCopyResp>, ErrorData> {
        self.with_session(req.session.session.clone(), move |repo, _| {
            ensure_not_pending(repo)?;
            ensure_worktree_bound(repo)?;
            repo.snapshot_working_copy_tracking(&req.add_paths.clone().unwrap_or_default())
                .map_err(internal)?;
            if repo.working_copy_chain().is_empty() {
                return Err(invalid("the working copy is clean — nothing to carve"));
            }

            // Own each commit's message, identity and selection tiers, so the
            // borrowed CarveEntry list outlives the carve call.
            let mut owned: Vec<(String, Option<Identity>, PartialTiers)> =
                Vec::with_capacity(req.commits.len());
            for c in req.commits {
                let tiers = parse_partial_selection(c.paths, c.hunks, c.patches)?;
                let identity = new_commit_identity(repo, c.identity);
                owned.push((c.message, identity, tiers));
            }
            if owned.is_empty() {
                return Err(invalid("carve needs at least one commit to create"));
            }
            let entries: Vec<CarveEntry> = owned
                .iter()
                .map(|(message, identity, (paths, hunks, patches))| CarveEntry {
                    message,
                    identity: identity.as_ref(),
                    selection: PartialSelection {
                        paths,
                        hunks,
                        patches,
                    },
                })
                .collect();

            let (outcome, change_ids) = repo.carve_working_copy(&entries).map_err(internal)?;
            drop(entries);
            let result = save_result(repo, &outcome);

            // Map the new change_ids (oldest-first) back to commit DTOs.
            let (_, commits) = full_history(repo)?;
            let refs = repo.commit_refs();
            let root = repo.root_commit_id().hex();
            let abbrev = IdAbbrev::new(&repo.repo);
            let committed = change_ids
                .iter()
                .filter_map(|cid| {
                    commits
                        .iter()
                        .find(|c| &c.change_id_hex() == cid)
                        .map(|info| commit_dto(info, &root, &refs, &abbrev, DetailFields::ALL))
                })
                .collect();
            let working_copy = working_copy_status_resp(repo)?;
            Ok(CarveWorkingCopyResp {
                result,
                committed,
                working_copy: Some(working_copy),
            })
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Fold each uncommitted hunk into the commit that introduced the lines it touches, in one rewrite (like `git absorb`/`jj absorb`) — the fast path for a pile of fixups spread across several ancestors, replacing a blame_squash_targets call plus one squash_working_copy per commit. Only hunks that blame unambiguously to a single commit move; ambiguous, binary or structural ones stay uncommitted (see `remaining` and `skipped`). Pass `dry_run: true` to preview the routing `plan` without changing anything, then call again to apply. `paths` restricts it to those files. Usually lands clean; a fold that can't merge cleanly is held back with `status: conflicts` like any rewrite."
    )]
    pub async fn absorb_working_copy(
        &self,
        Parameters(req): Parameters<AbsorbWorkingCopyReq>,
    ) -> Result<Yaml<AbsorbWorkingCopyResp>, ErrorData> {
        self.with_session(req.session.session.clone(), move |repo, _| {
            ensure_not_pending(repo)?;
            ensure_worktree_bound(repo)?;
            let paths = req.paths.unwrap_or_default();
            let outcome = repo
                .absorb_working_copy(&paths, req.dry_run)
                .map_err(internal)?;

            let plan = outcome
                .plan
                .iter()
                .map(|e| AbsorbPlanEntryDto {
                    change_id: e.target.change_id_hex(),
                    sha: e.target.id_hex(),
                    subject: e.target.subject.clone(),
                    files: e
                        .files
                        .iter()
                        .map(|f| AbsorbFileStatDto {
                            path: f.path.clone(),
                            added: f.added,
                            removed: f.removed,
                            hunks: f.hunks,
                        })
                        .collect(),
                })
                .collect();
            let skipped = outcome
                .skipped
                .iter()
                .map(|(path, reason)| AbsorbSkipDto {
                    path: path.clone(),
                    reason: reason.clone(),
                })
                .collect();
            let applied = outcome.applied.as_ref().map(|o| save_result(repo, o));
            // On a clean apply, report what's left uncommitted (the unattributed
            // remainder); on a dry run or conflicts there's nothing settled to read.
            let working_copy = match &outcome.applied {
                Some(SaveOutcome::Clean) => Some(working_copy_status_resp(repo)?),
                _ => None,
            };
            Ok(AbsorbWorkingCopyResp {
                dry_run: req.dry_run,
                plan,
                skipped,
                remaining: outcome.remaining,
                applied,
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
