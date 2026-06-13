//! Squash one commit into another — the engine primitive behind drag-to-squash
//! in the history list. Built on jj-lib's native [`squash_commits`]: the source
//! commit's changes are merged into the destination's tree and the source is
//! abandoned, then descendants rebase through the shared rewrite/export pipeline
//! (see [`crate::conflict::finish_mutation`]).
//!
//! Also home to the pure helpers that read git's `--autosquash` prefixes
//! (`fixup!` / `squash!` / `amend!`) so the UI can recommend drop targets and
//! compose the merged commit message.

use anyhow::{Context, Result};
use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::Repo as _;
use jj_lib::rewrite::{squash_commits, CommitWithSelection};

use crate::conflict::{op_subject, OpDescriptor, SaveOutcome};
use crate::history::{branch_commits, CommitInfo};
use crate::repo::Repo;
use crate::workcopy::PartialSelection;

/// Which `--autosquash`-style merge to perform when one commit is dropped onto
/// another. Derived from the source commit's subject prefix, or chosen in the
/// UI popup for an unprefixed commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SquashMode {
    /// Keep the destination's message; discard the source's.
    Fixup,
    /// Append the source's message (prefix line stripped) under the destination's.
    Squash,
    /// Replace the destination's message with the source's (prefix line stripped).
    Amend,
}

/// The git-autosquash subject prefixes recognized as the first token of a
/// commit subject, paired with the mode each one selects.
const PREFIXES: [(&str, SquashMode); 3] = [
    ("fixup!", SquashMode::Fixup),
    ("squash!", SquashMode::Squash),
    ("amend!", SquashMode::Amend),
];

/// Rows to highlight while dragging a prefixed commit, as indices into the
/// (newest-first) display list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SquashHighlights {
    /// Green: the real squash destination(s) — subject matches the target.
    pub targets: Vec<usize>,
    /// Yellow: other autosquash-prefixed commits aimed at the same target.
    pub siblings: Vec<usize>,
}

/// The squash mode a subject's leading git-autosquash token requests, if any.
/// Only the first whitespace token is consulted, so `squash! fixup! X` is a
/// Squash.
pub fn parse_squash_mode(subject: &str) -> Option<SquashMode> {
    let token = subject.split_whitespace().next()?;
    PREFIXES.iter().find(|(p, _)| *p == token).map(|(_, m)| *m)
}

/// The bare target subject a prefixed commit points at: strip every stacked
/// leading autosquash token, so `fixup! squash! X` → `X`. Returns `None` if the
/// subject carries no leading prefix token at all.
pub fn squash_target_subject(subject: &str) -> Option<String> {
    let mut rest = subject.trim_start();
    let mut stripped = false;
    loop {
        let token = rest.split_whitespace().next().unwrap_or("");
        if PREFIXES.iter().any(|(p, _)| *p == token) {
            rest = rest[token.len()..].trim_start();
            stripped = true;
        } else {
            break;
        }
    }
    stripped.then(|| rest.to_string())
}

/// The rows to highlight when the commit at display index `from` (a prefixed
/// commit) is dragged: green `targets` whose subject matches its bare target
/// subject, and yellow `siblings` — other prefixed commits aimed at the same
/// target. Both span the whole graph reachable from the branch head (like
/// [`crate::history::plan_reorder_candidates`]) and exclude `from`. Empty when
/// the dragged commit is unprefixed, off-branch, a merge, or nothing matches.
pub fn squash_recommendations(
    commits: &[CommitInfo],
    head: &CommitId,
    from: usize,
) -> SquashHighlights {
    let Some(src) = commits.get(from) else {
        return SquashHighlights::default();
    };
    let Some(target) = squash_target_subject(&src.subject) else {
        return SquashHighlights::default();
    };
    let branch = branch_commits(commits, head);
    if src.parents.len() != 1 || !branch.contains(&src.id) {
        return SquashHighlights::default();
    }
    let candidates: Vec<usize> = (0..commits.len())
        .filter(|&i| i != from && branch.contains(&commits[i].id))
        .collect();
    recommendations_in_chain(commits, &candidates, &target)
}

