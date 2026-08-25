# commedit-engine

All repository logic, built on `jj-lib` (jujutsu). Lib-only crate — it carries
**no GTK dependency** and is unit-testable headless. The GTK app and the MCP
server are both thin layers over this engine; the MCP surface is a superset of
the GTK app.

The two cross-cutting invariants the root `CLAUDE.md` orients to — the
jj-over-git transparency invariant and the mutation pipeline — have their full
mechanics here.

## The jj-over-git "transparency" model

The core invariant: plain `git` always sees an ordinary, attached-HEAD repo. Key files:

- `repo.rs` — `Repo::open` attaches jj to a throwaway git dir that shares only the ODB with the user's repo (symlinked `objects`). All jj state (op log, refs, detached HEAD) lives in a `TempDir` (`Repo::_workdir`), never touching the user's `.git`. `init_detached`/`load_detached` are the place sensitive to a jj-lib bump.
- `transparency.rs` — post-rewrite glue (`reattach_head`, `bridge_branch_to_git`, index reset) and session setup (`init_shared_git_dir`, `seed_session_head`).
- `Repo::sync_to_git_head` — fast-forward sync for out-of-band `git commit`s on HEAD. `reload_repo` handles branch switches or out-of-band rewrites (heavier full reset).

### Off-worktree branches

