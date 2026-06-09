//! Open (or create) a colocated jj workspace and keep it in sync with git.
//!
//! jj-lib's mutating operations are async because the backend trait is async;
//! the git backend is synchronous under the hood, so we drive them to
//! completion with [`pollster::block_on`].

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use jj_lib::backend::{Backend, BackendInitError, CommitId};
use jj_lib::commit::Commit;
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::git::{self, GitImportOptions, GitRefKind, REMOTE_NAME_FOR_LOCAL_GIT_REPO};
use jj_lib::git_backend::GitBackend;
use jj_lib::local_working_copy::LocalWorkingCopyFactory;
use jj_lib::op_store::RefTarget;
use jj_lib::operation::Operation;
use jj_lib::ref_name::{RefNameBuf, RemoteRefSymbol, WorkspaceName};
use jj_lib::repo::{MutableRepo, ReadonlyRepo, Repo as _};
use jj_lib::settings::UserSettings;
use jj_lib::signing::Signer;
use jj_lib::working_copy::WorkingCopyFactory;
use jj_lib::workspace::Workspace;
use tempfile::TempDir;

/// An opened, colocated jj+git repository.
///
/// Holds the loaded [`Workspace`] together with the [`ReadonlyRepo`] at the
/// current operation head. Mutating flows replace `repo` with the repo produced
/// by the committed transaction.
pub struct Repo {
    pub workspace: Workspace,
    pub repo: Arc<ReadonlyRepo>,
    pub settings: UserSettings,
    /// The branch (full ref name) that was checked out when we opened the repo.
    /// HEAD is re-attached to it after every mutation so jj's detached-HEAD
    /// colocated layout stays invisible to plain git. `None` if HEAD was already
    /// detached.
    git_head_branch: Option<String>,
    /// A conflicted rewrite held back from git while the user resolves it (see
    /// [`crate::conflict`]). `None` in the normal, conflict-free state.
    pub(crate) pending: Option<crate::conflict::PendingResolution>,
    /// The jj operation captured at the end of [`Repo::open`] — the
    /// fully-initialized, pre-session state (working copy included). Cursor index
    /// 0 (the "Edit history" dropdown's floor / [`Repo::revert_all`]) rolls the
    /// whole session back to it. `None` only in the (unreachable) window before
    /// `open` finishes capturing it.
    pub(crate) session_op: Option<Operation>,
    /// The git HEAD commit (hex) as of session start, shown as a subtitle on the
    /// "Edit history" dropdown's session-start floor.
    session_head: Option<String>,
    /// The operations performed this session, oldest first — the snapshots the
    /// "Edit history" time-travel dropdown steps through (see
    /// [`crate::conflict::OpEntry`]). `session_op` is the implicit floor below
    /// index 0.
    pub(crate) session_ops: Vec<crate::conflict::OpEntry>,
    /// The live cursor over `session_ops`: `0` is the session-start floor,
    /// `session_ops.len()` the latest recorded state. Undo decrements, redo
    /// increments; a fresh edit truncates any tail above it.
    pub(crate) op_cursor: usize,
    /// The description of the in-flight mutation, set when `pending` is set and
    /// recorded as a session op once the rewrite settles clean (held here, rather
    /// than in `PendingResolution`, so the abort/conflict state object is
    /// unchanged). See [`Repo::record_op`].
    pub(crate) pending_op_desc: Option<crate::conflict::OpDescriptor>,
    /// The throwaway directory holding jj's metadata (repo store + working-copy
    /// state) for this session. It lives outside the user's repository so
    /// commedit leaves no `.jj` behind and never touches a real jj user's
    /// metadata; RAII deletes it when the `Repo` drops. Never read — held only
    /// to keep the directory alive for the session's lifetime.
    _workdir: TempDir,
}

