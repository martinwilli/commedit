# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

comm(ed)it is a GTK4 desktop app for visually editing the *history* of a git
repo — any commit in the graph, not just the latest. Pick a commit, edit its
message, identity, or file content (as an editable unified diff), or reorder /
squash it by drag-and-drop; saving rewrites it in place and auto-rebases its
descendants.

**Read `README.md` first.** Its *Features* and *How it works* sections are the
user-facing pitch and the conceptual overview — the jj-over-git model, how
conflicts stay out of history, working-copy preservation, the index-backup safety
net, and session time-travel. This file does **not** restate them; it documents how
the code implements them and the non-obvious invariants to keep when changing it.

## Commands

```sh
cargo build                      # build the workspace
cargo clippy --workspace --all-targets  # lint; run before committing
cargo test                       # all tests (engine unit + integration)
cargo test -p commedit-engine    # engine only
cargo test -p commedit-mcp       # MCP server only
cargo test --test rewrite        # one integration test binary (each tests/*.rs is its own)
cargo test plan_reorder          # tests matching a name
cargo run -- /path/to/repo       # launch the GTK app against a repo (defaults to ".")
cargo run -p commedit-mcp -- /path/to/repo  # the MCP server on stdio (defaults to ".")
```

The GTK crate needs system GTK4 / libsourceview5 development libraries present.

Run `cargo clippy --workspace --all-targets` before committing and keep it
warning-free; each commit should build and pass tests on its own.

## Architecture

Three crates, split so the rewrite logic carries **no GTK dependency** and is
unit-testable headless:

- **`commedit-engine`** — all repository logic. Built on `jj-lib` (jujutsu).
- **`commedit-gtk`** — the UI (binary `commedit`). Depends on the engine.
- **`commedit-mcp`** — an MCP stdio server over the engine (binary
  `commedit-mcp`), the agent frontend. A lib + thin bin so tool handlers are
  integration-tested by calling them directly (`tests/*.rs`, scratch repos via
  a copy of the engine's `tests/common`). One process = one session: a
  launch-per-repo `Repo` in `Arc<Mutex<_>>`, every tool body on
  `spawn_blocking` with the lock taken inside. Tools live in `tools/{read,
  mutate,workcopy,conflict,ops}.rs` (one named rmcp router each, combined in
  `server.rs`) and delegate addressing/planning to `session.rs` — commit-ref
  resolution (sha / change id / unique ≥ 4-char prefix, `lookup_ref` deduping
  duplicates to the first entry so history beats the trash) against a fresh
  `history()` read, the session trash with its staged push/remove (applied
  only when a rewrite settles `Clean`), and `plan_splice`, which maps the
  agent semantics "make P the parent" (or `"root"`) onto the graph planner's
  gap-above-P candidates and asks for `child` at a fork. Responses are DTOs in
  `dto.rs` (`convert.rs` maps engine types; **no jj-lib type crosses**; field
  doc comments are the schema descriptions agents read); mutations return the
  status-tagged `SaveResultDto`, whose schema needs the explicit root
  `"type": "object"` MCP requires. Mutations are refused while a conflicted
  rewrite is pending — the conflict tools (commit-ref-keyed, change id
  preferred) or `abort_rewrite` settle it first — and `reload_repo` re-opens
  the repo in place (fresh session) to pick up out-of-band git changes.

### The jj-over-git "transparency" model (the central idea)

The invariant README's *How it works* describes — plain `git` keeps seeing an
ordinary, attached-HEAD repo the whole time — is upheld in the code like this:

- `repo.rs` — `Repo::open` attaches jj **not** to the user's `.git` but to a
  session-local, throwaway git dir whose object store is *shared* with the user's
  repo (a symlinked `objects`, set up by `transparency.rs`'s `init_shared_git_dir`),
  and imports git HEAD plus **only the checked-out branch's** local ref into jj's
  view (`import_git` / `import_some_refs`). Sharing *only* the ODB is the heart of
  the model: jj's rewritten commits land in the user's ODB (so plain `git` sees
  them), while everything jj would otherwise scribble into the user's `.git` — its
  repo store + working-copy state and every ref it writes (`refs/jj/keep/*` GC
  anchors, its detached HEAD, the bookmark export) — stays in the throwaway dir.
  `init_detached` spins up that fresh jj workspace under a `TempDir` (held as
  `Repo::_workdir`, RAII-deleted on session end) whose checkout target is the
  user's worktree but whose state lives outside it — so a real jj user's `.jj` is
  untouched, a non-jj user's tree isn't polluted (not even a transient `refs/jj`),
  no stale jj state survives between sessions, and concurrent sessions can't share
  a divergent op log. It reuses jj-lib's lower-level init primitives (no
  high-level constructor separates checkout target from state location) — **the
  one place sensitive to a jj-lib bump**.
- The import is **scoped to the current branch**: commedit only displays/edits
  HEAD's ancestors, so sibling branches/tags are left exactly where git has them
  and no jj-level bookmark confinement is needed. jj exports the moved branch into
  the *throwaway* git dir, so the mutation tail **mirrors that tip back into the
  user's repo** with `bridge_branch_to_git` (a compare-and-swap `git update-ref`,
  run before the worktree is materialized so the user's HEAD already resolves to
  the new tip). The only other safety net is the git-level head backstop
  (`protect_unrelated_heads` / `restore_unrelated_heads`).