`Repo::open_branch(path, cache, Some(branch))` (1-element set over `open_multi`) edits a branch that need *not* be checked out (the GTK/MCP `[PATH] [BRANCH]` arg). `Repo` carries `git_head_branch` (the *checked-out* branch) and an `EditableSet { primary, extra }` of full ref names — the **editable set** of branches imported as real bookmarks (`refs()` iterates primary-first; a singleton set == today's single-branch behavior). `is_worktree_bound()` is `primary == git_head`. The primary bookmark is `current_bookmark()` — imported, rewritten, exported; `head_commit_id()`/`edited_tip()` give the primary's tip (its ref off-worktree, else git HEAD), so history/reorder/squash and the `old_head` compare-and-swap follow the primary. Import widens to every editable bookmark; export is `bridge_branches_to_git(old_head, before)`, mirroring each editable bookmark whose tip changed vs the pre-rewrite `snapshot_heads()` map; `protect_unrelated_heads` exempts the whole set. When the launch branch's tip is unchanged (editing only a sibling) `reattach_head` is a disk no-op. Off-worktree (primary not checked out), the launch worktree's `snapshot_working_copy`/`materialize_after_rewrite` are skipped, the launch working-copy readers return empty/`None`, and the mutating WC ops bail via `require_worktree`.

**Per-worktree symmetry (1b).** Every editable branch checked out in a git worktree *other than the launch one* is mapped — keyed on the worktree path, not the branch name, so even an off-worktree primary is covered — onto its own jj **workspace** (`WorktreeView`: a `Workspace` over the worktree root + a private working-copy state dir + a per-worktree `@`), enumerated via `transparency::worktrees`. `snapshot_working_copy` also snapshots every such worktree (so each one's uncommitted changes ride the rebase); the `export_and_sync` tail re-materializes (`materialize_extra_worktree`: checkout `@'` + reset *that* worktree's index) every extra worktree whose **branch tip actually moved** (the launch worktree keeps its *unconditional* materialize, since its `@` can move without its tip — e.g. `revert_all`). A selected branch with no worktree is a pure ref-move. Editing a branch live in another worktree is therefore now allowed (we keep that worktree in sync) rather than refused.

## Index cache (`index_cache.rs`)

`import_git` serializes full ancestry and is slow on large repos. The cache persists the session's jj `repo/` tree so the next launch primes from it (~35s cold → ~1s warm).

- **Prime-and-flush, never operate-in-place.** Sessions copy the cache into their own `TempDir`; the no-shared-op-log invariant holds. Tests use `IndexCache::Disabled` and never touch `~/.cache`.
- **Keyed** on SHA-256 of the canonical objects-dir path; all worktrees share one entry.
- **Concurrency** via shared/exclusive `flock`; flush is skipped when contended (cheap to forgo).
- Deleting `~/.cache/commedit` is always safe.

## Mutation pipeline

Every edit follows: load target → `start_transaction` → rewrite/`move_commits` → `rebase_descendants()` → `finish_mutation(tx, ...)`. Never call `export_to_git` inline — it belongs inside `finish_mutation` (`conflict.rs`).

`finish_mutation` checks for conflicts: Clean → deferred export to git/worktree + session op recorded; Conflicted → `PendingResolution` stored, git completely untouched until resolved.

Key operations and their source files:
- Message/identity/batch rewrites: `rewrite.rs` (`rewrite_message`, `rewrite_identity`, `rewrite_batch`)
- Reorder / drop / restore: `rewrite.rs` + planning in `history.rs`
- Create / revert / cherry-pick: `create.rs` (shared `insert_new_commit`)
- Squash and autosquash routing: `squash.rs` (`squash_into`, `squash_recommendations`)
- Blame (`blame.rs`): `blame_old_side` annotates the *old* (pre-image) side of a commit's/selection's diff — each line → the commit that last touched it — via jj-lib's `FileAnnotator` (fed the diff's actual pre-image text, so it's correct across merge bases); drives the GTK diff-view blame column. `blame_old_side_path` is its file-scoped form (same base/domain, one file) for the GTK hunk-absorb hint, which only needs the dragged hunk's file. `blame_single_source` is the narrower drag-to-squash hint: when every line a dragged commit removes blames to one commit, the UI highlights it purple — its own first-parent walk, since it answers "do they all agree", not per-line origins. `blame_change_origins` is the ranked, multi-source generalization of that walk (counts every origin instead of bailing at the second): it backs the MCP `blame_squash_targets` content-blame squash-target finder
- Absorb (`absorb.rs`): `absorb_working_copy` routes each uncommitted hunk into the ancestor that introduced its lines and folds them all in one `transform_descendants` pass, over jj-lib 0.43's `jj_lib::absorb` (`split_hunks_to_trees` does the annotation + ambiguity rules, `absorb_hunks` the fold). jj re-anchors `@` itself; a `dry_run` returns the plan without a transaction. Usually clean, so it exits through `finish_mutation_auto_resolve` (a fold with no common merge anchor can conflict and is held)
- Split: `split.rs` (the `set_rewritten_commit` trick makes descendants follow the split child)
- Surgical text replace: `tree.rs` (`replace_in_files`)
- Working-copy commit/fold: `workcopy.rs`
- Message cleanup: `message.rs` (`cleanup_message`) — jj-lib writes a description verbatim, so every write path (`rewrite.rs`, `create.rs`, `split.rs`, `workcopy.rs`, `squash.rs`) runs the message through it first: trailing whitespace off each line, leading/trailing blank lines dropped, exactly one closing newline, which is what `git commit`'s own `--cleanup` guarantees and what messages written straight from a text buffer otherwise lack. Git's comment-stripping and blank-run collapsing are deliberately *not* mirrored (no comment template here, and blank runs are the author's formatting)

## Working-copy preservation (`workcopy.rs`)

Uncommitted changes live in jj's working-copy commit `@` and rebase forward with every mutation. `snapshot_working_copy` runs at open and before each mutation, tracking only git-tracked files (`tracked_paths_matcher`). Untracked files stay on disk untouched.

The `@` chain (created by `split_working_copy`) is session-local; `collapse_working_copy_chain` reconciles it on fresh open. A working-copy overlap with a rewrite enters the same deferred conflict flow as a commit conflict. `carve_working_copy` is the batch of `commit_working_copy_partial`: N `{message, identity, PartialSelection}` entries chained oldest-first on HEAD in one transaction, each commit holding the cumulative selection so its own diff is its entry's slice; `splice_selection_onto` (factored out of `prepare_partial_commit`) builds each cumulative tree, and every entry indexes the same `@`-vs-HEAD diff so hunk indices don't shift.

**A fold is a 3-way merge, so `clean` is not `correct` (`squash.rs`).** A partial fold splices the selection onto HEAD and squashes that into the destination, where jj merges it against the destination's *own* tree — it is not replayed there. A `patches`-tier patch is authored against HEAD, so where a **descendant** reworded the lines it touches the merge reads "both sides removed it", drops the mismatching parts and still returns `Clean`. Hence `SquashOutcome { applied, dest_changes, remaining }`, returned by `squash_into_inner` and `squash_prepared_working_copy_into`: `dest_changes` is the destination's tree before the fold vs after it (`fold_effect`, read off the transaction before commit), so a fold that landed nothing shows an empty list. The same plumbing gives `dry_run` — build the rewrite, read the effect, `drop(tx)` (jj mutates only on commit, absorb's pattern) — exposed as `squash_working_copy_into_ext` / `squash_working_copy_partial_into_ext`. The pre-existing entry points keep their `SaveOutcome` signature via `SquashOutcome::into_applied`. Caveat: `dest_changes` is captured *before* `rebase_descendants`, which is final only when the destination is an ancestor of the source — always true for a working-copy fold, the only caller that reads it.

Each *extra* editable worktree (see *Per-worktree symmetry*) has its own `@` keyed by a per-worktree jj workspace name; `snapshot_extra_worktree`/`materialize_extra_worktree` are the per-worktree analogues. They now mirror the launch path's **out-of-band `git commit` catch-up** (`catch_up_extra_worktrees` + `reanchor_extra_worktree`, the per-worktree `sync_to_git_head`/`ensure_working_copy_on_head`) and its **index-only-content backup** (`backup_index_only_content_at`/`prune_backup_refs_at`, namespaced by a per-worktree key so the launch and each sibling keep independent recovery points). An extra worktree also carries its own `@` **chain**: `split_working_copy_edits_at`/`commit_working_copy_entry_at` peel and commit it piece-by-piece, read by `worktree_chain_ids` and kept intact across snapshots by the chain-aware `reanchor_extra_worktree` (`wc_on_tip`). Only the **`PartialSelection`-based** partial commit/squash (`commit_working_copy_partial`/`squash_working_copy_partial_into`) stays launch/MCP-only. `working_copy_commit_id` and the launch chain/info readers (`working_copy_chain`, `working_copy_info`) stay keyed on the *launch* workspace, so the launch-only chain reconciliation (`collapse_working_copy_chain`) ignores extra worktrees.

**Per-worktree `@` editing.** `worktree_uncommitted() -> Vec<(branch short-name, Vec<WorkingCopyEntry>)>` reads every editable worktree's uncommitted changes for the unified GTK DAG (launch chain under the primary's name — `""` on a detached-HEAD launch — then each extra worktree's single dirty `@`; empty entries skipped). `wc_target_for_branch(branch) -> Option<WcTarget>` (`WcTarget::{Launch, Worktree(branch)}`) routes a working-copy mutation at the right `@`: `Launch` for the primary (worktree-bound; including the detached `""` launch), `Worktree` for an extra worktree, `None` for a branch with no worktree. The **whole-`@`** mutators (`squash_working_copy_into_at` / `drop_working_copy_at` / `edit_working_copy_file_at` / `commit_working_copy_at` / `restore_to_working_copy_at`) each take a leading `target: WcTarget` and mirror their launch counterparts; the helpers `resolve_wc`/`snapshot_wc`/`wc_tip`/`set_target_bookmark`/`materialize_*` dispatch on it. The GTK trash-row "restore to working tree" routes via `restore_to_working_copy_at`: each trashed commit's **origin branch** is recorded at drop time (a `change-id → branch` side-map in `Data.trashed_origin`, populated at the drop sites + on a deferred drop's clean resolution, carried through `PendingTrashOp::Drop`), so a sibling's dropped commit restores into *its* worktree's `@` (an origin branch with no worktree is refused; no recorded origin falls back to `Launch`). **Split** (`split_working_copy_edits_at`) and **per-entry commit** (`commit_working_copy_entry_at`) take a `target` too — so a sibling worktree carries a full `@` *chain* (`worktree_chain_ids`, kept intact by the chain-aware `reanchor_extra_worktree`), split and committed slice-by-slice exactly like the launch chain. Only the `PartialSelection`-based partial commit/squash stays **launch/MCP-only** (no GTK counterpart).

## Conflict resolution (`conflict.rs`, `replay.rs`)

Conflicted trees are never exported to git — git refs/HEAD/worktree stay frozen until the whole chain is clean. Resolve via `read_conflict`/`resolve_conflicts` (oldest-first) or `abort_rewrite`; all other mutations are refused while a `PendingResolution` is held. `read_conflict` materializes to Git-marker text via the shared `materialize_conflict_file`; `ConflictedFile::windowed(context)` trims that to the conflict hunks plus context lines (collapsing far runs into `[... N lines omitted ...]` sentinels) — the MCP `read_conflict` default, so a small conflict in a large file is cheap to inspect. Kept regions stay verbatim, so an `old` lifted from a window still matches the full text a `Patch` applies against.

A `FileResolution` per file is one of `Content { text, marker_len }` (whole resolved file, the GTK path), `Delete` (modify/delete + structural escape), or `Patch { edits: Vec<ConflictEdit> }` — old→new edits (`resolve_conflicts_ext`, taking a full `FileResolution` list, is the entry point). The patch arm re-materializes the conflict through the shared `materialize_conflict_file` so the edits apply against exactly the marker text `read_conflict` returned, applies each via `replace_checked` (unique match unless `ConflictEdit::all`), and threads that materializer's *fresh* `marker_len` back into `update_from_content` (a hardcoded length would mangle marker-lookalike content). A miss or ambiguous match surfaces `tree::ReplaceError`; markers left behind stay conflicted, a fully replaced block resolves clean.

Detection is **multi-head**: `collect_conflicts`/`settle` scan every editable head (not just the primary tip) *and* every worktree `@`, so a conflict on a sibling branch or a sibling worktree's uncommitted changes is caught too. **Resolution mirrors detection**: `resolve_change_on_chain` searches the same sources — every worktree's `@` (`all_worktree_chain_ids`: launch chain ∪ each extra worktree's `@`) and every editable head's rewritten range — so a genuine conflict on a *sibling* branch or a *sibling* worktree's `@` is resolvable, not abort-only (the GTK conflict view renders a conflicted sibling `@` via `worktree_chain_entries`). A singleton editable set with only the launch `@` reproduces the old single-head path byte-for-byte (guarded by tests).

Spurious conflicts from reorder/squash/drop/restore are auto-resolved once by `try_auto_resolve_spurious` using `replay.rs`'s asymmetric `replay_change` — planning each conflicted editable head read-only (`plan_spurious_head`) and re-parenting each worktree's own `@` in one tx:
- **`CleanTip`** (reorder/squash) — peel top-down from the clean tip.
- **`Drop`/`Restore`** — rebuild bottom-up from the clean prefix.

Genuine overlaps, binary/structural changes, or split chains fall back to the manual flow.

## History view (`history.rs`, `graph.rs`)

`history()` walks HEAD's ancestors only (like `git log <current-branch>`); `history_multi(heads, …)` walks the **union** of several branch tips' ancestries for the editable-set DAG. `graph.rs` computes gitk-style lanes (`compute_graph`) and `GraphLayout::boundaries` — the lane edges planning uses to find splice candidates. `change_id` (stable across rewrites) is the stable identity for re-selection after saves.

The **splice/squash planners** live here too. They gate on reachability — `branch_commits`, reachable from the *single* primary tip — and each has a `_multi` wrapper generalized over **every editable tip** (`Repo::editable_heads()`, via `branch_commits_multi`): `plan_reorder_candidates`/`_multi` (move), `plan_insert_candidates`/`_multi` (copy/cherry-pick, incl. `plan_cherry_pick_candidates`), `plan_squash`/`_multi`. A singleton head set reproduces the single-head planners byte-for-byte, so the MCP path is untouched; the GTK cross-branch drag (see `crates/commedit-gtk/CLAUDE.md`) is the only consumer of the `_multi` form, and the primary still anchors `new_tip` so a cross-branch move leaves the primary put and the sibling rides the rebase.

## Session op-log, undo & review (`repo.rs`)

Every clean mutation records a session op. A working-copy-direct edit records via `record_working_copy_op`, whose clean/conflicted gate keys on the **mutated** worktree's `@` (`working_copy_has_conflict_at(target)`), not always the launch's. `undo`/`redo`/`jump_to_op` funnel through `set_op_cursor` → `rewind_to_op` → `export_and_sync`, snapshotting the working copy and reconciling git/disk; the rewind additionally re-materializes any **sibling worktree whose `@` moved** (not just whose branch tip moved — `materialize_changed_worktrees`), so an `@`-only sibling op (edit/discard) tracks the DAG on disk through undo/redo. `session_changes` diffs current vs. session-start tree for the Review toggle.

## Structured diff editing

Three pure, GTK-free modules:
- `diff.rs` — render unified diffs (`render_diff`), apply patches (`apply_patch`), classify lines, `revert_groups`/`select_groups` for hunk-level revert and partial commit selection.
- `patch_edit.rs` — maps raw edit gestures onto structurally-valid `EditPlan`s (only `+` lines freely editable; context lines split into `-orig`/`+edited` pairs). `MoveLine` reorders `+` line(s) over their neighbour — valid because `+` lines are invisible to the old-file projection, so context/`-` anchors keep their order; `move_block_range` is shared with the GTK key handler for its selection-follow.
- `tabwidth.rs` — `TabWidthResolver` reads `.editorconfig`, `.vscode/settings.json`, `.clang-format` to pick display tab width per file.

**One line-splitting rule: `diff::split_lines`.** A line ends at `\n` and at nothing else, matching git and `apply_patch`. Never call `TextDiff::from_lines` — `similar`'s tokenizer also splits on a **bare `\r`**, so on a file carrying one it returns `DiffOp` indices into a sequence we don't have: segments past the `\r` pick up the wrong text, and the drift eventually panics out of bounds (a hard abort when it crosses a GTK signal trampoline). Always `TextDiff::from_slices` over `split_lines` output, so the ops index the very tokens we hold — `diff.rs`, `replay.rs`, `blame.rs` and `absorb.rs` all do. Caveat, pre-existing and separate: `split_lines` inherits `str::lines()`'s trailing-`\r` strip, so a **CRLF** file round-trips through `apply_patch` / `revert_groups` as LF.

## CLI (`cli.rs`)

`parse_repo_and_branch` parses both binaries' `[PATH] [BRANCH]` argument form: a lone arg is a path if it's an existing directory, else a branch in `.`.

## Module inventory

`blame.rs` · `cli.rs` · `conflict.rs` · `create.rs` · `diff.rs` · `graph.rs` · `history.rs` · `index_cache.rs` · `lib.rs` · `message.rs` · `patch_edit.rs` · `replay.rs` · `repo.rs` · `rewrite.rs` · `split.rs` · `squash.rs` · `tabwidth.rs` · `transparency.rs` · `tree.rs` · `workcopy.rs`. `default_config.toml` is embedded (see *Conventions*).

## Tests

Integration tests build scratch git repos via `tests/common/mod.rs` (`init_repo`, `git`, `git_log_subjects`) and **assert against plain `git`**. Each `tests/*.rs` is its own binary (`cargo test --test <name>`); they use `IndexCache::Disabled` and never touch `~/.cache`. Notable guards: `tests/spurious.rs` (auto-resolve), `tests/sibling_branch.rs` / `tests/multi_branch.rs` / `tests/off_worktree.rs` (editable-set & worktree symmetry), `tests/timetravel.rs` (op-log undo/redo).

## Conventions

- The engine never shells out to `git` for rewrites (that's jj-lib); it only uses the `git` CLI in `transparency.rs` for HEAD/worktree bookkeeping jj-lib doesn't expose cleanly.
- `jj-lib`'s backend trait is async but the git backend is synchronous. Drive every async call with `pollster::block_on(...)` — do not introduce a runtime.
- `default_config.toml` (embedded) supplies jj-lib's baseline settings, since jj-lib ships no defaults of its own.
- When a change touches engine behavior, consider whether it should also extend `README.md` and this file.
