# Dogfood tournament — operator stress-test for the commedit MCP surface

A reproducible experiment that drives the **real** commedit MCP server with subagents on
non-trivial history-editing tasks, grades them, and surfaces what works and what bites. Re-run
it whenever the MCP surface, the `commedit-operator` agent, or the bundled skills change — it is
the fastest way to catch a regression in *agent ergonomics* (which unit tests don't cover).

The design is teacher↔student: a controlling agent (the "teacher") defines tasks and an
**answer key**, hands each task to a **student** subagent, then verifies the result out-of-band
and scores it. **Three** students run per task:
- the shipped **`commedit-operator`** (Sonnet, all skills loaded);
- a skill-less **`general-purpose`** **control** that drives the `mcp__…__*` tools directly;
- a **`general-purpose`** **plain-git** baseline given *only* Bash/git — no MCP, no skills.

So each run yields two deltas: operator↔control (*does the operator prompt + skills help on top
of the MCP?*) and (operator|control)↔git (*does the MCP help at all over the tool everyone
already has?* — the headline that justifies the project). Each run also records **token cost and
wall-clock per student** (§5) — the only effort metric comparable across all three.

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
5. **The plain-git student bypasses the MCP server entirely.** It never calls `reload_repo`,
   shares nothing with the `Arc<Mutex<Repo>>`, and works on its worktree purely via
   `git -C <wt>` (constraint #4 bites it hardest). It is therefore the *one* student that could
   run in parallel — but **keep it serial** for a clean grading slate. ⚠️ The harness **forbids
   interactive git** (`rebase -i`, `add -i`), so it must do non-interactive surgery:
   `GIT_SEQUENCE_EDITOR=true git rebase -i --autosquash`, `rebase --onto`, scripted `--exec`,
   `commit --amend`, hand-resolved conflicts (+ `rerere`). That mirrors commedit's own
   non-interactive pitch, so the comparison is fair — and how hard this turns out to be is itself
   a finding.

---

## 2. The fixture (`stress/base`)

A ~25-commit orphan history of a tiny plain-text "todo CLI" (`src/*.txt`, `server.txt`,
`README.md`, `CHANGELOG.md`), with **one merge** (`Merge branch 'search'`) and a side branch
`stress/hotfix`. Each task targets a disjoint "smell" region so the six tasks never interfere:

| Region | Smell | Task |
|---|---|---|
| `Add config loader and fix logging typo` | one commit, two concerns (config.txt + util.txt) | T2 split |
| `Add parser` + `fixup! Add parser` | a floating autosquash fixup | T1 |
| `Use format_row` before `Add util helper format_row` | wrong logical order | T1 |
| `Debug: add state dump helper` (own file `src/debug.txt`) | stray commit, **cleanly droppable** | T1 |
| `Set timeout to 60` → `Set timeout to 120` (same line) | drop the first ⇒ **genuine conflict** | T3 |
| `Add athentication` (`temp@example.com`, `TOKEN_LEN = 8`) | typo subject + wrong author + bug, deep below tip | T4 |
| `stress/hotfix`: `Fix null deref in parser [BUG-123]` (Alex Fixer) | off-branch fix to pull in | T5 |
| `Add storage and stats helpers` (own files `src/store.txt` + `src/stats.txt`) | squash-up target for a dirty WC (no committed smell) | T6 |

Design notes that matter (learned the hard way):
- The drop target (T1) lives in **its own file** so the drop is a clean rebase. An earlier
  version edited `main.txt`, and dropping it **conflicted** with later `main.txt` edits — the
  spurious-drop auto-resolve did *not* trigger. (A finding in its own right; see [`runs/`](runs/).)
- The T3 conflict is genuine because a later commit modifies a line an earlier commit set, and
  dropping the earlier removes the anchor — confirmed it actually holds (not auto-resolved).
- The T5 hotfix touches `parse_all` while the T1 fixup touches `parse` (different lines) so the
  mid-history cherry-pick rebases cleanly past the fixup.
- T6 has **no committed smell** — its region is a clean buried commit (`Add storage and stats
  helpers`, own files so a fold rebases clean). The "smell" is injected into the *working copy* at
  run time by [`t6-dirty.sh`](t6-dirty.sh) (§5 step 1), since T6 tests the working-copy → history
  path, the one axis no other task touches.

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

## 4. The six tasks (spec + answer key + minimal path)

Change ids are stable across rewrites; the first run's were:
`f6b1ce85` Add parser · `1b6f85b9` fixup! · `a384cdf1` Add util helper · `8da4af94` Use format_row ·
`79732184` kitchen-sink · `8c8db3bb` timeout 60 · `93e8b060` timeout 120 · `efd2cee6` athentication ·
`ebd5b2c2` debug · hotfix sha `faa2ce30…` · `Add storage and stats helpers` (T6 fold target).
**Re-derive them with `list_history` each run** — the storage/stats commit is new this version,
so it had no run-1 change id.

| # | Task (intent given to the student) | Minimal path | PASS criteria (verify with `git`) |
|---|---|---|---|
| **T1** | Fold the floating `fixup! Add parser`; reorder so `Add util helper` precedes `Use format_row`; drop the `Debug:` commit. | `squash` + `reorder` + `drop` (3 mutations) | no `fixup!`/`Debug:` commits; helper is parent of use; `git diff stress/base` = only `src/debug.txt` deleted; clean |
| **T2** | Split the kitchen-sink commit into `Add config loader` (config.txt) then `Fix logging typo` (util.txt). | `split_commit(files=[util.txt REVERTED to pre-fix])` + 2 `edit_message` (3 mutations) | first commit touches only config.txt; second only util.txt (LGO→LOG); `git diff stress/base` empty; clean |
| **T3** | Drop `Raise server limits for load test`; resolve the resulting multi-file conflict so the final state is `timeout = 120`, `backlog = 256`, `max_conn = 1000`. | `drop` → loop `read_conflict(oldest)` → `resolve_conflicts(oldest)` until `pending:false` (chain may hold **two** conflicted commits across server.txt + `src/limits.txt`) | `timeout = 120`, `backlog = 256`, `max_conn = 1000`; the raise-limits commit gone; `pending:false`; clean |
| **T4** | On the deep `Add athentication` commit: fix subject typo, set author `Jane Doe`, fix `TOKEN_LEN 8→16`. | `edit_commits(msg+author)` + `replace_in_file` (2 mutations) | subject `Add authentication`; author correct; `TOKEN_LEN = 16`; descendants rebased; clean |
| **T5** | Cherry-pick `stress/hotfix`'s fix to right after `Add parser`; reword to drop `[BUG-123]`. | `cherry_pick_commit(full sha, new_parent)` + `replace_in_message` (2 mutations) | fix after `Add parser`; author `Alex Fixer` preserved; no `[BUG-123]`; `stress/hotfix` untouched; clean |
| **T6** | From the dirty WC ([`t6-dirty.sh`](t6-dirty.sh)): fold the `load`/`total` helpers into `Add storage and stats helpers`; then craft `Add backup command` (the `backup` fn + new `src/backup.txt`) and `Add average stat` (the `average` fn) as two new commits. | `show_commit(@)` → `squash_working_copy(dest, hunks=[store.txt#0, stats.txt#0])` → `commit_working_copy("Add backup command", paths=[store.txt, backup.txt], add_paths=[backup.txt])` → `commit_working_copy("Add average stat")` (1 read + 3 mutations) | `store.txt` `load`+`total` folded into the buried commit, descendants rebased; two new commits on top with the right partition (`backup`+`backup.txt`, then `average`); WC clean; `git diff stress/base` = all the dirt, distributed; clean |

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

### Plain-git baseline (what the git student faces)
The git student gets the *same* intents and PASS criteria — same topology, same file content —
but only `git`, and **non-interactive only** (no `rebase -i` prompt; drive the sequence editor).
Loose, not prescriptive: grade what it *actually* does (from its Tool Log), not adherence to this.
- **T1** — `GIT_SEQUENCE_EDITOR=true git rebase -i --autosquash <base>` folds the `fixup!`;
  reorder + drop need a scripted sequence editor (rewrite the todo list) or `rebase --onto`.
- **T2** — at a `rebase` stop, `reset HEAD^` the kitchen-sink commit and re-commit in two parts
  (config.txt, then util.txt). Still the discriminator — fiddly to split clean.
- **T3** — drop the `timeout 60` commit via rebase; hit the *same* genuine conflict; hand-resolve
  to `timeout = 120` and `rebase --continue`.
- **T4** — mark the deep `athentication` commit `edit` (scripted sequence editor), then
  `commit --amend --author="Jane Doe <…>" -m "Add authentication"` + edit `TOKEN_LEN`.
- **T5** — `git cherry-pick` / `rebase --onto` to land it after `Add parser`, reword via amend;
  leave `stress/hotfix` untouched.
- **T6** — no interactive `git add -p` (forbidden), so hand-craft partial patches: `git apply
  --cached <patch>` the `load`/`total` hunks → `commit --fixup=<storage sha>` → stash the rest →
  `GIT_SEQUENCE_EDITOR=true git rebase -i --autosquash <base>` to fold → unstash → commit the
  `backup` fn + `git add src/backup.txt` as one commit, then `average` as another. Fiddly; that's
  the point.

### Calibration (build the answer key before students run)
On the `stress/cal` worktree, solve each task yourself via the MCP tools, record the resulting
shas / `git log` / file contents, and **confirm difficulty** (esp. that T3 actually holds a
conflict and T2/T5 stay clean). For T6, run `./dogfood/t6-dirty.sh <cal>` after the reset to seed
the dirty WC, then confirm the fold rebases clean and the partition lands.
`git -C <cal> reset --hard stress/base && reload_repo` between tasks.

---

## 5. Execution protocol (strictly serial)

For each task T (1→6), each solver S (operator, then control, then git):

1. `git -C <wt> reset --hard stress/base -q && git -C <wt> clean -fdxq` — pristine start.
   **T6 only:** then `./dogfood/t6-dirty.sh <wt>` to seed the dirty working copy (identical for
   every solver). The other tasks start from a clean tree; T6 starts dirty — that's its whole point.
2. **op/ctl only:** `reload_repo(path=<wt>)` — bind the server to this worktree (also resets the
   op-log → clean grading slate). **git student:** skip this — it never touches the server.
3. Launch **one** student and **await it fully** (never two at once — op/ctl share the server;
   keep git serial too for a clean slate):
   - operator: `Agent(subagent_type="commedit:commedit-operator", …)`
   - control: `Agent(subagent_type="general-purpose", …)` — tell it the server is already bound, do **not** `reload_repo`, do **not** spawn agents, drive the `mcp__plugin_commedit_commedit__*` tools directly.
   - git: `Agent(subagent_type="general-purpose", …)` — give it **only** Bash/git; **forbid** MCP
     tools, commedit skills, and spawning agents; non-interactive git only (see §4 baseline).
   - All prompts: give the intent (incl. conflict-resolution intent for T3 — otherwise a
     conflict-aware operator correctly stops and asks), and **require a `## Tool Log`** appended.
4. **Verify out-of-band**: for op/ctl, while still bound to `<wt>`, `list_operations` then
   `git -C <wt> log --graph / show / diff stress/base / fsck / status`; for the git student,
   verify **purely from `git -C <wt>`** (`list_operations` is N/A — it never touched the server).
   Compare to the answer key.
5. **Capture metrics** (before the next `reload_repo` resets anything): record the student's
   **tokens + wall-clock** from its transcript (recipe below).
6. **Score** (rubric below). If correctness fails or there's a clear teachable miss, `SendMessage`
   the same student with targeted feedback and let it retry (cap: 1 round; 2 for T3). A held
   conflict left dangling is discarded by the next `reload_repo` (it's not pending-guarded).

### Metrics capture (tokens + wall-clock, per student)
Each subagent writes its own transcript; serial execution makes "newest file" unambiguous. Run
this **right after** the student returns, naming the *teacher's* session dir:
```bash
SUB=~/.claude/projects/-home-mwilli-repos-commedit/<TEACHER_SESSION>/subagents
F=$(ls -t "$SUB"/agent-*.jsonl | head -1)   # newest = the student just awaited (serial!)
python3 - "$F" <<'PY'
import sys, json
inp=out=cc=cr=0; first=last=None
for line in open(sys.argv[1]):
    d=json.loads(line); t=d.get("timestamp"); u=d.get("message",{}).get("usage")
    if t: first=first or t; last=t
    if u:
        inp+=u.get("input_tokens",0);  out+=u.get("output_tokens",0)
        cc+=u.get("cache_creation_input_tokens",0); cr+=u.get("cache_read_input_tokens",0)
print(f"in={inp} out={out} cache_create={cc} cache_read={cr} "
      f"billable~={inp+out+cc} span={first} -> {last}")
PY
```
`<TEACHER_SESSION>` is the controlling session's UUID (the dir under
`projects/-home-mwilli-repos-commedit/` that owns a `subagents/`). `cache_read` is cheap re-read
(≈0.1×), so report components — don't lump it into one number.

When done: `reload_repo(path=<this repo root>)` to rebind reads back to the checked-out branch.

### Grading rubric (1–5 each + overall)
correctness (gate) · **efficiency — tokens first** (the only cross-student currency; wall-clock
a noisy secondary; mutations-vs-minimal an MCP-students-only secondary, since `list_operations`
undercounts — `undo` prunes — read the **Tool Log** for true effort) · tool-fit (op/ctl: surgical
vs whole-file, `change_id` addressing, `suggest_squash_targets`, oldest-first conflict, correct
split partition; git: idiomatic *non-interactive* git — `--autosquash`, `--onto`, `rerere`) ·
robustness/recovery · reporting (compact, accurate, flags decisions) · cleanliness (`fsck`/`status`
clean, descendants rebased). Deltas per task: **operator↔control** (skill value) and
**(op|ctl)↔git** (MCP value — the headline).

> **Token caveat.** The operator pays a **prompt tax** (large system prompt + on-demand skills) it
> must earn back through fewer/cheaper turns. Report `cache_creation` vs `cache_read` separately so
> that one-time tax stays visible and isn't double-counted (cache_read ≈0.1×; first-turn
> cache_create ≈1.25× of fresh input). Optionally fold to a $ estimate via model pricing.

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

So always report `cache_create` vs `cache_read` **separately**, and fold to a $ estimate (model
pricing) when you need one comparable number across students.

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
- Keep students on the **shipped** operator + a skill-less MCP control + a **git-only** baseline,
  so all three deltas stay comparable across runs. The `reposetup.sh` provisioning loop already
  creates the `*-git` worktrees; teardown globs them.
- **Capture per-student metrics** every run (§5 step 5): tokens (components — esp. `cache_read`
  vs `cache_create`) + wall-clock from each `subagents/agent-*.jsonl`. Judge efficiency by token
  cost across MCP versions, not call counts.
- 🔒 **Never** run a commedit *mutation* while the server is bound to anything but a `stress/*`
  worktree, and keep all test refs under `stress/*`. Use `git -C <abs>` everywhere.
