//! Open (or create) a colocated jj workspace and keep it in sync with git.
//!
//! jj-lib's mutating operations are async because the backend trait is async;
//! the git backend is synchronous under the hood, so we drive them to
//! completion with [`pollster::block_on`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use jj_lib::backend::{Backend, BackendInitError, CommitId};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::git::{self, GitImportOptions, GitRefKind, REMOTE_NAME_FOR_LOCAL_GIT_REPO};
use jj_lib::git_backend::GitBackend;
use jj_lib::local_working_copy::LocalWorkingCopyFactory;
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::RefTarget;
use jj_lib::operation::Operation;
use jj_lib::ref_name::{RefNameBuf, RemoteRefSymbol, WorkspaceName, WorkspaceNameBuf};
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
    /// The **editable set**: the branches whose history this session may rewrite,
    /// imported as real jj bookmarks. A rewrite of any commit rebases descendants
    /// across the whole imported DAG and re-exports every bookmark that moved.
    ///
    /// The set's *primary* ([`EditableSet::primary`]) is the launch/opened branch;
    /// it equals [`Self::git_head_branch`] in the normal worktree-bound session and
    /// differs in *off-worktree* mode (the user opened a branch they have **not**
    /// checked out). A **1-element set** (primary only, no extras) reproduces the
    /// classic single-branch behavior byte-for-byte — this is what `commedit-mcp`
    /// and the classic GTK/CLI opens pass. The primary is `None` only on a detached
    /// HEAD with no branch selected. See [`Self::is_worktree_bound`].
    edited: EditableSet,
    /// The *extra* worktrees: every editable branch other than the launch one
    /// that is checked out in a git worktree, mapped onto its own jj workspace so
    /// a rewrite that moves that branch re-materializes *its* working copy and
    /// resets *its* index (full per-worktree symmetry — see [`WorktreeView`] and
    /// [`Repo::open_multi`]). The launch worktree is `self.workspace` (the jj
    /// `DEFAULT` workspace), never listed here; an editable branch with no
    /// worktree (`open_multi` couldn't map one) is a pure ref-move and absent too.
    /// Empty in the classic singleton/MCP path.
    pub(crate) extra_worktrees: Vec<WorktreeView>,
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
    /// metadata; RAII deletes it when the `Repo` drops. Its path is also where the
    /// index cache primes into and flushes from (and where
    /// [`Self::sync_to_git_head`] re-seeds the session git dir).
    _workdir: TempDir,
    /// The held index-cache slot for this session (a shared `flock` over the
    /// per-repo cache entry, plus its paths), or `None` when caching is disabled
    /// (the engine default / tests) or unavailable. [`Self::flush_index_cache`]
    /// persists the built index through it at close; [`Drop`] is the backstop.
    index_cache: Option<crate::index_cache::Handle>,
}

impl Drop for Repo {
    /// Backstop for [`Self::flush_index_cache`]: if a frontend didn't flush the
    /// index cache at clean shutdown, do it as the session's `Repo` drops. A no-op
    /// when caching is off or already flushed (the handle is taken on first flush).
    fn drop(&mut self) {
        self.flush_index_cache();
    }
}

/// The session git dir's location relative to the jj store dir
/// (`<state_dir>/repo/store` → `<state_dir>/git`), written into the git backend's
/// `git_target`. Storing it relative (not as the absolute `git_dir`) is what makes
/// the `repo/` tree relocatable: the index cache copies `repo/` into a different
/// session's `state_dir`, that session re-creates a fresh `git/` at the same
/// relative spot, and `git_target` still resolves. See [`Repo::load_detached`].
const RELATIVE_GIT_DIR: &str = "../../git";

/// The set of branches a session may rewrite — the "editable set". Stored as full
/// ref names (`refs/heads/…`). The [`Self::primary`] is the launch/opened branch
/// (the one the UI labels, HEAD re-attaches to, and the working copy tracks in
/// phase 1a); [`Self::extra`] are the additional branches folded into the editable
/// DAG. A set with no extras is a **singleton** and behaves exactly like the old
/// single-`target_branch` session.
///
/// Membership decides ancestor ride-along: an in-set bookmark follows a rewrite of
/// a commit it points at (its ref moves, descendants rebase); an out-of-set branch
/// is held in place by the protect-backstop. The primary is `None` only on a
/// detached HEAD with no branch argument, in which case the set is empty.
#[derive(Debug, Clone, Default)]
pub(crate) struct EditableSet {
    /// The launch/opened branch (full ref name), or `None` on a detached HEAD.
    primary: Option<String>,
    /// Additional editable branches (full ref names), excluding the primary and
    /// never duplicating it.
    extra: Vec<String>,
}

impl EditableSet {
    /// Every editable branch as a full ref name: the primary (if any) first, then
    /// the extras.
    fn refs(&self) -> impl Iterator<Item = &str> {
        self.primary
            .as_deref()
            .into_iter()
            .chain(self.extra.iter().map(String::as_str))
    }

    /// Whether `full` (a full ref name) is in the set.
    fn contains(&self, full: &str) -> bool {
        self.refs().any(|r| r == full)
    }
}

/// An editable branch that is checked out in a *separate* git worktree, mapped
/// onto its own jj workspace so commedit can keep that worktree in sync. Each
/// holds the jj [`Workspace`] anchored at the worktree's root (with its own
/// working-copy state and `@`), the full ref of the branch checked out there, and
/// the jj workspace name keying its `@` in the view. The launch worktree is *not*
/// one of these — it is the `Repo`'s primary [`Repo::workspace`] (the `DEFAULT`
/// jj workspace).
pub(crate) struct WorktreeView {
    /// The jj workspace anchored at the worktree's on-disk root, with its own
    /// working-copy state dir. Snapshotted before each mutation and re-checked-out
    /// after a rewrite that moves [`Self::branch`].
    pub(crate) workspace: Workspace,
    /// The full ref (`refs/heads/…`) of the branch checked out in this worktree —
    /// one of the editable set, never the launch branch.
    pub(crate) branch: String,
    /// The jj workspace name keying this worktree's `@` in the repo view
    /// (`get_wc_commit_id(name)`). Derived from [`Self::branch`]'s short name.
    pub(crate) name: WorkspaceNameBuf,
}

/// A local branch the multi-branch DAG view can fold in: its short-name, current
/// tip, whether it is the session's primary (launch/opened) branch, and whether it
/// is currently in the editable set (imported as a real bookmark). Produced by
/// [`Repo::local_branches`].
#[derive(Debug, Clone)]
pub struct BranchHead {
    pub name: String,
    pub head: CommitId,
    /// The session's primary (launch/opened) branch — the one whose working copy is
    /// tracked and whose name labels the UI.
    pub is_current: bool,
    /// Whether this branch is in the editable set right now: ticked in the dropdown,
    /// imported as a real jj bookmark, and a drop target. The primary is always
    /// editable; extras toggle via [`Repo::set_editable_branches`].
    pub is_editable: bool,
}