/// Like [`squash_recommendations`], but for dragging a *trashed* commit
/// (`restored`, a [`CommitInfo`] no longer in `commits`) back over the history:
/// highlight the branch rows its `fixup!`/`squash!`/`amend!` subject points at.
/// Empty when the trashed commit is unprefixed or nothing matches.
pub fn squash_recommendations_for(
    commits: &[CommitInfo],
    head: &CommitId,
    restored: &CommitInfo,
) -> SquashHighlights {
    let Some(target) = squash_target_subject(&restored.subject) else {
        return SquashHighlights::default();
    };
    let branch = branch_commits(commits, head);
    let candidates: Vec<usize> = (0..commits.len())
        .filter(|&i| commits[i].id != restored.id && branch.contains(&commits[i].id))
        .collect();
    recommendations_in_chain(commits, &candidates, &target)
}

/// From the chain `candidates`, pick the highlights for a drag whose bare target
/// subject is `target`: green `targets` (an exact subject match, falling back to
/// a starts-with match when none match exactly) and yellow `siblings` (other
/// prefixed commits aimed at the same `target`, disjoint from the targets).
fn recommendations_in_chain(
    commits: &[CommitInfo],
    candidates: &[usize],
    target: &str,
) -> SquashHighlights {
    let mut targets: Vec<usize> = candidates
        .iter()
        .copied()
        .filter(|&i| commits[i].subject == target)
        .collect();
    if targets.is_empty() && !target.is_empty() {
        targets = candidates
            .iter()
            .copied()
            .filter(|&i| commits[i].subject.starts_with(target))
            .collect();
    }
    let siblings: Vec<usize> = candidates
        .iter()
        .copied()
        .filter(|&i| !targets.contains(&i))
        .filter(|&i| squash_target_subject(&commits[i].subject).as_deref() == Some(target))
        .collect();
    SquashHighlights { targets, siblings }
}

/// Validate a squash of the commit at display row `from` onto the commit at row
/// `onto`: both must be distinct commits reachable from the branch head,
/// anywhere in the graph — jj's `squash_commits` rebases the source's changes
/// across branch lines, so squashing cousins from different merge sides works
/// (the destination's own line is where the result lands). The *source* must
/// not be a merge (its "own change" relative to two parents is its resolution,
/// which stays editable in place instead); a merge as the *destination* is fine
/// — the change folds into the merge's tree like an evil-merge edit. Returns
/// `(source_id, dest_id)` or `None`.
pub fn plan_squash(
    commits: &[CommitInfo],
    head: &CommitId,
    from: usize,
    onto: usize,
) -> Option<(CommitId, CommitId)> {
    if from == onto {
        return None;
    }
    let (src, dest) = (commits.get(from)?, commits.get(onto)?);
    let branch = branch_commits(commits, head);
    if src.parents.len() != 1 || !branch.contains(&src.id) || !branch.contains(&dest.id) {
        return None;
    }
    Some((src.id.clone(), dest.id.clone()))
}

/// Validate squashing a *trashed* commit (`restored`, a [`CommitInfo`] no longer
/// in `commits`) onto the commit at display row `onto`: `onto` must be reachable
/// from the branch head and not be the trashed commit itself. Returns
/// `(source_id, dest_id)` or `None`.
pub fn plan_squash_restore(
    commits: &[CommitInfo],
    head: &CommitId,
    restored: &CommitInfo,
    onto: usize,
) -> Option<(CommitId, CommitId)> {
    let dest = commits.get(onto)?;
    if dest.id == restored.id || !branch_commits(commits, head).contains(&dest.id) {
        return None;
    }
    Some((restored.id.clone(), dest.id.clone()))
}

/// The source commit's message contribution: the description with a leading
/// autosquash-prefix *line* removed (the whole `fixup! …` line). An unprefixed
/// source contributes its full description unchanged.
fn source_body(source_desc: &str) -> String {
    let first = source_desc.lines().next().unwrap_or("");
    if parse_squash_mode(first).is_some() {
        let rest = source_desc.split_once('\n').map_or("", |(_, rest)| rest);
        rest.trim_start_matches('\n').to_string()
    } else {
        source_desc.to_string()
    }
}

/// Compose the destination commit's new description for a squash of one source:
/// - `Fixup`: keep `dest_desc` unchanged.
/// - `Squash`: `dest_desc` + blank line + source body (prefix line stripped),
///   collapsing to just `dest_desc` when the body is empty.
/// - `Amend`: the source body (prefix line stripped) replaces `dest_desc`.
pub fn compose_squash_message(mode: SquashMode, dest_desc: &str, source_desc: &str) -> String {
    compose_squash_message_multi(mode, dest_desc, &[source_desc])
}

