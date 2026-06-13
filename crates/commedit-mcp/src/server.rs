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

Raw git vs commedit — the dividing line. A NEW commit on top of HEAD needs no \
rebase, so commit it with plain `git add` + `git commit`: simpler and cheaper \
than commit_working_copy, and the right tool for everyday committing. Reach for \
commedit when the work touches history that ALREADY EXISTS, or lands a commit \
BELOW the tip — rewording a message, changing an author or date, editing a \
commit's files, or reordering, squashing, splitting, dropping, reverting, \
cherry-picking or inserting a commit. commedit rewrites in place and rebases the \
descendants for you, on any commit reachable from HEAD (not just the tip), so \
reach for it instead of `git commit --amend`, `git rebase -i`, `git cherry-pick` \
or `git revert`. merge_out_commit can introduce a merge above a commit, to \
organize a linear history into a branchy one; building a merge between two real \
branches, and managing branches, worktrees or remotes, stay plain-git tasks too. \
(commedit imports git state at startup, so after a raw-git commit — like any \
out-of-band change — call reload_repo before using these tools again; see the \
working-tree note below.)

Addressing: every tool that takes a commit accepts its sha or its change_id, \
full or a unique prefix of at least 4 characters, case-insensitive. Mutations \
rewrite the target and its descendants, so shas change constantly; the \
change_id is stable across rewrites — address commits by change_id to chain \
mutations without re-listing history. An ambiguous prefix fails listing its \
matches; list_history shows both ids. To save tokens, list_history returns \
those ids already abbreviated to the shortest repo-unique prefix (>= 8 chars) — \
pass them straight back as refs rather than echoing full 40/32-char ids.

Bulk & paging: to edit many commits at once (e.g. re-date or reword a range), \
prefer edit_commits — it applies every message/identity edit in ONE transaction \
with a single rebase, atomically. list_history returns 30 commits by default; \
page deeper with its offset / next_offset rather than requesting a huge limit. \
list_history returns every verbose field by default; pass its `fields` to fetch \
only the ones you need (e.g. just the timestamps before a re-date, or `[]` for a \
header-only overview) to keep responses small.

Surgical edits: for a small change to a long message or file, prefer \
replace_in_message / replace_in_file — they take an exact `old`→`new` \
substitution (unique match unless replace_all) instead of the whole text, so \
the untouched content can't drift and the call stays small. Make `old` long \
enough to match exactly once; an ambiguous or missing match is rejected. \
edit_message / replace_files remain for wholesale rewrites.

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
top of HEAD (like git commit -a), but for that whole-tree case prefer plain git \
(simpler, no rebase); reach for commit_working_copy to commit only a deterministic \
SUBSET of the tree (its paths/hunks/patches selection) without leaving the \
session. It captures edits to already-tracked files only, so a brand-new \
(untracked) file is silently skipped unless named in its add_paths (the same holds \
for squash_working_copy). A mid-history insert, \
revert or pick may report conflicts like any rewrite. merge_out_commit \
introduces a merge directly above a single-parent commit, turning that commit \
into a one-commit side branch you can then move further commits onto.

Trash: dropped commits go to a session-scoped trash (list_trash) and can be \
grafted back (restore_commit) or folded into a commit (squash_commit).

Safety net: every landed mutation is a recorded operation — list_operations \
lists them, undo/redo step the cursor, jump_to_operation travels to any (0 \
rolls the whole session back to its start). The one unrecoverable action is \
discard_working_copy.

Uncommitted changes in the working tree are first-class: they ride through \
every rewrite automatically (working_copy_status shows them; session_diff \
shows everything this session changed, committed and uncommitted, against the \
session-start tree). Git state is \
imported only at startup — after any out-of-band git operation (a commit, \
branch switch, rebase made outside this server) call reload_repo before \
continuing; it starts a fresh session in place, discarding the trash, the \
operation log and any pending rewrite.

Verifying a rewrite: a topology-changing mutation (reorder, squash, split, drop, \
restore, create, revert, cherry_pick, squash_working_copy) returns a `topology` \
slice on a clean save — the affected commits with their new parents and children \
by change_id, plus a `merge_tip` when the new branch tip is a merge — so you can \
confirm the resulting shape in place instead of a follow-up list_history. Plain \
message/identity/file edits omit it (their shape is unchanged). show_graph reads \
that same shape for the whole branch on demand — every commit with its parents \
and children by change_id — to see how merges and side branches connect.

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