impl Repo {
    /// Open the repository at `workspace_root`: spin up a fresh, throwaway jj
    /// workspace whose metadata lives in a temp dir (see [`Self::init_detached`]),
    /// then import git refs/HEAD so jj's view matches the git repository.
    ///
    /// commedit edits the history of an *existing* git repository; it never
    /// initializes one. A folder that is not already a git repo is refused here,
    /// so a stray path (or a directory mistaken for a repo) can't silently spawn a
    /// fresh repository.
    ///
    /// jj's metadata is **never** written into the user's repo: a real jj user's
    /// `.jj` is left untouched (commedit only reads/writes git objects + refs, as
    /// the transparency contract already requires), and a non-jj user's tree is
    /// not polluted. The temp workspace is discarded when the session ends, so no
    /// stale jj state survives between runs.
    pub fn open(workspace_root: &Path) -> Result<Self> {
        if !workspace_root.join(".git").exists() {
            anyhow::bail!(
                "{} is not a git repository — commedit edits the history of an \
                 existing git repo and will not create one",
                workspace_root.display()
            );
        }
        let settings = build_settings(workspace_root)?;
        // Record the checked-out branch before jj touches HEAD, so we can
        // re-attach to it afterwards.
        let git_head_branch = crate::transparency::head_branch(workspace_root);
        // Put jj's metadata in a throwaway temp dir rather than workspace_root/.jj.
        let workdir = tempfile::Builder::new()
            .prefix("commedit-")
            .tempdir()
            .context("creating temporary jj workspace")?;
        let (workspace, repo) = Self::init_detached(&settings, workspace_root, workdir.path())?;

        let mut this = Self {
            workspace,
            repo,
            settings,
            git_head_branch,
            pending: None,
            session_op: None,
            session_head: None,
            session_ops: Vec::new(),
            op_cursor: 0,
            pending_op_desc: None,
            _workdir: workdir,
        };
        this.import_git()?;
        this.reattach_head()?;
        // A freshly-initialized jj workspace has @ sitting on the empty root
        // commit; reattach it onto the just-imported git HEAD (a single @ on the
        // tip) before snapshotting, so the working copy is based on the real
        // history rather than nothing.
        this.collapse_working_copy_chain()?;
        // Record any uncommitted changes into @ so they show in the history and
        // ride through rewrites from the start.
        this.snapshot_working_copy()?;
        // Remember the fully-initialized session-start state (after the working
        // copy snapshot, so it includes the original uncommitted changes) so
        // `revert_all` can roll the whole session back to it.
        this.session_op = Some(this.repo.operation().clone());
        this.session_head = this.head_commit();
        Ok(this)
    }

    /// Initialize a fresh jj workspace whose metadata — the repo store and the
    /// working-copy state — lives under `state_dir`, entirely outside the user's
    /// repository, while the working copy still operates on `workspace_root` (the
    /// user's worktree).
    ///
    /// This is jj-lib's `Workspace::init_external_git` taken apart and reassembled
    /// with the state paths pointed at `state_dir` instead of the hardcoded
    /// `workspace_root/.jj`: the git backend attaches to the in-tree `.git` so jj
    /// and git share one object database, but nothing is written into the user's
    /// repo. jj-lib offers no high-level constructor that separates the checkout
    /// target from the state location, so we replicate the (small) bodies of
    /// `init_with_factories` + the private `init_working_copy` here using public
    /// primitives. If a future jj-lib bump changes that init shape, this is the
    /// one place to revisit.
    fn init_detached(
        settings: &UserSettings,
        workspace_root: &Path,
        state_dir: &Path,
    ) -> Result<(Workspace, Arc<ReadonlyRepo>)> {
        let repo_dir = state_dir.join("repo");
        std::fs::create_dir(&repo_dir).context("creating jj repo dir")?;
        let wc_state = state_dir.join("working_copy");
        std::fs::create_dir(&wc_state).context("creating jj working-copy state dir")?;

        // Absolute path to the user's .git. The store lives outside the
        // workspace, so (unlike init_external_git) there is no relative-path
        // optimization — GitBackend::init_external joins this onto the store path,
        // and an absolute path makes that join resolve to .git itself.
        let git_dir = std::fs::canonicalize(workspace_root.join(".git"))
            .context("locating the .git directory")?;
        let backend_initializer =
            |settings: &UserSettings, store_path: &Path| -> Result<Box<dyn Backend>, BackendInitError> {
                let backend = GitBackend::init_external(settings, store_path, &git_dir)?;
                Ok(Box::new(backend))
            };

        let repo = pollster::block_on(ReadonlyRepo::init(
            settings,
            &repo_dir,
            &backend_initializer,
            Signer::from_settings(settings)?,
            ReadonlyRepo::default_op_store_initializer(),
            ReadonlyRepo::default_op_heads_store_initializer(),
            ReadonlyRepo::default_index_store_initializer(),
            ReadonlyRepo::default_submodule_store_initializer(),
        ))
        .context("initializing the jj repo")?;

        // Check out the root commit, then attach a working copy whose checkout
        // target is the user's worktree but whose state lives in `state_dir`.
        // (`Repo::open` reattaches @ onto the imported git HEAD afterwards.)
        let mut tx = repo.start_transaction();
        pollster::block_on(
            tx.repo_mut()
                .check_out(WorkspaceName::DEFAULT.to_owned(), &repo.store().root_commit()),
        )
        .context("checking out the root commit")?;
        let repo = pollster::block_on(tx.commit("add workspace"))
            .context("committing the initial workspace")?;

        let working_copy = LocalWorkingCopyFactory {}
            .init_working_copy(
                repo.store().clone(),
                workspace_root.to_path_buf(),
                wc_state,
                repo.op_id().clone(),
                WorkspaceName::DEFAULT.to_owned(),
                settings,
            )
            .context("initializing the working copy")?;

        let workspace = Workspace::new(workspace_root, repo_dir, working_copy, repo.loader().clone())
            .context("assembling the jj workspace")?;
        Ok((workspace, repo))
    }

