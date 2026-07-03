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

/// The agent's manual, served as the MCP `instructions` field. Holds the
/// cross-tool invariants an agent needs on every turn; each tool's own contract
/// lives in its description, and workflow recipes live in the bundled skills.
const INSTRUCTIONS: &str = "\
commedit edits the history of the checked-out git branch in place: any commit \
reachable from HEAD can be reworded, re-authored, have its files edited, or be \
split/reordered/dropped/squashed/reverted/cherry-picked, with descendants rebased \
automatically. New commits can be created and spliced in anywhere. The repository \
stays a plain git repo throughout — no jj state is left behind.

Sessions: this server hosts several independent editing sessions over the one \
repository — one per branch, editable in parallel. EVERY session tool takes a \
required `session` selector (no default): the edited branch's short name (e.g. \
`main`), or `HEAD` for a detached/unborn HEAD — stable across rewrites. \
list_sessions shows what's open; open_session(branch) starts another \
(worktree-bound if that branch is checked out, else off-worktree); close_session \
drops one (not the last). A branch can't be opened twice.

Raw git vs commedit: a NEW commit on top of HEAD needs no rebase — plain `git \
commit` (or commit_working_copy, which keeps the session coherent and hands back \
the new change_id plus what's left uncommitted) is simplest. Reach for commedit \
whenever the work touches EXISTING history or lands a commit BELOW the tip: it \
rewrites in place and rebases descendants, replacing `git commit --amend`, `git \
rebase -i`, `git cherry-pick` and `git revert`. Building a NEW merge between \
divergent branches, and managing branches/worktrees/remotes, stay plain-git tasks.

Off-worktree: a session may edit a branch you have NOT checked out (`commedit-mcp \
<path> <branch>`, or reload_repo's `branch`). Only that branch's ref moves — \
HEAD, index and worktree stay frozen, so there is no working copy and the \
working-copy tools are refused. reload_repo reports the edited branch and \
worktree_bound.

Addressing: a commit ref is its sha or change_id, full or a unique prefix (>= 4 \
chars, case-insensitive). Shas churn as mutations rewrite commits; the change_id \
is STABLE — address by change_id to chain mutations without re-listing. \
list_history pre-abbreviates ids; pass them straight back.

Editing text: prefer the surgical replace_in_message / replace_in_file — an exact \
`old`→`new` substitution (unique unless replace_all) — over rewriting the whole \
text, so untouched content can't drift; a miss reports the closest text with \
whitespace differences named. Gotcha: match the RAW stored text — a YAML `|` \
block scalar (how bodies print) adds leading indentation that is NOT part of the \
string, so a line copied from a response carries phantom spaces that make `old` \
miss. To edit many commits at once, prefer edit_commits — every edit in ONE \
transaction and rebase.

Conflicts: a rewrite whose rebase conflicts returns status=conflicts and is held \
IN FULL — git history, HEAD and worktree untouched, no other mutation until it \
settles. If it came from the edit you just made, abort_rewrite and redo is \
usually cheaper. Otherwise resolve the OLDEST conflicted commit first \
(read_conflict → remove all markers → resolve_conflicts echoing each file's \
marker_len); the earliest often auto-clears its descendants. A structural \
(resolvable=false) conflict can only be aborted.

Uncommitted changes are first-class: they ride through every rewrite \
(working_copy_status shows them; session_diff shows all this session changed vs \
the session-start tree). A plain `git commit` on HEAD is absorbed on the next \
tool call — no reload needed. reload_repo is only for out-of-band changes it \
can't absorb (a branch switch, or history rewritten by `git \
rebase`/`reset`/`--amend`): it restarts that session, discarding its trash, op \
log and any pending rewrite.

Verifying: a topology-changing mutation returns a `topology` slice on a clean \
save — the affected commits' new parents/children by change_id, plus `merge_tip` \
— so you confirm the shape without a follow-up read; show_graph reads the whole \
branch on demand. Every landed mutation is a recorded operation — \
undo/redo/jump_to_operation travel the session (0 = session start); only \
discard_working_copy is unrecoverable.

Reading results: every result is YAML. A long multi-line string (diff, file body) \
renders as a literal block scalar, or — when a line has a tab or trailing \
whitespace — as a YAML sequence of one string per line; rejoin those with \
newlines.";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CommeditServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("commedit", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}
