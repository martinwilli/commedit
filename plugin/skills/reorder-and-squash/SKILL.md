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

> **Drive a single move yourself; delegate the multi-step tidy-ups.** A lone
> `reorder_commit` / `squash_commit` / `drop_commit` you can address by change_id and
> expect to land clean — make it directly: the result returns the reshaped topology,
> so it is self-verifying with no follow-up `list_history`. **Delegate to the
> `commedit-operator` subagent** for the open-ended cases — sequencing a whole branch
> into logical order, a fold that conflicts, or working out where each fix belongs —
> work worth keeping out of your context. The tool detail below is what you (or the
> operator) work from.

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

### Find the target by content

When a fix carries **no** prefix — or you simply don't know which commit
introduced the code it touches — `blame_squash_targets(source)` content-blames
the lines the change removes/modifies and returns the owning commits ranked by
how many of those lines each owns; pass the top `change_id` into `squash_commit`
as `dest`. Omit `source` to blame the working copy (all uncommitted changes) —
*"where do my current edits belong?"* in one call, then `squash_working_copy`
into the answer. It ranks only commits whose lines the change actually edits, so
a pure addition (new code, nothing modified) has nothing to blame; the reported
`unattributed` count is lines tracing past a merge or outside the history.

## Drop

`drop_commit(commit)` removes a commit; its children rebase onto its parent. The
dropped commit goes to a session trash (`list_trash`), so it's recoverable —
graft it back with `restore_commit(commit, new_parent)` or fold it somewhere
with `squash_commit` (a trashed commit is a valid squash source). Merge commits
and a branch's only commit can't be dropped.

## When things go sideways

- **A conflicting rewrite is held back in full** — `status: conflicts`, with git
  history, HEAD and the working tree untouched until it settles, and no other
  mutation running meanwhile. Resolving it oldest-first, the binary/structural
  cases, and aborting are their own workflow — see the `resolve-conflicts` skill.
- **Address commits by `change_id`, not sha** — shas churn on every rewrite,
  change_ids are stable, so you can chain edits without re-running `list_history`.
- **After any out-of-band git operation** (a commit, branch switch or rebase made
  outside the session) call `reload_repo` before continuing.
- **Safety net & review.** Every landed change is a recorded operation you can
  walk back, dropped commits stay recoverable, and the session is one inspectable
  diff — stepping back, reviewing, or recovering is the `review-and-recover`
  skill. (`discard_working_copy` is the one irreversible action.)
