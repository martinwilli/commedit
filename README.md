<p align="center">
  <img src="assets/logo.svg" alt="comm(ed)it — the git commit editor" width="600">
</p>


comm(ed)it is a GTK4 desktop application for editing the history of a git
repository directly and visually — not just the latest commit, but any commit
in the graph. Browse the history like in `gitk`, pick a commit, and edit its
message or the actual content of the files it changed. Saving rewrites that
commit in place and automatically rebases its descendants, so a one-line fix
deep in the history is a couple of clicks rather than an interactive-rebase
session.

The file changes are presented as an editable unified diff. Editing is
*structured*: a firewall intercepts every change to the diff so the result is
always a patch that still applies — typing on a context line splits it into a
removed/added pair, deleting a removed line restores it, and `@@` headers stay
read-only. Each hunk carries an *expand context* control to reveal more of the
surrounding file. The intent is that you edit hunks intuitively while never
producing a broken patch.

You can also **reorder** commits by dragging them in the history, or **drop**
one into the trash (and drag it back to restore it). A reorder or drop is a
real rebase, so it can conflict. When it does, comm(ed)it never writes the
conflict into your git history: the rewrite is held back — `git` still sees your
original, untouched history — and the conflicted files are shown right in the
diff pane with `<<<<<<<` / `=======` / `>>>>>>>` markers. Resolve each by hand or
with the *Use ours / theirs / both* buttons; the rewrite is applied to git
automatically once every conflict is gone, or you can abort it and leave history
exactly as it was.

## Building and running

comm(ed)it is a Rust workspace; you need a Rust toolchain and the system
GTK4 and libsourceview5 development libraries (e.g. `libgtk-4-dev` and
`libgtksourceview-5-dev` on Debian/Ubuntu).

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

## License

comm(ed)it is licensed under the [MIT License](LICENSE).
