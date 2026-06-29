---
name: resolve-conflicts
description: >-
  Use when a commedit rewrite's rebase conflicts — it returns `status: conflicts`
  and is held back **in full**: unlike a `git rebase` that drops you into a
  conflicted working tree, git history, HEAD and the tree stay frozen until the
  chain is clean. Covers seeing what's held, resolving the deferred conflicts
  file-by-file oldest-first, the binary/structural cases that can't merge as text,
  and aborting (which costs nothing — git was never touched).
---

# Resolve a held conflict with commedit

> **A conflict is the `commedit-operator` subagent's signature job — hand it
> over.** When a rewrite returns `status: conflicts`, the clean move is to
> delegate "resolve the pending conflict" *with a resolution intent* (which side
> wins, or how to reconcile) — the loop below is verbose, exactly the kind of
> work worth keeping out of your context. Drive it yourself only for a small,
> obviously-textual conflict you can see end to end. Either way, the mechanics
> below are what you (or the operator) work from. **Never leave a held conflict
> unreported.**

commedit defers conflicts instead of writing them into your tree. A rewrite
whose rebase doesn't apply cleanly is **held back in full**: it returns
`status: conflicts`, and git history, `HEAD` and the working tree stay exactly
as they were — nothing conflicted is ever exported to git. **No other mutation
runs while one is held.** `pending_status` tells you what's pending (which
commits conflict, in which files); that's where to start if you arrive mid-session
or want to confirm a hold exists.

## The resolution loop

Work the **oldest conflicted commit first** and climb:

1. For each conflicted file in that commit, `read_conflict` returns its content
   with the conflict regions marked (both sides present).
2. Edit it down to the content you want — **remove every conflict marker**,
   keeping the reconciled result.
3. Submit it with `resolve_conflicts`, keyed by the commit's **`change_id`**
   (stable across the rewrite — shas are not). As with every commedit tool, pass
   the `session` id on the call.
4. Re-check `pending_status` and repeat on the next-oldest until it's empty.

Fixing the earliest conflict often **auto-clears its descendants**: a child's
conflict is frequently just the parent's unresolved change cascading down, so
once the ancestor is clean the rebase re-derives the children without markers.
That's why you climb from the bottom rather than picking at the tip.

When the chain goes clean, the whole rewrite exports to git/worktree in one go
and the operation is recorded — the session catches up as if it had never
stalled.

## When it isn't a text merge

Some conflicts can't be reconciled by editing text:

- **Binary files**, or **structural** clashes (a file modified on one side and
  deleted on the other, mode changes) — there are no markers to remove. Resolve
  by choosing a side, including a **`Delete`** resolution to drop the file, via
  `resolve_conflicts`.
- A **split chain** or a genuinely overlapping edit may have no mechanical answer
  at all. Then the only escape is `abort_rewrite`, which discards the held
  rewrite and returns the repo to its exact pre-mutation state — clean, nothing
  lost from history.

## Working-copy overlap

Uncommitted changes ride forward with every rewrite. If they overlap the rewrite
they enter the **same** deferred flow — a held conflict on the working-copy
commit `@`. Resolve it exactly like a commit conflict (`read_conflict` →
reconcile → `resolve_conflicts`), or `abort_rewrite` to back the whole thing out.

## Don't leave it half-done

A held conflict blocks the session until it's either fully resolved or aborted —
there is no partial landing. If you have no resolution instruction, **report it**
(which commits, which files) and ask how to proceed rather than guessing a side
or aborting silently. `abort_rewrite` is always the safe exit; the rewrite was
never written to git, so backing out costs nothing.
