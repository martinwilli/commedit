# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

comm(ed)it is a GTK4 desktop app for visually editing the *history* of a git
repo — any commit in the graph, not just the latest. Pick a commit, edit its
message, identity, or file content (as an editable unified diff), or reorder /
squash it by drag-and-drop; saving rewrites that commit in place and auto-rebases
its descendants. See `README.md` for the user-facing pitch.

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
  refs, exclude `.jj/` via `.git/info/exclude`, and reset the git index to the
  rewritten tip. The post-rewrite invariant verified by tests is: HEAD symbolic +
  `git fsck` passes + `git status` shows exactly the user's uncommitted changes
  (clean when there were none — see "Working-copy preservation").

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
`materialize_after_rewrite(old_head)` → `prune_orphaned_keep_refs`) in a second
transaction and returns `SaveOutcome::Clean`. If conflicted it stores a `PendingResolution`
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
- `squash_into` (`squash.rs`) + `plan_squash` / `squash_recommendations` —
  drag one commit *onto* another to merge it in. Built on jj-lib's native
  `squash_commits` (full-commit selection): the source's changes are applied to
  the target's tree, the source is abandoned, descendants rebase. Preserves the
  target's **author** but lets jj re-stamp the committer (git `--autosquash`
  style); the new message is `compose_squash_message`'d per `SquashMode` (Fixup
  keeps the target's, Squash appends the source's body, Amend replaces with it).
  Unlike reorder it does **not** set the head bookmark — the post-squash tip is
  always a rewrite-descendant of the old head (or, when the source *was* the tip,
  the abandoned tip's parent), which jj's automatic bookmark moves follow. The
  pure, GTK-free, inline-tested helpers (`parse_squash_mode`,
  `squash_target_subject`, `squash_recommendations`, `compose_squash_message`)
  read git's `fixup!`/`squash!`/`amend!` subject prefixes so the UI can recommend
  drop targets and compose the merged message.

### Working-copy preservation (`workcopy.rs`)

Uncommitted changes are first-class: they live in jj's **working-copy commit
`@`**, so a rewrite never loses them. `snapshot_working_copy` (run at `Repo::open`
and at the start of every mutation) re-parents `@` onto the current tip and
snapshots the on-disk tree into it — tracked edits **and** untracked, non-ignored
files; jj skips `.git`/`.jj` and honours `.gitignore` + `.git/info/exclude`. So
`@`-vs-parent *is* the uncommitted delta, which `rebase_descendants` carries
forward (`@`→`@'`) through the rewrite like any other descendant.
`materialize_after_rewrite` (in the deferred export, replacing the old
`sync_worktree`) checks `@'` out to disk via jj and resets the git index to the
new tip. Non-overlapping local edits merge cleanly onto the rewritten content.

An **overlap** (a local edit clashing with the rewrite) makes `@'` a *conflicted*
commit. `collect_conflicts` appends `@'` to the conflict set (it's a child of the
tip, so the ancestor walk misses it; `resolve_change_on_chain` likewise matches
`@`'s change id), so it goes through the **same deferred flow as a commit
conflict**: the whole rewrite is held back (git untouched), `@'` shows up as a
"Uncommitted changes" conflicted commit, and the user resolves it in the diff pane
(or `abort`s the lot). Only when `@'` and the chain are all clean does the
deferred export + materialize run.

Caveats this creates:
- jj has no index concept (it snapshots the disk, never `.git/index`), so staging
  collapses to unstaged after a rewrite. Staged content that lives *only* in the
  index (staged then reverted/deleted on disk) is invisible to `@`, so
  `backup_index_only_content` pins it to a `refs/commedit/backup/index-*` ref
  before the index reset — never lost, recoverable with `git read-tree`. This is a
  **silent** safety net (no stderr, no UI surface — documented in the README); a
  rewrite then `prune_backup_refs` keeps only the most-recent backup so they don't
  accumulate one per session.
- `prune_orphaned_keep_refs` now drops *our own* working-copy keep-refs (the
  current `@`, its superseded snapshots sharing its change id, and jj's empty
  scaffolding) so they don't surface as phantom commits in `git log --all`, while
  still preserving a manual jj user's anonymous head (real content + description,
  unrelated change id).
- The GTK UI shows `@` via `working_copy_info` as a **non-draggable row above the
  history list** — its own single-row list, deliberately *not* part of the history
  list, so the reorder/drop/squash index arithmetic and drag wiring are untouched.
  Its (multi-file) diff is **editable** — Save splits the combined buffer and
  writes each changed file via `edit_working_copy_file`; the branch tip doesn't
  move. During conflict resolution `@` is instead prepended into the conflict chain
  (the standalone row hidden) so it resolves inline like any commit.

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
`resolve_conflicts(change_hex, &[(path, text, marker_len)])` parses each edit back
(`update_from_content`), splices the resolved tree, re-rebases and re-settles —
returning `Clean` (and auto-exporting) once the last conflict is gone
(`resolve_conflict` is the single-file wrapper). `abort()` rolls jj back to the
captured pre-rewrite `Operation`; `jj_head_commit_id()` exposes the pending
(not-yet-exported) tip so the UI can display the chain being resolved. Resolve
**oldest-first**: fixing the earliest conflict often auto-clears its descendants
on rebase. Non-file (structural) conflicts can't be resolved as text — they're
flagged `resolvable: false` and the only escape is `abort`.

The conflict pane shows **all** of the selected commit's conflicted files at once,
as **snippets** (`render_conflict_snippets`) — each file's section is a header
then its `<<< … >>>` blocks with context, the long unconflicted runs elided behind
an expand cue — with the dropdown as a jump aid, mirroring the diff pane. Editing
is free-form *within* snippets, but a guard (`is_conflict_protected_line`) blocks
edits to the layout lines (file headers, elision cues, notices) so the
snippet→full reconstruction keeps its anchors. On Save the whole file is rebuilt
from the shown (edited) segments plus the recorded elided runs
(`reconstruct_conflict_file`, after stripping the inline resolve cues) and the
commit's files resolve together in one `resolve_conflicts` — sound because a
commit's conflicted paths are independent.

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
  / `HunkInfo`, both over the shared `window_groups`), classify lines
  (`classify_line`/`DiffLineKind`), and apply an edited patch back (`apply_patch`).
  `render_commit_diff` lays **all** of a commit's files into one buffer (separated
  by `diff --git` lines; per-file placement in `CombinedFile`) and
  `split_combined_patch` cuts the edited buffer back per file; `rewrite_files`
  (`tree.rs`) splices several files' new content into the tree in one rewrite
  (`rewrite_file` is a one-element wrapper). The conflict pane reuses the same
  windowing: `render_conflict_snippets` shows a conflicted file's `<<< … >>>`
  blocks with context (eliding the unconflicted runs behind a cue) and
  `reconstruct_conflict_file` rebuilds the whole file from the edited snippets plus
  the verbatim elided runs.