- Given a path *inside* a repo, `find_git_root` walks up to the enclosing `.git`
  (like `git` itself) and **refuses a path with no repo above it** rather than
  initializing one — commedit edits existing history, it never spawns a repo.
- Every mutating flow commits a jj transaction and replaces `self.repo`.
- `transparency.rs` — the glue that hides jj from git: re-attach HEAD to its
  original branch (jj uses detached HEAD by design), export jj bookmarks to git
  refs, reset the git index to the rewritten tip. The post-rewrite invariant
  tests assert: HEAD symbolic + `git fsck` passes + `git status` shows exactly the
  user's uncommitted changes (clean when there were none).

### Mutation pipeline (every edit follows the same shape)

`rewrite.rs` / `tree.rs` all do: load target commit → `start_transaction` →
`rewrite_commit(...).write()` (or `move_commits` for reorder) →
`rebase_descendants()` → `self.finish_mutation(tx, ...)`. They return
`Result<SaveOutcome>`, not `Result<()>`. When adding a new kind of edit, mirror
this sequence and end in `finish_mutation`; do **not** call `export_to_git`
inline.

`finish_mutation` (`conflict.rs`) is the shared tail: it commits the jj
transaction, then walks the branch tip's ancestors for `commit.has_conflict()`.
Clean → it runs the deferred export (`export_to_git` → `bridge_branch_to_git` →
`reattach_head` → `materialize_after_rewrite(old_head)`) in a second transaction
and returns `SaveOutcome::Clean`. Conflicted → it stores a `PendingResolution`,
returns `SaveOutcome::Conflicts`, and leaves git **completely untouched** (see
"Conflict resolution"). A clean save also records a session op (see "Session
op-log").

- `rewrite_message` / `rewrite_identity` — message + author/committer edits. Run
  identity **last** in a multi-part save: it overrides jj's habit of re-stamping
  the committer to "now".
- `reorder_commit` (`rewrite.rs`) + `plan_reorder_candidates` (`history.rs`) —
  drag-to-reorder, anywhere in the merge graph. Planning is pure and runs on the
  graph's lane layout (`graph.rs`): a display gap is crossed by one ancestry line
  per lane (`GraphLayout::boundaries`), and each line is a splice candidate
  `(new_parents=[parent], new_children=line's children)` — one candidate on a
  linear chain, several where parallel merge lanes pass (the UI then asks via a
  colored-line popover). "Dropped onto its own line" yields no candidate (a no-op);
  merge commits are never a drag source; the bottom gap adds a synthetic re-root
  candidate. jj's `move_commits` replaces only the matched parent edge of a merge
  child and keeps the others, so moving a commit out of a merge's ancestry leaves a
  degenerate-but-intact 2-parent merge (ancestor-redundant parents are deliberately
  not simplified). Reorder sets an explicit bookmark move (`set_head_bookmark`) in
  the rewrite transaction — the head commit isn't always rewritten, and it lets
  `finish_mutation` read the new tip back to scope its conflict walk. A top-gap
  splice (no new children) splices between the head and the working-copy chain's
  bottom entry, so uncommitted changes ride onto the new tip.
- `abandon_commit` / `restore_commit` (`rewrite.rs`) + `plan_drop` /
  `plan_restore_candidates` (`history.rs`) — drag-to-trash and drag-back,
  graph-wide: any single-parent commit reachable from head is droppable (its
  children rebase onto its parent), and restore offers the same per-line candidates
  as reorder. The abandoned commit object lingers in the ODB (kept reachable so a
  later restore can graft it back). Restore reuses the `reorder_commit` body.
- `squash_into` (`squash.rs`) + `plan_squash` / `squash_recommendations` — drag
  one commit *onto* another to fold it in, across the whole graph. Built on
  jj-lib's native `squash_commits`: the source's changes apply to the target's tree
  (rebasing across branch lines for cousins on different merge sides — the result
  lands on the target's line), the source is abandoned, descendants rebase. A merge
  is a valid *target* but never a *source*. Preserves the target's **author** but
  lets jj re-stamp the committer (git `--autosquash` style); the message is
  `compose_squash_message`'d per `SquashMode` (Fixup keeps the target's, Squash
  appends the source's body, Amend replaces with it). Unlike reorder it does **not**
  set the head bookmark — the post-squash tip is always a rewrite-descendant of the
  old head, which jj's automatic bookmark moves follow. The pure, inline-tested
  helpers (`parse_squash_mode`, `squash_target_subject`, `squash_recommendations`,
  `compose_squash_message`) read git's `fixup!`/`squash!`/`amend!` subject prefixes
  so the UI can recommend targets and compose the merged message.
- `split_commit` (`split.rs`) — the diff view's "Split" button (enabled only with
  pending diff edits). Takes the same `(path, content)` edits as `rewrite_files`:
  rewrites the target `C` → `C'` to the **edited** tree (keeping its change id /
  message / author), then `new_commit`s `N` holding `C`'s **original** tree as
  `C'`'s child (message `fixup! <subject>`, original author), so `C'` + `N`
  reproduce the original diff and descendants are untouched. The trick is
  `set_rewritten_commit(C, N)`, which **overwrites** the `C → C'` rewrite so
  `rebase_descendants` (and the bookmark and `@`) follow `C → N` — and `N` restores
  the original tree descendants were built on, so the rebase is clean. The tree
  splice is shared with `rewrite_files` via `tree::splice_files_into_tree`;
  `split_message` (pure, inline-tested) builds the message.
- `split_working_copy` (`split.rs`) + `squash_working_copy_into` (`squash.rs`) —
  the same Split button and drag-to-squash, but on an *uncommitted* entry (see
  "Working-copy preservation"). `split_working_copy` runs the identical
  `C→C'`/`new_commit N`/`set_rewritten_commit` recipe on a working-copy entry
  (resolved by stable change id *after* snapshotting, since the leaf `@`'s commit
  id churns), but commits the tx **directly** — like `edit_working_copy_file`, no
  `finish_mutation`/export — so HEAD/refs/index/worktree are untouched and disk
  stays byte-identical; the result is a *chain* of uncommitted entries.
  `squash_working_copy_into` snapshots, resolves the entry, and delegates to
  `squash_into(.., Fixup)`.
- `drop_working_copy` (`workcopy.rs`) — the trashbin's drop for an *uncommitted*
  entry: snapshot, resolve by change id, `record_abandoned_commit` +
  `rebase_descendants`, commit the tx **directly** and re-materialize (same
  git-untouched path). Abandoning the leaf `@` makes jj recreate an empty `@`;
  abandoning an intermediate split-chain entry rebases the deeper entries onto its
  parent. Unlike a dropped *commit* it's **not** restorable (no git object to graft
  back), so the UI neither lists it in the trash nor offers to drag it back.

