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

In every case descendants are rebased automatically.

Dropped commits go to a **session trash** and can be grafted back or folded into
another commit. A rewrite whose rebase **conflicts** is held back *in full* — git
history, `HEAD`, and the working tree stay untouched until you resolve it (file
by file, oldest commit first) or abort. And every landed change is a **recorded
operation**: undo/redo, jump to any earlier point, roll the whole session back,
or review everything the session changed as a single diff. If git history moves
out of band (a commit or rebase made outside the agent), a reload picks it up.

Tools surface under the `commedit` server (`open_session`, `list_history`,
`show_commit`, `edit_message`, `replace_in_file`, `edit_commits`, `create_commit`,
`revert_commit`, `cherry_pick_commit`, `reorder_commit`, `squash_commit`,
`split_commit`, `commit_working_copy`, `resolve_conflicts`, `undo`, …). The
server provides its own usage instructions to the agent on connect.

## Bundled skills

The plugin also ships skills that teach an agent *when* to reach for these
workflows — the ones comm(ed)it makes easy. A single clean edit you can address
directly, you drive yourself (each result is self-verifying); the heavier work —
conflicts, multi-step reshuffles, exploration — goes to the **`commedit-operator`
subagent** (below). They load on the matching intent, or invoke one explicitly
(e.g. `/commedit:commit-as-you-go`) to pin it at the start of a run.

- **`commit-as-you-go`** — for a multi-step task that will produce several
  commits: commit each logical unit eagerly as you work (extending or fixing a
  commit later is cheap), rather than writing everything and trying to split one
  big pile at the end (which is hard).
- **`revise-commit`** — reword a message, fix an author/committer or date, or
  edit the file contents of *any* commit reachable from `HEAD`, not just the tip
  `git commit --amend` reaches.
- **`reorder-and-squash`** — tidy a branch before review or merge: reorder
  commits, fold fix/WIP commits in (squash, fixup or amend, including autosquash
  `fixup!`/`squash!`/`amend!` prefixes — or a fix with *no* prefix, by
  content-blame), or drop a commit.
- **`insert-and-revert`** — add to history: create a commit and splice it
  anywhere in the graph, revert a commit, or cherry-pick one from another branch.
- **`resolve-conflicts`** — when a rewrite's rebase conflicts, commedit holds it
  back *in full* (nothing conflicted ever reaches git); resolve the deferred
  conflicts file-by-file, oldest commit first — by a targeted patch of the
  conflict markers (cheap, never retypes the file) or the whole resolved file —
  or abort.
- **`review-and-recover`** — lean on the session safety net: review everything
  the session changed as one diff, undo or jump back through recorded operations,
  recover a dropped commit, or orient yourself in the branch graph.