    /// Re-attach git HEAD to the originally checked-out branch, undoing jj's
    /// detached-HEAD colocated layout. No-op if HEAD was detached to begin with.
    pub(crate) fn reattach_head(&self) -> Result<()> {
        if let Some(branch) = &self.git_head_branch {
            crate::transparency::reattach_head(self.workspace.workspace_root(), branch)?;
        }
        Ok(())
    }

    /// The originally checked-out branch as a jj bookmark name (its
    /// `refs/heads/` prefix stripped), or `None` if HEAD was detached when the
    /// repo was opened.
    pub(crate) fn current_bookmark(&self) -> Option<RefNameBuf> {
        self.git_head_branch
            .as_ref()
            .map(|branch| branch.strip_prefix("refs/heads/").unwrap_or(branch).into())
    }

    /// Refuse a rewrite whose transaction leaves the checked-out branch's
    /// bookmark in a *conflicted* state — pointing at several commits at once.
    /// jj **cannot export a conflicted bookmark** to a single git ref:
    /// `diff_refs_to_export` silently *skips* it (it never lands in
    /// `GitExportStats::failed_bookmarks`, so the export tail can't notice). The
    /// rewrite would then commit on the jj side yet never reach git — the silent
    /// no-op that just piles up divergent commits.
    ///
    /// Checked against the *transaction's* post-rewrite view (`mut_repo`), so the
    /// test is purely on the outcome: a reorder/restore sets the head bookmark
    /// explicitly, resolving any pre-existing conflict, and passes; a
    /// message/identity/squash/split edit only relies on jj's automatic bookmark
    /// move, which can't collapse a conflict, so it stays conflicted and is
    /// refused here — before [`Self::finish_mutation`] commits the tx, so it is
    /// dropped untouched and nothing piles up. A bookmark is typically left
    /// conflicted because the git branch and its upstream diverged (jj's import
    /// merges the local and remote-tracking refs into one bookmark). No-op on a
    /// detached HEAD and on the normal resolved case.
    pub(crate) fn ensure_branch_exportable(&self, mut_repo: &MutableRepo) -> Result<()> {
        let Some(name) = self.current_bookmark() else {
            return Ok(());
        };
        if !mut_repo.get_local_bookmark(&name).has_conflict() {
            return Ok(());
        }
        let branch = self
            .git_head_branch
            .as_deref()
            .map(|b| b.strip_prefix("refs/heads/").unwrap_or(b))
            .unwrap_or("the current branch");
        anyhow::bail!(
            "branch '{branch}' is in a conflicted state in jj, so commedit can't \
             rewrite its history: jj cannot export a conflicted bookmark to a git \
             ref, so the edit would never reach git. This usually means the git \
             branch and its upstream have diverged (their refs merged into one \
             ambiguous bookmark on import). Reconcile the divergence first (merge \
             or rebase onto the remote, or `jj bookmark set {branch} -r <commit>`), \
             then reopen the repository."
        );
    }

    /// Point the originally checked-out branch at `target` inside `mut_repo`.
    /// Reordering can produce a new history head that is not a rewrite of the old
    /// head, so jj's automatic bookmark moves don't always follow; callers set it
    /// explicitly. No-op if HEAD was detached when the repo was opened.
    pub(crate) fn set_head_bookmark(&self, mut_repo: &mut MutableRepo, target: CommitId) {
        if let Some(name) = self.current_bookmark() {
            mut_repo.set_local_bookmark_target(&name, RefTarget::normal(target));
        }
    }

