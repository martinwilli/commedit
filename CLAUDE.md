# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

comm(ed)it is a GTK4 desktop app for visually editing the *history* of a git repo — any commit in the graph, not just the latest. Pick a commit, edit its message, identity, or file content (as an editable unified diff), or reorder / squash it by drag-and-drop; saving rewrites it in place and auto-rebases its descendants.

**Read `README.md` first.** Its *Features* and *How it works* sections cover the jj-over-git model, conflict handling, working-copy preservation, and session time-travel.

## Commands

```sh
cargo build                      # build the workspace
cargo fmt                        # format; run before committing
cargo clippy --workspace --all-targets  # lint; run before committing
cargo test                       # all tests (engine unit + integration)
cargo test -p commedit-engine    # engine only
cargo test -p commedit-mcp       # MCP server only
cargo test --test rewrite        # one integration test binary (each tests/*.rs is its own)
cargo test plan_reorder          # tests matching a name
cargo run -p commedit-gtk -- /path/to/repo  # launch the GTK app against a repo (defaults to ".")
cargo run -p commedit-gtk -- /path/to/repo feature  # edit an off-worktree branch (path+branch)
cargo run -p commedit-mcp -- /path/to/repo  # the MCP server on stdio (defaults to ".")
```

Both binaries take `[PATH] [BRANCH]` (parsed by `commedit_engine::cli::parse_repo_and_branch`): a lone arg is a path if it's an existing directory, else a branch in `.`.

Run `cargo fmt` and `cargo clippy --workspace --all-targets` before committing, and keep clippy warning-free; each commit should build and pass tests on its own.

The Claude Code plugin in `plugin/` bundles `commedit-mcp` as an MCP server. Follow *Developing locally* in [`plugin/README.md`](plugin/README.md) to build and install it for dogfooding. [`dogfood/`](dogfood/README.md) defines a reproducible teacher↔student tournament that drives the real MCP server with subagents and scores correctness/efficiency — re-run it whenever the MCP surface, the operator agent, or the bundled skills change, to catch agent-ergonomics regressions unit tests don't cover. Before re-running after editing the operator/skills, **refresh the plugin snapshot and restart** (or launch with `--plugin-dir plugin`): a persistent install caches a copy, and `claude plugin update` is a *silent no-op* when `plugin.json`'s `version` is unchanged, so a running session keeps serving the stale agent — `uninstall` + `install` forces a fresh snapshot.

## Architecture

Three crates, split so the rewrite logic carries no GTK dependency and is unit-testable headless:

- **`commedit-engine`** — all repository logic, built on `jj-lib` (jujutsu).
- **`commedit-gtk`** — the UI (binary `commedit`). Depends on the engine.
- **`commedit-mcp`** — MCP stdio server over the engine (binary `commedit-mcp`). A lib + thin bin so tool handlers are integration-tested directly (`tests/*.rs`). Tools live in `tools/{read,mutate,workcopy,conflict,ops}.rs`; the session registry, addressing and planning in `session.rs`; DTOs in `dto.rs`/`convert.rs` (no jj-lib types cross the boundary); results YAML-wrapped in `wrapper.rs`. The MCP surface is a superset of the GTK app.

### The jj-over-git "transparency" model

The core invariant: plain `git` always sees an ordinary, attached-HEAD repo. Key files:

- `repo.rs` — `Repo::open` attaches jj to a throwaway git dir that shares only the ODB with the user's repo (symlinked `objects`). All jj state (op log, refs, detached HEAD) lives in a `TempDir` (`Repo::_workdir`), never touching the user's `.git`. `init_detached`/`load_detached` are the place sensitive to a jj-lib bump.
- `transparency.rs` — post-rewrite glue (`reattach_head`, `bridge_branch_to_git`, index reset) and session setup (`init_shared_git_dir`, `seed_session_head`).
- `Repo::sync_to_git_head` — fast-forward sync for out-of-band `git commit`s on HEAD. `reload_repo` handles branch switches or out-of-band rewrites (heavier full reset).

#### Off-worktree branches

