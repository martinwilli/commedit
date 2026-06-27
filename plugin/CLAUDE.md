# plugin

The Claude Code plugin that bundles `commedit-mcp` as an MCP server.

**Read [`plugin/README.md`](README.md) *Developing locally* first** — it has the
full build/install paths. This file is just the gotchas that bite when
dogfooding from inside Claude Code.

## Layout

- `agents/commedit-operator.md` — the bundled history-editing subagent.
- `skills/*/SKILL.md` — the bundled workflow skills (`commit-as-you-go`, `revise-commit`, `reorder-and-squash`, `insert-and-revert`, `resolve-conflicts`, `review-and-recover`, `work-in-worktree`).
- `hooks/` — `hooks.json` + `on-worktree-enter.sh` (reminds the agent to retarget on worktree entry).
- `bin/launch.sh` — runtime launcher; picks `commedit-mcp-<os>-<arch>`. The binaries themselves are git-ignored and injected by the release workflow (a source checkout has none).
- `.mcp.json` — declares the `commedit` MCP server. `.claude-plugin/plugin.json` is the manifest (note its `version`, below).

## Rebuild gotchas (dogfooding)

A source checkout has **no binaries** in `bin/` — build yours first:

```sh
cargo build --release -p commedit-mcp
rm -f plugin/bin/commedit-mcp-linux-x86_64        # rm BEFORE cp — see below
cp target/release/commedit-mcp plugin/bin/commedit-mcp-linux-x86_64
```

- **`rm` before `cp`.** A running dogfooded `commedit` server holds the old binary open, so a plain `cp` fails with `Text file busy`. Unlinking first lets the running process keep its old inode while the new build lands at a fresh one. The still-running server keeps serving the *previous* build until you restart.
- **`--plugin-dir plugin` is the quick loop.** It reads `plugin/` in place — no marketplace, no snapshot, no `version` dance — so each relaunch picks up your current source (rebuilt binary *or* an edited agent/skill/prompt `.md`).
- **A persistent install caches a snapshot.** `claude plugin update` keys off `plugin.json`'s `version`: when it's **unchanged** (the usual mid-development case) the update is a **silent no-op** — it reports success and copies nothing, so an agent-only edit stays invisible. To force a fresh snapshot of the *same* version, **uninstall + install** (or bump `version`), then **restart** Claude Code — a refreshed snapshot never reaches an already-running session.
