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
cargo test --test rewrite        # one integration test binary (each tests/*.rs is its own)
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

The engine operates on jj attached to the user's git repo, but plain `git` must
keep seeing an ordinary, attached-HEAD repository the whole time. This invariant
drives much of the code:

- `repo.rs` — `Repo::open` attaches jj **not** to the user's `.git` but to a
  session-local, throwaway git dir whose object store is *shared* with the user's
  repo (a symlinked `objects`, set up by `transparency.rs`'s `init_shared_git_dir`),
  imports git HEAD and **only the checked-out branch's** local ref into jj's view
  (`import_git` via `import_some_refs`), and holds the `Workspace` + `ReadonlyRepo`.
  Sharing *only* the object database is the heart of the transparency model: jj's
  rewritten commits land in the user's ODB (so plain `git` sees them), while
  everything jj would otherwise scribble into the user's `.git` — its repo store +
  working-copy state **and** every ref it writes (`refs/jj/keep/*` GC anchors, its
  detached HEAD, the bookmark export) — stays in the throwaway dir. `init_detached`
  spins up that fresh jj workspace under a `TempDir` (held as `Repo::_workdir`,
  RAII-deleted on session end) whose checkout target is still the user's worktree
  but whose state + git dir live outside it — so a real jj user's `.jj` is left
  untouched, a non-jj user's tree is not polluted (not even a transient `refs/jj`
  on a browse-only session), and no stale jj state survives between sessions. (It
  reuses jj-lib's lower-level public init primitives because no high-level
  constructor separates the checkout target from the state location; that's the one
  place sensitive to a jj-lib bump.) Because each session is an isolated workspace,
  concurrent commedit sessions can no longer produce a divergent shared op log. The
  import is **scoped to the current branch** — commedit only ever displays/edits
  HEAD's ancestors, so a git ref jj never imports is absent from jj's export diff
  and sibling branches/tags are left exactly where git has them (the same
  divergence `git commit --amend` produces), while jj's commit index is built over
  HEAD's ancestry rather than the whole ref graph. So no jj-level bookmark
  confinement is needed. jj exports the one branch it moved into the throwaway git
  dir, not the user's repo, so the mutation tail **mirrors that branch tip back
  out** with `bridge_branch_to_git` (a compare-and-swap `git update-ref`, run
  before the worktree is materialized so the user's HEAD already resolves to the
  new tip); the only other safety net is the git-level head backstop
  (`protect_unrelated_heads`, backed by `transparency.rs`'s
  `restore_unrelated_heads`). Given a path *inside* a repo it walks up to the
  enclosing `.git` (`find_git_root`, like `git` itself); it **refuses a path with
  no git repo above it** rather than initializing one — commedit edits existing
  history, it never spawns a new repository. Every mutating flow commits a jj
  transaction and replaces `self.repo` with the result.
- `transparency.rs` — the glue that hides jj from git: re-attach HEAD to its
  original branch (jj uses detached HEAD by design), export jj bookmarks to git
  refs, and reset the git index to the rewritten tip. The post-rewrite invariant
  verified by tests is: HEAD symbolic + `git fsck` passes + `git status` shows
  exactly the user's uncommitted changes (clean when there were none — see
  "Working-copy preservation").

### Mutation pipeline (every edit follows the same shape)

`rewrite.rs` / `tree.rs` all do: load target commit → `start_transaction` →
`rewrite_commit(...).write()` (or `move_commits` for reorder) →
`rebase_descendants()` → `self.finish_mutation(tx, ...)`. They return
`Result<SaveOutcome>`, not `Result<()>`. When adding a new kind of edit, mirror
this sequence and end in `finish_mutation`; do **not** call `export_to_git`
inline.

`finish_mutation` (`conflict.rs`) is the shared tail: it commits the jj
transaction, then walks the branch tip's ancestors for `commit.has_conflict()`.
If clean it runs the deferred export (`export_to_git` → `bridge_branch_to_git` →
`reattach_head` → `materialize_after_rewrite(old_head)`) in a second
transaction and returns `SaveOutcome::Clean`. If conflicted it stores a `PendingResolution`
and returns `SaveOutcome::Conflicts`, leaving git **completely untouched** — see
"Conflict resolution" below.

