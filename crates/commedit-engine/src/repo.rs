//! Open (or create) a colocated jj workspace and keep it in sync with git.
//!
//! jj-lib's mutating operations are async because the backend trait is async;
//! the git backend is synchronous under the hood, so we drive them to
//! completion with [`pollster::block_on`].

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use jj_lib::config::{ConfigLayer, ConfigSource, StackedConfig};
use jj_lib::git::{self, GitImportOptions};
use jj_lib::repo::{ReadonlyRepo, StoreFactories};
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
        let settings = build_settings()?;
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

    /// The git commit HEAD currently points at — capture this before a rewrite
    /// so the working tree can be synced to the new tip afterwards.
    pub(crate) fn head_commit(&self) -> Option<String> {
        crate::transparency::head_commit(self.workspace.workspace_root())
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

/// Build a [`UserSettings`] from the embedded defaults plus a committer identity
/// taken from the environment (falling back to a generic commedit identity).
fn build_settings() -> Result<UserSettings> {
    let name = env_first(&["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME", "USER"])
        .unwrap_or_else(|| "commedit".to_string());
    let email = env_first(&["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"])
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
