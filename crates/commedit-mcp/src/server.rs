//! The MCP server type: shared session state, router assembly and the
//! `ServerHandler` implementation with the agent-facing instructions.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use commedit_engine::repo::Repo;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool_handler, ServerHandler};

use crate::session::TrashState;

/// One editing session over one repository. Cheap to clone — all state is
/// behind `Arc`s; the `std::sync::Mutex` serializes every tool body (each runs
/// in `spawn_blocking` and takes the lock inside, never across an `.await`).
#[derive(Clone)]
pub struct CommeditServer {
    pub(crate) repo: Arc<Mutex<Repo>>,
    pub(crate) trash: Arc<Mutex<TrashState>>,
    /// The resolved repository root, kept to re-open the repo on `reload_repo`.
    pub(crate) repo_path: PathBuf,
    tool_router: ToolRouter<Self>,
}

impl CommeditServer {
    pub fn new(repo: Repo) -> Self {
        let repo_path = repo.workspace_root().to_path_buf();
        Self {
            repo: Arc::new(Mutex::new(repo)),
            trash: Arc::new(Mutex::new(TrashState::default())),
            repo_path,
            tool_router: Self::router_read()
                + Self::router_mutate()
                + Self::router_workcopy()
                + Self::router_conflict()
                + Self::router_ops(),
        }
    }
}

/// The agent's manual, served as the MCP `instructions` field.
const INSTRUCTIONS: &str = "\
commedit edits the history of the checked-out git branch in place: any commit \
reachable from HEAD can be edited (message, identity, file contents), split, \
reordered, dropped or squashed, and its descendants are rebased automatically. \
New commits can also be created from scratch and spliced in anywhere. \
The repository stays a plain git repo throughout — no jj state is left behind.

When the task is to edit existing history — reword a message, change an \
author or date, edit a commit's files, or reorder, squash, split, drop or \
insert a commit — prefer these tools over raw git (reach for them instead of \
`git commit --amend`, `git rebase -i` or `git cherry-pick`): they rewrite in \
place and rebase the descendants for you, on any commit reachable from HEAD, \
not just the tip. Building merge commits and managing branches, worktrees or \
remotes stay plain-git tasks.

Addressing: every tool that takes a commit accepts its sha or its change_id, \
full or a unique prefix of at least 4 characters, case-insensitive. Mutations \
rewrite the target and its descendants, so shas change constantly; the \
change_id is stable across rewrites — address commits by change_id to chain \
mutations without re-listing history. An ambiguous prefix fails listing its \
matches; list_history shows both ids.

Conflicts: a mutation whose rebase conflicts returns status=conflicts and is \
held back IN FULL — git history, HEAD and the working tree stay untouched \
until it settles. Resolve the OLDEST conflicted commit first (read_conflict \
each resolvable file, remove all markers, resolve_conflicts echoing each \
file's marker_len); fixing the earliest often auto-clears descendants. \
abort_rewrite discards the held rewrite (and is the only way out of a \
structural, resolvable=false conflict). No other mutation runs while pending.

Creating commits: create_commit makes a new commit from given file contents \
(empty for an empty commit) and inserts it — on top of HEAD by default, or under \
any commit / at root via new_parent. revert_commit inserts the inverse of a \
commit (like git revert). cherry_pick_commit copies a commit's change in (like \
git cherry-pick) — the source may be off the current branch, named by its full \
sha. commit_working_copy turns the current uncommitted changes into a commit on \
top of HEAD (like git commit -a). A mid-history insert, revert or pick may \
report conflicts like any rewrite.

Trash: dropped commits go to a session-scoped trash (list_trash) and can be \
grafted back (restore_commit) or folded into a commit (squash_commit).

Safety net: every landed mutation is a recorded operation — undo/redo step \
them, jump_to_operation 0 rolls the whole session back to its start. The one \
unrecoverable action is discard_working_copy.

Uncommitted changes in the working tree are first-class: they ride through \
every rewrite automatically (working_copy_status shows them). Git state is \
imported only at startup — after any out-of-band git operation (a commit, \
branch switch, rebase made outside this server) call reload_repo before \
continuing; it starts a fresh session in place, discarding the trash, the \
operation log and any pending rewrite.

Reading results: every tool result is YAML. Long multi-line strings such as \
diffs and file contents render as a literal block scalar, or — when a line \
carries a tab or trailing whitespace — as a YAML sequence with one string per \
line; reassemble such a sequence by joining its entries with newlines.";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CommeditServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("commedit", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}