- `rewrite_message` / `rewrite_identity` — message + author/committer edits.
  Run identity **last** in a multi-part save: it overrides jj's habit of
  re-stamping the committer to "now".
- `reorder_commit` (`rewrite.rs`) + `plan_reorder_candidates` (`history.rs`) —
  drag-to-reorder, anywhere in the merge graph. Planning is pure and runs on the
  graph's lane layout (`graph.rs`, see "History view"): a display gap is crossed
  by one ancestry line per lane (`GraphLayout::boundaries`), and each line is a
  splice candidate `(new_parents=[parent], new_children=line's children)` — one
  candidate on a linear chain, several where parallel merge lanes pass (the UI
  then asks via a colored-line popover). The two halves of "dropped onto its own
  line" produce no candidate (the no-op); merge commits are never a drag source
  (their splice has no single line); the bottom gap adds a synthetic re-root
  candidate (the root edge isn't drawn). jj's `move_commits` replaces exactly
  the matched parent edge of a merge child and keeps the others, so moving a
  commit out of a merge's ancestry leaves a degenerate-but-intact 2-parent merge
  (ancestor-redundant parents are deliberately not simplified). Reorder sets an
  explicit bookmark move (`set_head_bookmark`) in the rewrite transaction — both
  because the head commit isn't always rewritten, and so `finish_mutation` can
  read the new tip back from the bookmark to scope its conflict walk. A top-gap
  splice (no new children) splices between the head and the working-copy chain's
  bottom entry instead, so the uncommitted changes ride onto the new tip.
- `abandon_commit` / `restore_commit` (`rewrite.rs`) + `plan_drop` /
  `plan_restore_candidates` (`history.rs`) — drag-to-trash and drag-back, also
  graph-wide: any single-parent commit reachable from head is droppable (its
  children — possibly a merge, which keeps its other parents — rebase onto its
  parent), and a restore offers the same per-line candidates as reorder. The
  abandoned commit object lingers in the ODB (kept reachable so a later restore
  can graft it back). Restore reuses the `reorder_commit` body.
- `squash_into` (`squash.rs`) + `plan_squash` / `squash_recommendations` —
  drag one commit *onto* another to merge it in, across the whole graph. Built
  on jj-lib's native `squash_commits` (full-commit selection): the source's
  changes are applied to the target's tree (rebasing across branch lines for
  cousins on different merge sides — the result lands on the target's line),
  the source is abandoned, descendants rebase. A merge is a valid *target* (the
  change folds into its tree like an evil-merge edit) but never a *source* (its
  own change is its resolution, editable in place). Preserves the
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
- `split_commit` (`split.rs`) — the diff view's "Split" button (left of Save,
  enabled only with pending diff edits). Takes the same `(path, content)` edits as
  `rewrite_files`: rewrites the target `C` → `C'` to the **edited** tree (keeping
  its change id / message / author), then `new_commit`s `N` holding `C`'s
  **original** tree as `C'`'s child (message `fixup! <subject>`, original author),
  so `C'` + `N` reproduce the original commit's diff and descendants are untouched.
  The trick is `set_rewritten_commit(C, N)`, which **overwrites** the `C → C'`
  rewrite `rewrite_commit` recorded so `rebase_descendants` (and the bookmark and
  `@`) follow `C → N` — `N` restores the original tree, the exact base descendants
  were built on, so the rebase is clean. No explicit head-bookmark set is needed
  (unlike reorder): `C` is genuinely rewritten, so jj carries the bookmark. The
  file-blob/tree-splicing step is shared with `rewrite_files` via
  `tree::splice_files_into_tree`; `split_message` (pure, inline-tested) builds the
  message.
