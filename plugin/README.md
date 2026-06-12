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

## Bundled skills

The plugin also ships skills that teach an agent *when* to reach for these
workflows — the ones comm(ed)it makes easy — and to **hand the execution to the
`commedit-operator` subagent** (below) rather than drive the MCP tools from the
main context. They load on the matching intent, or invoke one explicitly (e.g.
`/commedit:commit-as-you-go`) to pin it at the start of a run.

- **`commit-as-you-go`** — for a multi-step task that will produce several
  commits: commit each logical unit eagerly as you work (extending or fixing a
  commit later is cheap), rather than writing everything and trying to split one
  big pile at the end (which is hard).
- **`revise-commit`** — reword a message, fix an author/committer or date, or
  edit the file contents of *any* commit reachable from `HEAD`, not just the tip
  `git commit --amend` reaches.
- **`reorder-and-squash`** — tidy a branch before review or merge: reorder
  commits, fold fix/WIP commits in (squash, fixup or amend, including autosquash
  `fixup!`/`squash!`/`amend!` prefixes), or drop a commit.
- **`insert-and-revert`** — add to history: create a commit and splice it
  anywhere in the graph, revert a commit, or cherry-pick one from another branch.

In every case descendants are rebased automatically, and a rewrite whose rebase
conflicts is held back in full until you resolve or abort it.

## Bundled agent

The plugin also ships a subagent, **`commedit-operator`**, that takes the
commedit-interaction burden off the main agent. Hand it *what* to change — "reword
commit X", "squash the fixup into Y", "reorder Z before W", "re-date this range",
"create a commit from these files below HEAD", "resolve the pending conflict",
"undo the last operation" — and it picks the right tool, performs the action,
**verifies** it landed (through commedit or read-only `git`), and returns a
compact summary (outcome, affected `change_id`s, what it checked). It owns the
details the workflows above describe — `change_id` addressing, the smallest-tool
choice, conflict holds, the undo/abort safety net — and the bundled skills, which
it consults on demand. It edits history only via commedit and never touches
working-tree files itself, so the main agent stays in charge of *what* while the
operator handles *how*. Delegate one operation (or a tightly-related batch) per
call.

Reach for it **instead of running git yourself** for any commit or history
rewrite — `git commit`, `git commit --amend`, `git rebase -i`, `git cherry-pick`,
`git revert` — and not just at the tip. Building merge commits and managing
branches, remotes and pushes stay plain-git tasks.

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

## Developing locally

A source checkout has **no binaries** in `bin/` — the release workflow injects
them and only `launch.sh` is tracked. Build the one for your platform first
(`launch.sh` looks for `commedit-mcp-<os>-<arch>`, with `os` ∈ `linux`/`macos`
and `arch` ∈ `x86_64`/`aarch64`):

```sh
cargo build --release -p commedit-mcp
cp target/release/commedit-mcp plugin/bin/commedit-mcp-linux-x86_64
```

`bin/commedit-mcp-*` is git-ignored, so a local build never dirties the tree.

**Quick loop — one session.** `--plugin-dir` reads `plugin/` in place, so
rebuilding and relaunching picks up the new binary with no extra step:

```sh
claude --plugin-dir plugin /path/to/your/repo
```

**Persistent install — across all sessions.** Register `plugin/` through a local
"directory" marketplace (its plugin `source` must be a path *inside* the
marketplace, so symlink the checkout in), then install from it:

```sh
mkdir -p ~/commedit-marketplace/.claude-plugin
ln -sfn "$PWD/plugin" ~/commedit-marketplace/plugin
cat > ~/commedit-marketplace/.claude-plugin/marketplace.json <<'JSON'
{
  "name": "commedit-local",
  "owner": { "name": "you" },
  "plugins": [{ "name": "commedit", "source": "./plugin" }]
}
JSON
claude plugin marketplace add ~/commedit-marketplace
claude plugin install commedit@commedit-local   # then restart Claude Code
```

Installing **snapshots** the plugin into Claude Code's cache, so after each
rebuild re-copy the binary and refresh the snapshot — then restart to apply:

```sh
cargo build --release -p commedit-mcp
cp target/release/commedit-mcp plugin/bin/commedit-mcp-linux-x86_64
claude plugin update commedit@commedit-local
```

(If `update` won't refresh because `plugin.json`'s `version` is unchanged,
`claude plugin uninstall commedit@commedit-local` and install again.)
