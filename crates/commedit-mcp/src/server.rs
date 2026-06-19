//! The MCP server type: shared session state, router assembly and the
//! `ServerHandler` implementation with the agent-facing instructions.

use std::sync::{Arc, Mutex};

use commedit_engine::repo::Repo;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool_handler, ServerHandler};

use crate::session::TrashState;

/// One editing session over one repository. Cheap to clone — all state is
/// behind `Arc`s; the `std::sync::Mutex` serializes every tool body (each runs
/// in `spawn_blocking` and takes the lock inside, never across an `.await`).
/// `reload_repo` re-opens the live `repo.workspace_root()` (and can re-home to a
/// sibling worktree), so no separate repo-path needs keeping.
#[derive(Clone)]
pub struct CommeditServer {
    pub(crate) repo: Arc<Mutex<Repo>>,
    pub(crate) trash: Arc<Mutex<TrashState>>,
    tool_router: ToolRouter<Self>,
}

impl CommeditServer {
    pub fn new(repo: Repo) -> Self {
        Self {
            repo: Arc::new(Mutex::new(repo)),
            trash: Arc::new(Mutex::new(TrashState::default())),
            tool_router: Self::router_read()
                + Self::router_mutate()
                + Self::router_workcopy()
                + Self::router_conflict()
                + Self::router_ops(),
        }
    }

    /// A clone of the session's repo handle. `main` grabs this before `serve`
    /// consumes the server, so it can flush the index cache at clean shutdown (see
    /// [`commedit_engine::repo::Repo::flush_index_cache`]).
    pub fn repo_handle(&self) -> Arc<Mutex<Repo>> {
        self.repo.clone()
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
rebase, so for a one-off the simplest tool is plain `git add` + `git commit`. But \
when you are in a commedit session you will keep editing — crystallizing units as \
you go, then refining them — commit_working_copy is just as good and often better: \
it keeps the session coherent and returns the new commit's change_id plus what is \
left uncommitted, ready to chain the next edit. Reach for commedit whenever the \
work touches history that ALREADY EXISTS, or lands a commit BELOW the tip — \
rewording a message, changing an author or date, editing a commit's files, or \
reordering, squashing, splitting, dropping, reverting, cherry-picking or inserting \
a commit. commedit rewrites in place and rebases the descendants for you, on any \
commit reachable from HEAD (not just the tip), so reach for it instead of \
`git commit --amend`, `git rebase -i`, `git cherry-pick` or `git revert`. \
merge_out_commit can introduce a merge above a commit, to organize a linear \
history into a branchy one; building a merge between two real branches, and \
managing branches, worktrees or remotes, stay plain-git tasks too. (commedit \
imports git state at startup but catches up automatically on the next tool call \
when you commit on top of HEAD with plain git — so a plain commit needs no reload. \
reload_repo is only for changes it can't absorb in place: a branch switch, or \
history rewritten out of band by `git rebase`/`reset`/`commit --amend`.)

Off-worktree branches: this session may be editing a branch you have NOT checked \
out — launched as `commedit-mcp <path> <branch>`, or switched via reload_repo's \
`branch`. Then every history edit works exactly as above, but ONLY that branch's \
ref moves: HEAD, the index and the worktree stay frozen, so there is no working \
copy and the working-copy tools (commit_working_copy, squash_working_copy, \
split/discard, edit a working-copy file) are refused. reload_repo reports the \
edited branch and whether the session is worktree_bound.

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
enough to match exactly once; an ambiguous or missing match is rejected — a \
miss reports the closest text with any whitespace/indentation difference named, \
so correct `old` from that rather than re-guessing tabs from a rendered diff. \
edit_message / replace_files remain for wholesale rewrites.

Conflicts: a mutation whose rebase conflicts returns status=conflicts and is \
held back IN FULL — git history, HEAD and the working tree stay untouched \
until it settles. If the conflict came from the mutation you just issued (a \
mistyped replace_in_file, a wrong edit), abort_rewrite and redo it correctly \
is usually cheaper than resolving. Otherwise resolve the OLDEST conflicted \
commit first (read_conflict each resolvable file, remove all markers, \
resolve_conflicts echoing each file's marker_len); fixing the earliest often \
auto-clears descendants, so don't hand-resolve every commit. abort_rewrite \
discards the held rewrite (and is the only way out of a structural, \
resolvable=false conflict). No other mutation runs while pending.

Creating commits: create_commit makes a new commit from given file contents \
(empty for an empty commit) and inserts it — on top of HEAD by default, or under \
any commit / at root via new_parent. revert_commit inserts the inverse of a \
commit (like git revert). cherry_pick_commit copies a commit's change in (like \
git cherry-pick) — the source may be off the current branch, named by its full \
sha. commit_working_copy turns the current uncommitted changes into a commit on \
top of HEAD (like git commit -a) and returns the new commit (sha + change_id) \
plus the remaining working copy. For a one-off whole-tree commit plain git is \
simplest, but inside a session you keep editing it is just as good — it stays \
coherent and hands back the change_id to chain on — and it is the only way to \
commit a deterministic SUBSET of the tree (its paths/hunks/patches selection). It \
captures edits to already-tracked files only, so a brand-new \
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
session-start tree). Git state is imported only at startup, but the session \
catches up automatically on the next tool call when you commit on top of HEAD \
with plain git — so plain commits need no reload. reload_repo is only for \
out-of-band changes it can't absorb in place: a branch switch, or history \
rewritten by `git rebase`/`reset`/`commit --amend`; it starts a fresh session, \
discarding the trash, the operation log and any pending rewrite.

Verifying a rewrite: a topology-changing mutation (reorder, squash, split, drop, \
restore, create, revert, cherry_pick, merge_out, squash_working_copy) returns a \
`topology` slice on a clean save — the affected commits with their new parents and \
children by change_id, plus a `merge_tip` when the new branch tip is a merge — so \
you can confirm the resulting shape in place instead of a follow-up list_history. \
commit_working_copy and squash_working_copy additionally hand back the new commit \
and/or the remaining working copy. Plain \
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
