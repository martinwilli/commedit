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
cargo fmt                        # format; run before committing
cargo clippy --workspace --all-targets  # lint; run before committing
cargo test                       # all tests (engine unit + integration)
cargo test -p commedit-engine    # engine only
cargo test -p commedit-mcp       # MCP server only
cargo test --test rewrite        # one integration test binary (each tests/*.rs is its own)
cargo test plan_reorder          # tests matching a name
cargo run -p commedit-gtk -- /path/to/repo  # launch the GTK app against a repo (defaults to ".")
cargo run -p commedit-mcp -- /path/to/repo  # the MCP server on stdio (defaults to ".")
```

The GTK crate needs system GTK4 / libsourceview5 development libraries present.

Run `cargo fmt` and `cargo clippy --workspace --all-targets` before committing,
and keep clippy warning-free; each commit should build and pass tests on its own.

The Claude Code plugin in `plugin/` bundles `commedit-mcp` (and the agent skills)
as an MCP server. To build it and install it into your own Claude Code for
dogfooding — instead of `cargo run -p commedit-mcp` — follow *Developing locally*
in [`plugin/README.md`](plugin/README.md) (`bin/commedit-mcp-*` is git-ignored, so
the local build stays out of the tree).

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
  `"type": "object"` MCP requires. A topology-changing mutation's `Clean` arm
  also carries a `topology` slice (`save_result_topo` in `session.rs`, built
  *post-save* by re-reading `history()` and inverting parents into children —
  **not** `compute_graph`): the affected commits' new parents/children plus a
  `merge_tip` when the new tip is a merge, all by change_id, so the agent can
  verify the result without a follow-up `list_history`. Each handler captures
  the pre-mutation change_id set (`change_id_set`) and its anchor(s) before
  mutating; a freshly-minted commit (a split's fixup child, a
  created/reverted/cherry-picked commit, a restored trash commit) is found as
  `post − pre`. `merge_out_commit` carries the slice too (anchoring its
  merged-out commit; the new merge is the `post − pre`, surfacing its two
  parents). `commit_working_copy` and `squash_working_copy` wrap the outcome in a
  resp DTO (`CommitWorkingCopyResp`/`SquashWorkingCopyResp`) that adds the new
  commit and/or the remaining working copy, so a *partial* commit/fold is
  verifiable (what landed + what's left) without a follow-up read. Plain
  message/identity/file edits stay lean (`save_result`, no slice). The read-only
  `show_graph` tool exposes that same adjacency for the
  **whole** branch (`graph_adjacency` in `convert.rs` — the shared
  `adjacency_tables`/`render_adjacency` the topology slice also uses), so an agent
  can read the merge/branch shape on demand without a mutation. Every result is
  wrapped in `Yaml<T>`
  (`wrapper.rs`) — serialized as a single human-readable YAML text block with
  **no** `structured_content`/`outputSchema` (a client that gets structured
  content hides the text block); strings YAML can't render as a literal block
  (diffs with tabs, whitespace-only edits) become a line-sequence instead. The
  tool surface is a **superset** of the GTK app:
  `create_commit`/`cherry_pick_commit`, the bulk `edit_commits`,
  the surgical `replace_in_file`/`replace_in_message` and `commit_working_copy`
  (+ partial) have no UI counterpart; `revert_commit` and `merge_out_commit`
  (introducing a merge) are the exceptions that *also* have a GTK surface — the
  history list's right-edge hover buttons drop a revert, or a merge, onto a
  commit. Mutations
  are
  refused while a conflicted
  rewrite is pending — the conflict tools (commit-ref-keyed, change id
  preferred) or `abort_rewrite` settle it first. Out-of-band git changes are
  caught up automatically: every tool runs through `with_session`, which calls
  `Repo::sync_to_git_head` first (a no-op unless the live HEAD moved), so a plain
  `git commit` on top of HEAD is imported into the *existing* session — trash and
  op-log intact. `reload_repo` (`with_session_no_sync`, the lone opt-out) is the
  heavier full reset, reserved for what a fast-forward sync can't absorb: a branch
  switch or out-of-band history rewrite. It also takes an optional `path` to
  **re-home the session to a sibling worktree** of the same repo (its main checkout
  or any linked worktree) — `session.rs`'s `resolve_worktree_target` scope-guards it
  by **shared git common-dir** (refusing an unrelated repo) and the handler reopens
  the live `repo.workspace_root()` otherwise, so no separate repo-path is kept. This
  lets one session edit history isolated in a `git worktree` and re-home afterward;
  the `work-in-worktree` plugin skill drives the setup.

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
- Git state is imported only at `Repo::open`, so a plain `git commit` the user
  makes on top of HEAD *after* open is absent from jj's view — a read or mutation
  resolving from the live HEAD would fail ("commit … not found in index").
  `Repo::sync_to_git_head` catches up **in place**: it re-seeds the throwaway dir's
  branch ref from the user's repo (`transparency.rs`'s `seed_session_head`, the
  same seeding `init_shared_git_dir` does at open — jj imports refs from *that* dir,
  not the user's `.git`) and re-runs `import_git`, preserving the trash and op-log
  (the import is just a recorded jj op, **not** the full reopen `reload_repo` does).
  Fast-forward only: a branch switch or out-of-band history rewrite is refused (it
  bails, pointing at `reload_repo`). Auto-invoked at the head of
  `snapshot_working_copy` (so every mutation self-heals — the snapshot's
  `ensure_working_copy_on_head` then re-anchors `@` onto the new tip) and at the MCP
  `with_session` boundary (so reads self-heal too; `reload_repo` opts out via
  `with_session_no_sync`).
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
  the committer to "now". `rewrite_batch` (`BatchEdit`s; the MCP `edit_commits`
  tool, no GTK surface) applies many message/identity edits in **one** transaction
  with a single rebase: it orders targets ancestors-first, re-parents each onto its
  just-rewritten ancestors, and excludes them from the descendant committer
  re-stamp — so a whole parent→child range re-dates correctly and it's
  O(targets+descendants), not the O(n²) of looping the single-commit calls.
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
- `restore_to_working_copy` / `drop_keeping_changes` (`squash.rs` / `rewrite.rs`) —
  "uncommit": move a commit's changes into the working copy as *uncommitted* edits
  instead of dropping them (git's `reset --mixed`). `restore_to_working_copy` is the
  inverse of `squash_working_copy_into`: it delegates to `squash_restore_into`
  targeting the leaf `@` as the destination, so an **orphan** (trashed) commit's diff
  3-way-merges into `@` and lands as unstaged changes — the branch tip never moves
  (the export is a branch no-op; `materialize_after_rewrite` rewrites the worktree +
  resets the index), and an overlap with existing uncommitted changes goes through the
  ordinary working-copy-conflict flow. `drop_keeping_changes` is the in-history entry:
  `abandon_commit` (correct `SpuriousResolve::Drop` rebase of descendants) **then**
  `restore_to_working_copy` on the resulting orphan — deliberately two transactions so
  each half uses the strategy it was built for (a single squash into `@` would assume a
  clean post-drop tip, which `CleanTip` requires but a drop breaks). Backs the GTK
  trash-row restore button and the MCP `drop_commit keep_changes`. (`squash_into_inner`
  gained an optional op-log label so the dropdown reads "Restore … to working copy".)
- `create_commit` / `revert_commit` / `cherry_pick_commit` (`create.rs`; MCP, plus
  a GTK surface for `revert_commit` — the history list's right-edge hover button, see
  the GTK section) — synthesize a brand-new commit and splice it into the graph at a
  `(new_parent_ids, new_child_ids)` slot, the same slot a reorder/restore plan
  resolves: a fresh commit is structurally a "restore" of one that was never in
  history, so all three share the `insert_new_commit` body and opt into the
  `Restore` forward-rebuild spurious-resolve. `create_commit` builds the tree from
  `FileEdit`s on the parent (empty → an empty commit); `revert_commit` from a 3-way
  merge applying a commit's **inverse** diff (its parent's tree as "theirs", git
  `revert` style); `cherry_pick_commit` the mirror with the forward diff — and its
  `target` may be **any** commit in the shared ODB, even one off the current branch,
  since only its trees are read (the source is never touched; it keeps the source
  author + a `(cherry picked from …)` trailer). Merge commits can't be
  reverted/cherry-picked (no single parent to diff). A top-gap insert (empty
  `new_child_ids`) splices beneath the working-copy chain like reorder, so
  uncommitted changes ride on top.
- `merge_out_commit` (`create.rs`; MCP `merge_out_commit` and a GTK surface — the
  history list's right-edge hover button beside revert, see the GTK section) — the
  one entry point that *creates a merge* (everything else only edits/preserves
  them). The MCP handler maps the agent's target commit onto the gap-above slot the
  same way `create_commit` does (a `plan_splice` with `new_parent` = the commit,
  reusing the `child` fork disambiguator) and returns `save_result_topo` (anchoring
  `C`; the new merge `M` is the `post − pre`, so the slice surfaces `M`'s two
  parents — verifiable without a follow-up read).
  Given a single-parent commit `C` (parent `P`), it inserts a merge `M` in `C`'s
  gap-above slot with `new_parent_ids = [P, C]` (P first = mainline, C second = the
  merged-out side branch) and `M`'s tree = **`C`'s tree** passed explicitly to
  `insert_new_commit` (which it shares with create/revert/cherry-pick) — so the
  merge introduces no change of its own and `C`'s descendants rebase onto `M` as a
  no-op (always `Clean` absent a working-copy overlap). `P` is an ancestor of `C`,
  so `M` is a **degenerate merge** jj keeps intact (ancestor-redundant parents
  aren't simplified — the same reason a reorder out of a merge leaves a 2-parent
  merge); `C` becomes a one-commit side branch you then populate by moving commits
  onto it. Refused on a merge **or the root** — jj gives the root commit the
  virtual root as its single parent, so the guard rejects both `len != 1` and a
  sole virtual-root parent. `M` gets a pro-forma `Merge "<subject>"` message to
  reword later.
- `replace_in_files` (`tree.rs`; MCP `replace_in_file` / `replace_in_message`,
  no GTK surface) — the surgical counterpart to `rewrite_files`: targeted
  `old`→`new` text replacements (unique unless `all`) read from the target's tree,
  applied in order, spliced through the same rewrite/rebase/export pipeline. A miss
  or ambiguous match returns a downcastable `ReplaceError`.
- `squash_into` (`squash.rs`) + `plan_squash` / `squash_recommendations` — drag
  one commit *onto* another to fold it in, across the whole graph. Built on
  jj-lib's native `squash_commits`: the source's changes apply to the target's tree
  (rebasing across branch lines for cousins on different merge sides — the result
  lands on the target's line), the source is abandoned, descendants rebase. A merge
  is a valid *target* but never a *source*. Preserves the target's **author** but
  lets jj re-stamp the committer (git `--autosquash` style); the message is
  `compose_squash_message`'d per `SquashMode` (Fixup keeps the target's, Squash
  appends the source's body, Amend replaces with it) **unless** `squash_into`'s
  `message: Option<&str>` override is given, which becomes the target's message
  verbatim (fold-and-reword in one step; threaded through to the MCP
  `squash_commit`/`squash_working_copy` `message` field, no GTK surface). Unlike
  reorder it does **not** set the head bookmark — the post-squash tip is always a
  rewrite-descendant of the old head, which jj's automatic bookmark moves follow.
  The pure, inline-tested helpers (`parse_squash_mode`, `squash_target_subject`,
  `squash_recommendations`, `compose_squash_message`) read git's
  `fixup!`/`squash!`/`amend!` subject prefixes so the UI can recommend targets and
  compose the merged message; the MCP `suggest_squash_targets` read tool exposes
  `squash_recommendations` (resolve a prefixed source → its matching destination
  commit(s) + sibling fixups) so an agent can route an autosquash fold.
- `split_commit` (`split.rs`) — the diff view's "Split" button (enabled only with
  pending diff edits). Takes the same edits as `rewrite_files` — a write-only
  `(path, content)` entry point plus a `split_commit_edits(&[FileEdit])` form (used
  by the GTK Save/Split path, so a reverted addition peels through as a deletion):
  rewrites the target `C` → `C'` to the **edited** tree (keeping its change id /
  message / author), then `new_commit`s `N` holding `C`'s **original** tree as
  `C'`'s child (message `fixup! <subject>`, original author), so `C'` + `N`
  reproduce the original diff and descendants are untouched. The trick is
  `set_rewritten_commit(C, N)`, which **overwrites** the `C → C'` rewrite so
  `rebase_descendants` (and the bookmark and `@`) follow `C → N` — and `N` restores
  the original tree descendants were built on, so the rebase is clean. The tree
  splice is shared with `rewrite_files` via `tree::splice_edits_into_tree`;
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
  `squash_into(.., Fixup)`. Its partial sibling `squash_working_copy_partial_into`
  (`squash.rs`; MCP `squash_working_copy`'s `paths`/`hunks`/`patches`) folds only a
  **subset** of the uncommitted changes into `dest` — the `git add -p` to the
  whole-fold's `git commit -a`. In one tx it builds the selected subset as a
  throwaway commit `C` on HEAD (via `prepare_partial_commit`, the
  selection→tree builder shared with `commit_working_copy_partial`), rebuilds the
  leaf `@` to hold the **full** disk tree on top of `C` (so the worktree stays
  byte-identical and the unselected delta stays uncommitted), then squashes `C`
  into `dest` and rebases — `@` rebases back to the full tree, so disk never moves.
- `drop_working_copy` (`workcopy.rs`; the MCP `discard_working_copy` tool) — the
  trashbin's drop for an *uncommitted* entry: snapshot, resolve by change id,
  `record_abandoned_commit` + `rebase_descendants`, commit the tx **directly** and
  re-materialize (same git-untouched path). Abandoning the leaf `@` makes jj
  recreate an empty `@`; abandoning an intermediate split-chain entry rebases the
  deeper entries onto its parent. Unlike a dropped *commit* it's **not** restorable
  (no git object to graft back), so the UI neither lists it in the trash nor offers
  to drag it back.
- `commit_working_copy` / `commit_working_copy_partial` (`workcopy.rs`; MCP-only,
  no GTK surface) — crystallize the uncommitted changes into a real commit on HEAD
  and start a fresh empty `@`, like `git commit -a`. Unlike the working-copy-direct
  ops above this **moves the branch tip**, so it runs through `finish_mutation`
  (always Clean — a fresh tip has no descendants). The *partial* variant commits
  only a selected subset (`PartialSelection`, the `git add -p` jj's whole-tree
  snapshot has no concept of) yet rebuilds `@` holding the **full** disk tree, so
  the remainder stays uncommitted and the files stay byte-identical;
  `select_groups` picks the kept hunks. Both (and the working-copy squash) accept
  an `add_paths` list naming brand-new **untracked** files to include — see the
  snapshot's `add_paths` opt-in below; the snapshot otherwise carries only
  edits/deletions to tracked files, so a freshly created file is invisible to a
  commit/fold until named (the `git add` before the `git commit -a`).

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
trees) never deletes them. The one opt-in is `snapshot_working_copy_tracking(add_paths)`,
which unions caller-named new files into the matcher (force-tracked, so an explicitly
named path beats a `.gitignore` rule) — surfaced as the `add_paths` field of the MCP
`commit_working_copy`/`squash_working_copy` tools, so an agent can fold a brand-new
file in. Once snapshotted into `@` the file stays tracked for the session, so only
the first snapshot needs to name it. `materialize_after_rewrite` (in the deferred export)
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
  (Save → `edit_working_copy_file`, whose `new_content: Option<&str>` takes `None`
  to drop a file the entry adds; the tip doesn't move) and splittable (Split →
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
wrapper). The richer `resolve_conflicts_ext` takes a `FileResolution` per path —
`Content(text)` or `Delete`, which splices `Merge::absent()` to remove the path (the
clean fix for a modify/delete conflict); the text-only `resolve_conflicts` is a
`Content`-only wrapper, so the GTK frontend is untouched and `Delete` reaches the
engine only via the MCP `resolve_conflicts` tool. `abort()` rolls jj back to the
captured pre-rewrite `Operation`; `jj_head_commit_id()` exposes the pending
(not-yet-exported) tip so the UI can show the chain being resolved. Resolve
**oldest-first**: fixing the earliest conflict often auto-clears its descendants on
rebase. Non-file (structural) conflicts can't be resolved as text — flagged
`resolvable: false`, so in the GTK pane `abort` is the only escape, though a
`Delete` settles them too.

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

