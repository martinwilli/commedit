// Dogfood tournament execution script — see dogfood/README.md §5, mandatory per that section.
// Fan out the 12 tasks x 3 solvers x K repeats grid via Workflow, verify each repeat out-of-band
// with verify.sh, and return raw results for the teacher to score and price (see README §5
// "Metrics capture" for recovering real $ from each agent's transcript by agentId).
//
// Prereqs before launching: `./dogfood/reposetup.sh` (fresh fixture); teardown after (README §6).
//
// To run: Workflow({ scriptPath: 'dogfood/workflow.js' }), then edit K below and re-invoke for a
// different repeat count (k=1 smoke/regression, k>=3 for a comparative/ranking run — README §5).
export const meta = {
  name: 'dogfood-tournament',
  description: 'Dogfood tournament: 12 tasks x 3 solvers x K repeats, verified out-of-band',
  phases: [
    { title: 'Run cells' },
  ],
}

const K = 3 // repeats per cell — see README §5 "Repeats for variance" for how to pick this

const REPO = '/home/mwilli/repos/commedit'
const VERIFY = REPO + '/dogfood/verify.sh'
const wt = (n, s) => REPO + '/.worktrees/commedit-stress-t' + n + '-' + s

const TASKS = [
  {n:1, intent:"Fold the floating 'fixup! Add parser'; reorder so 'Add util helper' precedes 'Use format_row'; drop the 'Debug:' commit."},
  {n:2, intent:"Split the kitchen-sink commit into 'Add config loader' (config.txt) then 'Fix logging typo' (util.txt). NOTE: split_commit's files are spliced onto the ORIGINAL commit tree; to move util.txt's change to the child you must list it REVERTED to its parent content, not as its own new content."},
  {n:3, intent:"Drop the 'Raise server limits for load test' commit; this creates a multi-file conflict. Resolve it (you have explicit intent to resolve, not just stop and ask) oldest-first so the final state is timeout=120, backlog=256, max_conn=1000."},
  {n:4, intent:"On the deep 'Add athentication' commit: fix the subject typo (authentication), set author to Jane Doe, and fix TOKEN_LEN from 8 to 16."},
  {n:5, intent:"Cherry-pick stress/hotfix's fix commit to land right after 'Add parser'; reword it to drop the '[BUG-123]' tag from the message. Do not touch stress/hotfix itself."},
  {n:6, intent:"The working copy is DIRTY (already seeded). Fold the load/total helper hunks in src/store.txt and src/stats.txt into the buried commit 'Add storage and stats helpers' (this requires hunk-level selection, not whole-file — read the working copy diff first to find the right hunks). Then craft two NEW commits from what remains dirty: 'Add backup command' (the backup fn plus new untracked src/backup.txt — untracked files must be named in both add_paths and paths) and 'Add average stat' (the average fn).", dirty:true},
  {n:7, intent:"Edit the 'Merge branch search' merge commit: reword it to add a body (keep the subject, add a blank line then 1-2 sentences) — the merge must still have 2 parents afterward. Also fold the follow-up 'Make search case-insensitive' commit into the merge as a fixup (note: a merge can be a squash destination but never a source)."},
  {n:8, intent:"Revert the buried 'Add experimental telemetry' commit (inverse at the tip, keep the original in history). Separately: drop the buried 'Add metrics endpoint' commit, then restore it from the trash back onto the tip (not its old slot)."},
  {n:9, intent:"The three 'Add report header'/'body'/'footer' commits need their identity fixed: in ONE batch, set author+committer to name 'Jane Doe' email 'jane.doe@example.com' and re-date them to 2025-01-25/26/27 respectively (keep order). Prefer a single atomic batch call over three separate edits to avoid re-stamping issues."},
  {n:10, intent:"Reorder 'Bump cache capacity to 512' to come before 'Bump cache capacity to 256' — this is a same-line conflict and the rewrite will hold/conflict. First, do it and then ABORT the rewrite to confirm the safety net (history/tree must end up untouched, identical to stress/base). Then redo the same reorder and this time resolve the conflict oldest-first (you have explicit intent to resolve) so the final capacity=256, both commits present."},
  {n:11, intent:"See t11Prompt — cross-branch move (unused, dedicated prompt path)."},
  {n:12, intent:"The stray 'Tune retry policy' commit fixes code but has no fixup!/squash! prefix and names no target in its subject. Find which commit introduced the lines it edits (content-blame, not subject-match) and fold it in as a fixup."},
]