`Repo::open_branch(path, cache, Some(branch))` (1-element set over `open_multi`) edits a branch that need *not* be checked out (the GTK/MCP `[PATH] [BRANCH]` arg). `Repo` carries `git_head_branch` (the *checked-out* branch) and an `EditableSet { primary, extra }` of full ref names — the **editable set** of branches imported as real bookmarks (`refs()` iterates primary-first; a singleton set == today's single-branch behavior). `is_worktree_bound()` is `primary == git_head`. The primary bookmark is `current_bookmark()` — imported, rewritten, exported; `head_commit_id()`/`edited_tip()` give the primary's tip (its ref off-worktree, else git HEAD), so history/reorder/squash and the `old_head` compare-and-swap follow the primary. Import widens to every editable bookmark; export is `bridge_branches_to_git(old_head, before)`, mirroring each editable bookmark whose tip changed vs the pre-rewrite `snapshot_heads()` map; `protect_unrelated_heads` exempts the whole set. When the launch branch's tip is unchanged (editing only a sibling) `reattach_head` is a disk no-op. Off-worktree (primary not checked out), the launch worktree's `snapshot_working_copy`/`materialize_after_rewrite` are skipped, the launch working-copy readers return empty/`None`, and the mutating WC ops bail via `require_worktree`.

**Per-worktree symmetry (1b).** Every editable branch checked out in a git worktree *other than the launch one* is mapped — keyed on the worktree path, not the branch name, so even an off-worktree primary is covered — onto its own jj **workspace** (`WorktreeView`: a `Workspace` over the worktree root + a private working-copy state dir + a per-worktree `@`), enumerated via `transparency::worktrees`. `snapshot_working_copy` also snapshots every such worktree (so each one's uncommitted changes ride the rebase); the `export_and_sync` tail re-materializes (`materialize_extra_worktree`: checkout `@'` + reset *that* worktree's index) every extra worktree whose **branch tip actually moved** (the launch worktree keeps its *unconditional* materialize, since its `@` can move without its tip — e.g. `revert_all`). A selected branch with no worktree is a pure ref-move. Editing a branch live in another worktree is therefore now allowed (we keep that worktree in sync) rather than refused.

#### Cross-instance commit dragging

Opening one repo in several windows (typically one branch each) lets you drag a commit from one onto another's history to cherry-pick it across branches. Windows are separate processes (`main.rs` uses `ApplicationFlags::NON_UNIQUE`), so the history drag carries a text payload (`commedit-gtk/src/dnd.rs`: pid + `Repo::object_store_key` + dragged commits by sha) GTK ferries across the boundary — an in-process drop reads the same string back and ignores it, working from the live `drag_*` cells. A drop whose payload pid differs from `std::process::id()` is foreign; if its `repo_key` matches ours (same shared ODB, so the commit is reachable) it's cherry-picked at the gap via `lookup_commit_in_store` → `plan_cherry_pick_candidates` → `cherry_pick_commit` (a *copy*, `DragAction::COPY`, source window untouched). Different `repo_key` ⇒ refused with a status note (separate object stores never meet). Both drop targets now read the source row index from `drag_from` (every source sets it), since history travels as text not an `i32`.

### Multi-tenant MCP sessions (`commedit-mcp/src/session.rs`, `server.rs`)

The MCP server is **multi-tenant**: one server hosts several independent editing sessions over the *one* repository it launched against, addressable per tool call. State is a `SessionRegistry` (`Arc<Mutex<…>>` on `CommeditServer`): a `root` (the launch worktree, for branch/worktree resolution) plus `slots: HashMap<id, Arc<SessionSlot>>`, where `SessionSlot { repo: Mutex<Repo>, trash: Mutex<TrashState> }`. The engine is already multi-tenant-safe (each `Repo` owns its `TempDir`/git-dir/settings; the shared ODB is append-only; distinct branches export to distinct refs; the index-cache `flock` degrades gracefully) — no engine locking changes.

- **Three-tiered locking** (in `with_session`): the registry lock is held only to look up + `Arc::clone` the slot (short), the per-session repo mutex is held across the blocking jj work (single-writer-per-session, so different sessions run in parallel), git-level safety is the engine's. The one added rule: a (repo, branch) already live in a slot can't be opened twice (mirrors the engine's "branch checked out in another worktree" refusal). Never hold the registry lock while taking a repo lock (the deadlock-freedom invariant — `sessions_view` and `reload_repo` are written around it).
- **Branch-keyed addressing.** The session id *is* the edited branch's short-name (`session_id_for`); a detached/unborn HEAD reserves the id `"HEAD"`. `open_session(branch)` looks up `worktree_for_branch` and anchors the `Repo` at that worktree (worktree-bound) or at `root` (off-worktree) — git's branch→worktree mapping decides, never the caller. `reload_repo(session, …)` retargets one slot and **re-keys** it when the branch changes (refusing a collision). `close_session` refuses the last slot (the registry is never empty).
- **Required selector.** Every session-operating tool takes a required `session` via a flattened `SessionSel` DTO (the 9 argument-less tools use `Parameters<SessionSel>` directly); `list_sessions`/`open_session` need none. There is no implicit default. GTK (phase 2, not yet built) would reuse the same model with `Rc<RefCell<…>>` and an implicit focused-tab selector.

### Index cache (`index_cache.rs`)

`import_git` serializes full ancestry and is slow on large repos. The cache persists the session's jj `repo/` tree so the next launch primes from it (~35s cold → ~1s warm).

- **Prime-and-flush, never operate-in-place.** Sessions copy the cache into their own `TempDir`; the no-shared-op-log invariant holds. Tests use `IndexCache::Disabled` and never touch `~/.cache`.
- **Keyed** on SHA-256 of the canonical objects-dir path; all worktrees share one entry.
- **Concurrency** via shared/exclusive `flock`; flush is skipped when contended (cheap to forgo).
- Deleting `~/.cache/commedit` is always safe.

### Mutation pipeline

Every edit follows: load target → `start_transaction` → rewrite/`move_commits` → `rebase_descendants()` → `finish_mutation(tx, ...)`. Never call `export_to_git` inline — it belongs inside `finish_mutation` (`conflict.rs`).

`finish_mutation` checks for conflicts: Clean → deferred export to git/worktree + session op recorded; Conflicted → `PendingResolution` stored, git completely untouched until resolved.

Key operations and their source files:
- Message/identity/batch rewrites: `rewrite.rs` (`rewrite_message`, `rewrite_identity`, `rewrite_batch`)
- Reorder / drop / restore: `rewrite.rs` + planning in `history.rs`
- Create / revert / cherry-pick: `create.rs` (shared `insert_new_commit`)
- Squash and autosquash routing: `squash.rs` (`squash_into`, `squash_recommendations`)
- Drag-to-squash blame hint: `blame.rs` (`blame_single_source`) — when every line a dragged commit removes blames to one commit, the UI highlights it purple (a scoped line-origin blame; jj-lib has no annotate API)
- Split: `split.rs` (the `set_rewritten_commit` trick makes descendants follow the split child)
- Surgical text replace: `tree.rs` (`replace_in_files`)
- Working-copy commit/fold: `workcopy.rs`

### Working-copy preservation (`workcopy.rs`)

Uncommitted changes live in jj's working-copy commit `@` and rebase forward with every mutation. `snapshot_working_copy` runs at open and before each mutation, tracking only git-tracked files (`tracked_paths_matcher`). Untracked files stay on disk untouched.

The `@` chain (created by `split_working_copy`) is session-local; `collapse_working_copy_chain` reconciles it on fresh open. A working-copy overlap with a rewrite enters the same deferred conflict flow as a commit conflict.

Each *extra* editable worktree (see *Per-worktree symmetry*) has its own `@` keyed by a per-worktree jj workspace name; `snapshot_extra_worktree`/`materialize_extra_worktree` are the per-worktree analogues, minus the launch-only HEAD catch-up, `@`-chain, partial-commit and index-backup machinery (extra worktrees only edit existing commits, never the GTK working-copy view). `working_copy_commit_id` and the chain/info readers stay keyed on the *launch* workspace, so they ignore extra worktrees.

### Conflict resolution (`conflict.rs`)

Conflicted trees are never exported to git — git refs/HEAD/worktree stay frozen until the whole chain is clean. Resolve via `read_conflict`/`resolve_conflicts` (oldest-first) or `abort_rewrite`; all other mutations are refused while a `PendingResolution` is held.

Spurious conflicts from reorder/squash/drop/restore are auto-resolved once by `try_auto_resolve_spurious` using `replay.rs`'s asymmetric `replay_change`:
- **`CleanTip`** (reorder/squash) — peel top-down from the clean tip.
- **`Drop`/`Restore`** — rebuild bottom-up from the clean prefix.

Genuine overlaps, binary/structural changes, or split chains fall back to the manual flow.

### History view (`history.rs`, `graph.rs`)

`history()` walks HEAD's ancestors only (like `git log <current-branch>`). `graph.rs` computes gitk-style lanes (`compute_graph`) and `GraphLayout::boundaries` — the lane edges planning uses to find splice candidates. `change_id` (stable across rewrites) is the stable identity for re-selection after saves.

### Session op-log, undo & review (`repo.rs`)

Every clean mutation records a session op. `undo`/`redo`/`jump_to_op` funnel through `set_op_cursor` → `rewind_to_op` → `export_and_sync`, snapshotting the working copy and reconciling git/disk. `session_changes` diffs current vs. session-start tree for the Review toggle.

### Structured diff editing

Three pure, GTK-free engine modules:
- `diff.rs` — render unified diffs (`render_diff`), apply patches (`apply_patch`), classify lines, `revert_groups`/`select_groups` for hunk-level revert and partial commit selection.
- `patch_edit.rs` — maps raw edit gestures onto structurally-valid `EditPlan`s (only `+` lines freely editable; context lines split into `-orig`/`+edited` pairs).
- `tabwidth.rs` — `TabWidthResolver` reads `.editorconfig`, `.vscode/settings.json`, `.clang-format` to pick display tab width per file.

### GTK module layout (`commedit-gtk/src/`)

`build_ui` in `main.rs` is the orchestration hub; new GTK features land in topic modules (commit-prefixed by module name):

- `state.rs` — shared enums and the four widget bundles (`Widgets`/`Data`/`DragState`/`Callbacks`).
- `rows.rs` — commit/WC row build, `populate_*` refreshers (hide-never-unparent discipline), revert/merge-out/restore hover buttons, lint badge.
- `dragdrop.rs` — zone-based drag-and-drop (`show_zone`), squash/lane popovers, deferred `post_drag` (rewrites staged to `drag-end` to avoid mid-gesture segfaults). Also cross-instance drops (foreign payload → cherry-pick at the gap; see *Cross-instance commit dragging*).
- `dnd.rs` — pure (de)serialization of the cross-window drag payload (`DraggedCommits`); the text form GTK carries between processes.
- `conflict.rs` — conflict-mode callback builders and `conflict::wire` (abort, prev/next nav).
- `msglint.rs` — pure commit-message linter; learns repo style from history (`RepoStyle::learn`). GTK-only, no MCP counterpart.
- `search.rs` — pure substring commit search (`search_match` / `highlight_markup`).
- `linenums.rs` — pure gutter line-number logic: diff old/new (`diff_line_numbers`) and conflict ours/theirs (`conflict_line_numbers`).
- `diff_cues.rs` — the `GutterColumn` renderer (the file gutter holds two: old|new). Each column draws *either* a line number *or* a clickable cue button per line — they never coincide, so the buttons sit at the line-number level rather than in extra columns. Also the diff cue geometry (`diff_cue_cells`: expand→col_old, revert→col_new). Conflict cues (resolve, elision) are built in `conflict.rs`.
- `highlight.rs` — TextTag palette and syntect syntax colouring.
- `identity.rs` — author/committer identity/date fields and conversions.
- `spelling.rs` — libspelling glue for the message editor; pins language to keep enchant's personal dictionary stable across sessions.
- `window_state.rs` — persists window geometry (size, maximized, pane positions) across sessions.
- `buffer_util.rs` — buffer/selection/text helpers.

### jj-lib is async; we block

`jj-lib`'s backend trait is async but the git backend is synchronous. Drive every async call with `pollster::block_on(...)` — do not introduce a runtime.

## Conventions

- When planning a change, consider whether it should also extend `README.md` and this `CLAUDE.md`, and fold those doc updates into the plan.
- When integrating a feature branch into `master`, give the merge commit a body: keep git's `Merge branch 'feature/…'` subject, blank line, then one or two sentences on what it introduces. Reword with commedit's `edit_message` rather than `git commit --amend`.
- Engine integration tests build scratch git repos via `tests/common/mod.rs` (`init_repo`, `git`, `git_log_subjects`) and assert against plain `git`.
- The engine never shells out to `git` for rewrites (that's jj-lib); it only uses the `git` CLI in `transparency.rs` for HEAD/worktree bookkeeping jj-lib doesn't expose cleanly.
- `default_config.toml` (embedded) supplies jj-lib's baseline settings, since jj-lib ships no defaults of its own.
