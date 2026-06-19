---
name: insert-and-revert
description: >-
  Use when adding to history rather than editing it in place — create a
  brand-new commit and splice it anywhere in the graph (not just on top of
  HEAD), revert a commit (git-revert-style inverse), cherry-pick a commit
  from another branch, or introduce a merge above a commit. Each rebases existing
  descendants and can land mid-history.
---

# Insert, revert & cherry-pick with commedit

> **Drive a single insert yourself; delegate when it conflicts or needs scouting.**
> A lone `create_commit` / `revert_commit` / `cherry_pick_commit` / `merge_out_commit`
> at a slot you can name — make it directly: the result returns the new commit and
> reshaped topology, so it is self-verifying with no follow-up read. **Delegate to
> the `commedit-operator` subagent** when the splice conflicts (a mid-history insert
> where the slot diverged) or you must hunt down the right source or slot first. The
> tool detail below is what you (or the operator) work from.

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
A new commit on *top* of HEAD needs no rebase, so don't reach for a tool: just
`git add` / `git commit` the working changes yourself (see the `commit-as-you-go`
skill) — `commit_working_copy` is only for committing a deterministic *subset* of
the tree in-session.

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

## Merge out

`merge_out_commit(commit)` introduces a **merge** directly above a single-parent
commit `C` (parent `P`): the new merge `M` gets parents `[P, C]` and `C`'s tree,
so it adds no change of its own and `C` becomes a one-commit side branch you can
then move further commits onto. It is the one tool that *creates* a merge — a way
to organize a linear history into a branchy one. A merge commit or the repository
root can't be merged out (no single parent); `M` carries a pro-forma message to
reword afterwards. The result is branchy, so read its shape back with `show_graph`
(the standalone view of the `topology` a restructuring returns) before moving more
commits onto the new side branch.

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
