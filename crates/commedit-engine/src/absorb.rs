//! One-shot "absorb": route every unambiguously-attributable hunk of the
//! uncommitted changes into the commit that introduced the lines it touches,
//! folding a pile of fixups into the right ancestors in a single rewrite.
//!
//! This is the in-process equivalent of `jj absorb` / `hg absorb` / `git
//! absorb`, built directly on jj-lib's [`jj_lib::absorb`] module: it does the
//! content annotation and the ambiguity rules (a hunk that could belong to two
//! different ancestors, or whose context was itself edited, is left alone), and
//! folds every routed hunk in **one** `transform_descendants` pass — one rebase,
//! one deferred git export through the shared [`Repo::finish_mutation`] tail.
//! Because the net working-copy tree is preserved (a hunk only moves *down* into
//! an ancestor, it isn't dropped), the branch tip stays byte-for-byte identical
//! and the on-disk files never change. It still goes through
//! `finish_mutation_auto_resolve`: usually the fold is clean, but injecting a
//! hunk into an ancestor whose file is very different from the tip can leave
//! jj's 3-way merge without a common anchor and conflict — held back like any
//! other conflicted rewrite, git untouched, to resolve or abort.
//!
//! [`Repo::absorb_working_copy`] runs against the launch worktree's `@` and has a
//! `dry_run` mode returning the routing plan (which hunks would move where, what
//! is skipped, whether anything stays uncommitted) without touching anything, so
//! a caller can preview and veto before applying.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use futures::StreamExt as _;
use similar::{ChangeTag, TextDiff};

use jj_lib::absorb::{absorb_hunks, split_hunks_to_trees, AbsorbSource};
use jj_lib::backend::CommitId;
use jj_lib::matchers::{EverythingMatcher, FilesMatcher, Matcher};
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::revset::{RevsetExpression, SymbolResolver, SymbolResolverExtension};

use crate::conflict::{OpDescriptor, SaveOutcome};
use crate::diff::tree_changes;
use crate::history::CommitInfo;
use crate::repo::Repo;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    pollster::block_on(f)
}

/// One changed file in an [`AbsorbPlanEntry`]: the hunks that would move into
/// (or, for a wholesale deletion, out of) the target commit.
#[derive(Debug, Clone)]
pub struct AbsorbFileStat {
    /// Path relative to the repo root (internal, forward-slash form).
    pub path: String,
    /// Lines the routed hunks add to the target.
    pub added: usize,
    /// Lines the routed hunks remove from the target.
    pub removed: usize,
    /// Number of contiguous hunks routed to this target for this file.
    pub hunks: usize,
}

/// One destination commit in an absorb plan: the commit the routed hunks belong
/// to, and the per-file breakdown of what would fold into it.
#[derive(Debug, Clone)]
pub struct AbsorbPlanEntry {
    /// The target commit (its stable change id survives the rewrite, so a caller
    /// can still address it after applying).
    pub target: CommitInfo,
    /// The changed files whose hunks route to this target.
    pub files: Vec<AbsorbFileStat>,
}

/// The outcome of [`Repo::absorb_working_copy`], for both the dry-run preview and
/// the applied rewrite.
#[derive(Debug, Clone)]
pub struct AbsorbOutcome {
    /// Per destination commit, ancestors-first, the hunks that route to it.
    /// Empty when nothing in the working copy could be attributed.
    pub plan: Vec<AbsorbPlanEntry>,
    /// Paths skipped wholesale, each with jj's reason (binary, symlink, a
    /// conflict, a submodule): those can't be absorbed as text.
    pub skipped: Vec<(String, String)>,
    /// Whether any uncommitted change would remain after the absorb — an
    /// ambiguous or unattributable hunk that stays in the working copy.
    pub remaining: bool,
    /// The save outcome when applied; `None` for a dry run or when the plan is
    /// empty (nothing to apply).
    pub applied: Option<SaveOutcome>,
}

/// Count the lines added/removed and the number of contiguous hunks between two
/// file versions (a contiguous delete-then-insert run counts as one hunk).
fn diff_stat(old: &str, new: &str) -> (usize, usize, usize) {
    let diff = TextDiff::from_lines(old, new);
    let (mut added, mut removed, mut hunks) = (0, 0, 0);
    let mut in_hunk = false;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => in_hunk = false,
            ChangeTag::Delete => {
                removed += 1;
                if !in_hunk {
                    hunks += 1;
                    in_hunk = true;
                }
            }
            ChangeTag::Insert => {
                added += 1;
                if !in_hunk {
                    hunks += 1;
                    in_hunk = true;
                }
            }
        }
    }
    (added, removed, hunks)
}

