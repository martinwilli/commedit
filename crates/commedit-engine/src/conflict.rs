//! Conflict detection and resolution.
//!
//! A rewrite/reorder/abandon can leave jj-lib's `rebase_descendants` with
//! commits whose trees are *conflicted*. jj happily writes such commits, but its
//! git backend would serialize them as `.jjconflict-*` subtrees — garbage in a
//! real git history. So instead of exporting a conflicted chain we hold it back:
//! the rewrite is committed to jj's op log, but git refs / HEAD / the working
//! tree are left untouched (plain `git` keeps seeing the original clean history,
//! exactly like the keep-ref residue we already tolerate). The user then resolves
//! each conflicted commit's files in the UI, and only once the whole ancestor
//! chain of the branch tip is conflict-free do we perform the deferred export.
//!
//! This module owns that state machine. The mutation methods in [`crate::rewrite`]
//! / [`crate::tree`] all funnel their tail through [`Repo::finish_mutation`].

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use jj_lib::backend::{ChangeId, CommitId, CopyId, TreeValue};
use jj_lib::conflicts::{
    choose_materialized_conflict_marker_len, materialize_merge_result_to_bytes,
    materialize_tree_value, resolve_file_executable, update_from_content,
    ConflictMarkerStyle, ConflictMaterializeOptions, MaterializedTreeValue,
};
use jj_lib::merge::MergedTreeValue;
use jj_lib::merged_tree_builder::MergedTreeBuilder;
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::RefTarget;
use jj_lib::operation::Operation;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::{RepoPath, RepoPathBuf};
use jj_lib::transaction::Transaction;

use crate::repo::Repo;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    pollster::block_on(f)
}

/// Outcome of a mutation (or a resolution step): either the history is now
/// conflict-free and was exported to git, or one or more commits on the branch
/// tip's ancestor chain are conflicted and the rewrite is held pending in jj
/// while git stays untouched.
#[derive(Debug, Clone)]
pub enum SaveOutcome {
    /// Conflict-free: git refs, HEAD and the working tree were updated.
    Clean,
    /// Conflicted: nothing was exported. The engine now holds a pending
    /// resolution; drive it with [`Repo::read_conflict`] / [`Repo::resolve_conflict`]
    /// (or discard it with [`Repo::abort`]).
    Conflicts { commits: Vec<ConflictedCommit> },
}

/// A conflicted commit awaiting resolution.
#[derive(Debug, Clone)]
pub struct ConflictedCommit {
    /// Stable across the re-rewrites resolution causes — the UI keys on this.
    pub change_id: ChangeId,
    /// Current commit id; changes every time a resolution rebases this commit.
    pub commit_id: CommitId,
    pub subject: String,
    pub files: Vec<ConflictedPath>,
}

impl ConflictedCommit {
    pub fn change_id_hex(&self) -> String {
        self.change_id.hex()
    }
}

/// One conflicted path within a [`ConflictedCommit`].
#[derive(Debug, Clone)]
pub struct ConflictedPath {
    pub path: RepoPathBuf,
    /// Whether this is a plain file-content conflict that can be resolved by
    /// editing text. `false` for modify/delete-of-a-directory, symlink,
    /// submodule and other structural conflicts, which text editing can't fix
    /// (the only escape for those is [`Repo::abort`]).
    pub resolvable: bool,
}

impl ConflictedPath {
    pub fn path_str(&self) -> String {
        self.path.as_internal_file_string().to_string()
    }
}

/// A conflicted file materialized to Git-style conflict-marker text, ready to
/// show in the editor.
#[derive(Debug, Clone)]
pub struct ConflictedFile {
    /// The materialized content, with 2-way conflict markers
    /// (`<<<<<<<` / `=======` / `>>>>>>>`).
    pub text: String,
    /// The marker length jj used; [`Repo::resolve_conflict`] must echo it back so
    /// the resolved text parses against the same conflict shape.
    pub marker_len: usize,
    pub num_sides: usize,
}