impl Repo {
    /// Open the repository at `workspace_root`: spin up a fresh, throwaway jj
    /// workspace whose metadata lives in a temp dir (see [`Self::init_detached`]),
    /// then import git refs/HEAD so jj's view matches the git repository.
    ///
    /// `workspace_root` may point *inside* a repository: like `git` itself,
    /// [`find_git_root`] walks up the directory hierarchy to the enclosing `.git`
    /// and opens that repository's root. commedit edits the history of an
    /// *existing* git repository; it never initializes one, so a path with no git
    /// repo above it is refused here rather than silently spawning a fresh
    /// repository.
    ///
    /// jj's metadata is **never** written into the user's repo: a real jj user's
    /// `.jj` is left untouched (commedit only reads/writes git objects + refs, as
    /// the transparency contract already requires), and a non-jj user's tree is
    /// not polluted. The temp workspace is discarded when the session ends, so no
    /// stale jj state survives between runs.
    pub fn open(workspace_root: &Path) -> Result<Self> {
        Self::open_with_cache(workspace_root, crate::index_cache::IndexCache::Disabled)
    }

    /// Like [`Self::open`], but with an index cache (see [`crate::index_cache`]) so
    /// repeated launches against the same repo skip rebuilding jj's commit index
    /// from scratch. The frontends (GTK, MCP) pass [`IndexCache::Default`]; the
    /// engine's plain [`Self::open`] and the tests pass [`IndexCache::Disabled`] so
    /// they never touch the user's real cache.
    ///
    /// When a valid cached entry exists it is primed into this session's temp dir
    /// and loaded ([`Self::load_detached`]); otherwise — or if the primed load
    /// fails for any reason — it falls back to a clean cold [`Self::init_detached`],
    /// discarding the bad entry. Caching only ever makes the open faster, never
    /// fail. Either way the session holds a shared lock on the cache entry and
    /// flushes its (now up-to-date) index back at close.
    pub fn open_with_cache(
        workspace_root: &Path,
        cache: crate::index_cache::IndexCache,
    ) -> Result<Self> {
        Self::open_branch(workspace_root, cache, None)
    }

    /// Like [`Self::open_with_cache`], but edits the history of `branch` (a local
    /// branch name) rather than the branch checked out in the worktree. When
    /// `branch` is `None`, or names the checked-out branch, this is the ordinary
    /// worktree-bound session. Otherwise it is an *off-worktree* session: the
    /// session imports, rewrites and exports only that branch's ref, leaving HEAD,
    /// the index and the on-disk worktree frozen — so there is no working copy
    /// (working-copy operations are refused, see [`Self::is_worktree_bound`]).
    ///
    /// Refused when `branch` does not exist, or is checked out in *another*
    /// worktree of this repo (rewriting it there would desync that worktree).
    pub fn open_branch(
        workspace_root: &Path,
        cache: crate::index_cache::IndexCache,
        branch: Option<&str>,
    ) -> Result<Self> {
        let branches: Vec<String> = branch.map(str::to_string).into_iter().collect();
        Self::open_multi(workspace_root, cache, &branches)
    }

    /// Like [`Self::open_branch`], but edits a *set* of branches (the "editable
    /// set"): all of them are imported as real jj bookmarks, so rewriting any
    /// commit rebases descendants across the whole imported DAG and re-exports
    /// every bookmark that moved. The **first** entry is the primary (the launch
    /// branch the working copy tracks in phase 1a); an empty slice opens the
    /// checked-out branch (the classic worktree-bound open). A 1-element slice is
    /// byte-identical to [`Self::open_branch`] with that branch.
    ///
    /// Each branch is resolved to a full ref and verified to exist; an off-worktree
    /// branch (one not checked out here) live in *another* worktree is refused, as
    /// moving its ref would orphan that checkout (phase 1b lifts this for branches
    /// we can re-materialize).
    pub fn open_multi(
        workspace_root: &Path,
        cache: crate::index_cache::IndexCache,
        branches: &[String],
    ) -> Result<Self> {
        // Resolve a path inside the repo to the repository root that encloses it
        // (walking up to `.git`); bails if there is no git repo above it.
        let workspace_root = find_git_root(workspace_root)?;
        let workspace_root = workspace_root.as_path();
        let settings = build_settings(workspace_root)?;
        // Record the checked-out branch before jj touches HEAD, so we can
        // re-attach to it afterwards.
        let git_head_branch = crate::transparency::head_branch(workspace_root);
        // The editable set. The primary is the first requested branch (resolved to
        // a full ref, verified to exist), else the checked-out branch; the extras
        // are the remaining requested branches, deduplicated and excluding the
        // primary.
        let edited = {
            let mut resolved: Vec<String> = Vec::new();
            for name in branches {
                let full = crate::transparency::resolve_local_branch(workspace_root, name)?;
                if !resolved.contains(&full) {
                    resolved.push(full);
                }
            }
            let mut it = resolved.into_iter();
            let primary = it.next().or_else(|| git_head_branch.clone());
            EditableSet {
                primary,
                extra: it.collect(),
            }
        };
        // Map every editable branch onto the git worktree it is checked out in (if
        // any) that is *not* the launch worktree. Phase 1b registers a jj workspace
        // per such worktree so a rewrite that moves the branch re-materializes
        // *its* working copy and resets *its* index — so a branch live in another
        // worktree is now editable (it was refused before per-worktree sync
        // existed). The launch worktree's own branch (worktree-bound) is handled by
        // the working-copy path, not here; a branch with no worktree stays a pure
        // ref-move (no entry). Keyed on the worktree *path*, not the branch name, so
        // even the primary branch — when it lives in a worktree other than the
        // launch one (off-worktree open) — is kept in sync.
        let launch_root = std::fs::canonicalize(workspace_root).unwrap_or(workspace_root.into());
        let worktree_map = crate::transparency::worktrees(workspace_root)?;
        let extra_targets: Vec<(String, PathBuf)> = edited
            .refs()
            .filter_map(|full| {
                worktree_map
                    .iter()
                    .find(|(_, b)| b.as_deref() == Some(full))
                    .map(|(path, _)| (full.to_string(), path.clone()))
            })
            .filter(|(_, path)| {
                std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()) != launch_root
            })
            .collect();

        // Acquire the cache slot (a shared lock held for the session) and run
        // opportunistic eviction while the base is resolved.
        let cache_handle = crate::index_cache::resolve_base(cache).and_then(|base| {
            let objects = crate::transparency::git_objects_dir(workspace_root).ok()?;
            let handle = crate::index_cache::acquire(&base, &objects);
            crate::index_cache::sweep(&base);
            handle
        });

        let new_workdir = || -> Result<TempDir> {
            tempfile::Builder::new()
                .prefix("commedit-")
                .tempdir()
                .context("creating temporary jj workspace")
        };

        // Prime + load from the cache when a valid entry exists; else cold-init.
        // On any failure of the primed path, discard the entry and cold-init in a
        // fresh workdir — the cache must never break an open.
        let (workdir, workspace, repo) = match &cache_handle {
            Some(handle) if handle.valid => {
                let workdir = new_workdir()?;
                let primed = handle.prime(workdir.path()).and_then(|()| {
                    Self::load_detached(&settings, workspace_root, workdir.path(), &edited)
                });
                match primed {
                    Ok((workspace, repo)) => (workdir, workspace, repo),
                    Err(e) => {
                        eprintln!("commedit: index cache unusable ({e}); rebuilding from scratch");
                        handle.invalidate();
                        let workdir = new_workdir()?;
                        let (workspace, repo) = Self::init_detached(
                            &settings,
                            workspace_root,
                            workdir.path(),
                            &edited,
                        )?;
                        (workdir, workspace, repo)
                    }
                }
            }
            _ => {
                let workdir = new_workdir()?;
                let (workspace, repo) =
                    Self::init_detached(&settings, workspace_root, workdir.path(), &edited)?;
                (workdir, workspace, repo)
            }
        };