- **`work-in-worktree`** — reshape a branch's history isolated in a `git
  worktree`, leaving the main checkout untouched. The server hosts one editing
  **session per branch**: `open_session(branch=<the worktree's branch>)` opens a
  session bound to that worktree, which you then address with `session=<id>` on
  every commedit call (the plugin also reminds you on worktree entry). Re-home or
  tear down afterward.

To edit a branch you have **not** checked out — without a worktree at all —
`open_session(branch=<name>)` on a branch checked out nowhere opens it
**off-worktree**: commedit moves only that branch's ref and leaves `HEAD`, the
index and the worktree frozen, so there is no working copy and the working-copy
tools are refused. (The launch session can also start there when the server is run
as `commedit-mcp <path> <branch>`.) A branch can be edited by at most one session
at a time.

## Bundled agent

The plugin also ships a subagent, **`commedit-operator`**, for the history
editing you *shouldn't* do inline. A single mutation you can address directly
(you hold the `change_id`) and expect to land clean — a reword, re-date,
edit-one-file, reorder, squash, drop, create, revert, cherry-pick or merge-out —
you drive yourself: each result is self-verifying (it returns the new
`change_id`, the reshaped `topology`, and for working-copy commits the remaining
uncommitted changes), so no follow-up read is needed. Hand the operator the
*loops, searches and conflicts* instead — "resolve the pending conflict", "squash
the fixup into Y across this messy range", "re-date this whole range", "find where
this fix belongs and fold it in". It picks the tools, performs the action,
confirms it from the returned result, and returns a compact summary (outcome,
affected `change_id`s, what it verified). It owns the details the workflows above
describe — `change_id` addressing, the smallest-tool choice, conflict holds, the
undo/abort safety net — and the bundled skills, which it consults on demand. It
edits history only via commedit and never touches working-tree files itself.
Delegate one operation (or a tightly-related batch) per call.

A plain **new commit on top of HEAD** needs no rebase, so make it with raw
`git add` / `git commit` — or `commit_working_copy` to stay in-session and chain
on its returned `change_id`. Editing an *existing* merge — rewording it, squashing
into it, or moving commits across it — is commedit's; only building a *new* merge
that joins two divergent branches, and managing branches, remotes and pushes, stay
plain-git tasks.

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

**From your organisation.** If an admin uploaded the plugin to your claude.ai
team settings, it appears in members' `/plugin` list and installs per your org's
policy — none of the steps below are needed.

**For yourself.** Download `commedit-plugin.zip` from the [latest
release](https://github.com/martinwilli/commedit/releases) and unpack it into a
directory of its own:

```sh
unzip commedit-plugin.zip -d ~/.local/share/commedit/plugin
```

Then either load it for a single session, or install it persistently.

*Single session* — point Claude Code at the unpacked plugin as you launch it:

```sh
claude --plugin-dir ~/.local/share/commedit/plugin /path/to/your/repo
```

*Persistently, across all sessions* — Claude Code installs plugins from a
marketplace, so register a one-plugin local marketplace pointing at what you
unpacked, then install from it:

```sh
mkdir -p ~/.local/share/commedit/.claude-plugin
cat > ~/.local/share/commedit/.claude-plugin/marketplace.json <<'JSON'
{
  "name": "commedit-local",
  "owner": { "name": "you" },
  "plugins": [{ "name": "commedit", "source": "./plugin" }]
}
JSON
claude plugin marketplace add ~/.local/share/commedit
claude plugin install commedit@commedit-local   # then restart Claude Code
```

Either way, confirm the `commedit` tools are listed under `/plugin`, open a repo,
and ask the agent to list or edit history.

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

**When rebuilding from inside Claude Code**, `rm` the target before copying — do
this proactively: the dogfooded `commedit` server holds the old binary open, so a
plain `cp` fails with `Text file busy`. Unlinking first lets the running process
keep its old inode while the new build lands at a fresh one:

```sh
rm -f plugin/bin/commedit-mcp-linux-x86_64
cp target/release/commedit-mcp plugin/bin/commedit-mcp-linux-x86_64
```

The still-running server keeps serving the *previous* build until you restart
Claude Code, so relaunch to pick up the new one.

**Quick loop — one session (best for prompt / agent / skill tuning).**
`--plugin-dir` reads `plugin/` in place — no marketplace, no snapshot, no
`version` dance — so each relaunch picks up your current source, whether you
rebuilt the binary or only edited an agent / skill / prompt `.md`:

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

Installing **snapshots** the plugin into Claude Code's cache, so *any* edit —
the server binary **or** an agent / skill / prompt `.md` — only takes effect
once you re-snapshot **and restart**:

```sh
cargo build --release -p commedit-mcp   # only if you changed the server
cp target/release/commedit-mcp plugin/bin/commedit-mcp-linux-x86_64
claude plugin update commedit@commedit-local
```

⚠️ `claude plugin update` keys off `plugin.json`'s `version`: when it's
**unchanged** — the usual case mid-development — the update is a **silent
no-op**. It reports success and copies *nothing*, so your edit stays invisible
(an agent-only edit, with no binary rebuild, hits this every time). To force a
fresh snapshot of the *same* version, **uninstall + install** (or bump
`version`):

```sh
claude plugin uninstall commedit@commedit-local
claude plugin install   commedit@commedit-local
```

A refreshed snapshot never reaches an **already-running** session — **restart**
Claude Code to load it (or just use `--plugin-dir`, above, which skips the cache
entirely).