/// The held-back state of a rewrite whose chain is conflicted, carried across
/// the per-file resolution steps until the chain goes clean (then exported) or
/// the user aborts (then rolled back).
pub(crate) struct PendingResolution {
    /// jj operation to roll back to on abort — the view from before the rewrite.
    pre_op: Operation,
    /// git tip from before the op, for the eventual `sync_worktree`.
    old_head: Option<String>,
    /// op-log message of the originating mutation (kept for reference).
    #[allow(dead_code)]
    op_msg: String,
    /// Pre-rewrite local bookmark targets, to hold unrelated branches in place
    /// at export time (see [`Repo::confine_bookmark_moves`]).
    bookmarks: Vec<(RefNameBuf, RefTarget)>,
    /// Pre-rewrite git branch heads, for the export-time backstop
    /// (see [`Repo::protect_unrelated_heads`]).
    heads: BTreeMap<String, String>,
    /// Conflicted commits, oldest first; re-derived after every resolution.
    conflicts: Vec<ConflictedCommit>,
}

impl Repo {
    /// Whether a conflicted rewrite is currently held pending resolution.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The conflicted commits of the pending rewrite (oldest first), or `None`
    /// when nothing is pending.
    pub fn pending_conflicts(&self) -> Option<&[ConflictedCommit]> {
        self.pending.as_ref().map(|p| p.conflicts.as_slice())
    }

    /// The branch tip as jj currently sees it — the head of the (possibly
    /// conflicted, not-yet-exported) rewritten chain. While a resolution is
    /// pending, git's HEAD still points at the pre-rewrite tip, so the UI uses
    /// this to walk and display the *new* history being resolved.
    pub fn jj_head_commit_id(&self) -> Option<CommitId> {
        self.current_head_in_jj()
    }

    /// Commit the rewrite transaction, then either export to git (if the branch
    /// tip's ancestor chain is conflict-free) or hold the rewrite pending while
    /// the conflicts are resolved. Every mutation ends here in place of the old
    /// inline export tail.
    pub(crate) fn finish_mutation(
        &mut self,
        tx: Transaction,
        op_msg: &str,
        pre_op: Operation,
        old_head: Option<String>,
        bookmarks: Vec<(RefNameBuf, RefTarget)>,
        heads: BTreeMap<String, String>,
    ) -> Result<SaveOutcome> {
        self.repo = block_on(tx.commit(op_msg)).context("committing rewrite")?;
        self.pending = Some(PendingResolution {
            pre_op,
            old_head,
            op_msg: op_msg.to_string(),
            bookmarks,
            heads,
            conflicts: Vec::new(),
        });
        self.settle()
    }

    /// Apply the user's edited conflict text for one `(change_id, path)`. A thin
    /// wrapper over [`Repo::resolve_conflicts`] for the single-file case.
    pub fn resolve_conflict(
        &mut self,
        change_hex: &str,
        path: &str,
        edited_text: &str,
        marker_len: usize,
    ) -> Result<SaveOutcome> {
        self.resolve_conflicts(
            change_hex,
            &[(path.to_string(), edited_text.to_string(), marker_len)],
        )
    }

    /// Apply the user's edited conflict text for several files of the commit with
    /// change id `change_hex` at once: parse each back into file ids, splice every
    /// result into the commit's tree in one rewrite, rebase descendants, and
    /// re-settle the chain. Resolving a commit's conflicted paths together is
    /// sound because they are independent — no intermediate re-materialization is
    /// needed between them. Structural (non-file) paths are skipped. Returns the
    /// refreshed outcome — `Clean` once the last conflict is gone (the rewrite is
    /// exported at that point), otherwise the remaining `Conflicts`. `files` is
    /// `(path, edited_text, marker_len)` tuples.
    pub fn resolve_conflicts(
        &mut self,
        change_hex: &str,
        files: &[(String, String, usize)],
    ) -> Result<SaveOutcome> {
        crate::repo::catch_jj("resolving the conflict", || {
            self.resolve_conflicts_inner(change_hex, files)
        })
    }

