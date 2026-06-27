# commedit-gtk

The GTK4 UI (binary `commedit`). Depends on `commedit-engine`; pulls in gtk4,
sourceview5, libspelling, syntect. All repository logic lives in the engine —
see `crates/commedit-engine/CLAUDE.md` for the jj-over-git model, the mutation
pipeline, conflict resolution, and the history/planning primitives this UI
drives.

```sh
cargo run -p commedit-gtk -- /path/to/repo          # launch against a repo (defaults to ".")
cargo run -p commedit-gtk -- /path/to/repo feature  # edit an off-worktree branch (path + branch)
```

## Module layout (`commedit-gtk/src/`)

`build_ui` in `main.rs` is the orchestration hub; new GTK features land in topic modules (commit-prefixed by module name):

- `state.rs` — shared enums and the four widget bundles (`Widgets`/`Data`/`DragState`/`Callbacks`). Also the **unified display-row model**: `DisplayRow::{Commit(commit_idx), Wc{branch, entry}}` and the pure index helpers `row_commit_index` (display→commit, `None` for an `@` row), `row_commit_gap` (display gap→commit gap), `first_commit_row`, `find_wc_row` — the single choke points translating list-row indices to the commit space the planners use. See *Working-copy `@` nodes in the DAG*.
- `rows.rs` — commit/`@` row build, `populate_*` refreshers (hide-never-unparent discipline), revert/merge-out/restore hover buttons, lint badge. `populate_history` iterates the interleaved `display` list, building a commit row (`set_row_commit`) or a hollow working-copy `@` lane row (`set_row_wc`/`wc_row_box`) per entry; `graph_area` strokes a *ring* (vs a filled disc) when `hollow[display_idx]`. `populate_trash` still uses the commit-only `populate_rows`.
- `dragdrop.rs` — zone-based drag-and-drop (`show_zone`), squash/lane popovers, deferred `post_drag` (rewrites staged to `drag-end` to avoid mid-gesture segfaults). Also cross-instance drops (foreign payload → cherry-pick at the gap; see *Cross-instance commit dragging*) and **cross-branch drops** (Phase 3): the single-commit reorder arm plans across all editable heads (`plan_reorder_candidates_multi`); an in-branch line Moves straight away, a cross-branch line first asks Copy vs Move via `show_copy_move_popover` (`reorder_commit` vs `cherry_pick_commit`). The squash arm uses `plan_squash_multi` (a fold onto another branch just lands — squash consumes the source, so no Copy/Move). `lane_branches()` builds the per-drop `LaneBranches` map; the hover validators (`show_gap`/`set_squash_target`) use the `_multi` planners so a cross-branch gap/target gives feedback. `show_lane_popover` labels each candidate by branch name. Group (multi-select) drag and trash-restore stay in-branch. **All drag/drop state (`drag_from`/`drag_set`/`drop_gap`/`drop_onto`) is in *display* space** (list rows), translated to commit space at every planner / `commits[...]` site via `row_commit_index`/`row_commit_gap`, with planner *results* (squash recs, blame hint — commit indices) mapped back to rows via `Data.commit_rows`; popover anchoring and placeholder insertion correctly stay in display space (the `to` display gap vs `to_ci`/`onto_ci` commit values). An `@` row drags from the history list itself as `DragOrigin::WorkingCopy` (its branch + entry read from `display` at drop time), folding into a commit (`squash_working_copy_into_at`) or discarding onto the trash (`drop_working_copy_at`) — routed by `wc_target_for_branch`; there is no separate working-copy drag source.
- `lanebranch.rs` — pure lane→branch map for the unified DAG (no GTK/jj-repo access, unit-tested): a commit belongs to every editable branch whose tip reaches it over the displayed commits; `line_is_cross_branch` / `label_for` drive the Copy/Move detection and the lane-popover labels. Built from `editable_branches()` + each branch's `local_branches().head`.
- `dnd.rs` — pure (de)serialization of the cross-window drag payload (`DraggedCommits`); the text form GTK carries between processes.
- `conflict.rs` — conflict-mode callback builders and `conflict::wire` (abort, prev/next nav). In conflict mode the working-copy chain resolves inline, so `build_refresh_conflict` prepends each conflicted `@` entry into `commits` as an ordinary row and builds a trivial 1:1 `display` (all `Commit`, no hollow nodes, `draw_graph` = the plan graph) before `populate_history` — there are no separate `@` nodes while resolving.
- `msglint.rs` — pure commit-message linter; learns repo style from history (`RepoStyle::learn`). GTK-only, no MCP counterpart.
- `search.rs` — pure substring commit search (`search_match` / `highlight_markup`).
- `linenums.rs` — pure gutter line-number logic: diff old/new (`diff_line_numbers`) and conflict ours/theirs (`conflict_line_numbers`).
- `diff_cues.rs` — the `GutterColumn` renderer (the file gutter holds two: old|new). Each column draws *either* a line number *or* a clickable cue button per line — they never coincide, so the buttons sit at the line-number level rather than in extra columns. Also the diff cue geometry (`diff_cue_cells`: expand→col_old, revert→col_new). Conflict cues (resolve, elision) are built in `conflict.rs`.
- `highlight.rs` — TextTag palette and syntect syntax colouring.
- `identity.rs` — author/committer identity/date fields and conversions.
- `spelling.rs` — libspelling glue for the message editor; pins language to keep enchant's personal dictionary stable across sessions.
- `window_state.rs` — persists window geometry (size, maximized, pane positions) across sessions.
- `buffer_util.rs` — buffer/selection/text helpers.

