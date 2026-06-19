---
name: review-and-recover
description: >-
  Use to lean on the commedit session as a safety net and a lens — review
  everything the session changed as one diff, step back through recorded
  operations or undo/redo a landed change, recover a commit you dropped, or
  orient yourself in the history (commit order, a commit's diff, the branch
  graph, uncommitted changes) before acting or after a save.
---

# Review, time-travel & recover with commedit

> **These reads and recoveries are cheap and safe — drive them yourself.** Every
> landed change is a recorded operation you can walk back, and the whole session
> is one inspectable diff. You don't delegate orientation: this is how you scope
> a job *before* delegating it and how you confirm one *after*. The only
> irreversible action here is `discard_working_copy`.

commedit edits are not fire-and-forget. Each clean mutation is a recorded session
operation, dropped commits go to a recoverable trash, and the difference between
where the session started and where it stands is always one diff away.

## Orient

Read before you act, and address everything by **`change_id`** (stable across
rewrites) once you have it:

- `list_history` — commit order and the `change_id`/sha of each. Pass a small
  `fields` set (or `fields: []` for a header-only overview) and `offset` to page;
  don't request a huge `limit`.
- `show_commit` — one commit's message, diff, files, and numbered hunks.
- `show_graph` — the branch's parent/child shape: merges, side branches, where
  lines fork and converge. This is the standalone read of the `topology` a
  restructuring mutation returns — reach for it whenever the history is branchy.
- `working_copy_status` — uncommitted changes and the working-copy chain.

## Review the session

`session_diff` shows **everything the session changed** as a single diff —
current tree vs. the tree at session start. It's the "what have I done so far"
view: run it before a merge, a hand-off, or whenever you've chained several edits
and want the net effect in one place rather than commit by commit.

## Step back

Every landed change is an operation, so a wrong turn is recoverable:

- `list_operations` — the recorded ops, newest first; op `0` is session start.
- `undo` / `redo` — walk one step back or forward.
- `jump_to_operation` — land on any recorded point directly (`0` rolls the whole
  session back to its start).

Each of these reshapes git and the working tree to match the chosen point — it's
real time-travel over the session, not just a log.

## Recover a dropped commit

A `drop_commit` doesn't destroy anything — the commit goes to a session trash:

- `list_trash` — the dropped commits, by `change_id`.
- `restore_commit(commit, new_parent)` — graft one back into the graph at a slot.
- Or feed it to `squash_commit` as the **source** — a trashed commit is a valid
  squash source, so you can fold a dropped commit into another in one step.

## The one thing you can't undo

`discard_working_copy` throws away uncommitted changes for good — it is **not**
a recorded operation and there is no trash for it. Everything else on this page
is reversible through the op-log; this one isn't, so only run it on an explicit
instruction.
