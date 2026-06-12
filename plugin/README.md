# comm(ed)it — Claude Code plugin

[comm(ed)it](https://github.com/martinwilli/commedit) edits the history of your
checked-out git branch **in place**. This plugin bundles `commedit-mcp`, its MCP
server, so an agent in Claude Code can rewrite history the way the desktop app
does — and the repository stays a plain git repo the whole time, with no
jujutsu state left behind.

## What the agent can do

Any commit reachable from `HEAD` — not just the tip — is fair game:

- **Edit a commit** — its message, author/committer identity (name, email or
  date), or file contents. Surgical `old`→`new` text substitutions keep a small
  change small, and a bulk form rewrites many commits' messages or dates in one
  atomic pass (e.g. re-dating a whole range).
- **Restructure the branch** — split a commit in two, reorder it anywhere in the
  graph, drop it, or fold one commit into another (squash / fixup / amend,
  honouring git's `fixup!`/`squash!`/`amend!` autosquash prefixes).
- **Add to history** — create a brand-new commit from given file contents and
  splice it in *anywhere* (not only on top), revert a commit (its inverse diff,
  like `git revert`), or cherry-pick a commit's change in — and the source may
  live on *another* branch.
- **Work with uncommitted changes** — they're first-class and ride through every
  rewrite untouched; the agent can also turn them into a new commit, fold them
  into an existing one, do either for only *part* of the changes (hunk by hunk,
  the in-process `git add -p`), or discard them.

In every case descendants are rebased automatically, and the repository stays a
plain git repo the whole time — no jujutsu state left behind.

Dropped commits go to a **session trash** and can be grafted back or folded into
another commit. A rewrite whose rebase **conflicts** is held back *in full* — git
history, `HEAD`, and the working tree stay untouched until you resolve it (file
by file, oldest commit first) or abort. And every landed change is a **recorded
operation**: undo/redo, jump to any earlier point, roll the whole session back,
or review everything the session changed as a single diff. If git history moves
out of band (a commit or rebase made outside the agent), a reload picks it up.

Tools surface under the `commedit` server (`list_history`, `show_commit`,
`edit_message`, `replace_in_file`, `edit_commits`, `create_commit`,
`revert_commit`, `cherry_pick_commit`, `reorder_commit`, `squash_commit`,
`split_commit`, `commit_working_copy`, `resolve_conflicts`, `undo`, …). The
server provides its own usage instructions to the agent on connect.

## Requirements

- **`git`** on your `PATH` — the server drives the git CLI for working-copy and
  `HEAD` bookkeeping.

No GTK or other runtime libraries are needed (that's only for the desktop app).

## Supported platforms

The plugin ships a prebuilt server binary for each of:

- Linux x86-64
- Linux AArch64
- macOS (Apple Silicon)

A small launcher (`bin/launch.sh`) selects the right one at runtime. There is no
Windows build.

## Installing

**From your organisation.** If an admin uploaded this plugin to your team, it
appears in `/plugin` and installs/enables per your org's policy — no manual
steps.

**Locally, for testing.** Point Claude Code at the unpacked plugin directory:

```sh
claude --plugin-dir /path/to/commedit-plugin /path/to/your/repo
```

Then confirm the `commedit` tools are available (`/plugin`), open a repo, and ask
the agent to list or edit history.