function resetCmd(n, s, dirty) {
  const w = wt(n, s)
  let cmd = 'git -C ' + w + ' reset --hard stress/base -q && git -C ' + w + ' clean -fdxq'
  if (dirty) cmd += ' && ' + REPO + '/dogfood/t6-dirty.sh ' + w
  return cmd
}

function opctlPrompt(task, solver, repeatIdx) {
  const n = task.n
  const w = wt(n, solver)
  const branch = 'stress/t' + n + '-' + solver
  return [
    'You are driving the REAL commedit MCP server (tools named mcp__plugin_commedit_commedit__*) against a git worktree, for a stress-test cell. This is repeat ' + (repeatIdx+1) + ' of ' + K + ' of the same cell.',
    '',
    'Setup (do this first, via Bash):',
    '1. Run: ' + resetCmd(n, solver, task.dirty),
    '2. Ensure an MCP session is open on branch ' + branch + ' (worktree-bound at ' + w + '): call open_session(branch="' + branch + '"). If it errors because the branch is already open (from a prior repeat), instead call reload_repo(session="' + branch + '") to reset that session view to the freshly reset worktree.',
    '3. Use session="' + branch + '" on EVERY subsequent MCP call.',
    '',
    'Task intent: ' + task.intent,
    '',
    solver === 'ctl'
      ? 'You are the CONTROL solver: drive the mcp__plugin_commedit_commedit__* tools DIRECTLY yourself. Do NOT use any commedit skill, and do NOT spawn any subagents.'
      : 'You are the shipped commedit-operator: use your normal skills and tools to solve this.',
    '',
    'When done, reply with a "## Tool Log" section listing each MCP tool call you made in order (tool name + 1-line purpose), and a "## Result" one-line self-assessment.',
  ].join('\n')
}

function gitPrompt(task, repeatIdx) {
  const n = task.n
  const w = wt(n, 'git')
  return [
    'You are the plain-git baseline solver for a stress-test cell (repeat ' + (repeatIdx+1) + ' of ' + K + '). You have ONLY Bash/git access — no MCP tools, no commedit skills, do NOT spawn subagents, non-interactive git only (no interactive rebase prompts; script the sequence editor with GIT_SEQUENCE_EDITOR / GIT_EDITOR as needed).',
    '',
    'Setup: run: ' + resetCmd(n, 'git', task.dirty),
    'Then work only inside worktree ' + w + ' on branch stress/t' + n + '-git. Never touch shared refs like stress/base or other stress/* branches.',
    '',
    'Task intent: ' + task.intent,
    '',
    'When done, reply with a "## Tool Log" section listing the key git commands you ran in order, and a "## Result" one-line self-assessment.',
  ].join('\n')
}

