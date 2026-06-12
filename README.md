<p align="center">
  <img src="assets/logo.svg" alt="comm(ed)it — the git commit editor" width="600">
</p>


comm(ed)it is a GTK4 desktop application for editing the history of a git
repository directly and visually — not just the latest commit, but any commit in
the graph. Browse the history like in `gitk`, pick a commit, edit it, and save:
comm(ed)it rewrites that commit in place and automatically rebases its
descendants. A one-line fix deep in the history is a couple of clicks rather than
an interactive-rebase session.

<p align="center">
  <img src="assets/diffview.png" alt="comm(ed)it editing a commit: history list, identity fields, and an editable diff" width="800">
</p>

## Features

- **Edit any commit in place** — change its message, its author/committer
  identity and dates, or the actual content of the files it changed. Saving
  rewrites the commit and rebases everything built on top of it.

- **Structured diff editing** — file changes are an editable unified diff with a
  firewall that keeps the buffer a patch that still applies: typing on a context
  line splits it into a removed/added pair, deleting a removed line restores it,
  and `@@` headers stay read-only. It's syntax-highlighted per file type with
  changed words tinted inline, and each hunk has an *expand context* control to
  reveal more of the surrounding file.

- **Revert hunks or files** — every changed file carries a *revert* control to
  drop its change from the commit, and every hunk of a modified file one to drop
  just that hunk. It works whatever the change is: a modified file reverts to its
  old content, a file the commit *added* is dropped entirely, and one it *removed*
  is restored. Reverting doesn't save on its own; you then **Save** to drop the
  changes or **Split** to peel them off.

- **Split one commit into two** — when the diff has pending edits, **Split**
  keeps your edited version *and* inserts a follow-up commit holding everything
  you changed away from the original, so the two together still reproduce the
  commit's result and its descendants are untouched. Paired with *revert*, that's
  how you carve a commit apart.

- **Reorder, drop and restore** — drag commits to reorder them in the history,
  drop one into the trash, or drag it back to restore it. Works anywhere in a
  merge graph: side-branch commits move, trash and restore like mainline ones
  (merges keep their shape; the merge commit itself stays put). Where several
  ancestry lines cross the drop gap, a small picker of colored lines asks which
  one to splice into. A trashed commit also offers a hover button (a down arrow)
  that drops it out of the trash and writes its changes back to the working tree
  as **uncommitted** edits — the "uncommit" you'd reach for `git reset --mixed`.

- **Squash by dragging** — drag a commit *onto* another to fold it in. A commit
  with an autosquash prefix (`fixup!` / `squash!` / `amend!`) highlights its
  matching target while you drag and folds in on drop; an ordinary commit opens a
  small fixup / squash / amend picker.

- **Revert a commit in place** — hover a commit's row in the history list and a
  revert button appears at its right edge (aligned down the list); clicking it
  drops a `Revert "…"` commit directly on top of that commit, neutralizing its
  change while keeping both in history (its descendants rebase onto the revert).
  Unlike dropping a commit into the trash, the original stays and the undo is
  recorded explicitly.

- **Conflicts never reach your git history** — a reorder, drop or squash is a
  real rebase, so it can conflict. Spurious conflicts (nearby but independent
  edits) are resolved for you automatically; a genuine one is held back — `git`
  keeps seeing your original, untouched history — and shown right in the diff pane
  with conflict markers and *Use ours / theirs / both* buttons. Resolve them file
  by file; the rewrite reaches git only once the last is cleared, or you abort and
  leave history exactly as it was.

- **Uncommitted changes are first-class** — whatever you've edited or added on
  disk but not committed appears as its own row above the history, with the same
  editable diff (here **Save** writes back to the working tree). Fold it into a
  commit, **Split** off a piece, or drop it onto the trash to discard it — and it
  rides through every rewrite untouched, still uncommitted when you're done.

