# Dogfood tournament — operator stress-test for the commedit MCP surface

A reproducible experiment that drives the **real** commedit MCP server with subagents on
non-trivial history-editing tasks, grades them, and surfaces what works and what bites. Re-run
it whenever the MCP surface, the `commedit-operator` agent, or the bundled skills change — it is
the fastest way to catch a regression in *agent ergonomics* (which unit tests don't cover).

The design is teacher↔student: a controlling agent (the "teacher") defines tasks and an
**answer key**, hands each task to a **student** subagent, then verifies the result out-of-band
and scores it. **Three** students run per task:
- the shipped **`commedit-operator`** (Sonnet, all skills loaded);
- a skill-less **`general-purpose`** **control** (Sonnet) that drives the `mcp__…__*` tools directly;
- a **`general-purpose`** **plain-git** baseline (Sonnet) given *only* Bash/git — no MCP, no skills.

So each run yields two deltas: operator↔control (*does the operator prompt + skills help on top
of the MCP?*) and (operator|control)↔git (*does the MCP help at all over the tool everyone
already has?* — the headline that justifies the project). Each run also records **token cost and
wall-clock per student** (§5) — the only effort metric comparable across all three.

> An **Opus** plain-git baseline (`gito`) ran a model-only A/B in [run 4](runs/4.md) and was then
> **retired**: Opus barely moved the plain-git baseline (≈ equal correctness/score to the Sonnet
> `git` student) at ~2.6× the cost — cost without signal. Plain git's losses are the non-interactive
> tooling friction, not the model.

> [Run 5](runs/5.md) ran the complementary **operator** model A/B — the shipped `commedit-operator`
> on **Sonnet 4.6** vs **Haiku 4.5** (two students, no control/git baseline). Haiku came out **~2.5×
> cheaper and ~30% faster at near-equal call count**, tying Sonnet on most tasks, but with a **lower
> correctness floor**: on T6's silent untracked-file trap it *fabricated* file content (9/10 vs 10/10).

> Run history (Opus teacher; students on Sonnet unless a run says otherwise — run 5 added a Haiku
> operator). Each run's scorecard and findings live under [`runs/`](runs/), newest first.

---

## 1. Architecture constraints that shape the whole design

These are *load-bearing* — verified in the source. Don't fight them; design around them.

1. **The MCP server is multi-tenant.** One server hosts several independent editing **sessions**
   over the one repo it launched against — one session per branch, each with its own `Repo`, trash
   and op-log (`SessionRegistry` in `crates/commedit-mcp/src/session.rs`, wired in
   `crates/commedit-mcp/src/server.rs`). Sessions run in **parallel**; only calls on the **same**
   session serialize. The teacher opens one session per stress worktree via
   `open_session(branch=stress/<task>)` — each `.worktrees/<task>` has its `stress/<task>` branch
   checked out, so the session opens **worktree-bound** there — and each student addresses **its own**
   session by that branch id. ⇒ **Students CAN now run in parallel** (independent sessions share no
   state). A strictly-serial run remains the comparison **baseline**: the goal of going parallel is to
   reproduce the serial scorecard (same correctness) at lower wall-clock.
2. **Sessions are repo-scoped.** `resolve_worktree_target` (`crates/commedit-mcp/src/session.rs`)
   compares the git *common dir* and refuses any `reload_repo` path that isn't a worktree of the repo
   the server launched against (the plugin binds `${CLAUDE_PROJECT_DIR}`); likewise `open_session`
   only opens a branch of that same repo. ⇒ You **cannot** point a session at a standalone scratch
   repo. The fixture must live as **orphan branches + linked worktrees inside this repo** (`stress/*`,
   under `.worktrees/`), so each session opens (or `reload_repo` re-homes) onto one of them.
3. **Grading needs out-of-band ground truth.** The operator returns a compact 5-field summary
   and is told *not* to dump traces. So: (a) require each student to append a `## Tool Log`, and
   (b) verify yourself from the *teacher* session — `list_operations(session=<id>)` (landed
   mutations on that session **before** its next `reload_repo`, which resets that session's op-log),
   plus plain `git -C <wt> log/show/diff/fsck/status`. **`list_operations` undercounts effort** — `undo`
   prunes failed attempts from the chain — so combine it with the self-reported Tool Log.
4. **commedit does NOT follow the shell cwd.** It only follows `reload_repo(path=…)`. A plain
   `git` command in the teacher (or a student) runs in whatever cwd the shell happens to hold —
   which bit both a student and the teacher during the first run. **Always use `git -C <abs-path>`.**
5. **The plain-git student bypasses the MCP server entirely.** The Sonnet `git` baseline never calls
   `open_session`; it shares no session state and works on its own worktree purely via
   `git -C <wt>` (constraint #4 bites it hardest). All **three** students can now run in parallel —
   op/ctl because each drives its **own** session, the git student because it touches no session at
   all — so parallelism is no longer the git student's alone.
   ⚠️ The harness **forbids interactive git** (`rebase -i`, `add -i`), so they
   must do non-interactive surgery:
   `GIT_SEQUENCE_EDITOR=true git rebase -i --autosquash`, `rebase --onto`, scripted `--exec`,
   `commit --amend`, hand-resolved conflicts (+ `rerere`). That mirrors commedit's own
   non-interactive pitch, so the comparison is fair — and how hard this turns out to be is itself
   a finding.

---

## 2. The fixture (`stress/base`)

A ~35-commit orphan history of a tiny plain-text "todo CLI" (`src/*.txt`, `server.txt`,
`README.md`, `CHANGELOG.md`), with **one merge** (`Merge branch 'search'`) and a side branch
`stress/hotfix`. Each task targets a disjoint "smell" region so the tasks never interfere:

| Region | Smell | Task |
|---|---|---|
| `Add config loader and fix logging typo` | one commit, two concerns (config.txt + util.txt) | T2 split |
| `Add parser` + `fixup! Add parser` | a floating autosquash fixup | T1 |
| `Use format_row` before `Add util helper format_row` | wrong logical order | T1 |
| `Debug: add state dump helper` (own file `src/debug.txt`) | stray commit, **cleanly droppable** | T1 |
| `Raise server limits for load test` (timeout+backlog in server.txt, max_conn in `src/limits.txt`), re-touched by `Set timeout to 120` and `Bump backlog and max_conn` | drop it ⇒ **genuine conflict, 3 hunks over 2 files** | T3 |
| `Add athentication` (`temp@example.com`, `TOKEN_LEN = 8`) | typo subject + wrong author + bug, deep below tip | T4 |
| `stress/hotfix`: `Fix null deref in parser [BUG-123]` (Alex Fixer) | off-branch fix to pull in | T5 |
| `Add storage and stats helpers` (own files `src/store.txt` + `src/stats.txt`) | squash-up target for a dirty WC (no committed smell) | T6 |
| `Merge branch 'search'` (bare subject) + `Make search case-insensitive` directly above it | merge needs a body; the follow-up belongs *in* the merge | T7 |
| `Add experimental telemetry` (own file `src/telemetry.txt`) + `Add metrics endpoint` (own file `src/metrics.txt`), both buried | one to revert, one to drop-then-recover | T8 |
| `Add report header`/`body`/`footer` (own file `src/report.txt`), a contiguous range by `jdoe <jane.doe@bigcorp.example>` dated 2000 | wrong author+committer identity and bogus dates across 3 commits | T9 |
| `Add cache module` + `Bump cache capacity to 256` + `Bump cache capacity to 512` (own file `src/cache.txt`) | two same-line bumps that conflict when reordered | T10 |

Design notes that matter (learned the hard way):
- The drop target (T1) lives in **its own file** so the drop is a clean rebase. An earlier
  version edited `main.txt`, and dropping it **conflicted** with later `main.txt` edits — the
  spurious-drop auto-resolve did *not* trigger. (A finding in its own right; see [`runs/`](runs/).)
- The T3 conflict is genuine and now **spans 3 hunks over 2 files**: the dropped commit set
  `timeout`+`backlog` (server.txt) and `max_conn` (`src/limits.txt`), and two later commits
  (`Set timeout to 120`, `Bump backlog and max_conn`) each re-touch those same lines — so dropping
  the anchor conflicts in two descendant commits at once. Each is a true 3-way conflict (the values
  genuinely differ), so it holds (not auto-resolved); resolution must land `timeout=120`,
  `backlog=256`, `max_conn=1000`. Verified with a `git rebase --onto` drop before wiring it up.
- The T5 hotfix touches `parse_all` while the T1 fixup touches `parse` (different lines) so the
  mid-history cherry-pick rebases cleanly past the fixup.
- T6 has **no committed smell** — its region is a clean buried commit (`Add storage and stats
  helpers`, own files so a fold rebases clean). The "smell" is injected into the *working copy* at
  run time by [`t6-dirty.sh`](t6-dirty.sh) (§5 step 1), since T6 tests the working-copy → history
  path, the one axis no other task touches.
- T7 is the **merge-editing** axis — the one thing every other task only *preserves*. The fixture
  already had the `Merge branch 'search'` merge sitting idle; T7 finally edits it. `Make search
  case-insensitive` is the merge's **direct child** and the only thing between it and the merge that
  touches `src/search.txt`, so squashing it *into* the merge commutes cleanly (a merge is a valid
  squash *destination*, never a source). The merge's bare `-m` subject (no body) is the second smell
  — reword it to add one, per the repo's own merge convention. Verified the squash commutes and the
  reword preserves the two parents before wiring it up.
- T8 is the **revert + recover** axis (the whole `review-and-recover` skill, which had zero task
  coverage). Both commits live in **their own files** so the revert, the drop, and the restore are
  all clean rebases. Splitting the work over two files lets one half test `revert_commit` (inverse
  at the tip, original kept) and the other test the `drop → list_trash → restore_commit` round-trip
  independently. The restore lands the commit at the **tip**, not its original buried slot, so a
  lazy `undo` of the drop won't satisfy it — the student must actually go through the trash.
- T9 is the **bulk re-date / re-identify** axis — the headline `edit_commits` claim ("re-dating a
  whole parent→child range stays correct"), which T4 only ever touched on a *single* commit. The
  three `report` commits are a **contiguous range** with the wrong author+committer email and bogus
  year-2000 dates, baked by an explicit-date helper (so they don't burn the January `DAY` budget)
  and built ancestors-first. The smell is metadata only — the tree is unchanged, so the batch must
  leave `git diff stress/base` empty. The `committer timestamp is pinned` (run-2 finding #1 in
  reverse): set the committer email explicitly and the batch must *not* re-stamp the timestamp.
- T10 is the **conflict variant** that T3 doesn't reach: a *reorder* conflict (the `CleanTip`
  resolution strategy) rather than a *drop* conflict (the `Drop` strategy), plus the `abort_rewrite`
  path. The two cache bumps set the **same line** to **different values** (256, then 512), so
  reordering the 512-bump below the 256-bump cannot auto-merge — it reliably **holds** (no spurious
  auto-resolve to dodge), which is what makes the answer key deterministic. Verified with a
  cherry-pick in the reordered order before wiring it up; the held conflict is fully abortable.
  Deliberately *genuine* (not spurious) — constructing a reliably-spurious reorder is finicky (the
  T1 drop's spurious auto-resolve famously *didn't* fire; see [run 1](runs/1.md) finding #2), so
  whether a spurious reorder now auto-resolves stays a **calibration probe**, not a baked answer key.

---

## 3. Setup recipe

Run [`reposetup.sh`](reposetup.sh) — it is self-contained and idempotent (bails if `stress/base`
already exists; adjust `REPO` in the script if your checkout lives elsewhere):

```bash
./dogfood/reposetup.sh
```

It builds the §2 fixture: a fresh ~24-commit orphan history with the baked smells, the merge, the
side hotfix branch, and per-(task × solver) branches + linked worktrees under `.worktrees/`. All
refs are namespaced `stress/*`; nothing touches your real branches. See [§6](#6-re-run-checklist-as-the-mcp-evolves)
for teardown.

---

## 4. The tasks (spec + answer key + minimal path)

Change ids are stable across rewrites; the first run's were:
`f6b1ce85` Add parser · `1b6f85b9` fixup! · `a384cdf1` Add util helper · `8da4af94` Use format_row ·
`79732184` kitchen-sink · `8c8db3bb` raise limits (T3 drop target) · `93e8b060` timeout 120 ·
`efd2cee6` athentication · `ebd5b2c2` debug · hotfix sha `faa2ce30…` · plus `Bump backlog and
max_conn` (T3) and `Add storage and stats helpers` (T6 fold target).
**Re-derive them with `list_history` each run** — the `Bump backlog and max_conn` and
storage/stats commits are new this version, so they had no run-1 change id.

| # | Task (intent given to the student) | Minimal path | PASS criteria (verify with `git`) |
|---|---|---|---|
| **T1** | Fold the floating `fixup! Add parser`; reorder so `Add util helper` precedes `Use format_row`; drop the `Debug:` commit. | `squash` + `reorder` + `drop` (3 mutations) | no `fixup!`/`Debug:` commits; helper is parent of use; `git diff stress/base` = only `src/debug.txt` deleted; clean |
| **T2** | Split the kitchen-sink commit into `Add config loader` (config.txt) then `Fix logging typo` (util.txt). | `split_commit(files=[util.txt REVERTED to pre-fix])` + 2 `edit_message` (3 mutations) | first commit touches only config.txt; second only util.txt (LGO→LOG); `git diff stress/base` empty; clean |
| **T3** | Drop `Raise server limits for load test`; resolve the resulting multi-file conflict so the final state is `timeout = 120`, `backlog = 256`, `max_conn = 1000`. | `drop` → loop `read_conflict(oldest)` → `resolve_conflicts(oldest)` until `pending:false` (chain may hold **two** conflicted commits across server.txt + `src/limits.txt`) | `timeout = 120`, `backlog = 256`, `max_conn = 1000`; the raise-limits commit gone; `pending:false`; clean |
| **T4** | On the deep `Add athentication` commit: fix subject typo, set author `Jane Doe`, fix `TOKEN_LEN 8→16`. | `edit_commits(msg+author)` + `replace_in_file` (2 mutations) | subject `Add authentication`; author correct; `TOKEN_LEN = 16`; descendants rebased; clean |
| **T5** | Cherry-pick `stress/hotfix`'s fix to right after `Add parser`; reword to drop `[BUG-123]`. | `cherry_pick_commit(full sha, new_parent)` + `replace_in_message` (2 mutations) | fix after `Add parser`; author `Alex Fixer` preserved; no `[BUG-123]`; `stress/hotfix` untouched; clean |
| **T6** | From the dirty WC ([`t6-dirty.sh`](t6-dirty.sh)): fold the `load`/`total` helpers into `Add storage and stats helpers`; then craft `Add backup command` (the `backup` fn + new `src/backup.txt`) and `Add average stat` (the `average` fn) as two new commits. | `show_commit(@)` → `squash_working_copy(dest, hunks=[store.txt#0, stats.txt#0])` → `commit_working_copy("Add backup command", paths=[store.txt, backup.txt], add_paths=[backup.txt])` → `commit_working_copy("Add average stat")` (1 read + 3 mutations) | `store.txt` `load`+`total` folded into the buried commit, descendants rebased; two new commits on top with the right partition (`backup`+`backup.txt`, then `average`); WC clean; `git diff stress/base` = all the dirt, distributed; clean |
| **T7** | Edit the `Merge branch 'search'` merge: reword it to add a body (subject kept, then a blank line + one or two sentences); and fold the follow-up `Make search case-insensitive` into the merge. | `edit_message(merge, body added)` + `squash_commit(source=case-insensitive, dest=merge, mode=fixup)` (2 mutations) | merge subject still `Merge branch 'search'`, now with a body; merge still has **2 parents**; no standalone `Make search case-insensitive` commit; its `src/search.txt` change present; `git diff stress/base` empty; descendants rebased; clean |
| **T8** | The buried `Add experimental telemetry` shipped a privacy bug — **revert** it (inverse at the tip, keep the original). Separately, **drop** the buried `Add metrics endpoint`, then change your mind and **restore** it from the trash to the tip. | `revert_commit(telemetry)` + `drop_commit(metrics)` + `list_trash` (read) + `restore_commit(metrics, new_parent=tip)` (3 mutations + 1 read) | a revert commit removing `src/telemetry.txt` near the tip; original telemetry commit still present; `Add metrics endpoint` back in history (at the tip) with `src/metrics.txt` present; trash empty; `git diff stress/base` = only `src/telemetry.txt` deleted; descendants rebased; clean |
| **T9** | The three `report` commits (`Add report header`/`body`/`footer`) were authored on a misconfigured machine: fix all three in one batch — set author+committer email to `jane.doe@example.com`, name `Jane Doe`, and re-date them to 2025-01-25/26/27 (keep their order). | `edit_commits([3 × {commit, identity: name+email+author_time+committer_time}])` (1 mutation) | all three carry author+committer `Jane Doe <jane.doe@example.com>` and the 2025 dates, in order; subjects unchanged; descendants rebased; `git diff stress/base` empty (metadata-only); clean |
| **T10** | Reorder `Bump cache capacity to 512` to come *before* `Bump cache capacity to 256` — the same-line edits conflict and the rewrite holds. First **abort** to confirm the safety net (history/tree untouched), then redo it and **resolve** oldest-first so the final `capacity = 256`. | `reorder_commit(512-bump, new_parent=cache-module)` → `abort_rewrite`; then again → loop `read_conflict(oldest)`/`resolve_conflicts(oldest)` until `pending:false` (chain holds **two** conflicted commits) | after the abort: region identical to `stress/base`, `pending:false`; after the resolve: `512-bump` precedes `256-bump`, final tip `capacity = 256`, both commits present, `pending:false`; descendants rebased; clean |

**T2 is the discriminator for history-editing.** `split_commit`'s `files` are spliced onto the
*original* commit tree; a changed file you **omit stays in the retained commit** (the child gets
nothing). To move `util.txt`'s change to the child you must list it **reverted to its parent
content** — listing `config.txt` (its own content) silently produces "retained keeps both, child
empty."

**T6 is the discriminator for the working-copy → history path.** It is the one task starting from a
*dirty* working copy, and the only one exercising partial hunk selection (`paths`/`hunks`/`patches`).
Two traps: (a) `src/store.txt` and `src/stats.txt` each carry hunks bound for **different**
destinations, so the fold must pick hunk indices (`git add -p` territory), not whole files — read
them from `show_commit(@)` first; (b) `src/backup.txt` is **untracked**, so `commit_working_copy`
silently skips it unless it is named in **both** `add_paths` and `paths`. The plain-git baseline
feels both traps hardest — the harness forbids interactive `git add -p`, so it must hand-craft
partial patches.

**T7 is the discriminator for merge editing.** It's the only task that *changes* the merge rather
than preserving it — and the one where plain git is most painful (rewording or squashing into a
*buried* merge needs `rebase --rebase-merges` with a scripted sequence editor, which the harness's
`GIT_EDITOR=true` actively breaks; see [run 2](runs/2.md) finding #3). The trap for the MCP students
is the opposite of what you'd guess: a merge can be a squash *destination* but never a *source*
(`squash_commit` refuses it), and the two rewrites of the same merge re-stamp its committer (run-2
finding #1) — so don't grade committer identity here, and prefer reword-then-fixup so the dest
message survives.

**T8 is the discriminator for the recover/safety-net axis.** It is the only task whose *correct*
path runs a commit through the **trash** and back, and the only one using `revert_commit`. The trap
is the `drop → restore` round-trip: dropping defaults to the trash (restorable), so `restore_commit`
reads `list_trash` and grafts it back at a chosen parent — landing it at the **tip** (not its old
slot) is what distinguishes a real restore from an `undo`. For plain git the recover half has no
direct analogue — the reflog is the closest thing, which is itself the point of the comparison.

**T9 is the discriminator for batch metadata edits.** One `edit_commits` call re-dates and
re-identifies a whole 3-commit range atomically with a single rebase; the trap is doing it as three
separate `edit_identity` calls (three rebases, and each rewrite risks re-stamping the committer of
the ones below it). Because the change is metadata-only, the PASS gate is a tree-identical
`git diff stress/base` *plus* the exact author/committer/date fields — so it catches both a missed
commit and a committer re-stamp.

**T10 is the discriminator for the non-drop conflict + abort path.** T3 covers a *drop* conflict
(the `Drop` rebuild-from-the-clean-prefix strategy); T10 covers a *reorder* conflict (the `CleanTip`
peel-from-the-clean-tip strategy) — same oldest-first resolution loop, different internal path — and
adds the one thing no other task does: `abort_rewrite`, the proof that a held conflict leaves git
**completely frozen** (refs/HEAD/worktree untouched) until you either resolve or bail. The trap is
that aborting is *not* an `undo` of a landed mutation — nothing landed; the rewrite was never
exported. A conflict-aware student left to its own judgment will (correctly) stop and ask before
resolving, so the prompt must carry the resolve-and-abort intent explicitly (as T3's does).

### Plain-git baseline (what the git student faces)
The git student gets the *same* intents and PASS criteria — same topology, same file content —
but only `git`, and **non-interactive only** (no `rebase -i` prompt; drive the sequence editor).
Loose, not prescriptive: grade what it *actually* does (from its Tool Log), not adherence to this.
- **T1** — `GIT_SEQUENCE_EDITOR=true git rebase -i --autosquash <base>` folds the `fixup!`;
  reorder + drop need a scripted sequence editor (rewrite the todo list) or `rebase --onto`.
- **T2** — at a `rebase` stop, `reset HEAD^` the kitchen-sink commit and re-commit in two parts
  (config.txt, then util.txt). Still the discriminator — fiddly to split clean.
- **T3** — drop the `Raise server limits for load test` commit via rebase; hit the *same* genuine
  conflict, now in **two** descendant commits across server.txt + `src/limits.txt`; hand-resolve
  each (`timeout=120`, `backlog=256`, `max_conn=1000`) and `rebase --continue` (rerere helps).
- **T4** — mark the deep `athentication` commit `edit` (scripted sequence editor), then
  `commit --amend --author="Jane Doe <…>" -m "Add authentication"` + edit `TOKEN_LEN`.
- **T5** — `git cherry-pick` / `rebase --onto` to land it after `Add parser`, reword via amend;
  leave `stress/hotfix` untouched.
- **T7** — reword + squash-into a *buried merge* non-interactively: `rebase --rebase-merges <base>`
  with a scripted `GIT_SEQUENCE_EDITOR` (mark the `merge -C` line for reword and the follow-up for
  `fixup`/`squash`), supplying the new body via `-m`/`GIT_EDITOR` — fragile under the harness's
  `GIT_EDITOR=true`. Alternative: reset to the merge's first parent, `git merge -s ours`/re-merge
  with the new message + folded change, then `cherry-pick` the descendants. Fiddly; that's the point.
- **T8** — `git revert --no-edit <telemetry>` lands the inverse at the tip cleanly. The recover half
  has no git equivalent: drop the metrics commit with `rebase --onto <metrics>^ <metrics>`, then
  "restore" it by `cherry-pick`ing it back from the **reflog** (`git reflog`/`ORIG_HEAD`) to the
  tip — git has no trash, so finding the dropped commit *is* the task. Grade the reflog hunt.
- **T9** — re-date+re-identify a buried range non-interactively: `rebase <base> --exec` can't vary
  per-commit, so it's a scripted `rebase` stopping at each report commit for `commit --amend
  --author=… --date=… --reset-author`-style fixes, or a `filter-branch`/`filter-repo --env-filter`
  keyed on the commit. `--reset-author` only fixes the author; the committer needs
  `GIT_COMMITTER_*` env on the amend. Fiddly across three commits; that's the point.
- **T10** — reorder the two bumps via a scripted `GIT_SEQUENCE_EDITOR` `rebase <base>` (swap the two
  `pick` lines) or `rebase --onto`; hit the same same-line conflict, hand-resolve to `capacity=256`,
  `rebase --continue`. There is no "abort then redo" beat for git that maps to `abort_rewrite` — a
  mid-rebase `rebase --abort` is the closest analogue (it also restores the pre-rebase state), so
  grade whether the student demonstrates the bail-and-restore at all. `rerere` helps on the redo.
- **T6** — no interactive `git add -p` (forbidden), so hand-craft partial patches: `git apply
  --cached <patch>` the `load`/`total` hunks → `commit --fixup=<storage sha>` → stash the rest →
  `GIT_SEQUENCE_EDITOR=true git rebase -i --autosquash <base>` to fold → unstash → commit the
  `backup` fn + `git add src/backup.txt` as one commit, then `average` as another. Fiddly; that's
  the point.

### Calibration (build the answer key before students run)
On the `stress/cal` worktree, solve each task yourself via the MCP tools, record the resulting
shas / `git log` / file contents, and **confirm difficulty** (esp. that T3 actually holds its
multi-file conflict across both descendants, T2/T5 stay clean, T7's squash-into-merge keeps the
two parents, T8's dropped commit lands in the trash where `restore_commit` can reach it, T9's
re-date batch stays tree-identical, and T10's reorder actually *holds* a conflict — and that
`abort_rewrite` leaves the region identical to `stress/base`). For T6, run
`./dogfood/t6-dirty.sh <cal>` after the reset to seed
the dirty WC, then confirm the fold rebases clean and the partition lands.
Open the calibration session once with `open_session(branch=stress/cal)` (worktree-bound at
`<cal>`); between tasks `git -C <cal> reset --hard stress/base && reload_repo(session=<cal id>)`
(scoped — does not disturb other sessions).

---

## 5. Execution protocol (per-session, parallelizable)

For each task T (1→10), each solver S (operator, control, git):

1. `git -C <wt> reset --hard stress/base -q && git -C <wt> clean -fdxq` — pristine start.
   **T6 only:** then `./dogfood/t6-dirty.sh <wt>` to seed the dirty working copy (identical for
   every solver). The other tasks start from a clean tree; T6 starts dirty — that's its whole point.
2. **op/ctl only:** `open_session(branch=stress/<task>)` **once per worktree** — `stress/<task>` is
   checked out at `.worktrees/<task>`, so the session opens **worktree-bound** there. The returned id
   (= the branch short-name) is the session selector for every later MCP call on this task. **git
   student:** skip this — it never touches the server.
3. Launch the student(s) and **await fully**. Students for **different** tasks/sessions may run
   **concurrently**: op/ctl on distinct sessions share no state, and the git student shares nothing
   (it touches no session at all, working only on its own `stress/t<task>-git` worktree).
   (Two students on the **same** session must not overlap — but normally each task → its own session,
   so this is moot.)
   - operator: `Agent(subagent_type="commedit:commedit-operator", …)` (Sonnet)
   - control: `Agent(subagent_type="general-purpose", …)` (Sonnet) — do **not** spawn agents; drive the `mcp__plugin_commedit_commedit__*` tools directly.
   - git: `Agent(subagent_type="general-purpose", …)` (Sonnet) — give it **only** Bash/git; **forbid** MCP
     tools, commedit skills, and spawning agents; non-interactive git only (see §4 baseline).
   - All prompts: give the intent (incl. conflict-resolution intent for T3 — otherwise a
     conflict-aware operator correctly stops and asks), and **require a `## Tool Log`** appended.
     **op/ctl prompts must state the session id** and instruct the student to pass `session=<id>` on
     **every** MCP tool call (the server's own MCP instructions document the required selector).
4. **Verify out-of-band**: for op/ctl, `list_operations(session=<id>)` then
   `git -C <wt> log --graph / show / diff stress/base / fsck / status`; for the git student,
   verify **purely from `git -C <wt>`** (`list_operations` is N/A — it never touched the server).
   Compare to the answer key.
5. **Capture metrics** (before the next `reload_repo(session=<id>)` resets that session's op-log):
   record the student's **tokens + wall-clock** from its transcript (recipe below).
6. **Score** (rubric below). If correctness fails or there's a clear teachable miss, `SendMessage`
   the same student with targeted feedback and let it retry (cap: 1 round; 2 for T3). A held
   conflict left dangling in a session is discarded by reloading **that** session
   (`reload_repo(session=<id>)`; it's not pending-guarded).

> ⚠️ **Ref-write race under full parallelism (run-4 finding).** Running all 40 students at once puts
> ~40 git ref-writers (the plain-git rebases/commits **and** the MCP sessions' git-export bookkeeping)
> on the **one shared `.git` common-dir** simultaneously. A concurrent `pack-refs`/`gc --auto` can then
> silently drop a freshly-written loose ref, **reverting a student's correct result to an earlier value
> after it finished** (run 4 lost 3 of 40 this way; 3 more self-recovered from reflog). The object store
> is safe (append-only); only **ref updates** race. Before a fully-parallel run, prefer one of: a
> separate clone/common-dir per student; `git config gc.auto 0` (+ `maintenance.auto false`) on the
> repo; or capped concurrency. Either way, **verify out-of-band from `git` after the run settles** — the
> revert can land post-completion, so student self-reports are not authoritative. See [run 4](runs/4.md).

### Between repeats of a task on the same worktree
`git -C <wt> reset --hard stress/base` then `reload_repo(session=<id>)` — scoped, so it does **not**
disturb other sessions running other tasks. A held conflict left in the session is discarded by the
same reload.

### Metrics capture (tokens + wall-clock, per student)
Each subagent writes its own transcript. ⚠️ **Under parallel execution the "newest file" shortcut is
ambiguous** — several students write transcripts at once, so identify each student's transcript by
its **Agent id / return correlation** (the id the `Agent(…)` call returns), not by recency. The
`ls -t … | head -1` shortcut below is valid **only for a strictly-serial run**, where the newest
file is unambiguously the student you just awaited. Run this **right after** the student returns,
naming the *teacher's* session dir:
```bash
SUB=~/.claude/projects/-home-mwilli-repos-commedit/<TEACHER_SESSION>/subagents
F=$(ls -t "$SUB"/agent-*.jsonl | head -1)   # newest = the student just awaited — SERIAL RUNS ONLY
python3 - "$F" <<'PY'
import sys, json
inp=out=cc=cr=0; first=last=None
for line in open(sys.argv[1]):
    d=json.loads(line); t=d.get("timestamp"); u=d.get("message",{}).get("usage")
    if t: first=first or t; last=t
    if u:
        inp+=u.get("input_tokens",0);  out+=u.get("output_tokens",0)
        cc+=u.get("cache_creation_input_tokens",0); cr+=u.get("cache_read_input_tokens",0)
# $/Mtok by component. All three students run on Sonnet 4.6: input 3.00, output
# 15.00, 5-min cache WRITE 3.75 (1.25x input), cache READ 0.30 (0.1x input).
# Pricing each component at its own rate is the whole point: it stops the
# operator's one-time cache_create tax from being double-counted and surfaces
# git's cache_read volume.
IN,OUT,CW,CR = 3.00, 15.00, 3.75, 0.30        # Sonnet
cost = (inp*IN + out*OUT + cc*CW + cr*CR) / 1e6
print(f"in={inp} out={out} cache_create={cc} cache_read={cr} "
      f"billable~={inp+out+cc} cost=${cost:.4f} span={first} -> {last}")
PY
```
`<TEACHER_SESSION>` is the controlling session's UUID (the dir under
`projects/-home-mwilli-repos-commedit/` that owns a `subagents/`). `cache_read` is cheap re-read
(≈0.1×), so report components — don't lump it into one number.

**Report `cost=$…` per student in every run's scorecard** (it's the one figure comparable across
all three solvers — tokens-per-component aren't, since the mix differs). The snippet prices each
component at its true rate, which is what makes the one-time/repetition split honest:
- `cache_create` (≈1.25× input) is the operator's **one-time prompt-materialization tax** — paid
  once per cold spawn, *independent of how much work the agent does*. The tournament is its
  pessimistic case (every student is a fresh cold spawn re-paying it in full); a warm, reused
  session hits `cache_read` (≈0.1×) instead, so real repeated use amortizes it. It still lands in
  the $ estimate, so read a high operator `cost` as a **prompt floor, not effort**.
- `cache_read` (≈0.1×) is cheap *per token* but its **volume balloons for plain-git**, which
  re-reads its growing transcript on every one of its many calls. Pricing it properly (rather than
  excluding it, as `billable~` does) is what makes the operator's efficiency edge come out *larger*
  in dollars than `billable~` suggests — so always report `cost`, not just `billable~`.

When done: `close_session(session=<id>)` each per-task session (the registry refuses to drop the
**last** one, so leave the launch session — or just `reload_repo(session=<launch id>)` it back to the
checked-out branch).

### Grading rubric (1–5 each + overall)
correctness (gate) · **efficiency — `cost=$…` first** (per-component dollars, recipe under *Metrics
capture* — the only currency comparable across all three solvers; tokens-by-component and wall-clock
are noisier secondaries; mutations-vs-minimal an MCP-students-only secondary, since `list_operations`
undercounts — `undo` prunes — read the **Tool Log** for true effort) · tool-fit (op/ctl: surgical
vs whole-file, `change_id` addressing, `suggest_squash_targets`, oldest-first conflict, correct
split partition; git: idiomatic *non-interactive* git — `--autosquash`, `--onto`, `rerere`) ·
robustness/recovery · reporting (compact, accurate, flags decisions) · cleanliness (`fsck`/`status`
clean, descendants rebased). Deltas per task: **operator↔control** (skill value) and
**(op|ctl)↔git** (MCP value — the headline).

> **Token caveat.** The operator pays a **prompt tax** (large system prompt + on-demand skills) it
> must earn back through fewer/cheaper turns. Report `cache_creation` vs `cache_read` separately so
> that one-time tax stays visible and isn't double-counted (cache_read ≈0.1×; first-turn
> cache_create ≈1.25× of fresh input), **and the per-component `cost=$…`** (rates + recipe above) —
> the dollar figure is the one number comparable across all three solvers.

### Reading the cost numbers (what `billable~` does and doesn't mean)
Most of an MCP student's token cost is **not work** — it's a fixed **per-spawn
prompt-materialization tax**: each fresh subagent writes its whole prompt into cache once (base
Claude Code system prompt + the agent definition + all the always-on `commedit` tool schemas + MCP
connect instructions + any loaded skill). That `cache_create` block is **independent of how much the
agent then does**, so fewer tool calls or less wall-clock don't shrink it — those savings surface as
tiny `input`/`output` and as cheap `cache_read`, never in `cache_create`. Read a high operator
`billable~` as a **prompt floor, not effort**.

Two consequences when comparing students:
- **The tournament is the pessimistic case for this tax.** Every student is a fresh, isolated,
  serial, *cold* spawn, so each re-pays the floor in full. A warm session that reuses the operator
  hits `cache_read` (≈0.1×) instead of re-paying `cache_create` (cache TTL ~5 min) — real repeated
  use amortizes it.
- **`billable~` (input + output + cache_create) flatters plain-git.** Excluding `cache_read` is fair
  *per token* (the cheap ≈0.1× re-read), but plain-git's `cache_read` *volume* balloons — it
  re-reads its growing transcript on every one of its many calls. Price the components properly
  (operator cost dominated by the one `cache_create` write; git cost dominated by `cache_read` volume
  + real in/out across many calls) and the operator's efficiency edge comes out *larger* in money
  than `billable~` shows.

So always report `cache_create` vs `cache_read` **separately**, and fold them into the per-student
`cost=$…` (rates + recipe under *Metrics capture*) — that dollar figure is the headline efficiency
number in each scorecard, the only one comparable across all three solvers.

**Warming is organic; the lever is fewer turns, not a prefetch (run-4 finding).** Cross-spawn warming
of the shared prompt prefix happens on its own — in a fan-out the *first* spawn of each cache prefix
(keyed per agent-def × tool-set × model) pays the full `cache_create`, and every sibling within the
~5-min TTL reads it back as cheap `cache_read` (run 4: first operator 85k `cache_create`, the nine
siblings ~30k). So an explicit **warmup student buys little**: it only shifts that one cold spike to
`cache_read` (~$0.2 for the operator class) and **cannot reduce `cache_read`**, which is the dominant
component (~58% of operator cost) because every turn re-reads the whole always-on prefix. And it does
nothing for the non-operator students, whose `cache_create` is intrinsic task work (the control's
`ToolSearch` discovery; plain-git's long bash context), not a cold shared prefix. To actually cut cost,
cut **turns**, **shrink the always-on prefix** (every turn's `cache_read` shrinks with it), or **reuse
a warm session** across tasks — the per-task cold spawn here is the deliberate pessimistic case. See
[run 4](runs/4.md) Headline 3.

---

## 6. Re-run checklist (as the MCP evolves)

- **Teardown first** (idempotent):
  ```bash
  REPO=/home/mwilli/repos/commedit
  for wt in $(git -C "$REPO" worktree list --porcelain | awk '/^worktree/{print $2}' | grep commedit-stress); do
    git -C "$REPO" worktree remove --force "$wt"; done
  git -C "$REPO" worktree prune
  for b in $(git -C "$REPO" for-each-ref --format='%(refname:short)' 'refs/heads/stress/*'); do
    git -C "$REPO" branch -D "$b"; done
  # optional: git -C "$REPO" gc --prune=now
  ```
- Rebuild the fixture (`./dogfood/reposetup.sh`), re-derive change ids with `list_history` (they
  differ per build), re-run calibration (§4) — **don't trust last run's answer keys blindly**; a
  tool change may alter call counts or which path is "minimal."
- If tools were **added/renamed**: update the minimal paths in §4 and the rubric's tool-fit notes.
  Re-check whether the footguns recorded under [`runs/`](runs/) are fixed (does `split_commit` now
  warn on an empty child? does the interior-drop auto-resolve fire?).
- Keep students on the **shipped** operator + a skill-less MCP control + a Sonnet **git-only**
  baseline (`git`), so both deltas stay comparable across runs. The `reposetup.sh` provisioning loop
  creates the `*-git` worktrees; teardown globs them (the `commedit-stress` / `stress/*` prefixes).
- **Capture per-student metrics** every run (§5 step 5): tokens (components — esp. `cache_read`
  vs `cache_create`) + wall-clock from each `subagents/agent-*.jsonl`. Judge efficiency by token
  cost across MCP versions, not call counts.
- 🔒 **Never** run a commedit *mutation* on a session other than a `stress/*` one (don't open or
  mutate the launch session's real branch), and keep all test refs under `stress/*`. Use
  `git -C <abs>` everywhere.
