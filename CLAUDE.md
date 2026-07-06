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
```

Both binaries take `[PATH] [BRANCH]` (parsed by `commedit_engine::cli::parse_repo_and_branch`): a lone arg is a path if it's an existing directory, else a branch in `.`. The per-binary `cargo run -p …` forms live in the gtk / mcp crate guides.

Run `cargo fmt` and `cargo clippy --workspace --all-targets` before committing, and keep clippy warning-free; each commit should build and pass tests on its own.

## Architecture

Three crates, split so the rewrite logic carries no GTK dependency and is unit-testable headless. The dependency direction is one-way: both front-ends depend on the engine, never the reverse.

- **`commedit-engine`** — all repository logic, built on `jj-lib` (jujutsu). Lib-only. → [`crates/commedit-engine/CLAUDE.md`](crates/commedit-engine/CLAUDE.md)
- **`commedit-gtk`** — the UI (binary `commedit`). Depends on the engine. → [`crates/commedit-gtk/CLAUDE.md`](crates/commedit-gtk/CLAUDE.md)
- **`commedit-mcp`** — MCP stdio server over the engine (binary `commedit-mcp`); a lib + thin bin. The MCP surface is a superset of the GTK app. → [`crates/commedit-mcp/CLAUDE.md`](crates/commedit-mcp/CLAUDE.md)

Beyond the crates, [`plugin/`](plugin/CLAUDE.md) bundles `commedit-mcp` as a Claude Code plugin and [`dogfood/`](dogfood/CLAUDE.md) is the teacher↔student tournament that stress-tests the MCP surface and bundled agent/skills — each has its own `CLAUDE.md`.

### Two cross-cutting invariants

Every crate must respect these; their full mechanics are in the engine guide.

- **The jj-over-git transparency invariant.** Plain `git` always sees an ordinary, attached-HEAD repo: all jj state (op log, refs, detached HEAD) lives in a throwaway `TempDir` that shares only the ODB with the user's `.git`, and conflicted trees are never exported until the whole chain is clean. Full mechanics in [`crates/commedit-engine/CLAUDE.md`](crates/commedit-engine/CLAUDE.md).
- **The mutation pipeline.** Every edit follows `load target → start_transaction → rewrite/move_commits → rebase_descendants() → finish_mutation`. Never call `export_to_git` inline — export (and the clean/conflicted gate) belongs inside `finish_mutation`. Full mechanics in [`crates/commedit-engine/CLAUDE.md`](crates/commedit-engine/CLAUDE.md).

## Conventions

- When planning a change, consider whether it should also extend `README.md` and the relevant `CLAUDE.md` (the root for workspace-wide guidance, otherwise the per-crate / per-component file beside the code), and fold those doc updates into the plan.
- Separate commits per logical change/subsystem; each commit builds and passes tests on its own. Use imperative mood with a `prefix:` (refer to history for patterns — `doc:`, `engine:`, `gtk:`, `mcp:` …); describe the *why*.
- When integrating a feature branch into `master`, force a merge commit (`git merge --no-ff`) and give it a body in the same command: keep git's `Merge branch 'feature/…'` subject, then one or two sentences on what it introduces (`-m "Merge branch '…'" -m "…"`). No separate reword step — commedit is for rewriting existing history and rebasing descendants, which a fresh merge at HEAD doesn't need.
