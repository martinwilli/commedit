---
name: work-in-worktree
description: >-
  Use whenever a `git worktree` is in play for a commedit repo: you want to
  reshape a branch in isolation so the main checkout stays untouched, OR you (or
  the harness) created a worktree some other way — Claude Code's own worktree
  isolation, the `EnterWorktree` tool, or a subagent run with worktree isolation.
  commedit does NOT follow your working directory; it hosts one editing session
  per branch, each addressed by a `session` selector, so the move for a worktree
  is `open_session(branch=<the worktree's branch>)` — a dedicated, worktree-bound
  session — and then pass `session=<id>` on EVERY commedit tool. Covers creating
  the worktree, opening a session on it, and re-homing + teardown.
---

# Work on history in an isolated worktree

> **commedit addresses repos by a `session` selector, NOT by your working directory.**
> The plugin runs one MCP server that hosts an independent editing **session** per
> branch. `cd`-ing into a worktree, or running a subagent in its own worktree, moves
> only the *files* — it opens no session. To read or edit a worktree's history, **open
> a session on its branch**: `open_session(branch=<worktree's branch>)`. Because the
> worktree has that branch checked out, the session opens **worktree-bound** there; it
> returns an id (the branch short-name) that you then pass as `session=<id>` on **every**
> commedit call. There is no implicit default — a tool errors if you pass an unknown or
> closed `session`, so a misaddressed call fails loudly rather than silently hitting the
> wrong repo. Drive the worktree setup and the `open_session` yourself; delegate the
> history editing inside it as you normally would (tell the subagent the session id).

This is the way to keep the main checkout untouched while you reshape a branch. If
you *don't* need that isolation, skip the worktree — commedit edits the current
checkout in place and catches up to your plain commits automatically (see the
`commit-as-you-go` skill).

## Set up and open a session

1. **Create the worktree on a new branch**, off the branch you'll merge back into
   (usually `master`/`main`). Keep worktrees *inside* the repo under a dotted
   `.worktrees/` dir, excluded locally so git, your editor and commedit ignore it:
   ```sh
   echo '.worktrees/' >> .git/info/exclude   # one-time per clone; personal, not committed
   git worktree add -b <branch> .worktrees/<branch> <base>
   ```
   The dir is created on demand and stays invisible: a dotted name keeps it out of
   most tools' search/recursion, and commedit's working-copy snapshot honours
   `.git/info/exclude`, so it is never pulled into the working copy either. Prefer
   the local `.git/info/exclude` over a committed `.gitignore` entry — it's a
   personal workflow choice, not the team's. (If your build tool auto-discovers
   nested packages — e.g. a Cargo workspace with a *globbed* `members` — also exclude
   `.worktrees/` in its config; an explicit member list needs nothing.)

2. **Open a session on it:** `open_session(branch=<branch>)`. Because the worktree has
   `<branch>` checked out, the session opens **worktree-bound** there; confirm the
   returned `root` is the worktree and note the returned id (= `<branch>`). That id is
   the `session` selector you pass on every later commedit call on this branch.

3. **Do the work in the worktree** (passing `session=<branch>` on each commedit tool):
   - Edit files **under `.worktrees/<branch>`** (that is where the branch is checked out).
   - A new commit on top of HEAD → plain `git -C .worktrees/<branch> commit` (it needs
     no rebase; the session catches up automatically).
   - History edits → the commedit tools, addressed by **`change_id`** and the session
     selector; they land on the worktree's branch. Delegating an open-ended reshuffle or
     a conflict to the `commedit-operator` subagent works as usual — give it the session
     id so it addresses the same session.

## Don't isolate with a subagent worktree instead

Do **not** reach for the agent runner's own worktree isolation
(`Agent(isolation: "worktree")`) for the commedit part. That spins up a *different*
worktree the commedit server has no session for, so the subagent's commedit tools
can't address it — there is no session whose branch is checked out there. Always
pair an explicit `git worktree add` with `open_session(branch=…)`.

The plugin ships a `PostToolUse:EnterWorktree` hook that reminds you to open a
session when a worktree is entered — treat it as a backstop, not a substitute for
pairing the worktree with `open_session` yourself.

## Merge back and tear down (ordinary git)

When the branch is done — these are plain-git steps, not commedit's job (building a
merge between two real branches, and managing worktrees, are git tasks):

1. Finish/commit everything in the worktree — it must be **clean** to remove.
2. Merge the branch into your integration branch with git, e.g. from the main
   checkout: `git merge --no-ff <branch>`.
3. **Re-home or close** the session. To re-home it onto a sibling worktree,
   `reload_repo(session=<branch>, path=<main checkout>)` — the main checkout is a
   worktree of the same repo, so it's a valid target; switching the branch this way
   **re-keys** the session (its id becomes the new branch's short-name, returned as
   `session`). Or simply `close_session(session=<branch>)` if you're done with it (the
   registry refuses the **last** remaining session, so at least one always stays open).
4. `git worktree remove .worktrees/<branch>` and, if you like, `git branch -d <branch>`.

## Keep in mind

- **Sessions are independent and concurrent.** Each open session has its own `Repo`,
  trash and op-log; opening one for a worktree does **not** disturb any other. Several
  branches can be live at once — one session each — so you needn't tear one down to
  edit another. (`reload_repo(session=…)` resets only **that** session's trash/op-log.)
- **Scope-guarded.** `open_session` only opens a branch of the launched repo, and
  `reload_repo` only re-homes to a worktree of the *same* repository (they share a git
  common dir); an unrelated branch/path is refused. `open_session` is also refused if a
  session for that branch is already open, or the branch doesn't exist.