        let mut this = Self {
            workspace,
            repo,
            settings,
            git_head_branch,
            edited,
            extra_worktrees: Vec::new(),
            pending: None,
            session_op: None,
            session_head: None,
            session_ops: Vec::new(),
            op_cursor: 0,
            pending_op_desc: None,
            _workdir: workdir,
            index_cache: cache_handle,
        };
        this.import_git()?;
        // The worktree-bound tail only makes sense when the edited branch *is* the
        // checked-out one. Off-worktree there is no working copy to anchor and the
        // user's HEAD must stay put, so skip re-attaching HEAD, collapsing the @
        // chain, and snapshotting the disk — the session edits commits only.
        if this.is_worktree_bound() {
            this.reattach_head()?;
            // A freshly-initialized jj workspace has @ sitting on the empty root
            // commit; reattach it onto the just-imported git HEAD (a single @ on
            // the tip) before snapshotting, so the working copy is based on the
            // real history rather than nothing.
            this.collapse_working_copy_chain()?;
            // Record any uncommitted changes into @ so they show in the history and
            // ride through rewrites from the start.
            this.snapshot_working_copy()?;
        }
        // Register each extra editable branch's worktree as its own jj workspace,
        // anchoring an `@` on the branch tip and snapshotting that worktree's disk
        // — so its uncommitted changes ride through a later rewrite exactly like the
        // launch worktree's. Done after import so the branch tip is in jj's view.
        for (branch, path) in extra_targets {
            this.register_worktree(&branch, &path)?;
        }
        // Remember the fully-initialized session-start state (after the working
        // copy snapshot, so it includes the original uncommitted changes) so
        // `revert_all` can roll the whole session back to it.
        this.session_op = Some(this.repo.operation().clone());
        this.session_head = this.edited_tip();
        Ok(this)
    }

    /// Persist this session's built jj index back into the index cache, so the next
    /// launch against this repo primes from it instead of rebuilding from scratch
    /// (see [`crate::index_cache`]). Best-effort and **non-blocking**: it only
    /// writes when this is the last view on the cache entry, and is a no-op when
    /// caching is disabled/unavailable. The frontends call this at clean shutdown;
    /// [`Drop`] is the backstop. The handle is taken on the first call, so a later
    /// flush (e.g. from `Drop` after an explicit flush) does nothing.
    pub fn flush_index_cache(&mut self) {
        if let Some(handle) = self.index_cache.take() {
            handle.flush(self._workdir.path());
        }
    }

    /// Initialize a fresh jj workspace whose metadata — the repo store and the
    /// working-copy state — lives under `state_dir`, entirely outside the user's
    /// repository, while the working copy still operates on `workspace_root` (the
    /// user's worktree).
    ///
    /// This is jj-lib's `Workspace::init_external_git` taken apart and reassembled
    /// with the state paths pointed at `state_dir` instead of the hardcoded
    /// `workspace_root/.jj`. jj-lib offers no high-level constructor that
    /// separates the checkout target from the state location, so we replicate the
    /// (small) bodies of `init_with_factories` + the private `init_working_copy`
    /// here using public primitives. If a future jj-lib bump changes that init
    /// shape, this is the one place to revisit.
    ///
    /// The git backend attaches not to the user's `.git` but to a session-local,
    /// throwaway git dir (under `state_dir`) whose object store is *shared* with
    /// the user's repo (see [`crate::transparency::init_shared_git_dir`]). So jj
    /// and git share one object database — rewritten commits land in the user's
    /// ODB, keeping plain `git` able to see them — while every ref jj writes (its
    /// `refs/jj/keep/*` GC anchors, its detached HEAD, the bookmark export) stays
    /// in the throwaway dir, never the user's `.git`. The one branch ref jj moves
    /// is mirrored out by [`Self::bridge_branch_to_git`].
    fn init_detached(
        settings: &UserSettings,
        workspace_root: &Path,
        state_dir: &Path,
        edited: &EditableSet,
    ) -> Result<(Workspace, Arc<ReadonlyRepo>)> {
        let repo_dir = state_dir.join("repo");
        std::fs::create_dir(&repo_dir).context("creating jj repo dir")?;
        let wc_state = state_dir.join("working_copy");
        std::fs::create_dir(&wc_state).context("creating jj working-copy state dir")?;

        // The git dir jj writes into: session-local, with an object store shared
        // with the user's repo but private refs.
        let git_dir = state_dir.join("git");
        crate::transparency::init_shared_git_dir(
            &git_dir,
            workspace_root,
            edited.primary.as_deref(),
            &edited.extra,
        )
        .context("setting up the session git dir")?;
        let backend_initializer = |settings: &UserSettings,
                                   store_path: &Path|
         -> Result<Box<dyn Backend>, BackendInitError> {
            // Record the git dir as a path *relative* to the store dir
            // (`<state_dir>/repo/store` → `<state_dir>/git`) rather than the
            // absolute `git_dir`. This is what makes the `repo/` tree
            // relocatable: the index cache copies it to a different session's
            // `state_dir` and that session re-creates `git/` at the same
            // relative spot (see [`Self::load_detached`]), so `git_target` still
            // resolves. jj joins it onto `store_path` and canonicalizes.
            let backend =
                GitBackend::init_external(settings, store_path, Path::new(RELATIVE_GIT_DIR))?;
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
        pollster::block_on(tx.repo_mut().check_out(
            WorkspaceName::DEFAULT.to_owned(),
            &repo.store().root_commit(),
        ))
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

        let workspace = Workspace::new(
            workspace_root,
            repo_dir,
            working_copy,
            repo.loader().clone(),
        )
        .context("assembling the jj workspace")?;
        Ok((workspace, repo))
    }

    /// Load a jj workspace from a `state_dir` whose `repo/` tree was *primed* from
    /// the index cache (a recursive copy of a previous session's `repo/`, holding
    /// its already-built commit index — see [`crate::index_cache`]). The
    /// counterpart to [`Self::init_detached`]: instead of initializing an empty jj
    /// repo and indexing HEAD's whole ancestry from scratch, it loads the persisted
    /// index, so the following [`Self::import_git`] only has to index the commits
    /// added since the cache was written (usually none to a handful) — turning a
    /// ~30s cold open of a huge history into a ~1s incremental one.
    ///
    /// The `repo/` tree is already in place; this re-creates the session-local bits
    /// that are *not* cached and must be fresh per session: the shared git dir
    /// (objects symlinked to the user's ODB, HEAD seeded) at the relative location
    /// `repo/store/git_target` points at, and an empty working-copy state. The open
    /// tail (`reattach`/`collapse`/`snapshot`) then reconciles `@` onto the user's
    /// live HEAD exactly as on a cold open, so the rest of the session is identical.
    ///
    /// Fallible by design: a corrupt/partial cache, an index referencing an object
    /// the user has since GC'd, or a jj-lib on-disk format change all surface here
    /// as an error, and [`Self::open`] falls back to a clean [`Self::init_detached`]
    /// after discarding the bad entry. Like `init_detached`, this leans on jj-lib's
    /// lower-level loader primitives, so a jj-lib bump may need it revisited.
    fn load_detached(
        settings: &UserSettings,
        workspace_root: &Path,
        state_dir: &Path,
        edited: &EditableSet,
    ) -> Result<(Workspace, Arc<ReadonlyRepo>)> {
        use jj_lib::repo::{RepoLoader, StoreFactories};

        let repo_dir = state_dir.join("repo"); // primed by the caller
        let wc_state = state_dir.join("working_copy");
        std::fs::create_dir(&wc_state).context("creating jj working-copy state dir")?;

        // Fresh session-local git dir; the primed `repo/store/git_target` is the
        // relative `RELATIVE_GIT_DIR`, so it resolves to this newly-created dir.
        let git_dir = state_dir.join("git");
        crate::transparency::init_shared_git_dir(
            &git_dir,
            workspace_root,
            edited.primary.as_deref(),
            &edited.extra,
        )
        .context("setting up the session git dir")?;

        let loader =
            RepoLoader::init_from_file_system(settings, &repo_dir, &StoreFactories::default())
                .context("loading the primed jj repo")?;
        let repo = pollster::block_on(loader.load_at_head())
            .context("loading the primed jj repo at head")?;

        // Attach a *fresh* working copy at the loaded head op; `Repo::open`'s
        // collapse + snapshot re-anchor `@` onto the user's current HEAD, so the
        // primed view's stale `@`/bookmark don't matter.
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

        let workspace = Workspace::new(
            workspace_root,
            repo_dir,
            working_copy,
            repo.loader().clone(),
        )
        .context("assembling the jj workspace")?;
        Ok((workspace, repo))
    }

    /// Register an editable branch's git worktree as a *second* jj workspace, so
    /// commedit keeps that worktree in sync the same way it keeps the launch one.
    /// `branch` (full ref) is checked out at `worktree_root` on disk; the branch is
    /// already imported into jj's view (`open_multi` registers after `import_git`).
    ///
    /// It mints a fresh jj workspace name (from the branch short-name) and a
    /// per-worktree working-copy state dir under this session's temp workdir, anchors
    /// a fresh `@` on the branch's imported tip in the repo view (mirroring the launch
    /// worktree's open-time reattach), then snapshots that worktree's disk into its
    /// `@` so its uncommitted changes are tracked from the start. The new
    /// [`WorktreeView`] is recorded in [`Self::extra_worktrees`]; a [`Self::workspace`]
    /// remains the launch worktree's `DEFAULT` workspace, untouched.
    fn register_worktree(&mut self, branch: &str, worktree_root: &Path) -> Result<()> {
        let short = branch.strip_prefix("refs/heads/").unwrap_or(branch);
        // The jj workspace name keys this worktree's `@` in the view. Branch short-
        // names are unique among local branches, so this never collides with another
        // worktree's name (nor with the `DEFAULT` launch workspace).
        let name: WorkspaceNameBuf = short.into();
        // A private working-copy state dir for this worktree, alongside the launch
        // worktree's `working_copy/` under the session temp workdir.
        let wc_state = self._workdir.path().join(format!("working_copy-{short}"));
        std::fs::create_dir_all(&wc_state)
            .with_context(|| format!("creating working-copy state dir for worktree '{short}'"))?;

        let working_copy = LocalWorkingCopyFactory {}
            .init_working_copy(
                self.repo.store().clone(),
                worktree_root.to_path_buf(),
                wc_state,
                self.repo.op_id().clone(),
                name.clone(),
                &self.settings,
            )
            .with_context(|| format!("initializing the working copy for worktree '{short}'"))?;
        let workspace = Workspace::new(
            worktree_root,
            self.workspace.repo_path().to_path_buf(),
            working_copy,
            self.repo.loader().clone(),
        )
        .with_context(|| format!("assembling the jj workspace for worktree '{short}'"))?;

        let mut view = WorktreeView {
            workspace,
            branch: branch.to_string(),
            name: name.clone(),
        };
        // Anchor a fresh `@` on the branch's imported tip, so the following snapshot
        // records only that worktree's uncommitted delta (not its whole history).
        // The bookmark is keyed by the branch short-name (a ref name), distinct from
        // the workspace name (which keys the `@` in the view).
        let bookmark: RefNameBuf = short.into();
        if let Some(tip) = self
            .repo
            .view()
            .get_local_bookmark(&bookmark)
            .as_normal()
            .cloned()
        {
            let commit = self
                .repo
                .store()
                .get_commit(&tip)
                .context("loading the worktree branch tip")?;
            let mut tx = self.repo.start_transaction();
            pollster::block_on(tx.repo_mut().check_out(name, &commit))
                .context("anchoring the worktree working copy on its branch tip")?;
            pollster::block_on(tx.repo_mut().rebase_descendants())
                .context("rebasing after attach")?;
            self.repo = pollster::block_on(tx.commit("commedit: attach worktree working copy"))
                .context("committing the worktree attach")?;
        }
        // Snapshot the worktree's disk into its `@` (uncommitted changes ride along).
        self.snapshot_extra_worktree(&mut view)?;
        self.extra_worktrees.push(view);
        Ok(())
    }

    /// Re-attach git HEAD to the originally checked-out branch, undoing jj's
    /// detached-HEAD colocated layout. No-op if HEAD was detached to begin with.
    pub(crate) fn reattach_head(&self) -> Result<()> {
        if let Some(branch) = &self.git_head_branch {
            crate::transparency::reattach_head(self.workspace.workspace_root(), branch)?;
        }
        Ok(())
    }

    /// The branch this session edits as a jj bookmark name (its `refs/heads/`
    /// prefix stripped), or `None` on a detached HEAD with no branch selected.
    /// This is the bookmark imported, rewritten and exported — the checked-out
    /// branch in the normal session, a different branch when editing off-worktree.
    pub(crate) fn current_bookmark(&self) -> Option<RefNameBuf> {
        self.edited
            .primary
            .as_ref()
            .map(|branch| branch.strip_prefix("refs/heads/").unwrap_or(branch).into())
    }

    /// Whether this session edits the branch checked out in the worktree — so the
    /// working copy, HEAD and git index participate in every rewrite. `false` in
    /// *off-worktree* mode (the user opened a branch they have not checked out),
    /// where only the edited branch's ref moves and HEAD/index/worktree stay
    /// frozen, and there is consequently no working copy. A detached-HEAD session
    /// with no branch argument is worktree-bound (`None == None`).
    pub fn is_worktree_bound(&self) -> bool {
        self.edited.primary == self.git_head_branch
    }

    /// The edited branch's short name (its `refs/heads/` prefix stripped), or
    /// `None` on a detached HEAD with no branch selected. For UI/MCP labelling.
    pub fn target_branch_name(&self) -> Option<&str> {
        self.edited
            .primary
            .as_deref()
            .map(|b| b.strip_prefix("refs/heads/").unwrap_or(b))
    }

    /// The extra worktree whose checked-out branch is `branch` (short-name), if
    /// any — the lookup that routes a working-copy mutation or a spurious-`@`
    /// rebuild to the right [`WorktreeView`]. The launch worktree is *not* among
    /// these (it is `self.workspace`); a branch with no separate worktree (a pure
    /// ref-move, or the off-worktree primary) returns `None`.
    pub(crate) fn find_worktree(&self, branch: &str) -> Option<&WorktreeView> {
        self.extra_worktrees
            .iter()
            .find(|v| v.branch.strip_prefix("refs/heads/").unwrap_or(&v.branch) == branch)
    }

    /// Refuse a working-copy operation when editing off-worktree: a branch you
    /// have not checked out has no working copy, so committing/squashing/splitting
    /// /discarding uncommitted changes is meaningless. `op` names the action for
    /// the message (e.g. "commit the working copy").
    pub(crate) fn require_worktree(&self, op: &str) -> Result<()> {
        if !self.is_worktree_bound() {
            let branch = self.target_branch_name().unwrap_or("the selected branch");
            anyhow::bail!(
                "branch '{branch}' is not checked out, so it has no working copy; \
                 cannot {op}. Check out the branch (or open commedit without a branch \
                 argument) to edit the working copy."
            );
        }
        Ok(())
    }

    /// The tip commit (hex) of the branch being edited: the target branch's ref
    /// tip when off-worktree, else git HEAD. This is the pre-rewrite
    /// compare-and-swap precondition passed as `old_head` into the mutation tail,
    /// and the basis for [`Self::head_commit_id`]. Tracks the ref as it moves: a
    /// clean save advances the edited branch (HEAD when bound, the target ref
    /// off-worktree), so a later read sees the new tip.
    pub(crate) fn edited_tip(&self) -> Option<String> {
        if self.is_worktree_bound() {
            self.head_commit()
        } else {
            self.edited
                .primary
                .as_deref()
                .and_then(|b| crate::transparency::ref_commit(self.workspace.workspace_root(), b))
        }
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
        let branch = self.target_branch_name().unwrap_or("the current branch");
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

    /// Point an arbitrary local bookmark (`branch` short-name) at `target` inside
    /// `mut_repo` — the multi-head generalization of [`Self::set_head_bookmark`],
    /// used by the spurious-conflict rebuild to re-point every rebuilt editable
    /// branch, not just the primary.
    pub(crate) fn set_branch_bookmark(
        &self,
        mut_repo: &mut MutableRepo,
        branch: &str,
        target: CommitId,
    ) {
        let name: RefNameBuf = branch.into();
        mut_repo.set_local_bookmark_target(&name, RefTarget::normal(target));
    }

    /// The git commit HEAD currently points at — capture this before a rewrite
    /// so the working tree can be synced to the new tip afterwards.
    pub(crate) fn head_commit(&self) -> Option<String> {
        crate::transparency::head_commit(self.workspace.workspace_root())
    }

    /// The repository root (the user's worktree). Relative file paths from a
    /// commit's diff are resolved against it — e.g. to read the repo's
    /// editor-config files (see [`crate::tabwidth`]).
    pub fn workspace_root(&self) -> &Path {
        self.workspace.workspace_root()
    }

    /// A stable identity for this repository's object store, shared by every
    /// worktree and branch view of it: the SHA-256 of the canonical objects-dir
    /// path (the same key the index cache uses, see [`crate::index_cache`]). Two
    /// commedit windows opened on the same repository — whichever branch each
    /// edits — produce the same key; two different repositories produce different
    /// keys. A frontend offering cross-instance commit drags compares it to tell a
    /// sibling-branch window (whose commit lives in the shared ODB and can be
    /// cherry-picked, see [`Self::lookup_commit_in_store`]) from a foreign repo
    /// (whose objects this session can't reach). `None` if the object store can't
    /// be located.
    pub fn object_store_key(&self) -> Option<String> {
        let objects = crate::transparency::git_objects_dir(self.workspace_root()).ok()?;
        Some(crate::index_cache::key_for(&objects))
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
        // (A no-op off-worktree, where there is no working copy.)
        self.snapshot_working_copy()?;
        let store = self.repo.store().clone();

        // Off-worktree there is no working copy on the edited branch, so compare
        // the branch tip now against its session-start tip directly. Worktree-bound
        // prefer @ (it includes uncommitted changes), else HEAD.
        let (new_id, old_id) = if self.is_worktree_bound() {
            let Some(new_id) = self
                .working_copy_commit_id()
                .or_else(|| self.head_commit_id())
            else {
                return Ok(Vec::new());
            };
            // Session-start tree: the @ recorded in the session-start view, or its
            // HEAD where there was none (detached HEAD).
            let view =
                pollster::block_on(session_op.view()).context("reading the session-start view")?;
            let old_id = view
                .get_wc_commit_id(self.workspace.workspace_name())
                .cloned()
                .or_else(|| {
                    self.session_head
                        .as_deref()
                        .and_then(CommitId::try_from_hex)
                });
            (Some(new_id), old_id)
        } else {
            let new_id = self.current_head_in_jj();
            let old_id = self
                .session_head
                .as_deref()
                .and_then(CommitId::try_from_hex);
            (new_id, old_id)
        };
        let Some(new_id) = new_id else {
            return Ok(Vec::new());
        };
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
        // Exempt every branch in the *editable set* (the ones this session
        // legitimately moves) and protect every other local branch — including the
        // checked-out one when its branch is not in the set (off-worktree).
        let exempt: Vec<&str> = self.edited.refs().collect();
        // Consider only branches jj actually tracks in its view. The import is
        // *scoped* (`import_some_refs` admits only the editable set), so jj's
        // export can only ever move a ref it imported; a branch jj never imported
        // cannot have been moved by *this* session's export, so it must not be
        // force-restored to our snapshot. That guard is what makes the backstop
        // multi-tenant-safe: the MCP server hosts several sessions over one shared
        // git common-dir, and another session's branch — never imported here —
        // would otherwise look like an "unrelated move" and get clobbered back to
        // this session's stale snapshot (a silent revert-to-an-old-tip). A branch
        // jj *does* track but that sits outside the editable set (a just-unticked
        // one) is still protected, keeping the narrowing-freeze behavior.
        let managed: HashSet<&str> = self
            .repo
            .view()
            .git_refs()
            .keys()
            .map(|name| name.as_str())
            .collect();
        let scoped: BTreeMap<String, String> = before
            .iter()
            .filter(|(name, _)| managed.contains(name.as_str()))
            .map(|(name, oid)| (name.clone(), oid.clone()))
            .collect();
        let restored = crate::transparency::restore_unrelated_heads(
            self.workspace.workspace_root(),
            &exempt,
            &scoped,
        );
        if !restored.is_empty() {
            eprintln!(
                "commedit: reverted unintended move of branch(es) {}; \
                 only the editable branches are rewritten",
                restored.join(", ")
            );
        }
    }

    /// The tip of the branch being edited as a [`CommitId`] — git HEAD in the
    /// normal session, the target branch's ref tip when editing off-worktree —
    /// used to seed the history walk and scope reordering to that branch's linear
    /// chain.
    pub fn head_commit_id(&self) -> Option<CommitId> {
        CommitId::try_from_hex(self.edited_tip()?)
    }

    /// The virtual root commit's id — the parent the oldest real commit reports.
    /// The history-graph layout uses it to end that commit's ancestry line at its
    /// node instead of drawing an edge to a never-listed parent.
    pub fn root_commit_id(&self) -> CommitId {
        self.repo.store().root_commit_id().clone()
    }

    /// Resolve a full 40-char hex sha to its [`CommitId`] if that object exists
    /// in the shared object store — even when it is *not* reachable from HEAD
    /// (e.g. a commit on another branch). The object is read straight from the
    /// ODB, so it needs no jj-side ref or index entry: editing the checked-out
    /// branch imports only its own ref into jj's view, yet the symlinked object
    /// store still holds every other branch's commits. Returns `None` for
    /// malformed hex or a sha that is not an existing commit. Used to
    /// cherry-pick a commit from outside the current branch's history.
    pub fn lookup_commit_in_store(&self, hex: &str) -> Option<CommitId> {
        let id = CommitId::try_from_hex(hex)?;
        self.repo.store().get_commit(&id).ok().map(|_| id)
    }

    /// The repo's git-configured identity (`committer.*`/`user.*`, see
    /// [`build_settings`]) as an [`crate::rewrite::Identity`], stamped "now" for
    /// both author and committer — the baseline a freshly created commit gets
    /// when the caller supplies no identity. The MCP layer overlays any explicit
    /// author/committer fields on top of this.
    pub fn default_identity(&self) -> crate::rewrite::Identity {
        let sig = self.settings.signature();
        let now = crate::history::format_timestamp(&sig.timestamp);
        crate::rewrite::Identity {
            author_name: sig.name.clone(),
            author_email: sig.email.clone(),
            author_time: now.clone(),
            committer_name: sig.name,
            committer_email: sig.email,
            committer_time: now,
        }
    }

    /// The user's local branches and tags, grouped by the hex id of the commit
    /// they point at — the history view's ref pills. Read fresh from git on
    /// every call, so it tracks the branch moves a clean save exports.
    pub fn commit_refs(&self) -> BTreeMap<String, Vec<crate::transparency::RefDecoration>> {
        use crate::transparency::RefKind;
        let mut refs = crate::transparency::ref_decorations(self.workspace.workspace_root());
        // Flag the checked-out branch so the UI can pill it distinctly. Branch
        // names are unique among branches, so matching by name is unambiguous.
        if let Some(current) = self.current_bookmark() {
            for decoration in refs.values_mut().flatten() {
                if decoration.kind == RefKind::Branch && decoration.name == current.as_str() {
                    decoration.current = true;
                }
            }
        }
        refs
    }

    /// The user's local branches, read fresh from git — the candidates for the
    /// multi-branch DAG dropdown. Each entry pairs the branch short-name with its
    /// current tip, a flag marking the primary (the launch/opened branch), and a
    /// flag marking whether it is currently in the editable set (ticked / imported).
    /// Branches whose tip is not a readable commit are skipped.
    pub fn local_branches(&self) -> Vec<BranchHead> {
        let current = self.current_bookmark();
        let current_name = current.as_ref().map(|c| c.as_str());
        crate::transparency::local_head_oids(self.workspace.workspace_root())
            .into_iter()
            .filter_map(|(refname, sha)| {
                let name = refname.strip_prefix("refs/heads/").unwrap_or(&refname);
                let head = CommitId::try_from_hex(&sha)?;
                Some(BranchHead {
                    is_current: current_name == Some(name),
                    is_editable: self.edited.contains(&refname),
                    name: name.to_string(),
                    head,
                })
            })
            .collect()
    }

    /// The editable set as branch short-names (the `refs/heads/` prefix stripped) —
    /// the branches imported as real bookmarks and rewritable right now. The primary
    /// (if any) comes first. A GTK frontend reads this back to seed the multi-head
    /// history walk from the set's real bookmark tips and to reflect the dropdown's
    /// ticked state. A singleton (the classic/MCP open) returns just one name.
    pub fn editable_branches(&self) -> Vec<String> {
        self.edited
            .refs()
            .map(|r| r.strip_prefix("refs/heads/").unwrap_or(r).to_string())
            .collect()
    }

    /// The editable set as commit ids — every editable branch's current tip, the
    /// primary's first. This is the multi-head reachability set the cross-branch
    /// drag planners ([`Self::plan_reorder_candidates_multi`] et al.) walk to
    /// recognise a sibling branch's lane as a valid splice/squash destination. The
    /// primary's tip is [`Self::head_commit_id`] (git HEAD or the ref tip); each
    /// extra branch's tip is read fresh from its git ref. A singleton set returns
    /// just the primary head — the classic single-branch path. Empty only on a
    /// detached HEAD with no branch (no primary tip).
    pub fn editable_heads(&self) -> Vec<CommitId> {
        let mut heads = Vec::new();
        if let Some(primary) = self.head_commit_id() {
            heads.push(primary);
        }
        let root = self.workspace.workspace_root();
        for full in self.edited.extra.iter() {
            let short = full.strip_prefix("refs/heads/").unwrap_or(full);
            if let Some(id) = crate::transparency::ref_commit(root, short)
                .and_then(|h| CommitId::try_from_hex(&h))
            {
                if !heads.contains(&id) {
                    heads.push(id);
                }
            }
        }
        heads
    }

    /// History walk over the **union** of several branch tips' ancestries — the
    /// multi-branch DAG view. `heads` is the edited branch's tip plus the extra
    /// branches the user folded in (resolve their tips via [`Self::local_branches`]
    /// or [`Self::lookup_commit_in_store`]). Read-only: the extra heads are made
    /// index-visible in a transient transaction that is rolled back, never
    /// touching git, the op-log, or the edited branch (see
    /// [`crate::history::history_limited_multi`]).
    pub fn history_multi(
        &self,
        heads: &[CommitId],
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<crate::history::CommitInfo>, bool)> {
        crate::history::history_limited_multi(&self.repo, heads, offset, limit)
    }

    /// The branch tip jj just exported into its session-local git dir, read from
    /// jj's own view — the user's git ref still holds the pre-rewrite tip until
    /// [`Self::bridge_branch_to_git`] mirrors this out. The checked-out branch's
    /// local bookmark, falling back to jj's git HEAD on a detached HEAD.
    fn exported_tip(&self) -> Option<String> {
        if let Some(name) = self.current_bookmark() {
            if let Some(id) = self.repo.view().get_local_bookmark(&name).as_normal() {
                return Some(id.hex());
            }
        }
        self.repo.view().git_head().as_normal().map(|id| id.hex())
    }

    /// The tip jj exported for the bookmark `short` (a branch short-name) into its
    /// session-local git dir, read from jj's own view — `None` if that bookmark is
    /// absent or conflicted. The per-branch counterpart of [`Self::exported_tip`],
    /// used by [`Self::bridge_branches_to_git`] to mirror each moved editable
    /// bookmark back to the user repo.
    fn exported_bookmark_tip(&self, short: &str) -> Option<String> {
        let name: RefNameBuf = short.into();
        self.repo
            .view()
            .get_local_bookmark(&name)
            .as_normal()
            .map(|id| id.hex())
    }

    /// Mirror every editable bookmark jj moved in its throwaway git dir back into
    /// the user's real repository — the git ref moves commedit performs itself now
    /// that jj's objects land in the shared ODB but its refs stay session-local
    /// (see [`Self::init_detached`]). Runs in the export tail *before* materializing
    /// the working tree, so the user's HEAD resolves to the new tip by the time the
    /// index is reset.
    ///
    /// Each editable branch whose exported tip *differs* from its pre-rewrite oid in
    /// `before` ([`Self::snapshot_heads`]) is moved, compare-and-swapped against that
    /// per-branch old oid so a racing commedit instance is detected, not clobbered.
    /// A bookmark whose tip is unchanged is skipped entirely (so editing one branch
    /// never touches another's ref). On a detached HEAD (no primary branch) the
    /// rewritten tip is mirrored onto `HEAD --no-deref`, CAS'd against `old_head`.
    /// A mismatch (or any other failure) is logged and tolerated, reconciled on the
    /// next open — the same posture jj's own ref export takes.
    pub(crate) fn bridge_branches_to_git(
        &self,
        old_head: Option<&str>,
        before: &BTreeMap<String, String>,
    ) {
        let root = self.workspace.workspace_root();
        // Detached HEAD: no editable branch ref, mirror the rewritten tip onto HEAD.
        if self.edited.primary.is_none() {
            if let Some(new_tip) = self.exported_tip() {
                if let Err(e) =
                    crate::transparency::update_user_ref(root, "HEAD", &new_tip, old_head, true)
                {
                    eprintln!(
                        "commedit: could not move HEAD to the rewritten tip {new_tip} ({e}); \
                         git will reconcile on the next open"
                    );
                }
            }
            return;
        }
        // Mirror each editable branch whose jj-exported tip moved vs `before`.
        for full in self.edited.refs() {
            let short = full.strip_prefix("refs/heads/").unwrap_or(full);
            let Some(new_tip) = self.exported_bookmark_tip(short) else {
                continue; // absent or conflicted bookmark: nothing to mirror
            };
            let old = before.get(full).map(String::as_str);
            if old == Some(new_tip.as_str()) {
                continue; // unchanged: leave this branch's ref untouched
            }
            if let Err(e) = crate::transparency::update_user_ref(root, full, &new_tip, old, false) {
                eprintln!(
                    "commedit: could not move {full} to the rewritten tip {new_tip} ({e}); \
                     git will reconcile on the next open"
                );
            }
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
        let edited_short: Vec<String> = self
            .edited
            .refs()
            .map(|r| r.strip_prefix("refs/heads/").unwrap_or(r).to_string())
            .collect();
        let mut tx = self.repo.start_transaction();
        pollster::block_on(git::import_head(tx.repo_mut())).context("importing git HEAD")?;
        let options = GitImportOptions {
            abandon_unreachable_commits: false,
            record_synthetic_predecessors: false,
            remote_auto_track_bookmarks: HashMap::new(),
        };
        // Import the local bookmark for every branch in the editable set (a
        // singleton set reproduces the classic single-branch import); remote-
        // tracking refs and tags are excluded.
        let git_ref_filter = |kind: GitRefKind, symbol: RemoteRefSymbol<'_>| {
            kind == GitRefKind::Bookmark
                && symbol.remote == REMOTE_NAME_FOR_LOCAL_GIT_REPO
                && edited_short.iter().any(|n| n == symbol.name.as_str())
        };
        pollster::block_on(git::import_some_refs(
            tx.repo_mut(),
            &options,
            git_ref_filter,
        ))
        .context("importing the editable branches")?;
        pollster::block_on(tx.repo_mut().rebase_descendants()).context("rebasing after import")?;
        self.repo =
            pollster::block_on(tx.commit("import git refs")).context("committing import")?;
        Ok(())
    }

    /// Catch the session up to a git HEAD that moved *out of band* — e.g. the
    /// caller crystallized a unit with a plain `git commit` on top of HEAD while
    /// this session was open. jj only imports git state at [`Self::open`], so the
    /// new commit is otherwise absent from jj's view and every read or mutation
    /// that resolves from the live HEAD fails ("commit … not found in index").
    /// This re-imports it into the *existing* session, so reads and mutations keep
    /// working **without** the full reopen that would reset the trash and the
    /// operation log: the import is just another recorded jj operation, and the
    /// session's undo floor and trash survive.
    ///
    /// Returns whether an import was performed. A no-op when already in sync, on a
    /// detached HEAD, or while a conflicted rewrite is pending (git is untouched
    /// then, so the live HEAD is still the pre-rewrite tip jj already knows).
    /// Refuses a *branch switch*: a session is scoped to the one checked-out
    /// branch (only its ref is imported), so a different branch genuinely needs a
    /// fresh [`Self::open`] rather than a catch-up import.
    pub fn sync_to_git_head(&mut self) -> Result<bool> {
        if self.is_pending() {
            return Ok(false);
        }
        // Off-worktree the live HEAD/worktree belong to a different branch and are
        // irrelevant to the edited one; an out-of-band move of the edited branch
        // needs an explicit `reload_repo` rather than a HEAD catch-up.
        if !self.is_worktree_bound() {
            return Ok(false);
        }
        let live_branch = crate::transparency::head_branch(self.workspace.workspace_root());
        if live_branch != self.git_head_branch {
            anyhow::bail!(
                "the checked-out branch changed outside commedit (now {live_branch:?}, \
                 was {:?}); reopen the repository to edit it",
                self.git_head_branch
            );
        }
        let Some(live_head) = self.head_commit() else {
            return Ok(false); // detached HEAD: nothing branch-scoped to track
        };
        // jj's exported branch tip already matches the live git ref → in sync.
        if self.exported_tip().as_deref() == Some(live_head.as_str()) {
            return Ok(false);
        }
        // jj imports refs from the session-local git dir, not the user's `.git`,
        // so re-point its branch ref at the user's new tip before importing.
        let git_dir = self._workdir.path().join("git");
        crate::transparency::seed_session_head(
            &git_dir,
            self.workspace.workspace_root(),
            self.edited.primary.as_deref(),
        )?;
        self.import_git()?;
        Ok(true)
    }

    /// Absorb any out-of-band `git commit` made in a *sibling* worktree before its
    /// next snapshot — the per-worktree analogue of [`Self::sync_to_git_head`]. jj
    /// imports git state only at open, so a plain `git commit` in a linked worktree
    /// leaves that branch's bookmark (and the worktree's `@`) on the old tip; the
    /// next snapshot would otherwise record the just-committed change as if it were
    /// still uncommitted. For each extra worktree this refuses an out-of-band
    /// *branch switch* (the worktree's branch→jj-workspace mapping is then stale,
    /// mirroring the launch's "reopen the repository" guard), else detects whether
    /// its branch's live git tip is ahead of jj's bookmark. Every drifted branch's
    /// ref is re-seeded into the session git dir and a **single** [`Self::import_git`]
    /// catches jj up to all of them (it re-imports the whole editable set, so one
    /// import suffices); [`Repo::snapshot_extra_worktree`]'s re-anchor then moves
    /// each `@` onto its new tip before the snapshot. A no-op when nothing drifted,
    /// and skipped while a conflicted rewrite is pending (git is frozen, so the live
    /// tips are the ones jj already knows). Called by
    /// [`Repo::snapshot_extra_worktrees`].
    pub(crate) fn catch_up_extra_worktrees(&mut self) -> Result<()> {
        if self.is_pending() {
            return Ok(());
        }
        let mut drifted: Vec<String> = Vec::new();
        for view in &self.extra_worktrees {
            let root = view.workspace.workspace_root();
            // Structural guard: the worktree must still have *its* branch checked out.
            let live_branch = crate::transparency::head_branch(root);
            if live_branch.as_deref() != Some(view.branch.as_str()) {
                anyhow::bail!(
                    "the worktree at {} changed its checked-out branch outside commedit \
                     (now {:?}, was {:?}); reopen the repository to edit it",
                    root.display(),
                    live_branch,
                    view.branch
                );
            }
            let short = view
                .branch
                .strip_prefix("refs/heads/")
                .unwrap_or(&view.branch);
            let bookmark: RefNameBuf = short.into();
            let jj_tip = self
                .repo
                .view()
                .get_local_bookmark(&bookmark)
                .as_normal()
                .map(|id| id.hex());
            let live_tip = crate::transparency::ref_commit(root, &view.branch);
            if live_tip.is_some() && live_tip != jj_tip {
                drifted.push(view.branch.clone());
            }
        }
        if drifted.is_empty() {
            return Ok(());
        }
        // Re-seed each drifted branch's ref into the session git dir (no HEAD
        // change), then a single import catches jj up to all of them at once.
        let git_dir = self._workdir.path().join("git");
        let root = self.workspace.workspace_root();
        for full in &drifted {
            if let Some(tip) = crate::transparency::ref_commit(root, full) {
                crate::transparency::seed_session_ref(&git_dir, full, &tip)?;
            }
        }
        self.import_git()
    }

    /// Change the editable set *in place* — widen it (a branch ticked in the GTK
    /// dropdown) or narrow it (unticked) — **without** the full reopen
    /// [`Self::open_multi`] would do, so the session's undo op-log and trash survive
    /// a toggle. `branches` is the complete desired set as short-names or full refs;
    /// it is diffed against the current set, only the difference is applied, and the
    /// order is honoured (the first entry becomes the primary). A no-op when the set
    /// is unchanged.
    ///
    /// **Widening** seeds each newly-added branch's ref into the session git dir and
    /// re-runs [`Self::import_git`] (a *recorded* jj operation — the same path
    /// [`Self::sync_to_git_head`] uses — so the undo floor and trash are untouched),
    /// then registers its worktree if it lives in one. **Narrowing** drops the
    /// branch from the set (and deregisters its worktree): it then falls *outside*
    /// the set, so the export bridge stops mirroring it and the protect-backstop
    /// (`protect_unrelated_heads`, exempting only the set) freezes it on its current
    /// commit — the intended "unticked ⇒ frozen/forked" behavior, with no fragile jj
    /// bookmark de-import. The branch's git ref already holds its real tip (every
    /// clean save bridged it out), so leaving the stale in-view bookmark behind is
    /// harmless: it is never bridged again.
    ///
    /// Refused if `branches` is empty (the **last-branch rule**, mirroring the MCP's
    /// "the last session can't be closed"), if a named branch does not exist, or
    /// while a conflicted rewrite is pending (the held rewrite assumes a fixed set).
    pub fn set_editable_branches(&mut self, branches: &[String]) -> Result<()> {
        if self.is_pending() {
            anyhow::bail!(
                "a conflicted rewrite is being resolved; finish or abort it before \
                 changing the editable branch set"
            );
        }
        let root = self.workspace.workspace_root().to_path_buf();
        // Resolve to full refs (verifying existence), dedup, preserve order.
        let mut desired: Vec<String> = Vec::new();
        for name in branches {
            let full = crate::transparency::resolve_local_branch(&root, name)?;
            if !desired.contains(&full) {
                desired.push(full);
            }
        }
        if desired.is_empty() {
            anyhow::bail!(
                "the editable set cannot be emptied — at least one branch must stay \
                 editable"
            );
        }
        let current: Vec<String> = self.edited.refs().map(str::to_string).collect();
        if desired == current {
            return Ok(()); // already exactly this set (same order)
        }

        // Branches leaving the set: drop their registered worktree (if any). They
        // fall out of the exempt set, so the protect-backstop freezes them and the
        // bridge stops mirroring them — no jj de-import needed.
        let removed: Vec<String> = current
            .iter()
            .filter(|c| !desired.contains(c))
            .cloned()
            .collect();
        self.extra_worktrees
            .retain(|w| !removed.contains(&w.branch));

        // Re-key the set to the desired order (first = primary). Done before the
        // import so the import filter and the worktree mapping see the new set.
        let mut it = desired.iter().cloned();
        let primary = it.next();
        let extra: Vec<String> = it.collect();
        self.edited = EditableSet { primary, extra };

        // Branches joining the set: seed each ref into the session git dir so jj's
        // import can see its tip, then re-import the whole set as one recorded op.
        let added: Vec<String> = desired
            .iter()
            .filter(|d| !current.contains(d))
            .cloned()
            .collect();
        if !added.is_empty() {
            let git_dir = self._workdir.path().join("git");
            for full in &added {
                if let Some(tip) = crate::transparency::ref_commit(&root, full) {
                    crate::transparency::seed_session_ref(&git_dir, full, &tip)?;
                }
            }
            self.import_git()?;
            // Register a git worktree for any added branch checked out in one other
            // than the launch worktree, so a rewrite re-materializes it (mirrors the
            // mapping `open_multi` does at open).
            let launch_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
            let worktree_map = crate::transparency::worktrees(&root)?;
            for full in &added {
                let Some(path) = worktree_map
                    .iter()
                    .find(|(_, b)| b.as_deref() == Some(full.as_str()))
                    .map(|(path, _)| path.clone())
                else {
                    continue; // no worktree: a pure ref-move
                };
                if std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone()) == launch_root {
                    continue; // the launch worktree is handled by the working-copy path
                }
                self.register_worktree(full, &path)?;
            }
        }
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

/// Walk up from `start` to the nearest ancestor that is a git repository — a
/// directory holding a `.git` entry (a directory for a normal repo, a file for a
/// worktree/submodule checkout) — mirroring how `git` discovers its repository
/// from a subdirectory. Returns that repository's root.
///
/// commedit edits the history of an *existing* git repo and never creates one, so
/// a `start` with no git repo in itself or any ancestor is refused. The error
/// keeps the "not a git repository" wording the rest of the contract relies on.
fn find_git_root(start: &Path) -> Result<PathBuf> {
    // Resolve to an absolute, symlink-free path first: the parent walk then
    // terminates at the filesystem root rather than at a relative path's empty
    // parent, and a file path (e.g. a path to a tracked file) climbs to its
    // containing directory like any other.
    let resolved = std::fs::canonicalize(start)
        .with_context(|| format!("cannot access {}", start.display()))?;
    let mut dir = resolved.as_path();
    loop {
        if dir.join(".git").exists() {
            return Ok(dir.to_path_buf());
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => anyhow::bail!(
                "{} is not a git repository (nor is any parent directory) — \
                 commedit edits the history of an existing git repo and will \
                 not create one",
                resolved.display()
            ),
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
    let name = committer_field(
        workspace_root,
        "GIT_COMMITTER_NAME",
        "committer.name",
        "user.name",
    )
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
    config
        .add_layer(ConfigLayer::parse(ConfigSource::User, &identity).context("parsing identity")?);
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
        assert!(
            err.to_string().contains("testing failed inside jj-lib"),
            "{err}"
        );
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
