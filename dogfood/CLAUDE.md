# dogfood

A reproducible **teacher↔student tournament** that drives the *real* commedit
MCP server with subagents on non-trivial history-editing tasks, grades them
out-of-band, and surfaces what works and what bites — the fastest way to catch a
regression in *agent ergonomics* that unit tests don't cover.

**Read [`dogfood/README.md`](README.md) first** — it has the full design (the
teacher↔student model, the three students per task, the metrics) and the task
catalogue.

## When to re-run

Re-run whenever the **MCP surface**, the **`commedit-operator` agent**, or the
**bundled skills** change. After editing the operator or skills, the run depends
on a **fresh plugin snapshot**: refresh + restart (or launch with `--plugin-dir
plugin`), because a persistent install caches a copy and `claude plugin update`
is a silent no-op on an unchanged `plugin.json` `version` — see
`plugin/CLAUDE.md`.

## Layout

- `reposetup.sh` — builds the stress fixture; self-contained and idempotent (bails if `stress/base` already exists).
- `verify.sh` — the automated correctness **oracle**: `./dogfood/verify.sh <t1..t11> <worktree-path>` asserts each task's end state (subjects + file content, not shas) and must exit 0. Run it as the gate for every student.
- `t6-dirty.sh` — seeds T6's dirty working copy after a reset (T6 tests the working-copy → history path).
- `runs/` — recorded run scorecards and findings, newest first (`runs/1.md` … `runs/7.md`).
