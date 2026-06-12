---
name: revise-commit
description: >-
  Use when changing history that already exists — reword a commit message,
  fix an author/committer name or date, or edit the file contents (the diff)
  of a past commit. Works on any commit reachable from HEAD, including ones
  buried below the tip that `git commit --amend` can't reach; commedit
  rewrites the target in place and rebases its descendants automatically.
---

# Revise an existing commit with commedit

> **Delegate the edit to the `commedit-operator` subagent.** Tell it *what* to
> revise — "reword commit `<id>` to …", "fix the author/date on `<id>`", "edit
> `foo.rs` in commit `<id>` to …" — and it picks the smallest tool, runs it,
> **verifies** the result, and reports back compactly. The mechanics below are what
> the operator works from; reach for these tools directly only when no subagent is
> available, or when you *are* the operator.

`git commit --amend` only reaches the tip. commedit amends **any** commit
reachable from HEAD — its message, its identity, or the files it changed — and
rebases every descendant for you. Reach for the smallest tool that fits the
change, so the untouched content can't drift and the call stays small.

## Message

- **Small change** (a typo, renaming a term) → `replace_in_message`: an exact
  `old`→`new` substitution, unique unless `replace_all`. Only the delta travels.
- **Wholesale rewrite** → `edit_message` with the full new message.
- Wrap the body yourself — commedit stores the message verbatim.

## Identity & dates (metadata)

`edit_identity` sets any of the author/committer name, email and date.
**Omitted fields keep their current value**, and unlike other edits the
committer timestamp is *pinned*, not re-stamped to now. Dates are
`YYYY-MM-DD HH:MM:SS ±HHMM` or RFC 3339.

## File contents (the diff)

- Read the commit's current content first with `show_commit` — it returns the
  diff and the files as they stand.
- **Targeted change** → `replace_in_file`: one or more exact `old`→`new` edits
  (each unique unless `replace_all`; several may target one file), so untouched
  code is never resent and can't drift. Make `old` long enough to match once.
- **Whole-file, add or delete** → `replace_files`: complete new content per
  path, a path the commit lacks is added, `delete_paths` removes files.
- Editing a buried commit re-applies its descendants on top, so a conflict can
  surface (see below).

## A whole range at once

To reword, re-date or re-author **several** commits, use `edit_commits`: every
edit in **one** atomic transaction with a single rebase, applied ancestors-first
so a parent→child range re-dates correctly. Prefer it over looping the
single-commit tools — it's atomic and won't re-stamp committers across the cascade.

> Carving one commit into two (`split_commit`) is possible but error-prone — see
> the `commit-as-you-go` skill for why committing eagerly beats splitting later.

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
