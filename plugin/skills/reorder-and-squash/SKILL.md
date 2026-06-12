---
name: reorder-and-squash
description: >-
  Use when tidying a branch before review or merge — reorder commits into a
  logical sequence, fold fix/WIP commits into the ones they belong to (squash,
  fixup or amend, honouring git autosquash `fixup!`/`squash!`/`amend!`
  prefixes), or drop a commit entirely. Works on any commit reachable from
  HEAD; descendants rebase automatically.
---

# Reorder, squash & fixup with commedit

> **Delegate each move to the `commedit-operator` subagent.** Decide the target
> order or fold, then hand it the *what* — "reorder `<id>` before `<id>`", "squash
> the fixup `<id>` into `<id>`", "drop `<id>`" — and it resolves change_ids, runs
> the rebase, handles any conflict hold, **verifies**, and reports. The tool detail
> below is what the operator works from; use these tools directly only when no
> subagent is available, or when you *are* the operator.

This is `git rebase -i` without the editor choreography, on any commit reachable
from HEAD: each move is a real rebase that carries the descendants with it. Run
`list_history` first to see the change_ids, then address commits by
**`change_id`** so you can chain several edits without re-listing.

## Reorder

`reorder_commit(commit, new_parent)` moves a commit so `new_parent` becomes its
parent (or `new_parent: "root"` for the very first position). It's a true rebase,
so commits that don't commute report conflicts. When parallel lines converge on
`new_parent` (a fork), pass `child` to pick which line to splice under. Merge
commits can't be moved.

## Squash / fixup / amend

`squash_commit(source, dest)` folds `source` into `dest` anywhere in the graph:

- `mode: "fixup"` keeps `dest`'s message, `"squash"` appends `source`'s body,
  `"amend"` replaces it with `source`'s. The default follows `source`'s
  `fixup!`/`squash!`/`amend!` subject prefix, else `fixup`.
- Pass `message` to set `dest`'s resulting message verbatim — fold and reword in
  one call, instead of a follow-up `edit_message`.
- A merge can be a squash **destination** but never a **source**.

### Autosquash

If you've parked fixes as `fixup!` / `squash!` / `amend!` commits, let commedit
route them: `suggest_squash_targets(source)` reads the prefix and returns the
matching destination commit(s), the `mode` it requests, and any sibling
autosquash commits aimed at the same target. Pass a returned target straight
into `squash_commit` as `dest`.

## Drop

`drop_commit(commit)` removes a commit; its children rebase onto its parent. The
dropped commit goes to a session trash (`list_trash`), so it's recoverable —
graft it back with `restore_commit(commit, new_parent)` or fold it somewhere
with `squash_commit` (a trashed commit is a valid squash source). Merge commits
and a branch's only commit can't be dropped.

## When things go sideways

- **A conflicting rewrite is held back in full.** It returns `status: conflicts`;
  git history, HEAD and the working tree stay untouched until it settles. Resolve
  the **oldest** conflicted commit first (`read_conflict` each file → remove every
  marker → `resolve_conflicts`); fixing the earliest often auto-clears its
  descendants. `abort_rewrite` throws the held rewrite away. No other mutation
  runs while one is pending.
- **Address commits by `change_id`, not sha** — shas churn on every rewrite,
  change_ids are stable, so you can chain edits without re-running `list_history`.
- **After any out-of-band git operation** (a commit, branch switch or rebase made
  outside the session) call `reload_repo` before continuing.
- **Safety net:** every landed change is a recorded operation — `list_operations`,
  `undo` / `redo`, `jump_to_operation` (`0` rolls the session back to its start).
