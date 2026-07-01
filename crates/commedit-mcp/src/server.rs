//! The MCP server type: shared session state, router assembly and the
//! `ServerHandler` implementation with the agent-facing instructions.

use std::sync::{Arc, Mutex};

use commedit_engine::repo::Repo;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool_handler, ServerHandler};

use crate::session::SessionRegistry;

/// A multi-tenant server over one repository: it hosts several independent
/// editing sessions (one per branch/worktree), each addressed by id on every
/// tool call. Cheap to clone — all state is behind the registry `Arc`. See
/// [`SessionRegistry`] for the locking model.
#[derive(Clone)]
pub struct CommeditServer {
    pub(crate) sessions: Arc<Mutex<SessionRegistry>>,
    tool_router: ToolRouter<Self>,
}

impl CommeditServer {
    /// Build the server around an initial (launch) session over `repo`. The launch
    /// session is registered like any other — discoverable via `list_sessions`,
    /// addressable by its branch short-name — and carries no implicit-default
    /// status. Further sessions are opened with `open_session`. The repository the
    /// server is scoped to is the one `repo` belongs to.
    pub fn new(repo: Repo) -> Self {
        let root = repo.workspace_root().to_path_buf();
        Self {
            sessions: Arc::new(Mutex::new(SessionRegistry::with_launch_session(root, repo))),
            tool_router: Self::router_read()
                + Self::router_mutate()
                + Self::router_workcopy()
                + Self::router_conflict()
                + Self::router_ops(),
        }
    }

    /// A clone of the session registry handle. `main` grabs this before `serve`
    /// consumes the server, so it can flush every session's index cache at clean
    /// shutdown (see [`SessionRegistry::flush_all_caches`]).
    pub fn sessions_handle(&self) -> Arc<Mutex<SessionRegistry>> {
        self.sessions.clone()
    }
}

/// The agent's manual, served as the MCP `instructions` field.
const INSTRUCTIONS: &str = "\
commedit edits the history of the checked-out git branch in place: any commit \
reachable from HEAD can be edited (message, identity, file contents), split, \
reordered, dropped or squashed, and its descendants are rebased automatically. \
New commits can also be created from scratch and spliced in anywhere. \
The repository stays a plain git repo throughout — no jj state is left behind.

Sessions: this server hosts SEVERAL independent editing sessions over the one \
repository it launched against — one per branch, editable in parallel. EVERY \
session tool takes a required `session` selector; there is no default. The \
selector is the session id: the short name of the branch it edits (e.g. `main`), \
or `HEAD` for a detached/unborn-HEAD session — stable across rewrites, so always \
a safe handle. list_sessions shows what's open (the launch session included); \
open_session(branch) starts one over another branch (worktree-bound if that \
branch is checked out, else off-worktree); close_session(session) drops one (not \
the last). A branch already open can't be opened twice.

