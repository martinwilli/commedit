---
name: commit-as-you-go
description: >-
  Use when implementing a multi-step task that will produce several git
  commits — a feature, a refactor, or an autonomous run. Establishes the
  commit-as-you-go discipline: crystallize each logical unit into a commit
  eagerly with commedit, then refine, instead of writing everything and
  trying to split one big pile at the end.
---

# Commit as you go with commedit

With commedit, **extending or fixing a commit is cheap; splitting a finished pile
of changes into commits is expensive.** So commit *early and eagerly* at logical
boundaries as you work — never write everything and split it afterward. An
imperfect early commit is not a liability: you can reword, re-author, squash,
reorder or drop it later in one call, and descendants rebase automatically.

## The loop

1. **Plan the commit sequence first.** Before coding, decide the commits you
   intend — one per logical change or subsystem. Implement them one at a time.

2. **Crystallize each unit the moment it is coherent** (and its tests pass), then
   move on with a clean tree:
   - Edits/deletions to existing files → `commit_working_copy(message)`
     (it is `git commit -a`; leaves the working tree clean for the next unit).
   - A unit that **introduces new files** → name them in `commit_working_copy`'s
     `add_paths` (repo-relative paths). The working copy otherwise carries only
     tracked-file changes, so a file you just created is invisible until named.
     `add_paths` and tracked edits compose, so "add `foo.rs` + edit `bar.rs`"
     lands as **one** commit.
   - To author a commit somewhere other than on top of HEAD, or from explicit
     contents → `create_commit` (`new_parent` places it; existing descendants
     rebase onto it).

3. **Forgot something that belongs in an earlier commit?** Don't start a new pile.
   Make the edit on disk, then fold it into that commit with
   `squash_working_copy(dest=<change_id>)` — its message is kept. Add `paths` /
   `hunks` / `patches` to fold only part, and `add_paths` to fold in a new file.

4. **Fixing a commit already in history:**
   - message → `replace_in_message` (surgical) or `edit_message` (whole message)
   - code inside a commit → `replace_in_file` (surgical) or `replace_files`
   - re-date / re-author a range → `edit_commits` (one transaction)
   - move / drop a commit → `reorder_commit` / `drop_commit`

5. **Address commits by `change_id`, not sha.** Shas churn on every rewrite;
   change_ids are stable, so you can chain edits without re-running `list_history`.

6. **Avoid `split_commit`.** It makes you hand over the full retained file
   contents — error-prone. Needing it is the signal you should have committed more
   eagerly. If you must carve an existing pile, peel subsets with the
   `paths` / `hunks` / `patches` selection on `commit_working_copy` /
   `squash_working_copy` (run `show_commit` on the working-copy entry first to read
   each file's numbered hunks).

## When things go sideways

- A rewrite whose rebase conflicts returns `status=conflicts` and is held back **in
  full** — git history, HEAD and the working tree stay untouched until it settles.
  Resolve the **oldest** conflicted commit first (`read_conflict` each file, remove
  all markers, `resolve_conflicts`); fixing the earliest often auto-clears its
  descendants. `abort_rewrite` discards the held rewrite. No other mutation runs
  while one is pending.
- After any **out-of-band git operation** (a commit, branch switch or rebase made
  outside the commedit session), call `reload_repo` before continuing.
- The session is a safety net: every landed change is a recorded operation
  (`list_operations`, `undo` / `redo`, `jump_to_operation`). The only
  unrecoverable action is `discard_working_copy`.