function t11Prompt(solver, repeatIdx) {
  const trunkW = wt(11, solver)
  const trunkBranch = 'stress/t11-' + solver
  const sibBranch = 'stress/t11s-' + solver
  if (solver === 'git') {
    const sibW = REPO + '/.worktrees/commedit-stress-t11s-git'
    return [
      'Plain-git baseline, T11, repeat ' + (repeatIdx+1) + ' of ' + K + '. Bash/git only, non-interactive, no MCP, no skills, no subagents.',
      'Reset: git -C ' + trunkW + ' reset --hard stress/base -q && git -C ' + trunkW + ' clean -fdxq && git -C ' + sibW + ' checkout ' + sibBranch + ' -q && git -C ' + sibW + ' reset --hard stress/feature -q && git -C ' + sibW + ' clean -fdxq',
      "Task: 'Add version string' was committed on branch " + sibBranch + ' (worktree ' + sibW + ') but belongs on trunk ' + trunkBranch + ' (worktree ' + trunkW + "). Move it: land it on trunk, remove it from the feature branch, keep the feature's own 'Add experimental flag' commit intact on the feature branch. Only touch these two branches/worktrees.",
      'IMPORTANT: worktree ' + sibW + ' must stay checked out on ' + sibBranch + ' for the ENTIRE task. Never `git checkout` a different branch in that worktree (e.g. never `checkout stress/feature`) — if you need to compare against stress/feature, use `git show`/`git log stress/feature` instead of checking it out. Fix mistakes with `git reset --hard`/`git rebase --onto` while staying on ' + sibBranch + ', not by switching branches. The oracle grades the sibling by its BRANCH REF (' + sibBranch + '), not by worktree HEAD, so leaving this worktree on the wrong branch leaves that ref permanently stale even if the content looks right.',
      'Reply with "## Tool Log" and "## Result".',
    ].join('\n')
  }
  return [
    'You are the ' + (solver === 'op' ? 'shipped commedit-operator' : 'CONTROL solver (drive mcp__plugin_commedit_commedit__* tools directly, no skills, no subagents)') + ' for T11, repeat ' + (repeatIdx+1) + ' of ' + K + '.',
    'Setup: reset trunk worktree: git -C ' + trunkW + ' reset --hard stress/base -q && git -C ' + trunkW + ' clean -fdxq ; reset sibling branch ref: git -C ' + REPO + ' branch -f ' + sibBranch + ' stress/feature -q',
    'A trunk session should already exist or you should open it: open_session(branch="' + trunkBranch + '") (worktree-bound at ' + trunkW + '); if already open, reload_repo(session="' + trunkBranch + '").',
    'The sibling branch ' + sibBranch + ' is checked out NOWHERE (off-worktree) — YOU must open_session(branch="' + sibBranch + '") yourself (or reload_repo if already open from a prior repeat).',
    "Task: 'Add version string' was committed on " + sibBranch + " but belongs on trunk. Move it: cherry_pick_commit it onto the trunk session (session=\"" + trunkBranch + "\") at the trunk tip, then drop_commit it from the sibling session (session=\"" + sibBranch + "\"). Keep the sibling's own 'Add experimental flag' commit intact.",
    'Reply with "## Tool Log" and "## Result".',
  ].join('\n')
}

function verifyPrompt(n, solver) {
  const w = wt(n, solver)
  return 'Run exactly: ' + VERIFY + ' t' + n + ' ' + w + '\nReport the exit code and the final PASS/FAIL line verbatim. Also run: git -C ' + w + ' status --porcelain and report if it is empty (clean).'
}

const RESULT_SCHEMA = {
  type: 'object',
  properties: { pass: { type: 'boolean' }, raw: { type: 'string' } },
  required: ['pass', 'raw'],
}

async function runRepeat(task, solver, idx) {
  const isT11 = task.n === 11
  const solvePrompt = isT11 ? t11Prompt(solver, idx) : (solver === 'git' ? gitPrompt(task, idx) : opctlPrompt(task, solver, idx))
  const agentType = solver === 'op' ? 'commedit:commedit-operator' : 'general-purpose'
  const label = 't' + task.n + '-' + solver + '-r' + (idx+1)
  let solveResult
  try {
    solveResult = await agent(solvePrompt, { label, phase: 'Run cells', agentType })
  } catch (e) {
    return { label, pass: false, note: 'solve threw: ' + e }
  }
  const vp = verifyPrompt(task.n, solver)
  let verdict
  try {
    verdict = await agent(vp, { label: 'verify:' + label, phase: 'Run cells', schema: RESULT_SCHEMA })
  } catch (e) {
    verdict = { pass: false, raw: 'verify threw: ' + e }
  }
  return { label: label, task: task.n, solver: solver, repeat: idx+1, pass: !!(verdict && verdict.pass), verifyRaw: verdict ? verdict.raw : null, toolLog: solveResult }
}

async function runCell(task, solver) {
  const results = []
  for (let i = 0; i < K; i++) {
    results.push(await runRepeat(task, solver, i))
  }
  return { task: task.n, solver: solver, results: results }
}

phase('Run cells')
const SOLVERS = ['op', 'ctl', 'git']
const cells = []
for (const task of TASKS) for (const solver of SOLVERS) cells.push({ task: task, solver: solver })

const cellResults = await parallel(cells.map(c => () => runCell(c.task, c.solver)))

return { cellResults: cellResults.filter(Boolean) }