### Working-copy preservation (`workcopy.rs`)

Uncommitted changes are first-class: they live in jj's **working-copy commit `@`**,
so a rewrite never loses them. `snapshot_working_copy` (run at `Repo::open` and at
the start of every mutation) keeps `@` attached above the current tip and snapshots
the on-disk tree into the leaf `@` — **only edits/deletions to git-tracked files**,
never git's untracked files; jj also skips `.git`/`.jj` and honours `.gitignore` +
`.git/info/exclude`. So `@`-vs-parent *is* the uncommitted delta, which
`rebase_descendants` carries forward (`@`→`@'`) like any other descendant.

The tracked-only scope is enforced by the snapshot's `start_tracking_matcher`
(`tracked_paths_matcher`): commedit's throwaway jj workspace starts with an *empty*
on-disk tree state, so to the first snapshot every file looks brand-new — the
matcher must name exactly the paths in `@`'s parent tip (HEAD's tracked set) so
"track nothing" doesn't drop committed files and "track everything" doesn't pull in
untracked ones. Untracked files stay out of `@` yet **stay alive on disk**: jj never
tracks them, so `materialize_after_rewrite`'s checkout (which only diffs the tracked
trees) never deletes them. `materialize_after_rewrite` (in the deferred export)
checks `@'` out to disk via jj and resets the git index to the new tip — falling
back to a plain `sync_worktree` when there's no working-copy commit. Non-overlapping
local edits merge cleanly onto the rewritten content.