- **Review before you're done** — the toolbar's **Review** toggle flips the
  window into a read-only, full-window diff of every content change you've made
  this session: the repository now versus how it was when you opened it.

- **Travel through your edits** — the toolbar's **Edit history** button (the
  clock icon) opens a dropdown of every change you've made this session, newest
  first, with a marker on where you are now. Click any entry to roll the
  repository — commits, branch and working tree — back, or forward, to that
  snapshot; hovering one highlights the commit rows it touched. The bottom
  **Session start** entry undoes the whole session at once.

## Installing a binary release

Pre-built binaries for Linux (x86-64 and AArch64) and macOS (Apple Silicon) are
attached to each [GitHub release](../../releases). They are **dynamically linked** against
your system GTK, so they are not self-contained — you need a few runtime
dependencies installed first:

- **`git`** on your `PATH` — comm(ed)it drives the git CLI for working-copy and
  `HEAD` bookkeeping.
- **GTK 4** (≥ 4.10) and **GtkSourceView 5** (≥ 5.4) shared libraries.

Install the dependencies, then unpack the tarball and run it:

```sh
# macOS (Apple Silicon)
brew install git gtk4 gtksourceview5
tar -xzf commedit-macos-aarch64.tar.gz
xattr -dr com.apple.quarantine commedit   # the binary is unsigned; clear Gatekeeper
./commedit /path/to/repo

# Debian / Ubuntu (24.04+; 22.04 ships GTK 4.6, too old)
sudo apt install git libgtk-4-1 libgtksourceview-5-0
tar -xzf commedit-linux-x86_64.tar.gz   # or commedit-linux-aarch64.tar.gz on ARM64
./commedit /path/to/repo
```

The runtime library packages on other common distributions:

| Distribution    | Install command                                                      |
| --------------- | -------------------------------------------------------------------- |
| Fedora          | `sudo dnf install git gtk4 gtksourceview5`                            |
| Arch Linux      | `sudo pacman -S git gtk4 gtksourceview5`                             |
| openSUSE        | `sudo zypper install git libgtk-4-1 libgtksourceview-5-0`            |

Drop the `commedit` binary somewhere on your `PATH` (e.g. `~/.local/bin` or
`/usr/local/bin`) to launch it from anywhere. There is no Windows release.

## Building and running

comm(ed)it is a Rust workspace; you need a Rust toolchain, `git` on your `PATH`,
and the system GTK4 and libsourceview5 **development** libraries (e.g.
`libgtk-4-dev` and `libgtksourceview-5-dev` on Debian/Ubuntu, or `gtk4-devel`
and `gtksourceview5-devel` on Fedora).

```sh
cargo build                 # build the workspace
cargo test                  # run the engine and integration tests
cargo run -- /path/to/repo  # launch the app against a repo (defaults to ".")
```

## Use from AI agents (Claude Code plugin)

