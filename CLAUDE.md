# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

comm(ed)it is a GTK4 desktop app for visually editing the *history* of a git
repo — any commit in the graph, not just the latest. Pick a commit, edit its
message, identity, or file content (as an editable unified diff), or reorder it
by drag-and-drop; saving rewrites that commit in place and auto-rebases its
descendants. See `README.md` for the user-facing pitch.

## Commands

```sh
cargo build                      # build the workspace
cargo test                       # all tests (engine unit + integration)
cargo test -p commedit-engine    # engine only
cargo test --test rewrite        # one integration test binary (open/history/tree/rewrite)
cargo test plan_reorder          # tests matching a name
cargo run -- /path/to/repo       # launch the GTK app against a repo (defaults to ".")
```

The GTK crate needs system GTK4 / libsourceview5 development libraries present.

## Architecture

Two crates, split so the rewrite logic carries **no GTK dependency** and is
unit-testable headless:

- **`commedit-engine`** — all repository logic. Built on `jj-lib` (jujutsu).
- **`commedit-gtk`** — the UI (binary `commedit`). Depends on the engine.

### The jj-over-git "transparency" model (the central idea)

The engine operates on a **colocated jj+git repo**: jj does rewrite+rebase, but
plain `git` must keep seeing an ordinary, attached-HEAD repository the whole
time. This invariant drives much of the code:

- `repo.rs` — `Repo::open` attaches jj to an existing `.git` (or inits a
  colocated workspace), imports git refs/HEAD into jj's view, and holds the
  `Workspace` + `ReadonlyRepo`. Every mutating flow commits a jj transaction and
  replaces `self.repo` with the result.
- `transparency.rs` — the glue that hides jj from git: re-attach HEAD to its
  original branch (jj uses detached HEAD by design), export jj bookmarks to git
  refs, exclude `.jj/` via `.git/info/exclude`, and `git read-tree -m -u` the
  working tree to the rewritten tip. The post-rewrite invariant verified by
  tests is: HEAD symbolic + `git status` clean + `git fsck` passes.

### Mutation pipeline (every edit follows the same shape)

`rewrite.rs` / `tree.rs` all do: load target commit → `start_transaction` →
`rewrite_commit(...).write()` (or `move_commits` for reorder) →
`rebase_descendants()` → `self.finish_mutation(tx, ...)`. They return
`Result<SaveOutcome>`, not `Result<()>`. When adding a new kind of edit, mirror
this sequence and end in `finish_mutation`; do **not** call `export_to_git`
inline.

`finish_mutation` (`conflict.rs`) is the shared tail: it commits the jj
transaction, then walks the branch tip's ancestors for `commit.has_conflict()`.
If clean it runs the deferred export (`export_to_git` → `reattach_head` →
`sync_worktree(old_head)` → `prune_orphaned_keep_refs`) in a second transaction
and returns `SaveOutcome::Clean`. If conflicted it stores a `PendingResolution`
and returns `SaveOutcome::Conflicts`, leaving git **completely untouched** — see
"Conflict resolution" below.

- `rewrite_message` / `rewrite_identity` — message + author/committer edits.
  Run identity **last** in a multi-part save: it overrides jj's habit of
  re-stamping the committer to "now".
- `reorder_commit` (`rewrite.rs`) + `plan_reorder` (`history.rs`) — drag-to-reorder.
  Planning (pure index arithmetic on a newest-first list) is separate from the
  rebase. Reorder sets an explicit bookmark move (`set_head_bookmark`) in the
  rewrite transaction — both because the head commit isn't always rewritten, and
  so `finish_mutation` can read the new tip back from the bookmark to scope its
  conflict walk.
- `abandon_commit` / `restore_commit` (`rewrite.rs`) + `plan_drop` /
  `plan_restore` (`history.rs`) — drag-to-trash and drag-back. Dropping records
  an abandon and rebases children onto the commit's parent; the abandoned commit
  object lingers in the ODB (kept reachable so a later restore can graft it
  back). Restore reuses the `reorder_commit` body. Both share the same
  plan-then-rebase shape as reorder.