/// [`compose_squash_message`] generalized to several sources folded into one
/// destination (the multi-select squash), `sources` ordered newest-first:
/// - `Fixup`: keep `dest_desc`.
/// - `Squash`: `dest_desc` followed by each source body (prefix line stripped),
///   appended **oldest-first** so the merged message reads chronologically;
///   empty bodies are skipped.
/// - `Amend`: the **newest** source's body replaces `dest_desc` (the latest
///   intent wins).
pub fn compose_squash_message_multi(mode: SquashMode, dest_desc: &str, sources: &[&str]) -> String {
    match mode {
        SquashMode::Fixup => dest_desc.to_string(),
        SquashMode::Squash => {
            let mut out = dest_desc.trim_end().to_string();
            // Newest-first in, but append oldest-first so the bodies stack in
            // chronological order under the destination's own message.
            for source_desc in sources.iter().rev() {
                let body = source_body(source_desc);
                let body = body.trim();
                if body.is_empty() {
                    continue;
                }
                if out.is_empty() {
                    out = body.to_string();
                } else {
                    out = format!("{out}\n\n{body}");
                }
            }
            out
        }
        SquashMode::Amend => sources
            .first()
            .map(|s| source_body(s).trim().to_string())
            .unwrap_or_default(),
    }
}

impl Repo {
    /// Plan a drag-squash of the commit at display row `from` onto the commit at
    /// row `onto`, against the current branch chain. See [`plan_squash`].
    pub fn plan_squash(
        &self,
        commits: &[CommitInfo],
        from: usize,
        onto: usize,
    ) -> Option<(CommitId, CommitId)> {
        let head = self.head_commit_id()?;
        plan_squash(commits, &head, from, onto)
    }

    /// Plan a drag-squash of a *trashed* commit `restored` onto the commit at
    /// display row `onto`, against the current branch chain. See
    /// [`plan_squash_restore`].
    pub fn plan_squash_restore(
        &self,
        commits: &[CommitInfo],
        restored: &CommitInfo,
        onto: usize,
    ) -> Option<(CommitId, CommitId)> {
        let head = self.head_commit_id()?;
        plan_squash_restore(commits, &head, restored, onto)
    }

    /// Recommended green/yellow drop-target highlights for the prefixed commit
    /// at display row `from`. See [`squash_recommendations`].
    pub fn squash_recommendations(&self, commits: &[CommitInfo], from: usize) -> SquashHighlights {
        let Some(head) = self.head_commit_id() else {
            return SquashHighlights::default();
        };
        squash_recommendations(commits, &head, from)
    }

    /// Recommended highlights for dragging a *trashed* commit `restored` back
    /// over the history. See [`squash_recommendations_for`].
    pub fn squash_recommendations_for(
        &self,
        commits: &[CommitInfo],
        restored: &CommitInfo,
    ) -> SquashHighlights {
        let Some(head) = self.head_commit_id() else {
            return SquashHighlights::default();
        };
        squash_recommendations_for(commits, &head, restored)
    }

    /// Merge `source`'s changes into `dest`, set `dest`'s message, drop `source`
    /// from history, rebase descendants, and export — all in one transaction.
    /// `message`, when `Some`, becomes `dest`'s new message verbatim; when `None`
    /// the message is recomposed from `dest` and `source` per `mode`
    /// ([`compose_squash_message`]) — so a caller that already knows the merged
    /// message folds and rewords in one step instead of a follow-up edit.
    ///
    /// The destination's author (name, email and author date) is preserved; its
    /// committer is left to jj's rewrite default (re-stamped to the current
    /// identity/time), matching `git rebase --autosquash`.
    pub fn squash_into(
        &mut self,
        source: &CommitId,
        dest: &CommitId,
        mode: SquashMode,
        message: Option<&str>,
    ) -> Result<SaveOutcome> {
        crate::repo::catch_jj("squashing the commit", || {
            self.squash_into_inner(vec![source.clone()], dest, mode, message, false, None)
        })
    }