## Cross-instance commit dragging

Opening one repo in several windows (typically one branch each) lets you drag a commit from one onto another's history to cherry-pick it across branches. Windows are separate processes (`main.rs` uses `ApplicationFlags::NON_UNIQUE`), so the history drag carries a text payload (`dnd.rs`: pid + `Repo::object_store_key` + dragged commits by sha) GTK ferries across the boundary — an in-process drop reads the same string back and ignores it, working from the live `drag_*` cells. A drop whose payload pid differs from `std::process::id()` is foreign; if its `repo_key` matches ours (same shared ODB, so the commit is reachable) it's cherry-picked at the gap via `lookup_commit_in_store` → `plan_cherry_pick_candidates` → `cherry_pick_commit` (a *copy*, `DragAction::COPY`, source window untouched). Different `repo_key` ⇒ refused with a status note (separate object stores never meet). Both drop targets read the source row index from `drag_from` (every source sets it), since history travels as text not an `i32`.

## History view — the GTK layer

The engine computes the history and the planners (`crates/commedit-engine/CLAUDE.md` → *History view*); the UI parts are below.

**Editable-set dropdown.** The header branch dropdown *is* the editable-branch set — it drives the live `Repo`'s `EditableSet`, not a read-only fold. It **defaults to just the opened branch**; ticking a branch calls `Repo::set_editable_branches` (an *in-place* widen/narrow that re-seeds + `import_git`s the new branch, or drops one, **preserving the session op-log and trash** — never a full reopen), then `refresh()`. `refresh` seeds its walk from `Repo::editable_branches()` (the set's real bookmark tips, via `local_branches()`): a singleton walks `history_limited`, a wider set unions them with `history_multi`. The dropdown candidates are ordered by recency. **Every commit shown is editable** — there is no view-only gating (the old `is_view_only`/`editable_changes`/row-dimming and the drag-source / revert / merge-out refusals are gone). There is no pinned branch and **no last-branch rule**: unticking the final branch empties the set (`set_editable_branches(&[])` ⇒ `primary` becomes `None`, like a detached-HEAD launch), which simply shows no commits until a branch is re-ticked — a disk no-op, lossless round-trip. (The MCP's "last session can't be closed" is a separate, still-enforced session-registry guard.) A branch shown only as a ref *pill* on an editable lane's ancestry is a git decoration (`commit_refs()`), **not** a dropdown entry or drop target.

**Cross-branch drag (Phase 3).** A history drag that crosses a branch boundary moves or copies the commit across branches; same-branch reorder/squash is unchanged (no chooser). It drives the engine's `_multi` planners over `Repo::editable_heads()` — `plan_reorder_candidates_multi` (move), `plan_insert_candidates_multi` (copy/cherry-pick), `plan_squash_multi` — so a sibling branch's lane is a valid destination; the primary still anchors `new_tip`, so a cross-branch move leaves the primary bookmark put and the sibling rides the rebase (then `bridge_branches_to_git` exports whatever moved). The GTK glue lives in `lanebranch.rs` (pure, unit-tested) + `dragdrop.rs`; see those module notes above.

**Working-copy `@` nodes in the DAG (dual graph).** Every editable worktree's uncommitted `@` is shown as a **hollow lane node** spliced into the one history list directly above its branch tip — there is no separate working-copy list (`wc_list`/`populate_wc`/`refresh_wc` are gone). `Data.commits` stays **pure** (real commits only) so the planners + MCP path see it byte-identically; `refresh` additionally builds, from `Repo::worktree_uncommitted()`, an interleaved `Data.display: Vec<DisplayRow>` (each `@` group spliced above the commit whose id is its oldest entry's parent) plus a **second** `draw_graph = compute_graph(display_as_commitinfos)` and a parallel `hollow: Vec<bool>` — fed *only* to the row drawing areas — and a `commit_rows` (commit→display) map. The list rows mirror `display` 1:1, so a list index is a display index (see the index helpers in `state.rs`). Selecting an `@` row opens the craft-a-commit pane (the old `wc_list` handler logic now lives in `update_selection_pane`, keyed on `selected_wc_branch` + `selected_wc_change`); `select_click` keeps `@` rows out of multi-commit selections (mutually exclusive). Save/Split/fold/discard route through `wc_target_for_branch(branch)` → the engine's `*_at` mutators (Save-with-message uses `commit_working_copy_entry`/`commit_working_copy_entry_at` for its split-chain slice; Split uses `split_working_copy_edits_at`). Split is enabled for **any** editable worktree's `@` rows (launch or sibling), disabled only for a branch with no worktree; there is no GTK partial-commit cue (GTK "partial" is edit-the-diff + Split + commit-an-entry).

## Conventions

- New GTK features land in topic modules, commits prefixed by the module name (e.g. `gtk:` / by-feature; refer to history for patterns).
- **Hide-never-unparent** discipline in the `populate_*` refreshers: reuse rows, toggle visibility — don't unparent/reparent widgets mid-refresh.

## Tests

The pure modules carry inline `#[cfg(test)]` units (`lanebranch.rs`, `dnd.rs`, `msglint.rs`, `search.rs`, `linenums.rs`, the `state.rs` index helpers); there is **no `tests/` dir**. The live app is exercised end-to-end via the dogfood tournament — see `dogfood/CLAUDE.md`.
