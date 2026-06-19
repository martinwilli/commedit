---
name: work-in-worktree
description: >-
  Use whenever a `git worktree` is in play for a commedit repo: you want to
  reshape a branch in isolation so the main checkout stays untouched, OR you (or
  the harness) created a worktree some other way — Claude Code's own worktree
  isolation, the `EnterWorktree` tool, or a subagent run with worktree isolation.
  commedit does NOT follow your working directory; it is one server bound to the
  repo it opened, so until you `reload_repo(path=<worktree>)` EVERY commedit tool
  — reads like list_history and show_commit as much as edits — silently operates
  on the ORIGINAL repo, not the worktree. Covers creating the worktree,
  retargeting commedit onto it, and re-homing + teardown.
---

# Work on history in an isolated worktree

> **The commedit server follows `reload_repo path=…`, NOT your working directory.**
> The plugin runs one MCP server bound to the repo it opened. `cd`-ing into a
> worktree, or running a subagent in its own worktree, moves only the *files* — the
> commedit tools still read and rewrite the original repo (a `list_history` there
> shows the *wrong* repo's history, just as quietly as a misplaced edit). To read
> or edit a worktree's history, **retarget the server**: `reload_repo(path=<worktree>)`.
> Because it is the one
> shared server, the retarget is global — every later commedit call, including ones
> from the `commedit-operator` subagent, then operates on the worktree. Drive the
> worktree setup and the retarget yourself; delegate the history editing inside it
> as you normally would.

This is the way to keep the main checkout untouched while you reshape a branch. If
you *don't* need that isolation, skip the worktree — commedit edits the current
checkout in place and catches up to your plain commits automatically (see the
`commit-as-you-go` skill).

## Set up and retarget

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

2. **Point commedit at it:** `reload_repo(path=.worktrees/<branch>)`. Confirm the
   returned `root` is the worktree. The shared server now edits `<branch>`; every
   commedit tool — and the `commedit-operator` subagent — operates there from now on.

3. **Do the work in the worktree:**
   - Edit files **under `.worktrees/<branch>`** (that is where the branch is checked out).
   - A new commit on top of HEAD → plain `git -C .worktrees/<branch> commit` (it needs
     no rebase; the session catches up automatically).
   - History edits → the commedit tools, addressed by **`change_id`**; they land on
     the worktree's branch via the server. Delegating an open-ended reshuffle or a
     conflict to the `commedit-operator` subagent works as usual — it shares this
     server, so it edits the worktree too.

## Don't isolate with a subagent worktree instead

Do **not** reach for the agent runner's own worktree isolation
(`Agent(isolation: "worktree")`) for the commedit part. That spins up a *different*
worktree the shared commedit server knows nothing about, so the subagent's commedit
tools still hit whatever repo the server is pointed at — not that worktree. Always
pair an explicit `git worktree add` with `reload_repo(path=…)`.

The plugin ships a `PostToolUse:EnterWorktree` hook that reminds you to retarget
when a worktree is entered — treat it as a backstop, not a substitute for pairing
the worktree with `reload_repo` yourself.

## Merge back and tear down (ordinary git)

When the branch is done — these are plain-git steps, not commedit's job (building a
merge between two real branches, and managing worktrees, are git tasks):

1. Finish/commit everything in the worktree — it must be **clean** to remove.
2. Merge the branch into your integration branch with git, e.g. from the main
   checkout: `git merge --no-ff <branch>`.
3. **Re-home** the session: `reload_repo(path=<main checkout>)`. The main checkout
   is a worktree of the same repo, so it is a valid target; this also catches the
   session up to the now-advanced branch.
4. `git worktree remove .worktrees/<branch>` and, if you like, `git branch -d <branch>`.

## Keep in mind

- **Retargeting is a mode switch.** Every `reload_repo` starts a fresh session and
  discards the previous repo's trash and op-log (as any reload does). One repo is
  editable at a time; for two repos live at once you would need two sessions.
- **Scope-guarded.** `reload_repo` only re-homes to a worktree of the *same*
  repository (they share a git common dir); an unrelated path is refused.