The same engine is exposed to AI agents through a [Claude Code
plugin](https://code.claude.com/docs/en/plugins). With it installed, an agent
edits history the way the GTK app does — and then some: edit any commit's
message, identity or file contents, split, reorder, drop, restore and squash
commits, create new commits and revert or cherry-pick existing ones, fold
uncommitted changes in or commit them, drop a commit while keeping its changes
unstaged in the working tree (`drop_commit` with `keep_changes`), walk the
conflict-resolution loop, and
undo any of it — all while the repository stays a plain git repo and conflicted
rewrites are held back from git until they resolve or abort. Every tool
addresses commits by sha or by jj's stable change id (full or a unique prefix),
so the agent can chain mutations by change id as shas churn. Alongside the tools
the plugin ships skills that teach the agent *when* to reach for these workflows
and a `commedit-operator` subagent that drives them — see
[`plugin/README.md`](plugin/README.md) for the full description.

The plugin is self-contained: each [GitHub release](../../releases) attaches
`commedit-plugin.zip`, which bundles a prebuilt server for every supported
target (Linux x86-64, Linux AArch64, macOS Apple Silicon) plus a launcher that
picks the right one — nothing to compile, and the only requirement is `git` on
your `PATH` (the GTK runtime libraries are needed only for the desktop app).

### Installing the plugin

**From your organisation.** If an admin uploaded the zip to your claude.ai team
settings, the plugin appears in members' `/plugin` list and installs per your
org's policy — none of the steps below are needed.

**For yourself.** Download `commedit-plugin.zip` from the [latest
release](../../releases) and unpack it into a directory of its own:

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

## Keyboard shortcuts

- `Ctrl+S` — save the current edits, rewriting the selected commit in place.
- `Ctrl+D` — in the diff pane, delete the line(s) under the selection (drops
  `+` additions, restores `-` removals to context).
- `Ctrl+Z` / `Ctrl+Y` — in the diff pane, undo and redo your edits.
- `Ctrl+Q` — close the window.

## How it works

Under the hood, comm(ed)it is built on [jujutsu](https://github.com/jj-vcs/jj)
(`jj-lib`) for its rewrite-and-rebase engine: jj does the heavy lifting, but the
working copy and `git` itself see an ordinary, attached-HEAD git repository the
whole time. jj's own metadata — and every internal ref it would otherwise write —
is kept entirely out of your repository: it operates on a throwaway directory that
shares only your repo's object database and is discarded when you close the app,
so comm(ed)it never leaves a `.jj` directory or stray `refs/jj/*` behind, and
never disturbs a repo you already manage with jj.
It rewrites only the branch you have checked out; your other local branches and
tags stay exactly where they are (they simply diverge, as they would after a
`git commit --amend`). The code is split into a headless `commedit-engine` crate
(all repository logic, unit-tested against scratch repos) and a `commedit-gtk`
crate (the UI), so the rewrite logic carries no GTK dependency.

That transparency is what keeps conflicts out of your history. While a rewrite is
conflicted, comm(ed)it moves no git ref, `HEAD` or working-tree file: the
conflicted commit objects sit unreachable in the object store and plain `git`
keeps seeing your original history, until the chain resolves clean (then it
exports in one step) or you abort (then nothing happened). *Spurious* conflicts
— adjacent but independent edits that an ordinary 3-way merge can't place even
though the combined result is unambiguous — are reconstructed and resolved
automatically before any of that, so you only ever face conflicts that genuinely
need a decision. Some conflicts are structural (a directory, symlink or submodule
rather than text) and can't be resolved in the diff pane; for those, aborting the
rewrite is the only way out.

Your uncommitted changes ride through every rewrite because jj keeps them in its
working-copy commit and rebases them forward like any other descendant. The one
thing the jj model can't see is content that lives *only* in the git index — a
file you `git add`ed and then changed or removed on disk — so before each rewrite
resets the index, comm(ed)it pins the whole index to a
`refs/commedit/backup/index-*` ref. These are silent, transient safety nets: only
the most recent one is kept (older ones are pruned automatically on the next
rewrite), and you almost never need them. If you do, recover with
`git read-tree <ref>` (restage) or `git checkout <ref> -- .` (write to disk);
`git for-each-ref refs/commedit/backup/` lists any that exist.

## Disclaimer

This project has been completely vibe-coded. It rewrites git history, and it may
eat your commits and your git repository. Use it only on repositories you can
afford to lose, and keep a backup.

As a recovery anchor, the toolbar's **Edit history** dropdown can travel the
repository to any snapshot from this session — and its bottom **Session start**
entry rolls the whole session back to the state your repository was in when you
opened it, undoing every rewrite, reorder, squash and working-copy edit made
since. If a session goes wrong beyond that (the app crashes, say), `git reflog`
still holds the commit your branch pointed at when you opened it, so a
`git reset --hard` gets you back.

## License

comm(ed)it is licensed under the [MIT License](LICENSE).
