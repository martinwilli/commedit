---
name: commit-as-you-go
description: >-
  Use when implementing a multi-step task that will produce several git
  commits — a feature, a refactor, or an autonomous run. Establishes the
  commit-as-you-go discipline: crystallize each logical unit into a commit
  eagerly — plain `git commit` for a new commit on HEAD — then refine it in
  place with commedit, instead of writing everything and trying to split one
  big pile at the end.
---

# Commit as you go with commedit

> **Crystallize each new unit with plain git; refine it with commedit.** A fresh
> commit on top of HEAD needs no rebase, so `git add` + `git commit` is the simplest
> way to lay one down. commedit's strength is changing commits that *already exist* —
> folding a fix into an earlier commit, rewording, re-dating, reordering, dropping,
> or inserting a commit below the tip. **Drive a single, directly-addressed
> refinement yourself** (`reword <id>`, `reorder <id> before <id>`, `squash <fixup>
> into <id>`): each commedit result is self-verifying — it returns the new change_id
> and reshaped topology — and the session **catches up to your plain `git commit`s
> automatically**, so no `reload_repo` and no follow-up read. **Delegate to the
> `commedit-operator` subagent** when the step is a *loop, a search, or a conflict*:
> resolving a conflict, an open-ended reshuffle, or finding where a fix belongs by
> reading several diffs — work worth keeping out of your context. The tool-level
> guidance below is what you (or the operator) work from.

With commedit, **extending or fixing a commit is cheap; splitting a finished pile
of changes into commits is expensive.** So commit *early and eagerly* at logical
boundaries as you work — never write everything and split it afterward. An
imperfect early commit is not a liability: you can reword, re-author, squash,
reorder or drop it later in one call, and descendants rebase automatically.

## The loop

1. **Plan the commit sequence first.** Before coding, decide the commits you
   intend — one per logical change or subsystem. Implement them one at a time.

2. **Crystallize each unit the moment it is coherent** (and its tests pass) with
   plain git — a new commit on top of HEAD needs no rebase — then move on with a
   clean tree:
   - Edits/deletions to existing files → `git commit -am '<message>'`.
   - A unit that **introduces new files** → `git add <path…>` first, then
     `git commit` — the staged new files and tracked edits compose into **one**
     commit.
   - To author a commit somewhere *other than* on top of HEAD (below it, at a fork,
     or at the root), or from explicit contents → that *is* commedit's job:
     delegate "create a commit from these files below `<id>`" (`create_commit`
     places it and rebases existing descendants onto it), since git can't without
     a rebase.

3. **Forgot something that belongs in an earlier commit?** Don't start a new pile.
   Make the edit on disk, then fold it into that commit with
   `squash_working_copy(dest=<change_id>)` — its message is kept. Add `paths` /
   `hunks` / `patches` to fold only part, and `add_paths` to fold in a new file.
   A brand-new (untracked) file is **silently skipped** unless named in
   `add_paths`; in a partial fold it must be in **both** `add_paths` and `paths`.

4. **Fixing a commit already in history:**
   - message → `replace_in_message` (surgical) or `edit_message` (whole message)
   - code inside a commit → `replace_in_file` (surgical) or `replace_files`
   - re-date / re-author a range → `edit_commits` (one transaction)
   - move / drop a commit → `reorder_commit` / `drop_commit`

5. **Address commits by `change_id`, not sha.** Shas churn on every rewrite;
   change_ids are stable, so you can chain edits without re-running `list_history`.

6. **Splitting — carve forward, don't split back.** Carving a pile you have
   *not yet committed* is the easy, recommended split: commit part of the working
   copy now with the `paths` / `hunks` / `patches` selection on
   `commit_working_copy` (run `show_commit` on the working-copy entry first to read
   each file's numbered hunks), then commit or fold the rest. That is why eager
   commits mean you rarely need to split. Splitting a commit *already in history*
   is the hard case — `split_commit` is the only tool for it, but it makes you
   hand over the full retained file contents, so it's error-prone. Needing it is
   the signal you should have carved earlier; avoid it where you can, and delegate
   it to the `commedit-operator` subagent when you can't.

## When things go sideways

- **A conflicting rewrite is held back in full** — `status: conflicts`, with git
  history, HEAD and the working tree untouched until it settles, and no other
  mutation running meanwhile. Resolving it oldest-first, the binary/structural
  cases, and aborting are their own workflow — see the `resolve-conflicts` skill.
- A plain `git commit` on top of HEAD needs **no** `reload_repo` — the commedit
  session catches up to it automatically on the next tool call. Reserve `reload_repo`
  for out-of-band changes it can't absorb in place: a **branch switch**, or history
  **rewritten** by `git rebase`/`reset`/`commit --amend` (it resets the session's
  trash and op-log, so don't run it reflexively).
- **Off-worktree, this loop doesn't apply.** When commedit edits a branch you have
  *not* checked out (`reload_repo`'s `branch`, or launched as `<path> <branch>`),
  there is **no working copy**: no plain `git commit` to crystallize, and the
  `commit_working_copy` / `squash_working_copy` / `discard_working_copy` tools are
  refused. You edit the branch's existing commits directly instead. (This is a
  different thing from the `work-in-worktree` skill, where you *create* a worktree
  and keep a live working copy in it.)
- **Safety net & review.** Every landed change is a recorded operation you can
  walk back, dropped commits stay recoverable, and the session is one inspectable
  diff — stepping back, reviewing, or recovering is the `review-and-recover`
  skill. (`discard_working_copy` is the one irreversible action.)