- `split_working_copy` (`split.rs`) + `squash_working_copy_into` (`squash.rs`) —
  the same Split button and drag-to-squash, but acting on an *uncommitted* entry
  (see "Working-copy preservation"). `split_working_copy(change_hex, files)` runs
  the identical `rewrite C→C'` / `new_commit N` / `set_rewritten_commit(C, N)`
  recipe on a working-copy entry (resolved by its stable change id *after*
  snapshotting, since the leaf `@`'s commit id churns), but commits the tx
  **directly** — like `edit_working_copy_file`, no `finish_mutation`/export — so
  HEAD/refs/index/worktree are untouched and disk stays byte-identical; the result
  is a *chain* of uncommitted entries. `squash_working_copy_into(change_hex, dest)`
  snapshots, resolves the entry, and delegates to `squash_into(.., Fixup)`; folding
  the whole leaf `@` leaves jj's recreated empty `@` as a clean tree.
- `drop_working_copy` (`workcopy.rs`) — the trashbin's drop for an *uncommitted*
  entry: discard that entry's slice of the changes. Snapshots, resolves the entry
  by change id, `record_abandoned_commit`s it + `rebase_descendants`, then commits
  the tx **directly** and re-materializes (same git-untouched path as
  `split_working_copy`/`edit_working_copy_file`). Abandoning the leaf `@` makes jj
  recreate an empty `@` (a clean tree); abandoning an intermediate split-chain
  entry rebases the deeper entries onto its parent, keeping their changes. Unlike a
  dropped *commit* it's **not** restorable — there's no git object to graft back —
  so the UI neither adds it to the trash list nor offers to drag it back.

### Working-copy preservation (`workcopy.rs`)

Uncommitted changes are first-class: they live in jj's **working-copy commit
`@`**, so a rewrite never loses them. `snapshot_working_copy` (run at `Repo::open`
and at the start of every mutation) keeps `@` attached above the current tip (see
"the working-copy chain" below) and snapshots the on-disk tree into the leaf `@` —
**only edits/deletions to git-tracked files**, never git's untracked files; jj
also skips `.git`/`.jj` and honours `.gitignore` + `.git/info/exclude`. So
`@`-vs-parent *is* the uncommitted delta, which `rebase_descendants` carries
forward (`@`→`@'`) through the rewrite like any other descendant. The tracked-only
scope is enforced by the snapshot's `start_tracking_matcher`
(`tracked_paths_matcher`): commedit's throwaway jj workspace starts with an *empty*
on-disk tree state, so to the first snapshot every file looks brand-new — the
matcher must name exactly the paths in `@`'s parent tip (HEAD's tracked set) so
"track nothing" doesn't drop committed files and "track everything" doesn't pull
in untracked ones. Untracked files are left out of `@` yet **stay alive on disk**:
jj never tracks them, so `materialize_after_rewrite`'s checkout (which only diffs
the tracked trees) never deletes them — they survive a rewrite untouched and git
still sees them as `??`. `materialize_after_rewrite` (in the deferred export,
replacing the old `sync_worktree`) checks `@'` out to disk via jj and resets the
git index to the new tip. Non-overlapping local edits merge cleanly onto the
rewritten content.

