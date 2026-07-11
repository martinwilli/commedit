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

1. For each conflicted file in that commit, `read_conflict` returns its `text`
   with the conflict regions marked (`<<<<<<< … ======= … >>>>>>>`, both sides
   present) plus a `marker_len`. By default `text` is **windowed** — just the
   conflict hunks plus a few lines of context, with far runs collapsed into
   `[... N lines omitted ...]` sentinels — so inspecting a small conflict in a
   big file stays cheap. Widen with `context_lines`, or pass `full: true` for
   the whole file (needed only if you'll resolve by resending the entire `text`).
   The sentinels are display-only: never put one inside a patch `old`.
2. Decide the reconciled result for each marked region.
3. Hand it back with `resolve_conflicts`, keyed by the commit's **`change_id`**
   (stable across the rewrite — shas are not) and the `session` id. Per file,
   pick exactly one of three modes:
   - **`edits` — the default.** A list of targeted `{old, new}` patches applied
     to the exact `text` `read_conflict` returned (each `old` must match once,
     unless `replace_all`). The idiomatic move is a *single* edit whose `old` is
     the whole `<<<<<<< … >>>>>>>` block and whose `new` is the chosen resolved
     lines — the untouched context around it is never restated. Reach for this
     on anything but a tiny file: it sends only the delta (far cheaper in
     tokens) and **cannot corrupt content it never touches**. You never retype
     the file, so there is no way to silently mistranscribe the parts you
     weren't even changing.
   - **`text`.** The complete resolved file with every marker removed, echoing
     its `marker_len`. Reserve it for a genuinely tiny file, where resending the
     whole thing costs about the same as a patch. (It's also the mode the GTK
     app uses.)
   - **`delete: true`.** Drop the file — how a modify/delete conflict settles
     (see below).
4. Re-check `pending_status` and repeat on the next-oldest until it's empty.

A region only resolves once its markers are gone: an `edit` (or `text`) that
leaves a `<<<<<<<`/`=======`/`>>>>>>>` behind keeps the file conflicted. A patch
whose `old` doesn't match (or matches ambiguously) comes back as an error with a
hint — fix the edit and resubmit; nothing was applied.

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
  by choosing a side, including `delete: true` in `resolve_conflicts` to drop
  the file.
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