### Multi-commit selection

The history list is `SelectionMode::Multiple` (ctrl/shift-click). One closure,
`update_selection_pane` (`main.rs`), drives the right pane off `list.selected_rows()`
on every `selected-rows-changed`, branching **conflict-mode first** (show the anchor's
conflicted files, never the multi view) then by count: 0 → inert pane; 1 → the usual
fully-editable single-commit pane; >1 → a read-only batch view. Two selection cells, both
in the `Data` bundle: `selected_change` (`Option<String>` **anchor** — the commit the
single-commit ops target) and `selected_changes: Vec<String>` (the full set, newest-first).
`conflict` only ever touches the anchor (resolution is per-commit), but `dragdrop` reads
`selected_changes` to drag the whole selection as a group (see below). `refresh` re-selects the multi-set when its len
> 1 else the anchor, all guarded by `selection_sync` (so the N programmatic `select_row`s
don't re-fire the router per row), then calls `update_selection_pane` once. `selected_row()`
returns null under multiple-selection, so its callers use `selected_rows()` instead, and the
post-rewrite re-render no longer needs a manual `unselect_all` (refresh always drives the
router).

The batch view is read-only: the message shows a dim italic note (`set_note`, a
lazily-created `note-italic` tag, reused for the diff's "not representable" note); the diff
is the `combined_changes` result rendered with `diff_read_only` set — which forces the view
non-editable and makes `build_diff_buffer_text(read_only)` drop the revert cues (the harmless
expand cues stay) — or the note when it returns `None`. The four identity fields show the
shared value where the selection agrees, else empty with a "(differs)" italic placeholder +
`identity-differs` class (`set_identity_fields_common`, baseline recorded in
`multi_identity_baseline`). **Save** builds one `BatchEdit` per selected commit
(`identity_for_commit` — each field overridden only where the user changed it from the
baseline, else the commit's own value; an empty field is never written) and applies them in
one `rewrite_batch`; message/file edits are never collected in this mode.

**Dragging the whole selection** (`dragdrop`). The history drag source no longer refuses a
multi-selection: `connect_prepare` records the selected display indices (newest-first) in the
transient `DragState.drag_set` when the grabbed row is part of a >1 selection, else leaves it
empty (the ordinary single-commit drag). Indices stay valid for the gesture — the rewrite is
staged to `drag-end` as always. The zone-validation and drop handlers branch on
`drag_set.len() > 1`: a gap drop plans with `plan_reorder_set_candidates` (the set analogue of
`plan_reorder_candidates`, yielding `ReorderSetMove`s) and applies `reorder_commits`; a drop
onto a commit folds the set in with `squash_into_many`, **always** through `show_squash_popover`
(a group has no single autosquash prefix; *amend* takes the newest selected commit's message);
a trash drop validates each with `plan_drop` and abandons them in one `abandon_commits` rebase,
refusing to empty the displayed branch. `show_lane_popover` is generic over the move payload
(`Vec<(lane, T)>`) so single (`ReorderMove`) and group (`ReorderSetMove`) reorders share it.
`PendingTrashOp::Drop` carries a `Vec<CommitInfo>` so a conflicted group drop defers all its
trash entries together (consumed in `conflict.rs`). Working-copy and trash drags stay
single-commit (the multi-selection lives only in the history list). This drag is **GTK-only**:
unlike most features it has no MCP counterpart — an agent does the same sequentially via the
singular tools addressed by stable change_id, so it's the one documented exception to the
"MCP surface is a superset of the GTK app" rule above. The engine primitives behind it
(`reorder_commits`/`abandon_commits`/`squash_into_many`, each the `Vec` generalization of its
singular method, sharing the reverse-topo sort and `op_desc_for_many`) live in the engine
regardless.

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
  hunk* cue (the whole-file *revert file* cue just sets `new = old` directly, which
  for an added/removed file means absent/restored); its dual
  `select_groups(old, new, kept)` rebuilds `new`
  keeping only the named change groups (the rest reverted to `old`), backing the MCP
  partial working-copy commit (`commit_working_copy_partial`). `render_commit_diff`
  lays **all** of a commit's files
  into one buffer (separated by `diff --git` lines; per-file placement in
  `CombinedFile`) and `split_combined_patch` cuts the edited buffer back per file;
  `rewrite_files_edits` (`tree.rs`, taking `FileEdit`s so a `None` content removes a
  path) splices several files' new content into the tree in one rewrite. The conflict
  pane reuses the same windowing
  (`render_conflict_snippets` / `reconstruct_conflict_file`).
  `combined_changes(repo, &[CommitId])` (commits **oldest-first**) builds the minimal
  combined diff for the multi-select view: base = the oldest's parent tree, each
  commit's delta re-applied onto an accumulator via jj-lib's `MergedTree::merge` (the
  same primitive `create.rs` cherry-picks/reverts with), then `tree_changes` against
  the result — `Ok(None)` when a fold leaves a conflicted tree (the selection isn't
  representable as one diff). A contiguous range telescopes to `parent_of_oldest →
  newest`; a gapped/divergent one composes a cherry-pick stack.
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
  (the "hide, never unparent" discipline lives in `populate_rows`). Every row is the
  outer box `[graph area?, content overlay { row box }]`: the ancestry drawing area
  leads it on a history row (trash rows omit it — no graph), and the content box is
  always wrapped in an `Overlay` carrying a hover button that floats at the row's
  right edge (`halign End`) — so buttons line up down the list, aligned to its right
  boundary, and only overlap a subject wide enough to scroll under them (mirroring
  the id cell's copy icon, but row-wide). A **history** row gets a **revert button**
  (`add_revert_button`) and, beside it, a **merge-out button** (`add_merge_out_button`,
  the `go-next-symbolic` right arrow at `margin_end 28` so it sits just left of
  revert's `margin_end 8`); a **trash** row gets a **restore button**
  (`add_restore_button`, the `go-bottom-symbolic` down arrow). The three callbacks
  (`RevertCallback` / `MergeOutCallback` / `RestoreToWorktreeCallback`) thread through
  `populate_list`/`populate_trash` →
  `set_row_commit` → `commit_row_box`, are slot-filled late in `build_ui` (the
  `RestoreToWorktreeCallback` also rides in the `Callbacks` bundle so `dragdrop`
  repopulates the trash with buttons intact; revert + merge-out are re-passed by
  `refresh`), and defer to idle (like `run_post_drag`).
  Revert places a `revert_commit` on top of the row's commit (parent = the commit,
  children = the lane edge crossing the gap just above it, `graph.boundaries`).
  Merge-out (`merge_out_commit`) uses the **same** gap-above slot — same
  `graph.boundaries` child computation — to introduce a merge above the commit; both
  buttons share the `no-revert` tag on merge rows (a merge has no single parent), so
  neither reveals there. Both re-select the clicked commit on `Clean` (the new
  commit sits one row above).
  Restore calls `restore_to_working_copy` (the engine "uncommit"): on `Clean` it drops
  the commit from `trashed` and refreshes, on a working-copy overlap it enters conflict
  mode and defers the trash removal (`PendingTrashOp::Restore`, like a drag-restore).
  `set_row_commit`'s traversal finds the content overlay whether or not a graph area
  precedes it, so both row kinds share one path.
  The row box itself is `[id_cell, lint_badge, subject_label, pills, conflict_badge]`:
  the **lint badge** (`build_lint_badge`/`update_lint_badge`, a clickable emoji `Label`
  sitting *between* the id cell and the subject — so a flagged summary reads as prefixed
  by the 🤔, a conforming one is flush after the id since a hidden box child takes no
  space, and the right-edge revert/merge-out hover buttons can't cover it on a long,
  ellipsized subject) is a `crate::msglint` finding, shown only when the commit's summary
  drifts from the repo's learned style. It's an inline cell, not a right-edge overlay
  (the action buttons are). It's always present (hidden when clean) so the traversal
  stays uniform across history/trash rows; its `LintFixCallback` threads through
  `populate_list` → `set_row_commit` → `commit_row_box` like the revert/merge-out ones
  (slot-filled in `build_ui`, deferred to idle), while `refresh` learns the
  `RepoStyle` once per rebuild and passes it down to paint each row's badge (conflict
  mode and trash rows pass `None`, so neither lints). The handler auto-fixes the
  mechanical lints via `rewrite_message`, else selects the commit and focuses the
  message editor.
- `identity.rs` — the author/committer identity/date fields and conversions.
- `conflict.rs` — the pure conflict-text helpers **and** the conflict-mode wiring:
  the callback builders (`build_refresh_conflict`/`build_exit_conflict_mode`/
  `build_enter_conflict_mode`/`build_resolve_current`, called by `build_ui` in that
  strict dependency order) and `conflict::wire` (abort + prev/next-conflict nav).
- `dragdrop.rs` — the whole drag-and-drop surface behind `dragdrop::wire`: the
  reorder-gap/squash-target feedback (`show_zone`), the drag sources / drop targets,
  the deferred `post_drag` staging (`run_post_drag`), and the squash/lane popovers.
- `search.rs` — the **pure**, GTK-free commit-search core, inline-tested:
  `search_match` (case-insensitive substring/term matching over a subject —
  whitespace-split terms, each a required substring, order-independent AND; returns
  the matched char indices for *every* occurrence) and `highlight_markup` (escapes
  the subject and wraps the matched chars in a Pango `<span>`). Deliberately **not**
  a fuzzy subsequence — that over-matches by accepting the typed chars scattered
  anywhere, surprising in a find-in-list box (git tools — gitk, tig, `git log
  --grep` — all use substring search). The header's `SearchEntry` (packed right of
  Reload, focused by the `Ctrl+F` shortcut) drives `rows::apply_search_highlight`
  (re-paints every row's subject — matches highlighted, the rest plain text, so an
  empty query is also the reset path — and returns the matching row indices) and
  `rows::scroll_row_into_view` (nudges `history_scroll`'s vadjustment via the row's
  `compute_bounds`). `search-changed` re-highlights + scrolls to the first hit
  without changing the selection; `activate` (Enter) steps a cursor through the
  match indices, selecting each via the `refresh` `selection_sync` pattern.
  `refresh` re-applies an active query after it rebuilds the rows.
- `msglint.rs` — the **pure**, GTK-free commit-message linter, inline-tested, the
  same shape as `search.rs`. commedit is a general tool, so it imposes **no** house
  style: `RepoStyle::learn(subjects)` infers a repo's *own* de-facto conventions from
  its history — does a strong majority (`MAJORITY`) carry a `type:`/`subsystem:`
  prefix (the `+` form `history+mcp:` included), capitalize the summary *after* the
  prefix (`majority_case`), avoid a trailing period, and how long do subjects run (a
  p90×1.5 cutoff, double-gated by the `LONG_ABS_FLOOR` git wrap so a terse repo isn't
  nagged). Prefix **casing** is learned **per token** (`prefix_casings`: lowercased
  key → canonical exact spelling), not as one global lower/upper norm — a casing
  becomes canonical only when it's dominant (≥ `MAJORITY`) *and* recurs
  (≥ `MIN_PREFIX_OCCURRENCES`), so a legitimately-uppercase proper-noun prefix
  (`NEWS:` for the NEWS file, `README:` for README.md) is its own canonical form and
  never flagged, a one-off `GTK:` against many `gtk:` is, and a brand-new prefix seen
  once is left alone (it could be a new proper noun). Below `MIN_SAMPLE` human
  subjects, or with no clear majority on an axis, that axis is "no opinion" (empty
  `RepoStyle` field) and never flags. Auto-generated subjects
  (merges/reverts/un-squashed fixups/initial commit) carry no authorial style, so
  `is_autogen` excludes them from both the sample and linting. `lint_subject` returns
  the drift as `Lint`s (`LintKind::{MissingPrefix,PrefixCapitalization,Capitalization,
  TrailingPeriod,TooLong}`); `autofix_subject` applies only the *mechanical* ones
  (re-case the prefix to its canonical spelling, flip the summary's first letter,
  strip a stray trailing period but never an `…` ellipsis), leaving the judgment
  calls (missing prefix, over-long) for a human; `replace_subject` splices a fixed
  summary back into the full description for `rewrite_message`. Drives the `rows.rs`
  lint badge (see above) — the linter is GTK-only, no MCP counterpart.
- `spelling.rs` — thin **GTK glue** wiring GNOME **libspelling** onto the
  commit-message editor (`spelling::attach`, called in `build_ui` right after the
  `message_view`/`message_buffer` are built). libspelling targets `GtkSourceView`
  directly: its `TextBufferAdapter` attaches to the message `GtkSourceBuffer` (which
  the field already is) and *itself* drives the misspelling underlines + right-click
  corrections menu (set as the view's extra menu + a `"spelling"` action group), so
  there is **no** checker logic of our own — spell quality comes from the system
  enchant dictionaries. The view retains the adapter, so nothing is stored. Wired
  only on the **message** field, never the diff/`file_view`. GTK-only, no MCP/engine
  counterpart (unlike the `msglint` badge this is interactive, not a per-commit scan).
  It also persists the two preferences libspelling leaves to the app
  (`SpellSettings` ↔ `~/.config/commedit/spelling.conf`, saved on the adapter's
  enabled/language notify): the on/off tick, and the **language** — which we **pin**
  (`Checker::new` with a tag derived once from the locale via `glib::language_names`,
  validated against `Provider::supports_language`) rather than leaving to
  `Checker::default()`. The default re-derives per launch and can flip `en`↔`en_US`,
  which splits enchant's *per-language* personal-dictionary file so "Add to
  Dictionary" words appear to vanish across sessions; pinning keeps that file stable.

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
`rewrite_files_edits` (the `FileEdit` form — `collect_file_edits` returns
`Vec<FileEdit>`, so a revert can emit a *delete* as well as a write). A *modified*
file's `@@` headers carry a *revert hunk* cue; **every** changed file's `diff --git`
line carries a *revert file* cue (`DiffCue`) — including added and removed files,
which have no both-sides hunk model but a whole-file change to drop. Clicking a cue
sets the file's `new_text` to its `old_text` on the *render baseline* (`changes`):
modify→unmodify, **add→absent (`None`)**, **remove→restore** — the one uniform rule.
`orig_changes` keeps the pristine content so Save/Split still see the revert as a
divergence to apply. A *file* revert leaves no net change, so `visible_changes` drops
that file from the rendered buffer **and** the dropdown (the revert handler rebuilds
the dropdown and re-points it at the viewport-top file) rather than leaving an empty
notice behind — but it stays in the render baseline, so `collect_file_edits` iterates
that baseline (not the buffer) and still emits the dropped file's delete/restore from
its `new_text`. `visible_changes` only hides a file whose no-change state *diverges*
from `orig` (a true revert), so a mode-only no-textual-change file stays visible. A
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

- When planning a change, consider whether it should also extend `README.md`
  (the user-facing pitch + conceptual model) and this `CLAUDE.md` (implementation
  notes + invariants), and fold those doc updates into the plan — a feature or
  invariant change usually needs both kept in sync.
- Engine integration tests build scratch git repos via `tests/common/mod.rs`
  (`init_repo`, `git`, `git_log_subjects`) and assert against plain `git`.
- The engine never shells out to `git` for *rewrites* (that's jj-lib); it only uses
  the `git` CLI in `transparency.rs` for HEAD/worktree/exclude bookkeeping that
  jj-lib doesn't expose cleanly.
- `default_config.toml` (embedded) supplies jj-lib's baseline settings, since jj-lib
  ships no defaults of its own (the jj CLI normally provides them).
