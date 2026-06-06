//! Open (or create) a colocated jj workspace and keep it in sync with git.
//!
//! jj-lib's mutating operations are async because the backend trait is async;
//! the git backend is synchronous under the hood, so we drive them to
//! completion with [`pollster::block_on`].

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId;
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::git::{self, GitImportOptions};
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefNameBuf;
use jj_lib::repo::{MutableRepo, ReadonlyRepo, StoreFactories};
use jj_lib::settings::UserSettings;
use jj_lib::workspace::{default_working_copy_factories, Workspace};

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
}

impl Repo {
    /// Open the repository at `workspace_root`, creating the colocated jj
    /// metadata (`.jj`) if it does not exist yet, then import git refs/HEAD so
    /// jj's view matches the git repository.
    pub fn open(workspace_root: &Path) -> Result<Self> {
        let settings = build_settings(workspace_root)?;
        // Record the checked-out branch before jj touches HEAD, so we can
        // re-attach to it afterwards.
        let git_head_branch = crate::transparency::head_branch(workspace_root);
        let (workspace, repo) = if workspace_root.join(".jj").is_dir() {
            let workspace = Workspace::load(
                &settings,
                workspace_root,
                &StoreFactories::default(),
                &default_working_copy_factories(),
            )
            .context("loading existing jj workspace")?;
            let repo = pollster::block_on(workspace.repo_loader().load_at_head())
                .context("loading repo at head")?;
            (workspace, repo)
        } else if workspace_root.join(".git").exists() {
            // Existing git repo: attach jj to the in-tree .git so both tools
            // share the same object database (a colocated layout).
            let git_dir = workspace_root.join(".git");
            pollster::block_on(Workspace::init_external_git(&settings, workspace_root, &git_dir))
                .context("attaching jj to existing git repo")?
        } else {
            pollster::block_on(Workspace::init_colocated_git(&settings, workspace_root))
                .context("initializing colocated jj workspace")?
        };

        let mut this = Self {
            workspace,
            repo,
            settings,
            git_head_branch,
        };
        this.import_git()?;
        crate::transparency::ensure_jj_excluded(workspace_root)?;
        this.reattach_head()?;
        Ok(this)
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
    fn current_bookmark(&self) -> Option<RefNameBuf> {
        self.git_head_branch
            .as_ref()
            .map(|branch| branch.strip_prefix("refs/heads/").unwrap_or(branch).into())
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

    /// The current git HEAD commit id (hex), for display and recovery — e.g.
    /// printed on startup so the user can `git reset --hard <id>` if a rewrite
    /// goes wrong.
    pub fn head_commit_hex(&self) -> Option<String> {
        self.head_commit()
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
        // commedit's own jj working-copy commit(s) are ours to drop too.
        let owned: Vec<String> = self
            .repo
            .view()
            .wc_commit_ids()
            .values()
            .map(|id| id.hex())
            .collect();
        let _ = crate::transparency::prune_orphaned_keep_refs(
            self.workspace.workspace_root(),
            old_head,
            &owned,
        );
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

    /// Pull git refs and HEAD into jj's view as a single transaction. No-op
    /// (empty operation) when jj is already in sync with git.
    fn import_git(&mut self) -> Result<()> {
        let mut tx = self.repo.start_transaction();
        pollster::block_on(git::import_head(tx.repo_mut())).context("importing git HEAD")?;
        let options = GitImportOptions {
            auto_local_bookmark: true,
            abandon_unreachable_commits: false,
            remote_auto_track_bookmarks: HashMap::new(),
        };
        pollster::block_on(git::import_refs(tx.repo_mut(), &options)).context("importing git refs")?;
        pollster::block_on(tx.repo_mut().rebase_descendants()).context("rebasing after import")?;
        self.repo = pollster::block_on(tx.commit("import git refs")).context("committing import")?;
        Ok(())
    }
}

/// Embedded baseline config (see `default_config.toml`). jj-lib ships no
/// defaults of its own; the jj CLI provides them, so we mirror them here.
const DEFAULT_CONFIG: &str = include_str!("default_config.toml");

/// Build a [`UserSettings`] from the embedded defaults plus the committer
/// identity jj should stamp on commits it rewrites. We resolve it the way git
/// itself would: an explicit `GIT_AUTHOR_*`/`GIT_COMMITTER_*` override wins, then
/// the repo's configured `user.name`/`user.email` (the local/global/system git
/// config hierarchy), and only failing both do we fall back to a generic
/// commedit identity. This keeps rebased/reordered commits attributed to the
/// real git author rather than jj's defaults.
fn build_settings(workspace_root: &Path) -> Result<UserSettings> {
    let name = env_first(&["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"])
        .or_else(|| crate::transparency::config_value(workspace_root, "user.name"))
        .unwrap_or_else(|| "commedit".to_string());
    let email = env_first(&["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"])
        .or_else(|| crate::transparency::config_value(workspace_root, "user.email"))
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

fn env_first(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
}