    /// Fold a *set* of sources into one destination in a single transaction — the
    /// multi-select drag-squash. `sources` are passed newest-first (the display
    /// order); jj's `squash_commits` merges all their changes into `dest`'s tree
    /// at once and abandons them, descendants rebasing through the shared pipeline.
    /// The destination author is preserved (committer re-stamped); the message is
    /// composed per `mode` over all sources ([`compose_squash_message_multi`]) —
    /// **Amend takes the newest source's message** — unless `message` overrides it.
    pub fn squash_into_many(
        &mut self,
        sources: Vec<CommitId>,
        dest: &CommitId,
        mode: SquashMode,
        message: Option<&str>,
    ) -> Result<SaveOutcome> {
        crate::repo::catch_jj("squashing the commits", || {
            self.squash_into_inner(sources, dest, mode, message, false, None)
        })
    }

    /// Like [`Self::squash_into`], but `source` is a *trashed* commit — an orphan
    /// no longer reachable from any visible head. It is briefly re-added as a
    /// head inside the transaction so jj-lib's `squash_commits` can index it (its
    /// `is_ancestor` check panics on an un-indexed commit); the squash then
    /// abandons it again, so it leaves no trace as a head.
    pub fn squash_restore_into(
        &mut self,
        source: &CommitId,
        dest: &CommitId,
        mode: SquashMode,
        message: Option<&str>,
    ) -> Result<SaveOutcome> {
        crate::repo::catch_jj("squashing the commit from trash", || {
            self.squash_into_inner(vec![source.clone()], dest, mode, message, true, None)
        })
    }

    /// Fold a working-copy entry (identified by its stable change id, or the leaf
    /// `@` when `change_hex` is `None`) into the history commit `dest` as a
    /// Fixup — the primitive behind dragging an "uncommitted changes" row onto a
    /// commit. Snapshots first, then resolves the entry to its *current* commit id
    /// (the leaf's churns on snapshot) before delegating to [`Self::squash_into`];
    /// folding the whole leaf leaves jj's recreated empty `@` as a clean working
    /// copy. `message`, when `Some`, replaces `dest`'s message (a working-copy
    /// entry has no message of its own, so the Fixup default keeps `dest`'s).
    /// Returns the usual [`SaveOutcome`], so a conflicting fold enters the shared
    /// conflict-resolution flow.
    pub fn squash_working_copy_into(
        &mut self,
        change_hex: Option<&str>,
        dest: &CommitId,
        message: Option<&str>,
    ) -> Result<SaveOutcome> {
        // Snapshot before resolving so the resolved id survives squash_into's own
        // (now no-op) snapshot — otherwise a churned leaf id would go stale.
        self.snapshot_working_copy()?;
        let source = self
            .resolve_working_copy_change(change_hex)
            .context("no working-copy entry to fold")?;
        self.squash_into(&source, dest, SquashMode::Fixup, message)
    }

    /// Apply a *trashed* (orphan) commit's changes onto the working copy as
    /// uncommitted edits — the inverse of [`Self::squash_working_copy_into`]
    /// (which folds the working copy *into* a commit). `source` is an orphan no
    /// longer reachable from any head, so squashing it into the leaf working-copy
    /// commit `@` lands its diff as unstaged changes without moving the branch
    /// tip: `finish_mutation`'s export is a branch no-op and
    /// `materialize_after_rewrite` merely rewrites the worktree and resets the git
    /// index to the unchanged tip. An overlap with the user's existing uncommitted
    /// changes leaves `@` conflicted and enters the shared conflict-resolution flow
    /// like any other rewrite. The engine behind the GTK trash-row "restore to
    /// working tree" button and the second half of [`Self::drop_keeping_changes`].
    pub fn restore_to_working_copy(&mut self, source: &CommitId) -> Result<SaveOutcome> {
        crate::repo::catch_jj("restoring the changes to the working copy", || {
            // Snapshot before resolving so the leaf id survives squash_into_inner's
            // own (now no-op) snapshot — otherwise a churned leaf id would go stale.
            self.snapshot_working_copy()?;
            let dest = self
                .working_copy_commit_id()
                .context("no working copy to restore the changes into")?;
            let source_commit = self
                .repo
                .store()
                .get_commit(source)
                .context("loading the commit to restore")?;
            let label = format!("Restore {} to working copy", op_subject(&source_commit));
            self.squash_into_inner(
                vec![source.clone()],
                &dest,
                SquashMode::Fixup,
                None,
                true,
                Some(label),
            )
        })
    }