    /// Snapshot every local bookmark's target as of the current (pre-rewrite)
    /// repo view, to be handed to [`Self::confine_bookmark_moves`] after the
    /// rewrite so the unrelated ones can be held in place.
    pub(crate) fn local_bookmark_targets(&self) -> Vec<(RefNameBuf, RefTarget)> {
        self.repo
            .view()
            .local_bookmarks()
            .map(|(name, target)| (name.to_owned(), target.clone()))
            .collect()
    }

    /// Hold every local bookmark *except* the current branch at the target it
    /// had before the rewrite (`before`). jj moves every bookmark that pointed
    /// at a rewritten commit onto the rewrite, and `export_to_git` would then
    /// write all of them back to git — silently dragging unrelated branches
    /// (e.g. a user's backup branch that happened to share the rewritten tip)
    /// forward and clobbering them. Only the checked-out branch should follow
    /// our rewrite into git; the others must keep pointing at the now-old
    /// commits, which still exist in the object store. No-op on a detached HEAD,
    /// where there is no current branch to single out (behavior unchanged).
    pub(crate) fn confine_bookmark_moves(
        &self,
        mut_repo: &mut MutableRepo,
        before: &[(RefNameBuf, RefTarget)],
    ) {
        let Some(current) = self.current_bookmark() else {
            return;
        };
        for (name, target) in before {
            if *name == current {
                continue;
            }
            if mut_repo.get_local_bookmark(name) != *target {
                mut_repo.set_local_bookmark_target(name, target.clone());
            }
        }
    }

    /// The git commit HEAD currently points at — capture this before a rewrite
    /// so the working tree can be synced to the new tip afterwards.
    pub(crate) fn head_commit(&self) -> Option<String> {
        crate::transparency::head_commit(self.workspace.workspace_root())
    }

    /// The git HEAD commit (hex) captured at session start — the state
    /// [`Repo::revert_all`] restores. Shown in the UI's revert confirmation.
    pub fn session_start_head_hex(&self) -> Option<String> {
        self.session_head.clone()
    }

    /// The content delta of the whole session: the file changes between the tree
    /// the repo was opened with and the current tree. Identity/message-only
    /// edits don't touch any tree, so they don't appear here. Snapshots the
    /// working copy first so on-disk edits are included, then compares the
    /// working-copy commit `@` (the current tree, including uncommitted changes)
    /// against its session-start counterpart — falling back to the HEAD trees on
    /// a detached HEAD (no `@`). Powers the read-only "Review" view; empty right
    /// after [`Repo::revert_all`] restores the session-start state.
    pub fn session_changes(&mut self) -> Result<Vec<crate::diff::FileChange>> {
        let Some(session_op) = self.session_op.clone() else {
            return Ok(Vec::new());
        };
        // Fold any on-disk edits into @ so the review reflects the real tree.
        self.snapshot_working_copy()?;
        let store = self.repo.store().clone();

        // Current tree: prefer @ (includes uncommitted changes), else HEAD.
        let Some(new_id) = self
            .working_copy_commit_id()
            .or_else(|| self.head_commit_id())
        else {
            return Ok(Vec::new());
        };
        // Session-start tree: the @ recorded in the session-start view, or its
        // HEAD where there was none (detached HEAD).
        let view = pollster::block_on(session_op.view()).context("reading the session-start view")?;
        let old_id = view
            .get_wc_commit_id(self.workspace.workspace_name())
            .cloned()
            .or_else(|| self.session_head.as_deref().and_then(CommitId::try_from_hex));
        let Some(old_id) = old_id else {
            return Ok(Vec::new());
        };

        let new_tree = store
            .get_commit(&new_id)
            .context("loading the current commit")?
            .tree();
        let old_tree = store
            .get_commit(&old_id)
            .context("loading the session-start commit")?
            .tree();
        crate::diff::tree_changes(&store, &old_tree, &new_tree)
    }

    /// Snapshot every local branch (`refs/heads/*`) as git sees it now, to pair
    /// with [`Self::protect_unrelated_heads`] across a rewrite.
    pub(crate) fn snapshot_heads(&self) -> BTreeMap<String, String> {
        crate::transparency::local_head_oids(self.workspace.workspace_root())
    }

