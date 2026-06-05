//! Walk the repository into a flat, topologically ordered list of commits for
//! the history view (children before parents, like gitk).

use anyhow::{Context, Result};
use jj_lib::backend::{ChangeId, CommitId};
use jj_lib::commit::Commit;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::{ReadonlyRepo, Repo};
use jj_lib::revset::{RevsetExpression, SymbolResolver, SymbolResolverExtension};

/// A single row in the history view.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub id: CommitId,
    pub change_id: ChangeId,
    /// First line of the commit description.
    pub subject: String,
    /// Full commit description (message), including the subject line.
    pub description: String,
    pub author_name: String,
    pub author_email: String,
    pub parents: Vec<CommitId>,
}

impl CommitInfo {
    /// Hex commit id, for display and stable identification in the UI.
    pub fn id_hex(&self) -> String {
        self.id.hex()
    }

    /// Hex change id. Stable across rewrites (the commit id changes, the change
    /// id does not), so the UI uses it to re-select a commit after saving.
    pub fn change_id_hex(&self) -> String {
        self.change_id.hex()
    }

    fn from_commit(commit: &Commit) -> Self {
        let description = commit.description().to_string();
        let subject = description.lines().next().unwrap_or("").to_string();
        let author = commit.author();
        Self {
            id: commit.id().clone(),
            change_id: commit.change_id().clone(),
            subject,
            description,
            author_name: author.name.clone(),
            author_email: author.email.clone(),
            parents: commit.parent_ids().to_vec(),
        }
    }
}

/// List all visible commits in topological order (newest first), excluding the
/// virtual root commit.
pub fn history(repo: &ReadonlyRepo) -> Result<Vec<CommitInfo>> {
    // Mirror what `git log`/`gitk` show: commits reachable from the git refs and
    // git HEAD. jj's `all()` additionally surfaces divergent (pre-rewrite) and
    // working-copy commits, which git never created a ref for — they would show
    // up here as confusing duplicates of the commits that replaced them.
    let user_expression = RevsetExpression::git_refs()
        .union(&RevsetExpression::git_head())
        .ancestors();
    let symbol_resolver =
        SymbolResolver::new(repo, &([] as [&Box<dyn SymbolResolverExtension>; 0]));
    let expression = user_expression
        .resolve_user_expression(repo, &symbol_resolver)
        .context("resolving history revset")?;
    let revset = expression
        .evaluate(repo)
        .context("evaluating history revset")?;

    let store = repo.store();
    let root = store.root_commit_id().clone();
    let mut commits = Vec::new();
    for entry in revset.commit_change_ids() {
        let (id, _change_id) = entry.context("iterating history")?;
        if id == root {
            continue;
        }
        let commit = store.get_commit(&id).context("loading commit")?;
        commits.push(CommitInfo::from_commit(&commit));
    }
    Ok(commits)
}