    fn resolve_conflicts_inner(
        &mut self,
        change_hex: &str,
        files: &[(String, String, usize)],
    ) -> Result<SaveOutcome> {
        if self.pending.is_none() {
            bail!("no conflict resolution in progress");
        }
        let store = self.repo.store().clone();
        let commit_id = self.resolve_change_on_chain(change_hex)?;
        let commit = store
            .get_commit(&commit_id)
            .context("loading conflicted commit")?;
        let tree = commit.tree();

        // Parse each file's resolved text into a tree value up front (while `tree`
        // is still borrowable), then splice them all into one builder.
        let mut entries: Vec<(RepoPathBuf, MergedTreeValue)> = Vec::with_capacity(files.len());
        for (path, edited_text, marker_len) in files {
            let path: &RepoPath = RepoPath::from_internal_string(path).context("invalid path")?;
            let value = block_on(tree.path_value(path)).context("reading conflicted path")?;
            let Some(file_ids) = value.to_file_merge() else {
                continue; // structural conflict — not text-resolvable, leave it
            };
            let exec = value
                .to_executable_merge()
                .as_ref()
                .and_then(resolve_file_executable)
                .unwrap_or(false);

            let new_ids = block_on(update_from_content(
                &file_ids,
                &store,
                path,
                edited_text.as_bytes(),
                *marker_len,
            ))
            .context("parsing resolved content")?;

            // Lift the resolved/again-conflicted file ids back into a tree value,
            // preserving the executable bit.
            let merged_value: MergedTreeValue = new_ids.map(|oid| {
                oid.as_ref().map(|id| TreeValue::File {
                    id: id.clone(),
                    executable: exec,
                    copy_id: CopyId::placeholder(),
                })
            });
            entries.push((path.to_owned(), merged_value));
        }

        let mut builder = MergedTreeBuilder::new(tree);
        for (path, merged_value) in entries {
            builder.set_or_remove(path, merged_value);
        }
        let new_tree = block_on(builder.write_tree()).context("writing resolved tree")?;

        let mut tx = self.repo.start_transaction();
        block_on(
            tx.repo_mut()
                .rewrite_commit(&commit)
                .set_tree(new_tree)
                .write(),
        )
        .context("writing resolved commit")?;
        block_on(tx.repo_mut().rebase_descendants()).context("rebasing descendants")?;
        self.repo = block_on(tx.commit("commedit: resolve conflict"))
            .context("committing resolution")?;

        self.settle()
    }

    /// Run the deferred export now if the chain is already clean, otherwise
    /// report the conflicts that still remain. Normally the last
    /// [`Self::resolve_conflict`] settles automatically; this is the explicit
    /// hook for a UI that wants to drive finalization itself.
    pub fn finalize(&mut self) -> Result<SaveOutcome> {
        if self.pending.is_none() {
            return Ok(SaveOutcome::Clean);
        }
        self.settle()
    }

    /// Discard a pending conflicted rewrite, rolling jj's view back to the
    /// operation before it. Git was never touched while pending, so the original
    /// history is intact; the conflicted commit objects are left as unreachable
    /// garbage (like keep-ref residue).
    ///
    /// The rollback is *recorded* as a new operation that restores the
    /// pre-rewrite view, rather than merely reloading the in-memory view at
    /// `pre_op`. A bare `reload_at` never advances the op log, so the discarded
    /// conflicted operation would linger as a second op head; the next edit then
    /// forks off the restored op, leaving two divergent heads that a later
    /// load-at-head merges straight back into the abandoned rewrite (the "old jj
    /// state" resurfacing). Committing a restore op makes the clean state the
    /// single head, mirroring jj's own `undo`/`op restore`.
    pub fn abort(&mut self) -> Result<()> {
        if let Some(p) = self.pending.take() {
            let view = block_on(p.pre_op.view()).context("reading the pre-rewrite view")?;
            let mut tx = self.repo.start_transaction();
            tx.repo_mut().set_view(view.store_view().clone());
            self.repo = block_on(tx.commit("commedit: abort rewrite"))
                .context("recording the abort")?;
        }
        Ok(())
    }

    /// Roll the entire session back to its starting point: restore jj's view to
    /// the operation captured at [`Repo::open`] (the original commits *and* the
    /// session-start working copy) and re-export it to git, so plain `git` sees
    /// the original history and the working tree is reset to its session-start
    /// content. Discards every rewrite/reorder/squash/drop and every
    /// working-copy edit made this session — the in-app equivalent of
    /// `git reset --hard <session head>`.
    ///
    /// Like [`Self::abort`], the restore is *recorded* as a new operation rather
    /// than a bare reload (see that method's note on why a divergent op head
    /// would otherwise resurface the old state). Unlike `abort`, clean saves
    /// during the session already moved git refs / HEAD / the worktree, so the
    /// restored state must be exported and materialized back to disk — hence the
    /// `export_and_sync` tail. Reverting drops any pending conflicted rewrite
    /// first (git was never touched for it).
    pub fn revert_all(&mut self) -> Result<()> {
        crate::repo::catch_jj("reverting the session", || self.revert_all_inner())
    }