**The working-copy *chain*.** `@` need not sit directly on HEAD: the Split button
(`split_working_copy`) peels `@` into a short linear stack between HEAD and the leaf
`@` — `HEAD → @' (edited subset) → @ (leaf, full disk tree)` — none exported to git.
`working_copy_chain` enumerates these entries (newest first, empty ones filtered);
`working_copy_chain_ids` is the id-only walk. `ensure_working_copy_on_head` keeps the
chain intact (re-attaching only when the single-parent walk from `@` *doesn't* reach
the tip, e.g. plain `git` moved HEAD); the walk stops at the git tip **or** jj's
bookmark tip, since git HEAD lags while a conflicted rewrite is pending. The chain is
**session-local**: it persists in jj's op log, but git only sees the leaf as one
unstaged pile, so `Repo::open` calls `collapse_working_copy_chain` (re-attach `@`
onto HEAD, abandoning intermediates) *before* its snapshot — a fresh session
reconciles to git's single-pile view rather than resurrecting a split git can't
represent.

An **overlap** (a local edit clashing with the rewrite) makes a chain entry a
*conflicted* commit. `collect_conflicts` appends every conflicted chain entry
(they're descendants of the tip, so the ancestor walk misses them), so it goes
through the **same deferred flow as a commit conflict**: the whole rewrite is held
back, the entry shows as a "Uncommitted changes" conflicted commit, and the user
resolves it in the diff pane (or `abort`s). Only when chain and branch are all clean
does the deferred export + materialize run.

Caveats this creates:
- jj has no index concept (it snapshots the disk, never `.git/index`), so staging
  collapses to unstaged after a rewrite. Index-only content (staged, then
  reverted/deleted on disk) is invisible to `@`, so `backup_index_only_content` pins
  the index to a `refs/commedit/backup/index-*` ref before resetting it, and
  `prune_backup_refs` keeps only the most recent. Silent safety net — the recovery
  commands are in the README.
- jj's working-copy commits never surface in the user's `git log --all`: their
  `refs/jj/keep/*` anchors live in the throwaway git dir. Their objects do land in
  the shared ODB but are unreachable from any user ref, so git's own gc reclaims them.
- The GTK UI shows the working-copy chain as **rows above the history list**
  (`populate_wc`, mirrored in `wc_entries`), deliberately *not* part of the history
  list, so the reorder/drop/squash index arithmetic is untouched. A row is editable
  (Save → `edit_working_copy_file`, the tip doesn't move) and splittable (Split →
  `split_working_copy`), and is a drag *source* (`DragOrigin::WorkingCopy`): dropped
  onto a commit it folds in as a fixup — `show_zone` offers it only the red squash
  target, never the blue reorder gap (uncommitted entries can't be reordered into
  history). Dropped onto the trashbin it's discarded (`drop_working_copy`) without
  joining the trash list. During conflict resolution the rows are hidden and each
  conflicted entry resolves inline like any commit.

### Conflict resolution (`conflict.rs`)

`rebase_descendants` can produce commits with conflicted trees, which jj's git
backend serializes as `.jjconflict-*` subtrees — exporting those would corrupt the
git history. So (as README's *How it works* describes) the deferred export simply
**doesn't move any git ref / HEAD / worktree while the chain is conflicted**; the
conflicted objects stay unreachable in the shared ODB and the export runs only once
the whole chain is clean.

A reorder / squash / drop / restore's intermediate rebase can throw **spurious**
conflicts — commits touching adjacent-but-independent lines that jj's symmetric
3-way merge can't place even though the combined result is well-defined. Before
holding such a rewrite back, `settle` tries `try_auto_resolve_spurious` **once**,
opted into per-mutation by a `SpuriousResolve` strategy: `finish_mutation_auto_resolve`
sets `CleanTip` (reorder/squash), `finish_mutation_spurious` sets `Drop` / `Restore`,
and plain `finish_mutation` leaves it `Off` (message/identity/file/split edits hand
any conflict straight to the manual flow). It rebuilds the conflicted range with
**explicit trees** (so jj never re-merges) via `transform_tree` → `replay.rs`'s
asymmetric `replay_change`, replaying `base → theirs` onto `ours` while *trusting
`ours` for context* — the one thing a symmetric 3-way merge can't do. Two modes:

- **`CleanTip`** (reorder/squash) — the net change set is preserved, so the
  post-mutation tip is conflict-free and *is* the result. Anchor on it and peel each
  commit above off the one below (`replay own → parent`, `Dir::Peel`), top-down. A
  conflicted tip means a *true* conflict and bails.
- **`Drop` / `Restore`** — the change set itself changed, so the tip may be
  conflicted and can't anchor anything. Rebuild forward from the clean prefix
  instead, applying each surviving commit's own change onto its rebuilt parent
  (`replay parent → own`, `Dir::Forward`), bottom-up. `Restore` additionally seeds
  the orphaned restored commit's change.

In both modes `@`'s uncommitted delta is carried onto the rebuilt tip. A genuine
overlap, a structural/binary change, or a split working-copy chain returns `None`,
so the rewrite falls back to the manual flow. The rebuild rewrites the conflicted
range as a **single-parent chain**, so it also bails when that range isn't a
parent-linked single-parent run (a conflicted merge, or a range spanning a fork's
interleaved topo order) rather than silently linearizing it.

While a `PendingResolution` is held, the UI drives it by **change id** (commit ids
churn on every resolution step): `read_conflict(change_hex, path)` materializes a
file with Git 2-way markers (jj's diff3 base section stripped);
`resolve_conflicts(change_hex, &[(path, text, marker_len)])` parses each edit back,
splices the resolved tree, re-rebases and re-settles — returning `Clean` (and
auto-exporting) once the last conflict is gone (`resolve_conflict` is the single-file
wrapper). `abort()` rolls jj back to the captured pre-rewrite `Operation`;
`jj_head_commit_id()` exposes the pending (not-yet-exported) tip so the UI can show
the chain being resolved. Resolve **oldest-first**: fixing the earliest conflict
often auto-clears its descendants on rebase. Non-file (structural) conflicts can't be
resolved as text — flagged `resolvable: false`, the only escape is `abort`.

The conflict pane shows **all** of the selected commit's conflicted files at once, as
**snippets** (`render_conflict_snippets`) — each file's `<<< … >>>` blocks with
context, the long unconflicted runs elided behind an expand cue. Editing is free-form
*within* snippets, but a guard (`is_conflict_protected_line`) blocks edits to the
layout lines so the snippet→full reconstruction keeps its anchors. On Save the whole
file is rebuilt from the shown segments plus the recorded elided runs
(`reconstruct_conflict_file`) and the commit's files resolve together in one
`resolve_conflicts`.

### jj-lib is async; we block

`jj-lib`'s backend trait is async but the git backend is synchronous, so the engine
drives every async call to completion with `pollster::block_on(...)`. Follow that
pattern rather than introducing a runtime.

### History view

`history.rs` walks the **ancestors of HEAD only** (`history(repo, head)` with `head`
= `Repo::head_commit_id`, the live branch tip) — like `git log <current-branch>`.
Other local branches, remote-tracking refs and off-branch tags are intentionally
excluded; every displayed commit is structurally editable except merge commits,
which stay fixed (never a drag source, though a valid squash target). Using the live
head (not jj's `git_head()`, which lags a rewrite until re-imported) avoids
resurfacing stale, pre-rewrite commits. `change_id` (stable across rewrites) is what
the UI uses to re-select a commit after a save.

`graph.rs` lays the list into gitk-style lanes (`compute_graph`, pure lane
arithmetic, no jj access): per row the node lane, the half-row drawing edges, and —
the part planning runs on — `GraphLayout::boundaries`, the `LaneEdge`s (lane,
children, parent) crossing each row's bottom edge. A lane edge usually bundles one
child; converging lines bundle several, and splicing into that line re-parents them
all. The GTK side recomputes the layout in lockstep with `commits` on every refresh
(`Data.graph`), draws it per row in `rows.rs` (`graph_area`, colors cycled by lane
via `lane_color`), and plans drops against it.

### Session op-log, revert & review

`Repo::open` captures the session-start operation (`session_op`) and HEAD
(`session_start_head_hex`). Every clean mutation then records a session op
(`record_op`) carrying an `OpDescriptor` — a label + the change-ids it touched;
while a conflicted rewrite is pending the descriptor waits in `pending_op_desc` and
is recorded only once it finally settles clean (`finalize` settles a still-pending
conflict). `session_ops()` lists the recorded `OpEntry`s (oldest first) and
`op_cursor()` is the live position — `0` is the session-start floor, `len()` the
latest state.

`undo` / `redo` step the cursor; `jump_to_op(target)` travels to any recorded
snapshot; `revert_all` is now just `set_op_cursor(0)`. All funnel through
`set_op_cursor` → `rewind_to_op` → `export_and_sync`: it snapshots the working copy
first (so on-disk edits survive in jj's op log), drops any held conflict (you can't
step the timeline mid-resolution), restores the target view as a *new recorded
operation* (a bare reload would leave a divergent op head that resurfaces the
abandoned state — see `abort`'s note), then re-exports + materializes to git/disk
(clean saves during the session already moved refs, so the rewind must reconcile
back). Every recorded op was a clean exported state, so the rewind always lands
`SaveOutcome::Clean`.

The UI surface (README's *Travel through your edits*) is the header's **"Edit
history"** dropdown (`history_button`): each entry calls `jump_to_op`, the bottom
**"Session start"** floor is `set_op_cursor(0)`. `session_changes` (`repo.rs`) diffs
the current working-copy tree against its session-start counterpart, powering the
read-only **Review** toggle. All of this is a no-op before the first operation.

### Structured diff editing (the other hard part)

The diff pane is an *editable* unified diff, with a "firewall" guaranteeing the
buffer always still applies as a patch. Three pure, GTK-free modules:

- `diff.rs` — extract a commit's per-file changes (`commit_changes`), render a
  unified diff with per-hunk expandable context (`render_diff` + `ContextExpansion`
  / `HunkInfo`), classify lines (`classify_line` / `DiffLineKind`), and apply an
  edited patch back (`apply_patch`). `revert_groups(old, new, first, last)` rebuilds
  `new` with one hunk's change groups dropped back to `old`, backing the *revert
  hunk* / *revert file* cues. `render_commit_diff` lays **all** of a commit's files
  into one buffer (separated by `diff --git` lines; per-file placement in
  `CombinedFile`) and `split_combined_patch` cuts the edited buffer back per file;
  `rewrite_files` (`tree.rs`) splices several files' new content into the tree in one
  rewrite. The conflict pane reuses the same windowing
  (`render_conflict_snippets` / `reconstruct_conflict_file`).
- `patch_edit.rs` — `plan_edit(text, selection, gesture)` maps a raw edit gesture
  (Insert/Newline/Backspace/Delete) to a structurally-valid `EditPlan`. Rules: only
  `+` content is freely editable; typing on a context line splits it into a
  `-orig`/`+edited` pair; `@@`/header/meta lines are read-only. Columns are
  *character* offsets where col 0 is the prefix char (matches GTK's
  `iter_at_line_offset`).
- `tabwidth.rs` — `TabWidthResolver` reads the repo's editor-config files to pick a
  file's display tab width (resolved per file as the user navigates). First match
  wins, so the more specific config beats the global default: `.editorconfig`
  (glob-matched, cascaded, via `ec4rs`) → `.vscode/settings.json` language-specific
  (`[langId].editor.tabSize`, matched by extension) → `.clang-format`
  `TabWidth`/`IndentWidth` (C family) → `.vscode/settings.json` global. Built once at
  `Repo::open` (the GTK side keys off `Repo::workspace_root`).

### GTK module layout

`commedit-gtk` is a **binary crate** (no lib target), so every module is
`mod`-declared in `main.rs`. The file was split by topic to stop `build_ui`'s
growth; new GTK features land in (and are commit-prefixed by) the relevant module,
not in `main.rs`:

- `state.rs` — the shared vocabulary: the enums (`DragOrigin`/`PaneMode`/
  `ConflictCtx`/`ConflictFileView`/`Side`/`DiffCue`/`PendingTrashOp`), the `Renderer`
  alias, the cue/hint `const`s, **and** the four grouped bundles `Widgets`/`Data`/
  `DragState`/`Callbacks` (every field an `Rc` or widget handle, so `Clone` is cheap).
- `buffer_util.rs` — buffer/selection/text helpers (`buffer_text`, `iter_at`,
  `buffer_selection`, `apply_patch_edit`, `splice_buffer_text`, `change_label`).
- `highlight.rs` — the TextTag palette, syntect colouring (`highlight_diff` /
  `highlight_conflict`), and the inline "pill" geometry/painting.
- `rows.rs` — commit/working-copy row build + the drag-safe `populate_*` refreshers
  (the "hide, never unparent" discipline lives in `populate_rows`).
- `identity.rs` — the author/committer identity/date fields and conversions.
- `conflict.rs` — the pure conflict-text helpers **and** the conflict-mode wiring:
  the callback builders (`build_refresh_conflict`/`build_exit_conflict_mode`/
  `build_enter_conflict_mode`/`build_resolve_current`, called by `build_ui` in that
  strict dependency order) and `conflict::wire` (abort + prev/next-conflict nav).
- `dragdrop.rs` — the whole drag-and-drop surface behind `dragdrop::wire`: the
  reorder-gap/squash-target feedback (`show_zone`), the drag sources / drop targets,
  the deferred `post_drag` staging (`run_post_drag`), and the squash/lane popovers.

`build_ui` (in `main.rs`) stays the orchestration hub — widget construction, the
diff-pane render/firewall/navigation closures, `save`/`refresh`, the "Edit history"
dropdown, and `present`. It assembles the four bundles by **cloning its existing
locals** (so a bundle field and the local point at the *same* `Rc`/widget — no
duplicated state), then hands them by reference to `dragdrop::wire`, the conflict
builders, and `conflict::wire`. Those modules clone the individual handles their
closures capture out of the bundles; the staged `post_drag` boxes capture cloned
individual `Rc`s (never a borrow of a bundle). When migrating code that reads
`d.commits.borrow()` etc., **keep the statement-level borrow scoping** the original
had — e.g. `build_resolve_current` binds `repo.borrow_mut()`'s outcome before its
`match` because the arms re-borrow `repo`.

The diff pane shows the **whole change in one buffer**; the file dropdown is a jump
aid — selecting a file scrolls its `diff --git` header to the top (`scroll_to_file`),
scrolling updates the dropdown to the file at the top edge (a `nav_sync` guard stops
the two fighting), and `highlight_diff` switches syntect language per file. Both nav
entry points funnel through `scroll_to_file` / `scroll_to_conflict_file`, where
`apply_tab_width` sets the view's tab width from the repo's editor configs for the
top file. Save splits the buffer per file and applies every edit in one
`rewrite_files`. Each `@@` header carries a *revert hunk* cue and each `diff --git`
line a *revert file* cue (`DiffCue`); clicking one `revert_groups`-rewrites the shown
diff against the *render baseline* (`changes`), while `orig_changes` keeps the
pristine content so Save/Split still see the revert as a divergence to apply — a
revert never saves on its own.

History drag-and-drop is **zone-based** (`show_zone`): a row's top/bottom quarter
opens a reorder gap (shown when the gap has ≥1 lane-edge candidate), its middle half
marks a squash target (`set_squash_target`); dragging an autosquash-prefixed commit
highlights recommended targets green and sibling fixups yellow, and dropping an
unprefixed commit onto another opens the fixup/squash/amend popover
(`show_squash_popover`). A gap drop with several candidates (parallel merge lanes
crossing the gap) opens `show_lane_popover` instead — one color-swatch button per
candidate line, colors matching the drawn lanes (`rows::lane_color`); a single
candidate splices directly. A drop only *stages* its rewrite into `post_drag`, run at
idle from `drag-end` — rewriting history mid-gesture frees a row GTK still tracks as
the drop target and segfaults, so `populate_rows` also only hides (never unparents)
surplus rows.

## Conventions

- Engine integration tests build scratch git repos via `tests/common/mod.rs`
  (`init_repo`, `git`, `git_log_subjects`) and assert against plain `git`.
- The engine never shells out to `git` for *rewrites* (that's jj-lib); it only uses
  the `git` CLI in `transparency.rs` for HEAD/worktree/exclude bookkeeping that
  jj-lib doesn't expose cleanly.
- `default_config.toml` (embedded) supplies jj-lib's baseline settings, since jj-lib
  ships no defaults of its own (the jj CLI normally provides them).
