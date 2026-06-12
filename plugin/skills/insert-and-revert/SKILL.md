---
name: insert-and-revert
description: >-
  Use when adding to history rather than editing it in place — create a
  brand-new commit and splice it anywhere in the graph (not just on top of
  HEAD), revert a commit (git-revert-style inverse), or cherry-pick a commit
  from another branch. Each rebases existing descendants and can land
  mid-history.
---

# Insert, revert & cherry-pick with commedit

These tools *introduce* a commit and splice it into the graph at a chosen slot.
The slot is the same everywhere: `new_parent` names the commit that becomes the
new commit's parent — **omit it** for the top of HEAD, or pass `"root"` for the
very first position; when parallel lines converge on that point, `child` picks
the line. Existing descendants rebase onto the inserted commit, so a mid-history
insert can conflict where the slot has diverged (see below). Address the
reference commits by **`change_id`**.

## Create

`create_commit(message, files)` builds a new commit from whole-file contents
spliced onto the parent's tree (`delete_paths` to remove a path, omit both for an
empty commit). Use it to author a commit *below* HEAD, or from explicit contents.
To turn the **current uncommitted changes** into a commit on top of HEAD instead,
use `commit_working_copy` (see the `commit-as-you-go` skill).

## Revert

`revert_commit(commit)` inserts a commit applying the **inverse** of `commit`'s
change (like `git revert`) — back a change out while keeping it in the record.
Merge commits can't be reverted.

## Cherry-pick

`cherry_pick_commit(commit)` inserts a commit re-applying `commit`'s **forward**
change (like `git cherry-pick`). The source may live **outside** the current
branch — pass its full 40-char sha (from `git log <branch>`); its branch is
never touched, only its change is copied. The source's author is preserved and a
`(cherry picked from …)` provenance trailer is recorded. Merge commits can't be
cherry-picked.

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