    fn revert_all_inner(&mut self) -> Result<()> {
        let Some(session_op) = self.session_op.clone() else {
            return Ok(());
        };
        // Drop any held-back conflicted rewrite; git was never touched for it.
        self.pending = None;
        // The export tail needs the *current* (rewritten) on-disk state to sync
        // away from and the unrelated branches to hold in place.
        let old_head = self.head_commit();
        let bookmarks = self.local_bookmark_targets();
        let heads = self.snapshot_heads();
        // jj's recorded git-ref state tracks what it last wrote to git's
        // refs/*; the session's clean saves left it at the rewritten tips. Keep
        // a copy: `set_view` below rewinds this record to the session-start
        // values, but git's actual on-disk refs are still at the rewritten tips,
        // so the export would see no bookmark/ref diff and push nothing. We
        // re-stamp these afterwards so the export reconciles git with reality.
        let on_disk_git_refs: Vec<_> = self
            .repo
            .view()
            .git_refs()
            .iter()
            .map(|(name, target)| (name.clone(), target.clone()))
            .collect();
        // Restore the session-start view and record it as a new operation.
        let view = block_on(session_op.view()).context("reading the session-start view")?;
        let mut tx = self.repo.start_transaction();
        tx.repo_mut().set_view(view.store_view().clone());
        // Re-point the recorded git refs at what git actually holds on disk, so
        // the deferred export detects bookmark(session-start) != git-ref(current)
        // and pushes the restored tips back to git.
        for (name, target) in &on_disk_git_refs {
            tx.repo_mut().set_git_ref_target(name, target.clone());
        }
        self.repo = block_on(tx.commit("commedit: revert all to session start"))
            .context("recording the revert")?;
        // Push the restored state back to git and check the original working
        // copy back out to disk. The session-start state was a clean exported
        // git history, so the restored chain is always conflict-free.
        self.export_and_sync(old_head, &bookmarks, &heads)
    }

    /// Materialize one conflicted file of the commit with change id `change_id`
    /// to Git-style 2-way conflict-marker text, for display in the editor.
    pub fn read_conflict(&self, change_hex: &str, path: &str) -> Result<ConflictedFile> {
        let path: &RepoPath = RepoPath::from_internal_string(path).context("invalid path")?;
        let store = self.repo.store();
        let commit_id = self.resolve_change_on_chain(change_hex)?;
        let commit = store.get_commit(&commit_id).context("loading commit")?;
        let tree = commit.tree();
        let value = block_on(tree.path_value(path)).context("reading conflicted path")?;
        let mat = block_on(materialize_tree_value(store, path, value, tree.labels()))
            .context("materializing conflict")?;
        match mat {
            MaterializedTreeValue::FileConflict(fc) => {
                let marker_len = choose_materialized_conflict_marker_len(&fc.contents);
                let opts = ConflictMaterializeOptions {
                    marker_style: ConflictMarkerStyle::Git,
                    marker_len: Some(marker_len),
                    merge: store.merge_options().clone(),
                };
                let bytes = materialize_merge_result_to_bytes(&fc.contents, &fc.labels, &opts);
                let text = String::from_utf8(bytes.to_vec())
                    .context("conflicted file is not valid UTF-8")?;
                let text = strip_base_sections(&text, marker_len);
                let text = simplify_marker_labels(&text, marker_len);
                Ok(ConflictedFile {
                    text,
                    marker_len,
                    num_sides: fc.ids.num_sides(),
                })
            }
            MaterializedTreeValue::OtherConflict { .. } => {
                bail!("this conflict can't be resolved as text (structural conflict)")
            }
            _ => bail!("path is not conflicted"),
        }
    }

    /// Resolve `change_hex` to the commit carrying it on the *current* branch
    /// chain (the ancestors of jj's head — the same set [`Self::collect_conflicts`]
    /// walks). Conflict resolution always targets a commit on the pending
    /// rewritten chain, so scoping the lookup to that chain — rather than the
    /// store-wide `resolve_change_id` — disambiguates change ids that have
    /// divergent siblings left over from concurrent or earlier operations, which
    /// would otherwise make the global resolver bail as ambiguous.
    fn resolve_change_on_chain(&self, change_hex: &str) -> Result<CommitId> {
        let change_id = ChangeId::try_from_hex(change_hex).context("invalid change id")?;
        // The working-copy chain (@ and any split-off entries) sits above the
        // branch tip, so the ancestor walk below never sees it; match those
        // entries first.
        for wc_id in self.working_copy_chain_ids() {
            if let Ok(commit) = self.repo.store().get_commit(&wc_id) {
                if commit.change_id() == &change_id {
                    return Ok(wc_id);
                }
            }
        }
        let head = self
            .current_head_in_jj()
            .context("no current branch head to resolve the conflict against")?;
        let infos = crate::history::history(&self.repo, &head)?;
        infos
            .into_iter()
            .find(|i| i.change_id == change_id)
            .map(|i| i.id)
            .with_context(|| {
                format!("change {change_hex} is not on the current branch chain")
            })
    }