    /// Backstop the per-bookmark confinement at the git-ref level: restore any
    /// local branch other than the checked-out one to its pre-rewrite commit
    /// (`before`), reverting an unintended move the ref export may have made.
    /// Logs to stderr when it intervenes, so any remaining leak is visible
    /// rather than silently corrupting an unrelated (e.g. backup) branch.
    pub(crate) fn protect_unrelated_heads(&self, before: &BTreeMap<String, String>) {
        let restored = crate::transparency::restore_unrelated_heads(
            self.workspace.workspace_root(),
            self.git_head_branch.as_deref(),
            before,
        );
        if !restored.is_empty() {
            eprintln!(
                "commedit: reverted unintended move of branch(es) {}; \
                 only the current branch is rewritten",
                restored.join(", ")
            );
        }
    }

    /// HEAD as a [`CommitId`] — the tip of the branch being edited, used to scope
    /// reordering to the current branch's linear chain.
    pub fn head_commit_id(&self) -> Option<CommitId> {
        CommitId::try_from_hex(self.head_commit()?)
    }

    /// Scrub the `refs/jj/keep/*` GC-protection refs that this rewrite orphaned
    /// (see [`crate::transparency::prune_orphaned_keep_refs`]). `old_head` is the
    /// branch tip from before the operation. Best-effort cleanup: it runs after a
    /// mutation has already been committed and exported, so a failure here must not
    /// invalidate that successful rewrite — errors are intentionally swallowed.
    pub(crate) fn prune_orphaned_keep_refs(&self, old_head: &str) {
        // commedit's own jj working-copy commits are ours to drop: the user's
        // uncommitted changes are preserved by materializing @ to the working
        // tree, so neither @ nor its superseded snapshots need linger as keep-refs
        // (which would surface phantom working-copy commits in `git log --all`).
        let owned = self.owned_workingcopy_keep_refs();
        let _ = crate::transparency::prune_orphaned_keep_refs(
            self.workspace.workspace_root(),
            old_head,
            &owned,
        );
    }

    /// The `refs/jj/keep/*` oids that protect commedit's own working-copy
    /// commits — the current `@`, its superseded snapshots (sharing its change
    /// id), and jj's empty, description-less scaffolding commits. A manual jj
    /// user's anonymous head (real content and description, an unrelated change
    /// id) is deliberately excluded so its keep-ref survives.
    fn owned_workingcopy_keep_refs(&self) -> Vec<String> {
        let root = self.workspace.workspace_root();
        let wc_change = self
            .working_copy_commit_id()
            .and_then(|id| self.repo.store().get_commit(&id).ok())
            .map(|c| c.change_id().clone());
        crate::transparency::keep_ref_oids(root)
            .into_iter()
            .filter(|oid| {
                let Some(cid) = CommitId::try_from_hex(oid) else {
                    return false;
                };
                let Ok(commit) = self.repo.store().get_commit(&cid) else {
                    return false;
                };
                Some(commit.change_id()) == wc_change.as_ref()
                    || (commit.description().is_empty() && self.is_empty_commit(&commit))
            })
            .collect()
    }

    /// Whether `commit` carries no changes over its parent(s) — true for jj's
    /// empty working-copy scaffolding and a clean `@`.
    fn is_empty_commit(&self, commit: &Commit) -> bool {
        match pollster::block_on(commit.parent_tree(self.repo.as_ref())) {
            Ok(parent_tree) => {
                commit.tree().tree_ids_and_labels() == parent_tree.tree_ids_and_labels()
            }
            Err(_) => false,
        }
    }

    /// Update the working tree from the pre-rewrite tip (`old_head`) to the
    /// current HEAD, keeping `git status` clean without clobbering local edits.
    pub(crate) fn sync_worktree(&self, old_head: Option<String>) -> Result<()> {
        let root = self.workspace.workspace_root();
        if let (Some(old), Some(new)) = (old_head, crate::transparency::head_commit(root)) {
            crate::transparency::sync_worktree(root, &old, &new)?;
        }
        Ok(())
    }

