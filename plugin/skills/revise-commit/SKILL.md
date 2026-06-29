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

> **Drive a single revision yourself; delegate the messy ones.** When you hold the
> target's change_id and the edit is one clean call — `replace_in_message`,
> `edit_message`, `edit_identity`, `replace_in_file` — make it directly: the result
> returns the new head, and a surgical replace fails loudly on a missed match, so it
> is self-verifying with no follow-up read. **Delegate to the `commedit-operator`
> subagent** when it turns into a loop or a search — a conflict to resolve, or
> finding which buried commit to edit by reading several diffs. The mechanics below
> are what you (or the operator) work from.

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

> Carving one commit into two (`split_commit`) has a precise contract — `files`
> is the content to KEEP; to move a file's change out, pass it at its parent
> content (a no-op split is refused). See the `commit-as-you-go` skill for the
> details and why committing eagerly beats splitting later.

## When things go sideways

- **A conflicting rewrite is held back in full** — `status: conflicts`, with git
  history, HEAD and the working tree untouched until it settles, and no other
  mutation running meanwhile. Resolving it oldest-first, the binary/structural
  cases, and aborting are their own workflow — see the `resolve-conflicts` skill.
- **Address commits by `change_id`, not sha** — shas churn on every rewrite,
  change_ids are stable, so you can chain edits without re-running `list_history`.
  Each tool also needs a `session` id (the branch short-name) — pass it on every
  call; editing across branches or worktrees is the `work-in-worktree` skill.
- **A plain `git commit` on top of HEAD needs no reload** — the session catches up
  automatically on the next tool call. Reserve `reload_repo(session, …)` for an
  out-of-band change it can't absorb: a **branch switch**, or history **rewritten**
  by `git rebase`/`reset`/`commit --amend` (it restarts that session's trash and
  op-log, so don't run it reflexively).
- **Safety net & review.** Every landed change is a recorded operation you can
  walk back, dropped commits stay recoverable, and the session is one inspectable
  diff — stepping back, reviewing, or recovering is the `review-and-recover`
  skill. (`discard_working_copy` is the one irreversible action.)
