<p align="center">
  <img src="assets/logo.svg" alt="comm(ed)it — the git commit editor" width="600">
</p>


comm(ed)it is a GTK4 desktop application for editing the history of a git
repository directly and visually — not just the latest commit, but any commit
in the graph. Browse the history like in `gitk`, pick a commit, and edit its
message, its author/committer identity and dates, or the actual content of the
files it changed. Saving rewrites that commit in place and automatically rebases
its descendants, so a one-line fix deep in the history is a couple of clicks
rather than an interactive-rebase session.

<p align="center">
  <img src="assets/diffview.png" alt="comm(ed)it editing a commit: history list, identity fields, and an editable diff" width="800">
</p>

The file changes are presented as an editable unified diff. Editing is
*structured*: a firewall intercepts every change to the diff so the result is
always a patch that still applies — typing on a context line splits it into a
removed/added pair, deleting a removed line restores it, and `@@` headers stay
read-only. Each hunk carries an *expand context* control to reveal more of the
surrounding file, and the diff is syntax-highlighted per file type with changed
words tinted within each line. The intent is that you edit hunks intuitively
while never producing a broken patch.

Each hunk also carries a *revert hunk* control, and every file a *revert file*
one, to **drop** those changes from the commit. Reverting doesn't save on its
own — it just rewrites the shown diff — so you then **Save** to drop the changes
or **Split** to peel them into a separate commit. (Reverting re-renders from the
unedited diff, so it discards any in-progress manual edits, just like *expand
context*; to undo reverts, reselect the commit.)

Whenever the diff has pending edits, a **Split** button appears beside **Save**.
Where *Save* rewrites the commit to your edited diff, *Split* keeps that edited
version *and* inserts a new follow-up commit holding everything you changed away
from the original — so the two together still reproduce the commit's original
result and its descendants are untouched, but one commit has become two. Paired
with *revert*, that's how you carve a commit apart: revert the hunks you want to
separate out, then *Split*, and exactly those changes move into a commit of
their own.

You can also **reorder** commits by dragging them in the history, or **drop**
one into the trash (and drag it back to restore it). A reorder or drop is a
real rebase, so it can conflict. When it does, comm(ed)it never writes the
conflict into your git history: the rewrite is held back — `git` still sees your
original, untouched history — and the conflicted files are shown right in the
diff pane with `<<<<<<<` / `=======` / `>>>>>>>` markers. Resolve each by hand or
with the *Use ours / theirs / both* buttons. When a rewrite conflicts across
several files you resolve them one at a time, saving each in turn; the rewrite
is applied to git automatically once the last conflict is cleared, or you can
abort it and leave history exactly as it was. Some conflicts are structural
(a directory, symlink, or submodule rather than text) and can't be resolved in
the diff pane — for those, aborting the rewrite is the only way out.

And you can **squash** one commit into another by dragging it *onto* a commit —
drop on the middle of a row (its top and bottom edges still open a gap to
reorder). A commit marked with git's autosquash prefix — `fixup! <subject>`,
`squash! <subject>` or `amend! <subject>` — lights up its matching target
**green** while you drag (and any sibling autosquash commits aimed at the same
target **yellow**), and folds in immediately when dropped: `fixup!` keeps the
target's message, `squash!` appends the dragged commit's message to it, and
`amend!` replaces it. Dropping an ordinary commit onto another instead opens a
small popup to pick fixup / squash / amend (or cancel — handy if the drop was an
accident). A squash is a rebase too, so it can conflict, and is then held back
and resolved exactly like a reorder.

Your **uncommitted changes** aren't left out of all this. Whatever you've edited
or added on disk but not yet committed appears as its own row (or rows) *above*
the history list — selectable like a commit, with the same editable diff, where
**Save** writes back to the working tree rather than rewriting history. You can
drag such a row **onto a commit** to fold those changes into it (as a fixup),
**Split** it to peel off a piece, or drop it onto the **trash** to discard it.
And because the working copy rides through every rewrite untouched, your changes
are still there — and still uncommitted — once any reorder, squash or edit
finishes.

Finally, the toolbar's **Review** toggle flips the whole window into a
read-only, full-window diff of every content change you've made this session —
the repository as it stands now versus how it was when you opened it — so you
can sanity-check the cumulative result before you call it done.

## Installing a binary release

Pre-built binaries for Linux (x86-64) and macOS (Apple Silicon) are attached to
each [GitHub release](../../releases). They are **dynamically linked** against
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
tar -xzf commedit-linux-x86_64.tar.gz
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

## Keyboard shortcuts

- `Ctrl+S` — save the current edits, rewriting the selected commit in place.
- `Ctrl+D` — in the diff pane, delete the line(s) under the selection (drops
  `+` additions, restores `-` removals to context).

Under the hood, comm(ed)it is built on [jujutsu](https://github.com/jj-vcs/jj)
(`jj-lib`) for its rewrite-and-rebase engine, operating on a transparently
colocated jj+git repository: jj does the heavy lifting, but the working copy
and `git` itself see an ordinary, attached-HEAD git repository the whole time.
The code is split into a headless `commedit-engine` crate (all repository logic,
unit-tested against scratch repos) and a `commedit-gtk` crate (the UI), so the
rewrite logic carries no GTK dependency.

## Disclaimer

This project has been completely vibe-coded. It rewrites git history, and it may
eat your commits and your git repository. Use it only on repositories you can
afford to lose, and keep a backup.

As a recovery anchor, the toolbar's **Revert all** button rolls the whole
session back to the state your repository was in when you opened it — one click
undoes every rewrite, reorder, squash and working-copy edit made since. If a
session goes wrong beyond that (the app crashes, say), `git reflog` still holds
the commit your branch pointed at when you opened it, so a `git reset --hard`
gets you back.

Your uncommitted changes (edits on disk and untracked files) ride through every
rewrite and are restored to the working tree as-is. The one thing the underlying
jj model can't see is content that lives *only* in the git index — a file you
`git add`ed and then changed or removed on disk — so before each rewrite resets
the index, comm(ed)it pins the whole index to a `refs/commedit/backup/index-*`
ref. These are silent, transient safety nets: only the most recent one is kept
(older ones are pruned automatically on the next rewrite), and you almost never
need them. If you do, recover with `git read-tree <ref>` (restage) or
`git checkout <ref> -- .` (write to disk); `git for-each-ref refs/commedit/backup/`
lists any that exist.

## License

comm(ed)it is licensed under the [MIT License](LICENSE).