    /// Pull git HEAD and the **checked-out branch's** local ref into jj's view as
    /// a single transaction. No-op (empty operation) when jj is already in sync
    /// with git.
    ///
    /// We import *only* the current branch, not every ref. commedit only ever
    /// displays and edits the ancestors of HEAD (see [`crate::history`]), so
    /// scoping the import to that one branch is all the view needs — and it keeps
    /// jj's commit index built over HEAD's ancestry rather than the whole ref
    /// graph. The deeper reason is correctness: a git ref that is never imported
    /// is invisible to jj's export (`diff_refs_to_export` only ever touches
    /// `local_bookmarks ∪ git_refs`, and `git_refs` records only imported refs),
    /// so sibling branches and tags are left exactly where they sit in git — the
    /// same outcome git's own `commit --amend`/rebase produce. The old
    /// import-everything path instead moved every bookmark that shared the
    /// rewritten tip and had to undo that by hand at export via
    /// [`Self::confine_bookmark_moves`].
    ///
    /// The filter admits only the local (`remote == "git"`) bookmark whose name is
    /// the checked-out branch; remote-tracking refs (`origin/*`) and tags are all
    /// excluded. Excluding the remote-tracking ref also sidesteps the diverged-
    /// upstream trap where jj merges the local and remote refs into one
    /// *conflicted* bookmark it can't export (still defended by
    /// [`Self::ensure_branch_exportable`]). On a detached HEAD the filter matches
    /// nothing, so only `import_head` runs — there is no branch to edit anyway.
    /// `record_synthetic_predecessors: false` keeps imported commits free of
    /// jj-only predecessor metadata.
    fn import_git(&mut self) -> Result<()> {
        let current = self.current_bookmark();
        let mut tx = self.repo.start_transaction();
        pollster::block_on(git::import_head(tx.repo_mut())).context("importing git HEAD")?;
        let options = GitImportOptions {
            abandon_unreachable_commits: false,
            record_synthetic_predecessors: false,
            remote_auto_track_bookmarks: HashMap::new(),
        };
        let git_ref_filter = |kind: GitRefKind, symbol: RemoteRefSymbol<'_>| {
            current.as_ref().is_some_and(|name| {
                kind == GitRefKind::Bookmark
                    && symbol.remote == REMOTE_NAME_FOR_LOCAL_GIT_REPO
                    && symbol.name == *name
            })
        };
        pollster::block_on(git::import_some_refs(tx.repo_mut(), &options, git_ref_filter))
            .context("importing the checked-out branch")?;
        pollster::block_on(tx.repo_mut().rebase_descendants()).context("rebasing after import")?;
        self.repo = pollster::block_on(tx.commit("import git refs")).context("committing import")?;
        Ok(())
    }
}

/// Run a closure that drives jj-lib, turning any panic it raises into an error.
///
/// jj-lib signals internal invariant violations with `panic!` rather than a
/// `Result` — e.g. "graph has cycle" when a rebase is asked to operate on a
/// corrupt/divergent operation graph (a repo whose bookmark is itself conflicted
/// and points at several divergent commits). Our mutations run inside GTK signal
/// and idle callbacks, whose surrounding frames are C (`nounwind`), so an
/// uncaught panic there aborts the whole process. Catching it here turns it into
/// an ordinary error the UI can report, keeping the session alive.
///
/// Safe to recover from because every mutation only replaces `self.repo` (and
/// sets `self.pending`) on its success path: a panic out of the jj call leaves
/// the in-memory repo exactly as it was, so the caught error is non-destructive.
pub(crate) fn catch_jj<T>(what: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            let detail = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            anyhow::bail!("{what} failed inside jj-lib ({detail}); the repository may have divergent or conflicted history")
        }
    }
}

/// Embedded baseline config (see `default_config.toml`). jj-lib ships no
/// defaults of its own; the jj CLI provides them, so we mirror them here.
const DEFAULT_CONFIG: &str = include_str!("default_config.toml");

/// Build a [`UserSettings`] from the embedded defaults plus the identity jj
/// should stamp on commits it rewrites. jj re-stamps the *committer* on every
/// rewrite (authors are carried over from the original commit), so we resolve it
/// with git's exact committer precedence: the `GIT_COMMITTER_*` environment
/// override wins, then the `committer.*` config key, then the generic `user.*`
/// key (each honouring the system/global/local git config hierarchy), and only
/// failing all three do we fall back to a generic commedit identity. Honouring
/// `committer.*` is what makes commedit and plain `git commit` agree on who
/// committed — a repo that sets `committer.email` no longer needs `user.email`
/// duplicated just for commedit's sake.
fn build_settings(workspace_root: &Path) -> Result<UserSettings> {
    let name = committer_field(workspace_root, "GIT_COMMITTER_NAME", "committer.name", "user.name")
        .unwrap_or_else(|| "commedit".to_string());
    let email = committer_field(
        workspace_root,
        "GIT_COMMITTER_EMAIL",
        "committer.email",
        "user.email",
    )
    .unwrap_or_else(|| "commedit@localhost".to_string());
    let identity = format!("[user]\nname = {name:?}\nemail = {email:?}\n");

    let mut config = StackedConfig::empty();
    config.add_layer(
        ConfigLayer::parse(ConfigSource::Default, DEFAULT_CONFIG).context("parsing defaults")?,
    );
    config.add_layer(
        ConfigLayer::parse(ConfigSource::User, &identity).context("parsing identity")?,
    );
    UserSettings::from_config(config).context("building user settings")
}