    /// The branch tip as jj currently sees it (read from the checked-out
    /// bookmark). `None` on a detached HEAD, where there is no branch to scope a
    /// conflict walk to.
    pub(crate) fn current_head_in_jj(&self) -> Option<CommitId> {
        let name = self.current_bookmark()?;
        self.repo
            .view()
            .get_local_bookmark(&name)
            .as_normal()
            .cloned()
    }

    /// Walk the ancestors of `head` (oldest first) collecting the commits whose
    /// trees are conflicted, with their conflicted paths.
    fn collect_conflicts(&self, head: Option<&CommitId>) -> Result<Vec<ConflictedCommit>> {
        let Some(head) = head else {
            return Ok(Vec::new());
        };
        let infos = crate::history::history(&self.repo, head)?;
        let store = self.repo.store();
        let mut out = Vec::new();
        for info in infos.iter().rev() {
            let commit = store.get_commit(&info.id).context("loading commit")?;
            if !commit.has_conflict() {
                continue;
            }
            let tree = commit.tree();
            let mut files = Vec::new();
            for (path, value) in tree.conflicts() {
                let value = value.context("reading conflict entry")?;
                files.push(ConflictedPath {
                    path,
                    resolvable: value.to_file_merge().is_some(),
                });
            }
            out.push(ConflictedCommit {
                change_id: info.change_id.clone(),
                commit_id: info.id.clone(),
                subject: info.subject.clone(),
                files,
            });
        }
        // The working-copy chain (@ and any split-off entries) is a *descendant*
        // of the tip, so the ancestor walk above never sees it. Append each
        // conflicted entry, oldest first (the chain is newest-first), so an
        // overlap between the user's uncommitted changes and the rewrite defers
        // the export and is resolved in the diff pane like any other commit.
        for wc_id in self.working_copy_chain_ids().into_iter().rev() {
            let commit = store
                .get_commit(&wc_id)
                .context("loading a working-copy chain commit")?;
            if !commit.has_conflict() {
                continue;
            }
            let tree = commit.tree();
            let mut files = Vec::new();
            for (path, value) in tree.conflicts() {
                let value = value.context("reading conflict entry")?;
                files.push(ConflictedPath {
                    path,
                    resolvable: value.to_file_merge().is_some(),
                });
            }
            out.push(ConflictedCommit {
                change_id: commit.change_id().clone(),
                commit_id: wc_id,
                subject: "Uncommitted changes".to_string(),
                files,
            });
        }
        Ok(out)
    }

    /// After committing a rewrite/resolution, decide whether the chain is clean
    /// (export and clear pending) or still conflicted (refresh pending).
    fn settle(&mut self) -> Result<SaveOutcome> {
        let head = self.current_head_in_jj();
        let conflicts = self.collect_conflicts(head.as_ref())?;
        if conflicts.is_empty() {
            let p = self.pending.take().expect("settle requires a pending resolution");
            self.export_and_sync(p.old_head, &p.bookmarks, &p.heads)?;
            Ok(SaveOutcome::Clean)
        } else {
            let p = self.pending.as_mut().expect("settle requires a pending resolution");
            p.conflicts = conflicts.clone();
            Ok(SaveOutcome::Conflicts { commits: conflicts })
        }
    }