### Conflict resolution (`conflict.rs`)

`rebase_descendants` can produce commits with conflicted trees. jj's git backend
serializes those as `.jjconflict-*` subtrees, so exporting them would corrupt the
git history. Transparency is preserved purely by **not moving any git ref /
HEAD / worktree while the chain is conflicted** — the conflicted commit objects
sit unreachable in the ODB (like keep-ref residue) and plain `git` keeps seeing
the pre-rewrite history. The deferred export only runs once the whole chain is
clean.

While a `PendingResolution` is held, the UI drives it by **change id** (commit
ids churn on every resolution step): `read_conflict(change_hex, path)`
materializes a file with Git 2-way markers (jj's diff3 base section is stripped);
`resolve_conflict(change_hex, path, text, marker_len)` parses the edit back
(`update_from_content`), splices the resolved tree, re-rebases and re-settles —
returning `Clean` (and auto-exporting) once the last conflict is gone. `abort()`
rolls jj back to the captured pre-rewrite `Operation`; `jj_head_commit_id()`
exposes the pending (not-yet-exported) tip so the UI can display the chain being
resolved. Resolve **oldest-first**: fixing the earliest conflict often
auto-clears its descendants on rebase. Non-file (structural) conflicts can't be
resolved as text — they're flagged `resolvable: false` and the only escape is
`abort`.

### jj-lib is async; we block

`jj-lib`'s backend trait is async but the git backend is synchronous, so the
engine drives every async call to completion with `pollster::block_on(...)`.
Follow that pattern rather than introducing a runtime.

### History view

`history.rs` walks the **ancestors of HEAD only** (`history(repo, head)` with
`head` = `Repo::head_commit_id`, the live branch tip) — like `git log
<current-branch>`. Other local branches, remote-tracking refs (`origin/*`) and
tags off the current branch are intentionally excluded, and only commits on this
chain are droppable/reorderable. Using the live head (not jj's `git_head()`,
which lags a rewrite until re-imported) avoids resurfacing stale, pre-rewrite
commits. `change_id` (stable across rewrites) is what the UI uses to re-select a
commit after a save.

### Structured diff editing (the other hard part)

The diff pane is an *editable* unified diff, with a "firewall" guaranteeing the
buffer always still applies as a patch. Two pure, GTK-free modules:

- `diff.rs` — extract a commit's per-file changes (`commit_changes`), render a
  unified diff with per-hunk expandable context (`render_diff` + `ContextExpansion`
  / `HunkInfo`), classify lines (`classify_line`/`DiffLineKind`), and apply an
  edited patch back (`apply_patch`). `rewrite_file` (`tree.rs`) splices the new
  content into the commit's tree.
- `patch_edit.rs` — `plan_edit(text, selection, gesture)` maps a raw edit gesture
  (Insert/Newline/Backspace/Delete) to a structurally-valid `EditPlan`. Rules:
  only `+` content is freely editable; typing on a context line splits it into a
  `-orig`/`+edited` pair; `@@`/header/meta lines are read-only. Columns are
  *character* offsets where col 0 is the prefix char (matches GTK's
  `iter_at_line_offset`).

`commedit-gtk/src/main.rs` is the whole UI: `build_ui` wires the history list,
message/identity fields, and the SourceView diff buffer, intercepting key events
through `plan_edit` and re-rendering via a boxed `Renderer` closure when hunks
expand.

## Conventions

- Engine integration tests build scratch git repos via `tests/common/mod.rs`
  (`init_repo`, `git`, `git_log_subjects`) and assert against plain `git`.
- The engine never shells out to `git` for *rewrites* (that's jj-lib); it only
  uses `git` CLI in `transparency.rs` for HEAD/worktree/exclude bookkeeping that
  jj-lib doesn't expose cleanly.
- `default_config.toml` (embedded) supplies jj-lib's baseline settings, since
  jj-lib ships no defaults of its own (the jj CLI normally provides them).
