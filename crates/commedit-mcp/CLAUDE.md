# commedit-mcp

MCP stdio server over the engine (binary `commedit-mcp`). A **lib + thin bin**,
so tool handlers are integration-tested directly (`tests/*.rs`). Depends on
`commedit-engine`, `rmcp`, `serde_yaml`. The repository logic lives in the engine
(see `crates/commedit-engine/CLAUDE.md`); this crate is the agent-facing surface,
which is a **superset of the GTK app**.

```sh
cargo run -p commedit-mcp -- /path/to/repo   # MCP server on stdio (defaults to ".")
```

## Multi-tenant sessions (`session.rs`, `server.rs`)

The MCP server is **multi-tenant**: one server hosts several independent editing sessions over the *one* repository it launched against, addressable per tool call. State is a `SessionRegistry` (`Arc<Mutex<…>>` on `CommeditServer`): a `root` (the launch worktree, for branch/worktree resolution) plus `slots: HashMap<id, Arc<SessionSlot>>`, where `SessionSlot { repo: Mutex<Repo>, trash: Mutex<TrashState> }`. The engine is already multi-tenant-safe (each `Repo` owns its `TempDir`/git-dir/settings; the shared ODB is append-only; distinct branches export to distinct refs; the index-cache `flock` degrades gracefully) — no engine locking changes.

> **Ref-export confinement is per-session, scoped to jj-tracked refs.** Several sessions share the *one* git common-dir, so its `refs/heads/*` are global to all of them. `protect_unrelated_heads` (the backstop that reverts a leaked bookmark move) must therefore look only at refs *this* session's jj actually imported (`view().git_refs()`): the scoped import (`import_some_refs`) means jj can only ever move a ref it tracks, so a branch another session owns — never imported here — must be left alone, not force-restored to this session's stale snapshot. Without that scoping, two concurrent sessions clobber each other's branch refs back to an earlier tip (a silent revert with a dirty worktree); `tests/refrace_repro.rs` is the regression guard. The editable-branch bridge (`bridge_branches_to_git`) is already race-safe via its per-branch compare-and-swap against `before`.

- **Three-tiered locking** (in `with_session`): the registry lock is held only to look up + `Arc::clone` the slot (short), the per-session repo mutex is held across the blocking jj work (single-writer-per-session, so different sessions run in parallel), git-level safety is the engine's. The one added rule: a (repo, branch) already live in a slot can't be opened twice (mirrors the engine's "branch checked out in another worktree" refusal). Never hold the registry lock while taking a repo lock (the deadlock-freedom invariant — `sessions_view` and `reload_repo` are written around it).
- **Branch-keyed addressing.** The session id *is* the edited branch's short-name (`session_id_for`); a detached/unborn HEAD reserves the id `"HEAD"`. `open_session(branch)` looks up `worktree_for_branch` and anchors the `Repo` at that worktree (worktree-bound) or at `root` (off-worktree) — git's branch→worktree mapping decides, never the caller. `reload_repo(session, …)` retargets one slot and **re-keys** it when the branch changes (refusing a collision). `close_session` refuses the last slot (the registry is never empty).
- **Required selector.** Every session-operating tool takes a required `session` via a flattened `SessionSel` DTO (the 9 argument-less tools use `Parameters<SessionSel>` directly); `list_sessions`/`open_session` need none. There is no implicit default. GTK (phase 2, not yet built) would reuse the same model with `Rc<RefCell<…>>` and an implicit focused-tab selector.

## DTO boundary & tools

- **DTO boundary** — `dto.rs`/`convert.rs`: no jj-lib types cross the MCP boundary; engine types are converted to plain serializable DTOs. Results are YAML-wrapped in `wrapper.rs`.
- **Tools** live in `tools/{read,mutate,workcopy,conflict,ops}.rs` (`tools/mod.rs` ties them together); the router and `#[tool]` dispatch are in `server.rs`. `error.rs` is the shared error type.
- The MCP surface is a **superset of the GTK app** — it exposes everything the UI can do plus the `PartialSelection`-based partial commit/squash that has no GTK counterpart.

## Tests

`tests/*.rs` exercise the tool handlers directly (lib, not over stdio), with scratch repos via `tests/common/mod.rs`. Each file is its own binary (`cargo test --test <name>`): e.g. `tests/handlers.rs`, `tests/sessions.rs`, `tests/workcopy.rs`, `tests/conflict_loop.rs`, `tests/retarget.rs`, `tests/off_worktree.rs`, and the cross-session `tests/refrace_repro.rs` race guard.

## See also

- `plugin/CLAUDE.md` — the Claude Code plugin that bundles this server.
- `dogfood/CLAUDE.md` — the teacher↔student tournament that stress-tests this MCP surface and the bundled operator agent / skills.