**The working-copy *chain*.** `@` need not sit directly on HEAD: the Split button
(`split_working_copy`) peels `@` into a short linear stack of jj commits between
HEAD and the leaf `@` — `HEAD → @' (edited subset) → @ (leaf, full disk tree)` —
none exported to git. `working_copy_chain` enumerates these entries (newest first,
empty ones filtered); `working_copy_chain_ids` is the id-only walk reused below.
`ensure_working_copy_on_head` keeps the chain intact (it re-attaches only when the
single-parent walk from `@` *doesn't* reach the tip, e.g. plain `git` moved HEAD).
The walk stops at the git tip **or** jj's bookmark tip, since git HEAD lags while a
conflicted rewrite is pending. Dragging an entry onto a commit folds it in as a
Fixup (`squash_working_copy_into`); folding the leaf leaves jj's recreated empty
`@`. The chain is **session-local**: it persists in jj's op log, but git only sees
the leaf as one unstaged pile, so `Repo::open` calls `collapse_working_copy_chain`
(re-attach `@` directly onto HEAD, abandoning intermediates) *before* its snapshot
— a fresh session reconciles to git's single-pile view rather than resurrecting a
split git can't represent.

An **overlap** (a local edit clashing with the rewrite) makes a chain entry a
*conflicted* commit. `collect_conflicts` appends every conflicted chain entry
(they're descendants of the tip, so the ancestor walk misses them — appended
oldest-first via `working_copy_chain_ids`; `resolve_change_on_chain` likewise
matches any chain entry's change id), so it goes through the **same deferred flow
as a commit conflict**: the whole rewrite is held back (git untouched), the entry
shows up as a "Uncommitted changes" conflicted commit, and the user resolves it in
the diff pane (or `abort`s the lot). Only when the chain and the branch are all
clean does the deferred export + materialize run.

Caveats this creates:
- jj has no index concept (it snapshots the disk, never `.git/index`), so staging
  collapses to unstaged after a rewrite. Staged content that lives *only* in the
  index (staged then reverted/deleted on disk) is invisible to `@`, so
  `backup_index_only_content` pins it to a `refs/commedit/backup/index-*` ref
  before the index reset — never lost, recoverable with `git read-tree`. This is a
  **silent** safety net (no stderr, no UI surface — documented in the README); a
  rewrite then `prune_backup_refs` keeps only the most-recent backup so they don't
  accumulate one per session.
- jj's working-copy commits (the current `@`, its superseded snapshots, jj's empty
  scaffolding) never surface as phantom commits in the user's `git log --all`,
  because their `refs/jj/keep/*` anchors live in the throwaway git dir, not the
  user's repo (see the `repo.rs` note above) — no scrubbing needed. Their objects
  do land in the shared ODB but are unreachable from any user ref, so git's own gc
  reclaims them.
- The GTK UI shows the working-copy chain via `working_copy_chain` as **rows above
  the history list** (`populate_wc`) — their own list (`wc_entries` mirrors them),
  deliberately *not* part of the history list, so the reorder/drop/squash index
  arithmetic is untouched. Selecting a row shows that entry's diff; it's
  **editable** (Save writes each changed file via `edit_working_copy_file(change_hex,
  …)`, the tip doesn't move) and **splittable** (Split → `split_working_copy`). Each
  row is a drag *source* whose drop onto a commit folds it in as a fixup
  (`DragOrigin::WorkingCopy`): the history list is the drop target, but for these
  drags `show_zone` offers only the red onto-a-commit squash target — never the blue
  reorder gap (uncommitted entries can't be reordered into history). Dropping the
  row onto the **trashbin** instead discards it (`drop_working_copy`); since it
  can't be restored, the trash drop handler runs the discard and refreshes but
  (unlike a dropped commit) never pushes it to `trashed` or repopulates the trash
  list. During conflict resolution the rows are hidden and each conflicted entry is
  prepended into the conflict chain so it resolves inline like any commit.

### Conflict resolution (`conflict.rs`)

`rebase_descendants` can produce commits with conflicted trees. jj's git backend
serializes those as `.jjconflict-*` subtrees, so exporting them would corrupt the
git history. Transparency is preserved purely by **not moving any git ref /
HEAD / worktree while the chain is conflicted** — the conflicted commit objects
sit unreachable in the shared ODB (reclaimed by git's own gc) and plain `git`
keeps seeing the pre-rewrite history. The deferred export only runs once the whole chain is
clean.

A reorder / squash / drop / restore's intermediate rebase can throw **spurious**
conflicts — commits touching adjacent-but-independent lines that jj's symmetric
3-way merge can't place even though the combined result is well-defined. Before
holding such a rewrite back, `settle` tries `try_auto_resolve_spurious` **once**,
opted into per-mutation by a `SpuriousResolve` strategy: `finish_mutation_auto_resolve`
sets `CleanTip` (reorder/squash), `finish_mutation_spurious` sets `Drop` / `Restore`,
and plain `finish_mutation` leaves it `Off` — so message/identity/file edits and
split hand any conflict straight to the manual flow below. It rebuilds the
conflicted range with **explicit trees** (so jj never re-merges) via
`transform_tree` → `replay.rs`'s asymmetric `replay_change`, replaying `base →
theirs` onto `ours` while *trusting `ours` for context* — the one thing a symmetric
3-way merge (jj/git/diff3) can't do. Two reconstruction modes:

- **`CleanTip`** (reorder/squash) — the net change set is preserved, so the
  post-mutation tip is conflict-free and *is* the result. Anchor on it and peel
  each commit above off the one below (`replay own → parent`, `Dir::Peel`),
  top-down. A conflicted tip means a *true* conflict and bails.
- **`Drop` / `Restore`** — the change set itself changed (a commit removed /
  re-inserted), so the tip may be conflicted and can't anchor anything. Rebuild
  forward from the clean prefix instead, applying each surviving commit's own
  original change onto its rebuilt parent (`replay parent → own`, `Dir::Forward`),
  bottom-up — which also keeps the chain order of adjacent insertions. `Restore`
  additionally seeds the orphaned restored commit's change (absent from the
  pre-restore history).

In both modes the working copy `@`'s uncommitted delta is carried onto the rebuilt
tip. A genuine overlap, a structural/binary change, or a split working-copy chain
returns `None` / bails, so the rewrite falls back to the manual flow below. The
rebuild rewrites the conflicted range as a **single-parent chain**, so it also
bails when that range isn't a parent-linked single-parent run — a conflicted
merge, or a range spanning a fork's interleaved topo order, goes to the manual
flow rather than being silently linearized (a linear run *above* a merge still
auto-resolves; the anchor below the range may be a merge, it's only read as a
tree).

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
tags off the current branch are intentionally excluded; every displayed commit
is structurally editable (move/drop/restore/squash) except merge commits, which
stay fixed (never a drag source, though a valid squash target). Using the live
head (not jj's `git_head()`, which lags a rewrite until re-imported) avoids
resurfacing stale, pre-rewrite commits. `change_id` (stable across rewrites) is
what the UI uses to re-select a commit after a save.

`graph.rs` lays the list out into gitk-style lanes (`compute_graph`, pure lane
arithmetic, no jj access): per row the node lane, the half-row drawing edges,
and — the part planning runs on — `GraphLayout::boundaries`, the `LaneEdge`s
(lane, children, parent) crossing each row's bottom edge. A lane edge usually
bundles one child; converging lines (a merge fork reusing a lane already
descending to the same parent) bundle several, and splicing into that line
re-parents them all — the candidates always match the drawn pixels. The GTK
side recomputes the layout in lockstep with `commits` on every refresh
(`Data.graph`), draws it per row in `rows.rs` (`graph_area`, colors cycled by
lane via `lane_color`), and plans drops against it.

### Session revert & review

`Repo::open` captures the session-start operation (`session_op`) and HEAD
(`session_head`, exposed as `session_start_head_hex` for the revert confirmation).
`revert_all` (`conflict.rs`) restores the whole session in one step — backing the
toolbar's **Revert all** button: it drops any pending conflicted rewrite, rewinds
jj's view to `session_op` (recorded as a new op, like `abort`), and — unlike
`abort`, since clean saves during the session already moved git refs / HEAD / the
worktree — runs the same `export_and_sync` tail to materialize the session-start
tree back to git and disk. `session_changes` (`repo.rs`) diffs the current
working-copy tree against its session-start counterpart (the cumulative content
delta), powering the read-only **Review** toggle. Both are no-ops before the first
operation.

### Structured diff editing (the other hard part)

The diff pane is an *editable* unified diff, with a "firewall" guaranteeing the
buffer always still applies as a patch. Two pure, GTK-free modules:

- `diff.rs` — extract a commit's per-file changes (`commit_changes`), render a
  unified diff with per-hunk expandable context (`render_diff` + `ContextExpansion`
  / `HunkInfo`, both over the shared `window_groups`), classify lines
  (`classify_line`/`DiffLineKind`), and apply an edited patch back (`apply_patch`).
  `revert_groups(old, new, first, last)` rebuilds `new` with one hunk's change
  groups dropped back to `old` (group indexing shared with `render_diff`/`HunkInfo`),
  backing the diff view's *revert hunk* / *revert file* cues.
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

### GTK module layout

`commedit-gtk` is a **binary crate** (no lib target), so every module is
`mod`-declared in `main.rs`. The file was split by topic to stop `build_ui`'s
growth; new GTK features land in (and are prefixed in commit messages by) the
relevant module, not in `main.rs`:

- `state.rs` — the shared vocabulary: `DragOrigin`/`PaneMode`/`ConflictCtx`/
  `ConflictFileView`/`Side`/`DiffCue`, the `Renderer` alias, the cue/hint/label
  `const`s, **and** the four grouped bundles `Widgets`/`Data`/`DragState`/
  `Callbacks` (every field an `Rc` or widget handle, so `Clone` is cheap).
- `buffer_util.rs` — buffer/selection/text helpers (`buffer_text`, `iter_at`,
  `buffer_selection`, `apply_patch_edit`, `splice_buffer_text`, `change_label`).
- `highlight.rs` — the TextTag palette, syntect colouring (`highlight_diff`/
  `highlight_conflict`), and the inline "pill" geometry/painting (`pill`,
  `pills_on_line`).
- `rows.rs` — commit/working-copy row build + the drag-safe `populate_*`
  refreshers (the "hide, never unparent" discipline lives in `populate_rows`).
- `identity.rs` — the author/committer identity/date fields and the
  `Identity`↔fields conversions.
- `conflict.rs` — the pure conflict-text helpers (header/cue/block helpers,
  `with_resolve_cues`, the buffer scanners) **and** the conflict-mode wiring: the
  callback builders (`build_refresh_conflict`/`build_exit_conflict_mode`/
  `build_enter_conflict_mode`/`build_resolve_current`, called by `build_ui` in
  that strict dependency order) and `conflict::wire` (abort + prev/next-conflict
  navigation).
- `dragdrop.rs` — the whole drag-and-drop surface behind `dragdrop::wire`: the
  zone reorder-gap/squash-target feedback, the drag sources / drop targets, the
  deferred `post_drag` staging (`run_post_drag`), and the unprefixed-squash
  `show_squash_popover`.

`build_ui` (in `main.rs`) stays the orchestration hub — widget construction, the
diff-pane render/firewall/navigation closures, `save`/`refresh`, and `present`.
It assembles the four bundles by **cloning its existing locals** (so a bundle
field and the local point at the *same* `Rc`/widget — no duplicated state), then
hands them by reference to `dragdrop::wire`, the conflict builders, and
`conflict::wire`. Those modules clone the individual handles their closures
capture out of the bundles; the staged `post_drag` boxes still capture cloned
individual `Rc`s (never a borrow of a bundle). When migrating code that reads
`d.commits.borrow()` etc., **keep the statement-level borrow scoping** the
original had — e.g. `build_resolve_current` binds `repo.borrow_mut()`'s outcome
before its `match` because the arms re-borrow `repo`.

The diff pane shows the **whole change in one buffer**; the file dropdown is a
jump aid — selecting a file scrolls its `diff --git` header to the top
(`scroll_to_file`), scrolling the view updates the dropdown to the file at the top
edge (a `nav_sync` guard stops the two fighting), and `highlight_diff` switches
syntect language per file at each `--- a/PATH`. Save splits the buffer per file
and applies every edit in one `rewrite_files`. Each `@@` header also carries a
*revert hunk* cue and each `diff --git` line a *revert file* cue (`DiffCue`);
clicking one `revert_groups`-rewrites the shown diff against the *render baseline*
(`changes`), while `orig_changes` keeps the pristine content so Save/Split still
see the revert as a divergence to apply — a revert never saves on its own. History
drag-and-drop is **zone-based** (`show_zone` in `dragdrop`): a row's top/bottom
quarter opens a reorder gap (the placeholder — shown when the gap has at least
one lane-edge candidate), its middle half marks a squash target
(`set_squash_target`); dragging an autosquash-prefixed commit highlights
recommended targets green and sibling fixups yellow, and dropping an unprefixed
commit onto another opens the fixup/squash/amend popover (`show_squash_popover`).
A gap drop with several candidates (parallel merge lanes crossing the gap) opens
`show_lane_popover` instead — one color-swatch button per candidate line, colors
matching the drawn lanes (`rows::lane_color`), anchored at the gap's row
boundary; a single candidate splices directly.
A working-copy row dragged onto a commit (`DragOrigin::WorkingCopy`) instead folds
in silently as a fixup — `show_zone` offers it only the red onto-a-commit target,
never the blue reorder gap. A drop only *stages* its rewrite into `post_drag`, run
at idle from `drag-end` — rewriting history mid-gesture frees a row GTK still tracks
as the drop target and segfaults, so `populate_rows` also only hides (never
unparents) surplus rows.

## Conventions

- Engine integration tests build scratch git repos via `tests/common/mod.rs`
  (`init_repo`, `git`, `git_log_subjects`) and assert against plain `git`.
- The engine never shells out to `git` for *rewrites* (that's jj-lib); it only
  uses `git` CLI in `transparency.rs` for HEAD/worktree/exclude bookkeeping that
  jj-lib doesn't expose cleanly.
- `default_config.toml` (embedded) supplies jj-lib's baseline settings, since
  jj-lib ships no defaults of its own (the jj CLI normally provides them).