    /// Fold a **subset** of the uncommitted changes into the history commit
    /// `dest`, leaving the remainder uncommitted — the partial counterpart of
    /// [`Self::squash_working_copy_into`] (the `git add -p` to its `git commit -a`,
    /// but folding into an existing commit). The subset is addressed by
    /// [`PartialSelection`] exactly as [`Self::commit_working_copy_partial`]
    /// addresses it (relative to HEAD).
    ///
    /// Done in one transaction: a throwaway commit `C` holding the selected subset
    /// is created on HEAD and the leaf `@` is rebuilt to hold the **full** on-disk
    /// tree on top of it (so disk stays byte-identical and the unselected delta
    /// stays uncommitted), then `C` is squashed into `dest` — its change folded in,
    /// `C` abandoned, descendants (including `@`) rebased. Because the rebased `@`
    /// equals the full disk tree again, the worktree never moves. `message`, when
    /// `Some`, becomes `dest`'s new message; else `dest`'s is kept (Fixup, like a
    /// working-copy fold). A clashing fold enters the shared conflict flow.
    pub fn squash_working_copy_partial_into(
        &mut self,
        sel: PartialSelection<'_>,
        dest: &CommitId,
        message: Option<&str>,
    ) -> Result<SaveOutcome> {
        crate::repo::catch_jj("folding part of the working copy", || {
            self.squash_working_copy_partial_into_inner(sel, dest, message)
        })
    }