Raw git vs commedit — the dividing line. A NEW commit on top of HEAD needs no \
rebase, so a one-off is simplest as plain `git add` + `git commit`. Reach for \
commedit whenever the work touches history that ALREADY EXISTS, or lands a commit \
BELOW the tip — rewording, re-authoring/re-dating, editing a commit's files, or \
reordering, squashing, splitting, dropping, reverting, cherry-picking or inserting \
a commit: it rewrites in place and rebases descendants on any commit reachable \
from HEAD, replacing `git commit --amend`, `git rebase -i`, `git cherry-pick` and \
`git revert`. Inside a session, commit_working_copy is also as good as a plain \
commit and often better — it keeps the session coherent and returns the new \
change_id plus what's left uncommitted, ready to chain. merge_out_commit \
introduces a merge above a commit (linear→branchy), and an EXISTING merge can be \
reworded, squashed into, or have commits moved across it (reorder_commit's \
`child` splices into a parent edge). Only building a NEW merge between divergent \
branches, and managing branches/worktrees/remotes, stay plain-git tasks.

Off-worktree branches: this session may edit a branch you have NOT checked out — \
launched as `commedit-mcp <path> <branch>`, or switched via reload_repo's \
`branch`. Every history edit works as above, but ONLY that branch's ref moves: \
HEAD, index and worktree stay frozen, so there is no working copy and the \
working-copy tools (commit/squash_working_copy, split/discard) are refused. \
reload_repo reports the edited branch and whether the session is worktree_bound.

Addressing: a commit ref is its sha or its change_id, full or a unique prefix of \
>= 4 chars, case-insensitive. Shas churn as mutations rewrite the target and its \
descendants; the change_id is stable — address by change_id to chain mutations \
without re-listing. An ambiguous prefix fails listing its matches. list_history \
returns ids pre-abbreviated to the shortest repo-unique prefix (>= 8 chars); pass \
them straight back rather than echoing full ids.

Bulk & paging: to edit many commits at once (e.g. re-date/reword a range), prefer \
edit_commits — every edit in ONE atomic transaction and rebase. list_history \
returns 30 commits and every verbose field by default; page deeper with \
offset/next_offset (not a huge limit) and pass `fields` for only what you need \
(`[]` for a header-only overview) to keep responses small.

Surgical edits: for a small change to a long message or file, prefer \
replace_in_message / replace_in_file — an exact `old`→`new` substitution (unique \
unless replace_all) instead of the whole text, so untouched content can't drift. \
Make `old` unique; a miss reports the closest text with whitespace differences \
named — correct `old` from that. Match the RAW stored text: a YAML `|` block \
scalar (how messages/file bodies print) adds leading indentation that is NOT part \
of the string, so a line copied from the response carries phantom spaces that \
make `old` miss. edit_message / replace_files remain for wholesale rewrites.

Conflicts: a mutation whose rebase conflicts returns status=conflicts and is held \
IN FULL — git history, HEAD and the working tree untouched until it settles, and \
no other mutation runs meanwhile. If it came from the edit you just issued, \
abort_rewrite and redo is usually cheaper than resolving. Otherwise resolve the \
OLDEST conflicted commit first (read_conflict each resolvable file, remove all \
markers, resolve_conflicts echoing each file's marker_len); fixing the earliest \
often auto-clears descendants. abort_rewrite is the only way out of a structural \
(resolvable=false) conflict.

Creating commits: create_commit inserts a new commit from given file contents \
(empty for an empty commit) — on top of HEAD, or under any commit / at root via \
new_parent. revert_commit inserts a commit's inverse (git revert); \
cherry_pick_commit copies one in (git cherry-pick), its source possibly \
off-branch by full sha. commit_working_copy turns uncommitted changes into a \
commit on HEAD (git commit -a) and is the only way to commit a deterministic \
SUBSET of the tree (paths/hunks/patches). It (and squash_working_copy) captures \
already-tracked files only — a brand-new untracked file is skipped unless named \
in add_paths. A mid-history insert/revert/pick may conflict like any rewrite.

Folding a fix into the commit it belongs to: a fix with a `fixup!`/`squash!`/\
`amend!` subject is routed by suggest_squash_targets; when you don't know which \
commit introduced the fixed code (the common case), blame_squash_targets \
content-blames the touched lines and ranks the owning commits — omit `source` to \
blame the working copy, then pass the top change_id to squash_working_copy (or \
squash_commit for a committed fix). Both are read-only.

Trash: dropped commits go to a session-scoped trash (list_trash), grafted back \
(restore_commit) or folded into a commit (squash_commit).

Safety net: every landed mutation is a recorded operation — list_operations \
lists them, undo/redo step the cursor, jump_to_operation travels to any (0 rolls \
the session back to its start). Only discard_working_copy is unrecoverable.

Uncommitted changes are first-class: they ride through every rewrite \
automatically (working_copy_status shows them; session_diff shows all this \
session changed, committed and uncommitted, against the session-start tree). Git \
state is imported at startup and the session catches up on the next tool call \
when you commit on HEAD with plain git — so a plain commit needs no reload. \
reload_repo(session, …) is only for out-of-band changes it can't absorb: a branch \
switch, or history rewritten by `git rebase`/`reset`/`commit --amend`. It \
restarts THAT session, discarding its trash, op log and any pending rewrite \
(other sessions untouched); switching its branch re-keys the session to the new \
branch's short-name (returned in the result).

Verifying a rewrite: a topology-changing mutation (reorder, squash, split, drop, \
restore, create, revert, cherry_pick, merge_out, squash_working_copy) returns a \
`topology` slice on a clean save — the affected commits with their new parents \
and children by change_id, plus `merge_tip` when the new tip is a merge — so you \
confirm the shape in place, no follow-up list_history. commit/squash_working_copy \
also hand back the new commit and/or remaining working copy; plain \
message/identity/file edits omit it (shape unchanged). show_graph reads that same \
shape for the whole branch on demand.

Reading results: every tool result is YAML. Long multi-line strings (diffs, file \
contents) render as a literal block scalar, or — when a line carries a tab or \
trailing whitespace — as a YAML sequence, one string per line; reassemble by \
joining its entries with newlines.";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CommeditServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("commedit", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}