/// Resolve one committer identity field (name or email) the way git resolves the
/// committer: the `GIT_COMMITTER_*` environment override first, then the
/// `committer.*` config key, then the generic `user.*` key. Config lookups go
/// through git so they honour its system/global/local hierarchy.
fn committer_field(
    workspace_root: &Path,
    env_key: &str,
    committer_key: &str,
    user_key: &str,
) -> Option<String> {
    std::env::var(env_key)
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| crate::transparency::config_value(workspace_root, committer_key))
        .or_else(|| crate::transparency::config_value(workspace_root, user_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// jj-lib signals internal invariant violations with `panic!`; our mutations
    /// run inside GTK's C (`nounwind`) callback frames, where an uncaught panic
    /// aborts the whole process. [`catch_jj`] must turn such a panic into an
    /// ordinary `Err` so the session survives, and pass success through. The
    /// op-log divergence that used to provoke jj's "graph has cycle" panic across
    /// concurrent commedit sessions is now structurally impossible (each session
    /// gets an independent, throwaway jj workspace — see [`Repo::init_detached`]),
    /// but jj can still panic for other reasons, so this safety net stays.
    #[test]
    fn catch_jj_turns_a_panic_into_an_error() {
        // Silence the default hook's backtrace for the deliberate panic below.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let caught = catch_jj("testing", || -> Result<()> { panic!("graph has cycle") });
        let passed = catch_jj("testing", || Ok(7));
        std::panic::set_hook(prev);

        let err = caught.expect_err("a panic must surface as Err");
        assert!(err.to_string().contains("graph has cycle"), "{err}");
        assert!(err.to_string().contains("testing failed inside jj-lib"), "{err}");
        assert_eq!(passed.unwrap(), 7);
    }

    /// A *conflicted* checked-out-branch bookmark (pointing at several commits)
    /// can't be exported — jj silently skips it, so the edit would never reach
    /// git. [`Repo::ensure_branch_exportable`] must refuse it with a clear error.
    /// Reaching this state through commedit's own flow is no longer possible now
    /// that each session gets an isolated jj workspace (no shared op-log
    /// divergence — see [`Repo::init_detached`]), so we manufacture it directly to
    /// keep the guard covered.
    #[test]
    fn a_conflicted_branch_bookmark_is_refused() {
        use jj_lib::op_store::RefTarget;
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .current_dir(dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "T")
                .env("GIT_AUTHOR_EMAIL", "t@e")
                .env("GIT_COMMITTER_NAME", "T")
                .env("GIT_COMMITTER_EMAIL", "t@e")
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["-c", "init.defaultBranch=main", "init", "-q"]);
        std::fs::write(dir.join("f.txt"), "a\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "A"]);
        std::fs::write(dir.join("f.txt"), "b\n").unwrap();
        git(&["commit", "-q", "-am", "B"]);

        let repo = Repo::open(dir).expect("open");
        let head = repo.head_commit_id().expect("head");
        let commits = crate::history::history(&repo.repo, &head).expect("history");
        let (a, b) = (commits[0].id.clone(), commits[1].id.clone());

        // Manufacture a conflicted `main` bookmark (two divergent targets).
        let mut tx = repo.repo.start_transaction();
        let name = repo.current_bookmark().expect("on a branch");
        tx.repo_mut()
            .set_local_bookmark_target(&name, RefTarget::from_legacy_form([], [a, b]));
        assert!(tx.repo_mut().get_local_bookmark(&name).has_conflict());

        let err = repo
            .ensure_branch_exportable(tx.repo_mut())
            .expect_err("a conflicted branch bookmark must be refused");
        assert!(err.to_string().contains("conflicted"), "{err}");
    }
}