- `patch_edit.rs` — `plan_edit(text, selection, gesture)` maps a raw edit gesture
  (Insert/Newline/Backspace/Delete) to a structurally-valid `EditPlan`. Rules:
  only `+` content is freely editable; typing on a context line splits it into a
  `-orig`/`+edited` pair; `@@`/header/meta lines are read-only. Columns are
  *character* offsets where col 0 is the prefix char (matches GTK's
  `iter_at_line_offset`).

`commedit-gtk/src/main.rs` is the whole UI: `build_ui` wires the history list,
message/identity fields, and the SourceView diff buffer, intercepting key events
through `plan_edit` and re-rendering via a boxed `Renderer` closure when hunks
expand. The diff pane shows the **whole change in one buffer**; the file dropdown
is a jump aid — selecting a file scrolls its `diff --git` header to the top
(`scroll_to_file`), scrolling the view updates the dropdown to the file at the top
edge (a `nav_sync` guard stops the two fighting), and `highlight_diff` switches
syntect language per file at each `--- a/PATH`. Save splits the buffer per file
and applies every edit in one `rewrite_files`. History drag-and-drop is
**zone-based** (`show_zone`): a row's top/bottom
quarter opens a reorder gap (the placeholder), its middle half marks a squash
target (`set_squash_target`); dragging an autosquash-prefixed commit highlights
recommended targets green and sibling fixups yellow, and dropping an unprefixed
commit onto another opens the fixup/squash/amend popover (`show_squash_popover`).
A drop only *stages* its rewrite into `post_drag`, run at idle from `drag-end` —
rewriting history mid-gesture frees a row GTK still tracks as the drop target and
segfaults, so `populate_rows` also only hides (never unparents) surplus rows.

## Conventions

- Engine integration tests build scratch git repos via `tests/common/mod.rs`
  (`init_repo`, `git`, `git_log_subjects`) and assert against plain `git`.
- The engine never shells out to `git` for *rewrites* (that's jj-lib); it only
  uses `git` CLI in `transparency.rs` for HEAD/worktree/exclude bookkeeping that
  jj-lib doesn't expose cleanly.
- `default_config.toml` (embedded) supplies jj-lib's baseline settings, since
  jj-lib ships no defaults of its own (the jj CLI normally provides them).