    fn squash_working_copy_partial_into_inner(
        &mut self,
        sel: PartialSelection<'_>,
        dest: &CommitId,
        message: Option<&str>,
    ) -> Result<SaveOutcome> {
        // Snapshot + build the selected subset's tree (`t_commit`) and the full
        // on-disk tree, against HEAD — shared with commit_working_copy_partial.
        let (head, head_tree, full_tree, t_commit) = self.prepare_partial_commit(&sel)?;

        let name = self.workspace.workspace_name().to_owned();
        let pre_op = self.repo.operation().clone();
        let old_head = self.head_commit();
        let heads = self.snapshot_heads();

        let store = self.repo.store().clone();
        let dest_commit = store
            .get_commit(dest)
            .context("loading destination commit")?;
        // A working-copy entry carries no message, so the Fixup default keeps the
        // destination's; an explicit override replaces it.
        let new_desc = match message {
            Some(m) => m.to_string(),
            None => dest_commit.description().to_string(),
        };
        let dest_author = dest_commit.author().clone();
        let desc = OpDescriptor::new(
            format!("Squash working copy into {}", op_subject(&dest_commit)),
            vec![dest_commit.change_id().hex()],
        );

        let mut tx = self.repo.start_transaction();
        // C: the selected subset, committed on HEAD.
        let created = pollster::block_on(
            tx.repo_mut()
                .new_commit(vec![head.clone()], t_commit)
                .set_description("commedit: partial squash staging")
                .write(),
        )
        .context("writing the partial commit")?;
        let created_id = created.id().clone();
        // leaf': the remainder — the full on-disk tree as a child of C, with @
        // pointed at it (via `edit`, not `check_out`: @ must *hold* the full tree so
        // disk stays byte-identical and leaf'-vs-C is the unselected delta).
        let remainder = pollster::block_on(
            tx.repo_mut()
                .new_commit(vec![created_id.clone()], full_tree)
                .write(),
        )
        .context("writing the working-copy remainder")?;
        pollster::block_on(tx.repo_mut().edit(name, &remainder))
            .context("pointing the working copy at the remainder")?;

        // Fold C entirely into dest: its whole change (`C.tree` vs HEAD's tree) is
        // the selected subset. rebase_descendants then carries @ (and any commits
        // between dest and HEAD) onto the rewritten line.
        let sel_for_c = CommitWithSelection {
            selected_tree: created.tree(),
            parent_tree: head_tree,
            commit: created,
        };
        let squashed = pollster::block_on(squash_commits(
            tx.repo_mut(),
            std::slice::from_ref(&sel_for_c),
            &dest_commit,
            /* keep_emptied = */ false,
        ))
        .context("squashing commits")?
        .context("squash produced no commit (empty selection)")?;
        pollster::block_on(
            squashed
                .commit_builder
                .set_description(new_desc)
                .set_author(dest_author)
                .write(),
        )
        .context("writing squashed commit")?;
        pollster::block_on(tx.repo_mut().rebase_descendants()).context("rebasing descendants")?;

        // The subset is preserved as a net change, so the post-squash tip is clean
        // even if an interior commit conflicts spuriously — same CleanTip resolve
        // as a whole-working-copy fold / reorder.
        self.finish_mutation_auto_resolve(
            tx,
            "commedit: squash working copy (partial)",
            desc,
            pre_op,
            old_head,
            heads,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn squash_into_inner(
        &mut self,
        sources: Vec<CommitId>,
        dest: &CommitId,
        mode: SquashMode,
        message: Option<&str>,
        source_is_orphan: bool,
        label: Option<String>,
    ) -> Result<SaveOutcome> {
        if sources.is_empty() {
            anyhow::bail!("no commits to squash");
        }
        // Capture the on-disk working copy into @ so it rebases with the rewrite.
        self.snapshot_working_copy()?;
        let pre_op = self.repo.operation().clone();
        let old_head = self.head_commit();
        let heads = self.snapshot_heads();

        // jj's `squash_commits` wants the sources newest-first (reverse topo); the
        // UI passes them so already, but sort defensively (a no-op for one source,
        // and an orphan source — always single — never reaches the index here).
        let sources = self.sort_reverse_topological(sources)?;
        let store = self.repo.store().clone();
        let source_commits: Vec<Commit> = sources
            .iter()
            .map(|s| store.get_commit(s).context("loading source commit"))
            .collect::<Result<_>>()?;
        let dest_commit = store
            .get_commit(dest)
            .context("loading destination commit")?;

        // A full-commit selection per source: take all of each source's changes.
        let sels: Vec<CommitWithSelection> = source_commits
            .iter()
            .map(|c| {
                let parent_tree = pollster::block_on(c.parent_tree(self.repo.as_ref()))
                    .context("loading source parent tree")?;
                Ok(CommitWithSelection {
                    commit: c.clone(),
                    selected_tree: c.tree(),
                    parent_tree,
                })
            })
            .collect::<Result<_>>()?;

        // Settle the message and capture the author before the borrows move into
        // the transaction: an explicit override wins, else recompose per `mode`
        // over every source (newest-first; Amend takes the newest's message).
        let source_descs: Vec<&str> = source_commits.iter().map(|c| c.description()).collect();
        let new_desc = match message {
            Some(m) => m.to_string(),
            None => compose_squash_message_multi(mode, dest_commit.description(), &source_descs),
        };
        let dest_author = dest_commit.author().clone();
        let label = label.unwrap_or_else(|| {
            if let [only] = source_commits.as_slice() {
                format!(
                    "Squash {} into {}",
                    op_subject(only),
                    op_subject(&dest_commit)
                )
            } else {
                format!(
                    "Squash {} commits into {}",
                    source_commits.len(),
                    op_subject(&dest_commit)
                )
            }
        });
        let affected: Vec<String> = source_commits
            .iter()
            .map(|c| c.change_id().hex())
            .chain(std::iter::once(dest_commit.change_id().hex()))
            .collect();
        let desc = OpDescriptor::new(label, affected);

        let mut tx = self.repo.start_transaction();
        if source_is_orphan {
            // The trashed source isn't reachable from any visible head, so it's
            // absent from the index — make it visible so squash_commits'
            // is_ancestor check finds it instead of panicking on the lookup. The
            // squash abandons it (full selection), so rebase_descendants drops it
            // from the heads again and it leaves no trace.
            for c in &source_commits {
                pollster::block_on(tx.repo_mut().add_head(c))
                    .context("making the trashed commit visible")?;
            }
        }
        let squashed = pollster::block_on(squash_commits(
            tx.repo_mut(),
            &sels,
            &dest_commit,
            /* keep_emptied = */ false,
        ))
        .context("squashing commits")?
        .context("squash produced no commit (empty selection)")?;

        // Preserve the destination's author; leaving the committer unset lets
        // jj re-stamp it (otherwise `rewrite_commit` would do so anyway).
        pollster::block_on(
            squashed
                .commit_builder
                .set_description(new_desc)
                .set_author(dest_author)
                .write(),
        )
        .context("writing squashed commit")?;

        pollster::block_on(tx.repo_mut().rebase_descendants()).context("rebasing descendants")?;

        // A squash preserves the net change set, so the post-squash tip is clean
        // and identical to the original even when an interior commit conflicts
        // spuriously — opt into the same CleanTip auto-resolution as a reorder.
        self.finish_mutation_auto_resolve(
            tx,
            "commedit: squash commit",
            desc,
            pre_op,
            old_head,
            heads,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        compose_squash_message, parse_squash_mode, plan_squash, plan_squash_restore,
        squash_recommendations, squash_recommendations_for, squash_target_subject,
        SquashHighlights, SquashMode,
    };
    use crate::history::CommitInfo;
    use jj_lib::backend::{ChangeId, CommitId};

    /// A bare [`CommitInfo`] with id `id`, single parent `parent`, and `subject`.
    fn ci(id: u8, parent: u8, subject: &str) -> CommitInfo {
        CommitInfo {
            id: CommitId::new(vec![id]),
            change_id: ChangeId::new(vec![id]),
            subject: subject.to_string(),
            description: subject.to_string(),
            author_name: String::new(),
            author_email: String::new(),
            committer_name: String::new(),
            committer_email: String::new(),
            author_time: String::new(),
            committer_time: String::new(),
            parents: vec![CommitId::new(vec![parent])],
        }
    }

    fn cid(id: u8) -> CommitId {
        CommitId::new(vec![id])
    }

    #[test]
    fn parses_the_leading_prefix_token() {
        assert_eq!(parse_squash_mode("fixup! foo"), Some(SquashMode::Fixup));
        assert_eq!(parse_squash_mode("squash! foo"), Some(SquashMode::Squash));
        assert_eq!(parse_squash_mode("amend! foo"), Some(SquashMode::Amend));
        assert_eq!(parse_squash_mode("fixup!"), Some(SquashMode::Fixup));
        assert_eq!(parse_squash_mode("fix things"), None);
        // Only the first token decides the mode.
        assert_eq!(
            parse_squash_mode("squash! fixup! x"),
            Some(SquashMode::Squash)
        );
    }

    #[test]
    fn extracts_the_bare_target_subject() {
        assert_eq!(squash_target_subject("fixup! foo"), Some("foo".to_string()));
        assert_eq!(
            squash_target_subject("fixup! squash! foo"),
            Some("foo".to_string())
        );
        assert_eq!(squash_target_subject("plain subject"), None);
        // Internal whitespace of the remainder is preserved.
        assert_eq!(
            squash_target_subject("squash!  spaced  target"),
            Some("spaced  target".to_string())
        );
    }

    /// Newest-first chain: `fixup! second` (0) <- second (1) <- first (2) <- root.
    fn fixup_history() -> Vec<CommitInfo> {
        vec![
            ci(3, 2, "fixup! second"),
            ci(2, 1, "second"),
            ci(1, 0, "first"),
        ]
    }

    #[test]
    fn recommends_the_matching_target() {
        let h = fixup_history();
        let r = squash_recommendations(&h, &cid(3), 0);
        assert_eq!(
            r,
            SquashHighlights {
                targets: vec![1],
                siblings: vec![]
            }
        );
    }

    #[test]
    fn marks_other_fixups_as_siblings() {
        // 3 = fixup! second (dragged), 4 = squash! second (sibling), 2 = second.
        let h = vec![
            ci(3, 4, "fixup! second"),
            ci(4, 2, "squash! second"),
            ci(2, 1, "second"),
            ci(1, 0, "first"),
        ];
        let r = squash_recommendations(&h, &cid(3), 0);
        assert_eq!(r.targets, vec![2]); // the real "second"
        assert_eq!(r.siblings, vec![1]); // the other prefixed commit
    }

    #[test]
    fn unprefixed_or_offchain_has_no_recommendations() {
        let h = fixup_history();
        // Row 1 ("second") is not prefixed.
        assert_eq!(
            squash_recommendations(&h, &cid(3), 1),
            SquashHighlights::default()
        );
    }

    #[test]
    fn plans_a_valid_squash_and_rejects_self_or_offchain() {
        let h = fixup_history();
        assert_eq!(plan_squash(&h, &cid(3), 0, 1), Some((cid(3), cid(2))));
        assert_eq!(plan_squash(&h, &cid(3), 1, 1), None); // onto itself
        assert_eq!(plan_squash(&h, &cid(3), 0, 9), None); // out of range
    }

    /// A commit with an explicit parent set, for merge topologies (0 = root).
    fn merge_ci(id: u8, parents: &[u8], subject: &str) -> CommitInfo {
        let mut c = ci(id, 0, subject);
        c.parents = parents.iter().map(|&p| CommitId::new(vec![p])).collect();
        c
    }

    /// Merge 4(3, 2) of main-1 (3) and side-1 (2), both branched off base (1).
    fn merge_history() -> Vec<CommitInfo> {
        vec![
            merge_ci(4, &[3, 2], "merge"),
            ci(3, 1, "main-1"),
            ci(2, 1, "side-1"),
            ci(1, 0, "base"),
        ]
    }

    #[test]
    fn plans_a_squash_across_merge_branches() {
        let h = merge_history();
        // Cousins on different sides squash (jj rebases the change across)…
        assert_eq!(plan_squash(&h, &cid(4), 2, 1), Some((cid(2), cid(3))));
        // …and the merge is a valid destination (an evil-merge style fold)…
        assert_eq!(plan_squash(&h, &cid(4), 1, 0), Some((cid(3), cid(4))));
        // …but a merge is not a source: its own change is its resolution.
        assert_eq!(plan_squash(&h, &cid(4), 0, 1), None);
    }

    #[test]
    fn recommendations_span_merge_branches() {
        // The fixup sits on the side branch, its target on the mainline.
        let h = vec![
            merge_ci(4, &[3, 2], "merge"),
            ci(3, 1, "main-1"),
            ci(2, 1, "fixup! main-1"),
            ci(1, 0, "base"),
        ];
        let r = squash_recommendations(&h, &cid(4), 2);
        assert_eq!(r.targets, vec![1]);
        assert_eq!(r.siblings, vec![]);
    }

    #[test]
    fn plans_a_squash_from_trash_onto_a_chain_commit() {
        // Chain 3 <- 2 <- 1; `dropped` (id 9) is a trashed commit not in it.
        let h = vec![ci(3, 2, "third"), ci(2, 1, "second"), ci(1, 0, "first")];
        let dropped = ci(9, 1, "dropped");
        // Squashing the trashed commit onto "second" (row 1) is valid.
        assert_eq!(
            plan_squash_restore(&h, &cid(3), &dropped, 1),
            Some((cid(9), cid(2)))
        );
        // Out of range, and onto the trashed commit itself, are rejected.
        assert_eq!(plan_squash_restore(&h, &cid(3), &dropped, 9), None);
        let self_drop = ci(2, 1, "second");
        assert_eq!(plan_squash_restore(&h, &cid(3), &self_drop, 1), None);
    }

    #[test]
    fn recommends_a_target_for_a_trashed_prefixed_commit() {
        let h = fixup_history(); // 3=fixup! second, 2=second, 1=first
                                 // A trashed `fixup! second` (id 9, not in the chain) points at "second".
        let dropped = ci(9, 1, "fixup! second");
        let r = squash_recommendations_for(&h, &cid(3), &dropped);
        // Row 1 is "second"; row 0 is the (in-chain) other fixup, a sibling.
        assert_eq!(r.targets, vec![1]);
        assert_eq!(r.siblings, vec![0]);
        // An unprefixed trashed commit recommends nothing.
        let plain = ci(9, 1, "whatever");
        assert_eq!(
            squash_recommendations_for(&h, &cid(3), &plain),
            SquashHighlights::default()
        );
    }

    #[test]
    fn composes_the_merged_message() {
        // Fixup keeps the destination message.
        assert_eq!(
            compose_squash_message(SquashMode::Fixup, "second", "fixup! second\n\nignored"),
            "second"
        );
        // Squash appends the source body (prefix line stripped).
        assert_eq!(
            compose_squash_message(
                SquashMode::Squash,
                "second",
                "squash! second\n\nmore detail"
            ),
            "second\n\nmore detail"
        );
        // Empty body collapses to just the destination message.
        assert_eq!(
            compose_squash_message(SquashMode::Squash, "second", "squash! second"),
            "second"
        );
        // Amend replaces with the source body only.
        assert_eq!(
            compose_squash_message(SquashMode::Amend, "second", "amend! second\n\nnew message"),
            "new message"
        );
        // An unprefixed source contributes its full description.
        assert_eq!(
            compose_squash_message(SquashMode::Squash, "second", "tweak code"),
            "second\n\ntweak code"
        );
    }
}