    /// The deferred export: push the (now conflict-free) rewrite to git in its
    /// own transaction, then re-attach HEAD and sync the working tree — the
    /// transparency tail that used to run inline in each mutation.
    fn export_and_sync(
        &mut self,
        old_head: Option<String>,
        bookmarks: &[(RefNameBuf, RefTarget)],
        heads: &BTreeMap<String, String>,
    ) -> Result<()> {
        let mut tx = self.repo.start_transaction();
        self.confine_bookmark_moves(tx.repo_mut(), bookmarks);
        crate::transparency::export_to_git(tx.repo_mut())?;
        self.repo = block_on(tx.commit("commedit: export to git"))
            .context("committing export")?;
        self.reattach_head()?;
        self.protect_unrelated_heads(heads);
        // Write the rebased working-copy commit @' back to disk (preserving the
        // user's uncommitted changes through the rewrite), in place of the old
        // git read-tree sync.
        self.materialize_after_rewrite(old_head.clone())?;
        if let Some(old) = old_head {
            self.prune_orphaned_keep_refs(&old);
        }
        Ok(())
    }
}

/// Turn jj's Git diff3-style markers (which include a `|||||||` base section)
/// into plain Git 2-way markers by dropping each base section: everything from a
/// `|||||||…` line up to (but not including) the following `=======` line.
fn strip_base_sections(text: &str, marker_len: usize) -> String {
    let is_marker = |line: &str, ch: char| {
        let count = line.chars().take_while(|&c| c == ch).count();
        count >= marker_len
    };
    let mut out = String::new();
    let mut in_base = false;
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        if is_marker(body, '|') {
            in_base = true;
            continue;
        }
        if is_marker(body, '=') {
            in_base = false;
            out.push_str(line);
            continue;
        }
        if in_base {
            continue;
        }
        out.push_str(line);
    }
    out
}

/// Rewrite jj's verbose conflict-marker labels into something a plain-git user
/// recognizes. jj annotates each side with its change id, git commit id,
/// description and a role, e.g.
/// `<<<<<<< lywxrykm c2eece18 "foo" (rebase destination)`. We keep the git short
/// id and the description and drop the jj change id (meaningless without jj) and
/// the trailing role annotation, leaving `<<<<<<< c2eece18 "foo"`. Labels are
/// cosmetic — the round-trip parse keys on the marker run length, not the text
/// after it — so this only affects what the user reads.
fn simplify_marker_labels(text: &str, marker_len: usize) -> String {
    let marker_run = |line: &str, ch: char| {
        let n = line.chars().take_while(|&c| c == ch).count();
        (n >= marker_len).then_some(n)
    };
    let mut out = String::new();
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let marker = ['<', '=', '>']
            .into_iter()
            .find_map(|ch| marker_run(body, ch).map(|n| (ch, n)));
        match marker {
            // Marker chars are ASCII, so the run length is also the byte offset of
            // the label that follows it.
            Some((_, run)) => {
                let (prefix, rest) = body.split_at(run);
                out.push_str(prefix);
                let label = simplify_label(rest.trim());
                if !label.is_empty() {
                    out.push(' ');
                    out.push_str(&label);
                }
                if line.ends_with('\n') {
                    out.push('\n');
                }
            }
            None => out.push_str(line),
        }
    }
    out
}

/// Reduce one marker label to `<commit id> "<description>"`: drop the leading jj
/// change-id token and any trailing ` (…)` role annotation. Returns an empty
/// string for an empty label (e.g. the bare `=======` separator).
fn simplify_label(label: &str) -> String {
    if label.is_empty() {
        return String::new();
    }
    // Drop the leading jj change-id token (jj always emits it first).
    let rest = label
        .split_once(char::is_whitespace)
        .map(|(_, r)| r.trim())
        .unwrap_or("");
    // Drop a trailing " (role)" annotation; the last " (" can't fall inside the
    // quoted description, which closes with `"` before the annotation begins.
    match rest.rsplit_once(" (") {
        Some((core, _)) if rest.ends_with(')') => core.trim().to_string(),
        _ => rest.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::simplify_marker_labels;

    #[test]
    fn simplifies_jj_marker_labels() {
        let input = "\
<<<<<<< lywxrykm c2eece18 \"foo\" (rebase destination)
keep ours
=======
keep theirs
>>>>>>> mswnszso df01ec69 \"bar\" (rebased revision)
";
        let expected = "\
<<<<<<< c2eece18 \"foo\"
keep ours
=======
keep theirs
>>>>>>> df01ec69 \"bar\"
";
        assert_eq!(simplify_marker_labels(input, 7), expected);
    }

    #[test]
    fn handles_missing_description_and_bare_separator() {
        let input = "<<<<<<< abcdefgh 1234abcd (rebase destination)\n=======\n";
        let expected = "<<<<<<< 1234abcd\n=======\n";
        assert_eq!(simplify_marker_labels(input, 7), expected);
    }
}
