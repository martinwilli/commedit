# Dogfood tournament — operator stress-test for the commedit MCP surface

A reproducible experiment that drives the **real** commedit MCP server with subagents on
non-trivial history-editing tasks, grades them, and surfaces what works and what bites. Re-run
it whenever the MCP surface, the `commedit-operator` agent, or the bundled skills change — it is
the fastest way to catch a regression in *agent ergonomics* (which unit tests don't cover).

The design is teacher↔student: a controlling agent (the "teacher") defines tasks and an
**answer key**, hands each task to a **student** subagent, then verifies the result out-of-band
and scores it. Two students run per task: the shipped **`commedit-operator`** (Sonnet, all
skills loaded) and a skill-less **`general-purpose`** control — so each run also measures *how
much the operator prompt + skills actually help*.

> Run history (Opus teacher, Sonnet students). Each run's scorecard and findings live under
> [`runs/`](runs/), newest first.

---

## 1. Architecture constraints that shape the whole design

These are *load-bearing* — verified in the source. Don't fight them; design around them.

1. **The MCP server is single-tenant, global state.** It's one `Arc<Mutex<Repo>>`
   (`crates/commedit-mcp/src/server.rs`); `reload_repo` does `*repo = fresh`
   (`crates/commedit-mcp/src/tools/ops.rs`). Every subagent you spawn shares the **one**
   session server. ⇒ **Students must run strictly serially.** Two concurrent students would
   clobber each other's bound repo. (No worktree isolation, no `Agent(isolation:"worktree")`,
   no parallel students — they'd all hit the same server.)
2. **`reload_repo` is repo-scoped.** `resolve_worktree_target` (`crates/commedit-mcp/src/session.rs`)
   compares the git *common dir* and refuses any path that isn't a worktree of the repo the
   server launched against (the plugin binds `${CLAUDE_PROJECT_DIR}`). ⇒ You **cannot** point a
   running session at a standalone scratch repo. The fixture must live as **orphan branches +
   linked worktrees inside this repo** (`stress/*`, under `.worktrees/`), so `reload_repo` can
   re-home onto each one.
3. **Grading needs out-of-band ground truth.** The operator returns a compact 5-field summary
   and is told *not* to dump traces. So: (a) require each student to append a `## Tool Log`, and
   (b) verify yourself from the *teacher* session — `list_operations` (landed mutations, bound to
   the student's worktree **before** the next `reload_repo`, which resets the op-log), plus plain
   `git -C <wt> log/show/diff/fsck/status`. **`list_operations` undercounts effort** — `undo`
   prunes failed attempts from the chain — so combine it with the self-reported Tool Log.
4. **commedit does NOT follow the shell cwd.** It only follows `reload_repo(path=…)`. A plain
   `git` command in the teacher (or a student) runs in whatever cwd the shell happens to hold —
   which bit both a student and the teacher during the first run. **Always use `git -C <abs-path>`.**

---

## 2. The fixture (`stress/base`)

A ~24-commit orphan history of a tiny plain-text "todo CLI" (`src/*.txt`, `server.txt`,
`README.md`, `CHANGELOG.md`), with **one merge** (`Merge branch 'search'`) and a side branch
`stress/hotfix`. Each task targets a disjoint "smell" region so the five tasks never interfere:

| Region | Smell | Task |
|---|---|---|
| `Add config loader and fix logging typo` | one commit, two concerns (config.txt + util.txt) | T2 split |
| `Add parser` + `fixup! Add parser` | a floating autosquash fixup | T1 |
| `Use format_row` before `Add util helper format_row` | wrong logical order | T1 |
| `Debug: add state dump helper` (own file `src/debug.txt`) | stray commit, **cleanly droppable** | T1 |
| `Set timeout to 60` → `Set timeout to 120` (same line) | drop the first ⇒ **genuine conflict** | T3 |
| `Add athentication` (`temp@example.com`, `TOKEN_LEN = 8`) | typo subject + wrong author + bug, deep below tip | T4 |
| `stress/hotfix`: `Fix null deref in parser [BUG-123]` (Alex Fixer) | off-branch fix to pull in | T5 |

Design notes that matter (learned the hard way):
- The drop target (T1) lives in **its own file** so the drop is a clean rebase. An earlier
  version edited `main.txt`, and dropping it **conflicted** with later `main.txt` edits — the
  spurious-drop auto-resolve did *not* trigger. (A finding in its own right; see [`runs/`](runs/).)
- The T3 conflict is genuine because a later commit modifies a line an earlier commit set, and
  dropping the earlier removes the anchor — confirmed it actually holds (not auto-resolved).
- The T5 hotfix touches `parse_all` while the T1 fixup touches `parse` (different lines) so the
  mid-history cherry-pick rebases cleanly past the fixup.

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

## 4. The five tasks (spec + answer key + minimal path)

Change ids are stable across rewrites; the first run's were:
`f6b1ce85` Add parser · `1b6f85b9` fixup! · `a384cdf1` Add util helper · `8da4af94` Use format_row ·
`79732184` kitchen-sink · `8c8db3bb` timeout 60 · `93e8b060` timeout 120 · `efd2cee6` athentication ·
`ebd5b2c2` debug · hotfix sha `faa2ce30…`. **Re-derive them with `list_history` each run.**

| # | Task (intent given to the student) | Minimal path | PASS criteria (verify with `git`) |
|---|---|---|---|
| **T1** | Fold the floating `fixup! Add parser`; reorder so `Add util helper` precedes `Use format_row`; drop the `Debug:` commit. | `squash` + `reorder` + `drop` (3 mutations) | no `fixup!`/`Debug:` commits; helper is parent of use; `git diff stress/base` = only `src/debug.txt` deleted; clean |
| **T2** | Split the kitchen-sink commit into `Add config loader` (config.txt) then `Fix logging typo` (util.txt). | `split_commit(files=[util.txt REVERTED to pre-fix])` + 2 `edit_message` (3 mutations) | first commit touches only config.txt; second only util.txt (LGO→LOG); `git diff stress/base` empty; clean |
| **T3** | Drop `Raise server limits for load test`; resolve the resulting multi-file conflict so the final state is `timeout = 120`, `backlog = 256`, `max_conn = 1000`. | `drop` → loop `read_conflict(oldest)` → `resolve_conflicts(oldest)` until `pending:false` (chain may hold **two** conflicted commits across server.txt + `src/limits.txt`) | `timeout = 120`, `backlog = 256`, `max_conn = 1000`; the raise-limits commit gone; `pending:false`; clean |
| **T4** | On the deep `Add athentication` commit: fix subject typo, set author `Jane Doe`, fix `TOKEN_LEN 8→16`. | `edit_commits(msg+author)` + `replace_in_file` (2 mutations) | subject `Add authentication`; author correct; `TOKEN_LEN = 16`; descendants rebased; clean |
| **T5** | Cherry-pick `stress/hotfix`'s fix to right after `Add parser`; reword to drop `[BUG-123]`. | `cherry_pick_commit(full sha, new_parent)` + `replace_in_message` (2 mutations) | fix after `Add parser`; author `Alex Fixer` preserved; no `[BUG-123]`; `stress/hotfix` untouched; clean |

**T2 is the discriminator.** `split_commit`'s `files` are spliced onto the *original* commit
tree; a changed file you **omit stays in the retained commit** (the child gets nothing). To move
`util.txt`'s change to the child you must list it **reverted to its parent content** — listing
`config.txt` (its own content) silently produces "retained keeps both, child empty."

### Calibration (build the answer key before students run)
On the `stress/cal` worktree, solve each task yourself via the MCP tools, record the resulting
shas / `git log` / file contents, and **confirm difficulty** (esp. that T3 actually holds a
conflict and T2/T5 stay clean). `git -C <cal> reset --hard stress/base && reload_repo` between tasks.

---

## 5. Execution protocol (strictly serial)

For each task T (1→5), each solver S (operator, then control):

1. `git -C <wt> reset --hard stress/base -q && git -C <wt> clean -fdxq` — pristine start.
2. `reload_repo(path=<wt>)` — bind the server to this worktree (also resets the op-log → clean grading slate).
3. Launch **one** student and **await it fully** (never two at once — shared server):
   - operator: `Agent(subagent_type="commedit:commedit-operator", …)`
   - control: `Agent(subagent_type="general-purpose", …)` — tell it the server is already bound, do **not** `reload_repo`, do **not** spawn agents, drive the `mcp__plugin_commedit_commedit__*` tools directly.
   - Both prompts: give the intent (incl. conflict-resolution intent for T3 — otherwise a
     conflict-aware operator correctly stops and asks), and **require a `## Tool Log`** appended.
4. **Verify out-of-band** while still bound to `<wt>`: `list_operations`, then
   `git -C <wt> log --graph / show / diff stress/base / fsck / status`; compare to the answer key.
5. **Score** (rubric below). If correctness fails or there's a clear teachable miss, `SendMessage`
   the same student with targeted feedback and let it retry (cap: 1 round; 2 for T3). A held
   conflict left dangling is discarded by the next `reload_repo` (it's not pending-guarded).

When done: `reload_repo(path=<this repo root>)` to rebind reads back to the checked-out branch.

### Grading rubric (1–5 each + overall)
correctness (gate) · efficiency (mutations vs minimal; thrash; retries — read the **Tool Log**,
not just `list_operations`) · tool-fit (surgical vs whole-file; `change_id` addressing;
`suggest_squash_targets`; oldest-first conflict; correct split partition) · robustness/recovery ·
reporting (compact, accurate, flags decisions) · cleanliness (`fsck`/`status` clean, descendants
rebased). Plus the **operator-vs-control delta** per task.

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
- Keep students on the **shipped** operator + a skill-less control so the operator-vs-control
  delta stays comparable across runs.
- 🔒 **Never** run a commedit *mutation* while the server is bound to anything but a `stress/*`
  worktree, and keep all test refs under `stress/*`. Use `git -C <abs>` everywhere.