impl Repo {
    /// Absorb the uncommitted changes into their originating ancestors.
    ///
    /// Each hunk of the working copy that blames unambiguously to a single commit
    /// in the branch's history is folded into that commit; ambiguous hunks (a
    /// change spanning two ancestors, or touching context another edit changed)
    /// and binary/structural files are left uncommitted. `paths` restricts the
    /// absorb to those files (empty = all changed files). With `dry_run`, nothing
    /// is written — the returned [`AbsorbOutcome`] carries only the routing plan.
    ///
    /// Launch-worktree only, like the partial commit/squash: it operates on the
    /// launch `@`. Refuses when the tree is clean, HEAD is detached/unborn, or the
    /// working copy has been split into a chain (there is no single source `@`).
    pub fn absorb_working_copy(
        &mut self,
        paths: &[String],
        dry_run: bool,
    ) -> Result<AbsorbOutcome> {
        self.require_worktree("absorb the working copy")?;
        crate::repo::catch_jj("absorbing the working copy", || {
            self.absorb_working_copy_inner(paths, dry_run)
        })
    }

    fn absorb_working_copy_inner(
        &mut self,
        paths: &[String],
        dry_run: bool,
    ) -> Result<AbsorbOutcome> {
        // Fold the on-disk changes into `@` first, then refuse a clean tree.
        self.snapshot_working_copy()?;
        if self.working_copy_info().is_none() {
            bail!("no uncommitted changes to absorb");
        }
        // The source is a single `@` sitting directly on the branch tip. A split
        // `@` chain (from the GTK Split gesture) has no single source commit whose
        // parent is HEAD, so absorb doesn't apply.
        if self.working_copy_chain_ids().len() > 1 {
            bail!(
                "the working copy is split into multiple entries; commit or recombine \
                 them before absorbing"
            );
        }
        let leaf_id = self
            .working_copy_commit_id()
            .context("no working copy to absorb")?;
        let head_id = self
            .head_commit_id()
            .context("the repository has no branch head; cannot absorb the working copy")?;

        let store = self.repo.store().clone();
        let source_commit = store
            .get_commit(&leaf_id)
            .context("loading the working-copy commit")?;
        let source = block_on(AbsorbSource::from_commit(&*self.repo, source_commit))
            .context("reading the absorb source")?;

        // The tree the hunks are relative to — the branch tip's tree, i.e. the
        // source `@`'s parent. Diffing a target's selected tree against this gives
        // exactly the hunks routed to that target.
        let base_tree = store
            .get_commit(&head_id)
            .context("loading the branch head")?
            .tree();

        let matcher: Box<dyn Matcher> = if paths.is_empty() {
            Box::new(EverythingMatcher)
        } else {
            let repo_paths: Vec<RepoPathBuf> = paths
                .iter()
                .map(|p| {
                    RepoPathBuf::from_internal_string(p)
                        .with_context(|| format!("invalid path '{p}'"))
                })
                .collect::<Result<_>>()?;
            Box::new(FilesMatcher::new(repo_paths))
        };

        // The annotation domain: the branch tip and all its ancestors (`::head`).
        // The annotator narrows it per file; hunks tracing outside the range (or
        // to a merge boundary) are left unattributed and stay uncommitted. Scoped
        // so the symbol resolver's borrow of `self.repo` ends here (the resolved
        // expression it returns is owned).
        let destinations = {
            let symbol_resolver =
                SymbolResolver::new(&*self.repo, &([] as [&Box<dyn SymbolResolverExtension>; 0]));
            RevsetExpression::commits(vec![head_id])
                .ancestors()
                .resolve_user_expression(&*self.repo, &symbol_resolver)
                .context("resolving the absorb domain")?
        };

        let selected = block_on(split_hunks_to_trees(
            &*self.repo,
            &source,
            &destinations,
            matcher.as_ref(),
        ))
        .context("routing hunks to their origins")?;

        let skipped: Vec<(String, String)> = selected
            .skipped_paths
            .iter()
            .map(|(p, reason)| (p.as_internal_file_string().to_string(), reason.clone()))
            .collect();

        // Materialize each target's selected tree once — for the plan's per-file
        // stats — then wrap it back into a builder so `absorb_hunks` can reuse it
        // without re-annotating.
        let mut detail: HashMap<CommitId, Vec<AbsorbFileStat>> = HashMap::new();
        let mut rebuilt: HashMap<CommitId, MergedTreeBuilder> = HashMap::new();
        for (cid, builder) in selected.target_commits {
            let selected_tree = block_on(builder.write_tree()).context("writing selected tree")?;
            let changes = tree_changes(&store, &base_tree, &selected_tree)?;
            let files = changes
                .iter()
                .map(|fc| {
                    let (added, removed, hunks) = diff_stat(
                        fc.old_text.as_deref().unwrap_or(""),
                        fc.new_text.as_deref().unwrap_or(""),
                    );
                    AbsorbFileStat {
                        path: fc.path.clone(),
                        added,
                        removed,
                        hunks,
                    }
                })
                .collect();
            detail.insert(cid.clone(), files);
            rebuilt.insert(cid, MergedTreeBuilder::new(selected_tree));
        }

        // Order the plan ancestors-first. The domain streams newest-first
        // (children before parents), so a higher stream index is an older commit.
        // Scoped so the evaluated revset's borrow of `self.repo` ends here.
        let ordered: Vec<CommitId> = {
            let revset = destinations
                .clone()
                .evaluate(&*self.repo)
                .context("evaluating the absorb domain")?;
            block_on(revset.stream().collect::<Vec<_>>())
                .into_iter()
                .collect::<std::result::Result<_, _>>()
                .context("streaming the absorb domain")?
        };
        let rank: HashMap<&CommitId, usize> =
            ordered.iter().enumerate().map(|(i, c)| (c, i)).collect();
        let mut target_ids: Vec<CommitId> = detail.keys().cloned().collect();
        target_ids.sort_by_key(|c| std::cmp::Reverse(rank.get(c).copied().unwrap_or(usize::MAX)));

        let plan: Vec<AbsorbPlanEntry> = target_ids
            .iter()
            .map(|cid| {
                let commit = store.get_commit(cid).context("loading the absorb target")?;
                Ok(AbsorbPlanEntry {
                    target: CommitInfo::from_commit(&commit),
                    files: detail.get(cid).cloned().unwrap_or_default(),
                })
            })
            .collect::<Result<_>>()?;

        // Nothing could be attributed: no transaction, everything stays put.
        if rebuilt.is_empty() {
            return Ok(AbsorbOutcome {
                plan,
                skipped,
                remaining: true,
                applied: None,
            });
        }

        let name = self.workspace.workspace_name().to_owned();
        let pre_op = self.repo.operation().clone();
        let old_head = self.edited_tip();
        let heads = self.snapshot_heads();

        let mut tx = self.repo.start_transaction();
        block_on(absorb_hunks(tx.repo_mut(), &source, rebuilt)).context("absorbing hunks")?;
        // absorb_hunks rebases descendants of the targets it visits, but leaves
        // any straggler (and the parent_mapping) for a final rebase_descendants,
        // which jj's transaction commit requires be drained.
        block_on(tx.repo_mut().rebase_descendants()).context("rebasing descendants")?;

        // The rebuilt `@` (jj recreates a fresh empty one when the whole pile was
        // absorbed) sits on the new tip; its parent is the rewritten branch head,
        // and whether it still differs from that parent tells us if anything stayed
        // uncommitted.
        let (remaining, new_head) = {
            let wc = tx
                .repo()
                .view()
                .get_wc_commit_id(&name)
                .cloned()
                .and_then(|id| tx.repo().store().get_commit(&id).ok());
            match wc {
                Some(wc) => {
                    let parent_tree =
                        block_on(wc.parent_tree(tx.repo())).context("reading @'s parent tree")?;
                    let remaining =
                        wc.tree().tree_ids_and_labels() != parent_tree.tree_ids_and_labels();
                    (remaining, wc.parent_ids().first().cloned())
                }
                None => (false, None),
            }
        };
        if let Some(head) = new_head {
            self.set_head_bookmark(tx.repo_mut(), head);
        }

        if dry_run {
            // Drop the transaction: jj is only mutated on commit, so nothing lands.
            drop(tx);
            return Ok(AbsorbOutcome {
                plan,
                skipped,
                remaining,
                applied: None,
            });
        }

        let affected: Vec<String> = plan.iter().map(|e| e.target.change_id.hex()).collect();
        let desc = OpDescriptor::new(format!("Absorb into {} commit(s)", plan.len()), affected);
        let outcome = self.finish_mutation_auto_resolve(
            tx,
            "commedit: absorb working copy",
            desc,
            pre_op,
            old_head,
            heads,
        )?;
        Ok(AbsorbOutcome {
            plan,
            skipped,
            remaining,
            applied: Some(outcome),
        })
    }
}
