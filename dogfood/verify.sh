#!/usr/bin/env bash
# Deterministic correctness oracle for the dogfood tournament.
#
#   ./dogfood/verify.sh <t1..t11> <worktree-path>
#
# Codifies each task's PASS criteria from README §4 as git-observable assertions,
# so the correctness GATE no longer rides on teacher attention and is byte-comparable
# across runs. Three properties make it the right tool for the job:
#
#   * Pure `git -C <wt>` — touches no MCP session, so it grades the plain-git
#     baseline too, and can be re-run AFTER a parallel run settles to catch the
#     ref-write race (run 4): a silently-reverted result fails the oracle even
#     though the student self-reported success.
#   * sha-free — addresses commits by their stable fixture SUBJECT and by file
#     CONTENT, never by sha/change_id (both churn on every rebuild and rewrite).
#   * exit 0 = PASS, 1 = FAIL, 2 = usage — and a final `PASS: T<n>` / `FAIL: T<n>`
#     line, so it is equally usable by eye, by a wrapper loop, or in CI.
#
# It grades the END STATE only. Transient beats (T10's abort, the conflict-resolution
# loop itself) are graded from the student's Tool Log — see README §4.
#
# NB: no `pipefail` on purpose. Many checks are `git … | grep -q PATTERN`, and a
# matching `grep -q` exits early, SIGPIPE-killing the upstream git; under pipefail
# that reads as a FAILED pipeline despite the match (a found `fixup!` would then
# score as PASS). Without pipefail the pipeline status is grep's, which is correct.
set -u

BASE="${BASE:-stress/base}"
TASK="${1:?usage: verify.sh <t1..t11> <worktree-path>}"
WT="${2:?usage: verify.sh <t1..t11> <worktree-path>}"

g()  { git -C "$WT" "$@"; }
fail=0
ok()  { printf '  \xe2\x9c\x93 %s\n' "$*"; }
bad() { printf '  \xe2\x9c\x97 %s\n' "$*"; fail=1; }

subjects()    { g log --format='%s' HEAD; }
sha_of()      { g log --format='%H|%s' HEAD | awk -F'|' -v s="$1" '$2==s{print $1; exit}'; }
parent_subj() { local c; c=$(sha_of "$1"); [ -n "$c" ] && g log -1 --format='%s' "${c}^" 2>/dev/null; }
has_subject() { subjects | grep -qxF "$1"; }
files_of()    { g diff-tree --no-commit-id -r --name-only "$1" 2>/dev/null; }     # vs first parent
status_of()   { g diff-tree --no-commit-id -r --name-status "$1" 2>/dev/null; }   # "<status>\t<path>"
blob()        { g show "$1:$2" 2>/dev/null; }
in_tree()     { g cat-file -e "HEAD:$1" 2>/dev/null; }
namestatus()  { g diff --name-status "$BASE" HEAD 2>/dev/null; }
tree_eq_base(){ [ -z "$(g diff "$BASE" HEAD 2>/dev/null)" ]; }
porcelain()   { g status --porcelain 2>/dev/null; }
log_pos()     { subjects | grep -nxF "$1" | head -1 | cut -d: -f1; }              # 1-based, newest-first

check_clean() {
  [ -z "$(porcelain)" ] && ok "working tree clean" || bad "working tree dirty: $(porcelain | head -3 | tr '\n' ';')"
  g fsck --no-progress --connectivity-only >/dev/null 2>&1 && ok "fsck connectivity ok" || bad "fsck reported corruption"
}

t1() {
  subjects | grep -q '^fixup!' && bad "fixup! commit still present" || ok "no fixup! commit"
  subjects | grep -q '^Debug:' && bad "Debug: commit still present" || ok "Debug: commit dropped"
  [ "$(parent_subj 'Use format_row in main')" = "Add util helper format_row" ] \
    && ok "helper is parent of use (reordered)" \
    || bad "parent of 'Use format_row in main' is '$(parent_subj 'Use format_row in main')' (want 'Add util helper format_row')"
  [ "$(namestatus)" = $'D\tsrc/debug.txt' ] \
    && ok "diff vs base = only src/debug.txt deleted" \
    || bad "diff vs base not just debug.txt: [$(namestatus | tr '\n' ';')]"
  check_clean
}

t2() {
  tree_eq_base && ok "tree identical to base" || bad "tree differs from base: $(namestatus | tr '\n' ';')"
  local c sink="" cfg="" typo=""
  for c in $(g rev-list HEAD); do
    local f; f=$(files_of "$c")
    grep -qx 'src/config.txt' <<<"$f" && grep -qx 'src/util.txt' <<<"$f" && { sink="$c"; break; }
  done
  [ -z "$sink" ] && ok "no kitchen-sink commit (config+util together)" \
    || bad "a commit still changes both config.txt and util.txt: $(g log -1 --format=%s "$sink")"
  for c in $(g rev-list HEAD); do
    [ "$(status_of "$c")" = $'A\tsrc/config.txt' ] && { cfg="$c"; break; }
  done
  [ -n "$cfg" ] && ok "config-loader commit adds only src/config.txt: $(g log -1 --format=%s "$cfg")" \
    || bad "no commit adds only src/config.txt"
  for c in $(g rev-list HEAD); do
    [ "$(files_of "$c")" = "src/util.txt" ] || continue
    local d; d=$(g show "$c" -- src/util.txt 2>/dev/null)
    grep -q '^-.*LGO' <<<"$d" && grep -q '^+.*LOG' <<<"$d" && { typo="$c"; break; }
  done
  [ -n "$typo" ] && ok "typo-fix commit touches only util.txt and flips LGO->LOG: $(g log -1 --format=%s "$typo")" \
    || bad "no commit flips LGO->LOG touching only util.txt"
  [ -n "$typo" ] && [ -n "$cfg" ] && [ "$(g rev-parse "${typo}^")" = "$cfg" ] \
    && ok "config-loader is immediate parent of typo-fix (order ok)" \
    || bad "config-loader is not the parent of the typo-fix"
  check_clean
}

t3() {
  has_subject 'Raise server limits for load test' && bad "raise-limits commit still present" || ok "raise-limits commit dropped"
  local sv lim; sv=$(blob HEAD server.txt); lim=$(blob HEAD src/limits.txt)
  grep -q '^timeout = 120$'  <<<"$sv"  && ok "timeout = 120"     || bad "timeout != 120"
  grep -q '^backlog = 256$'  <<<"$sv"  && ok "backlog = 256"     || bad "backlog != 256"
  grep -q '^max_conn = 1000$' <<<"$lim" && ok "max_conn = 1000"  || bad "max_conn != 1000"
  tree_eq_base && ok "tree identical to base (resolved to base-tip values)" \
    || bad "tree differs from base: $(namestatus | tr '\n' ';')"
  check_clean
}

t4() {
  has_subject 'Add authentication' && ok "subject fixed to 'Add authentication'" || bad "'Add authentication' not found"
  subjects | grep -qxF 'Add athentication' && bad "typo subject 'Add athentication' still present" || ok "typo subject gone"
  local c; c=$(sha_of 'Add authentication')
  [ -n "$c" ] && { [ "$(g log -1 --format='%an' "$c")" = "Jane Doe" ] && ok "author name Jane Doe" || bad "author '$(g log -1 --format=%an "$c")' (want Jane Doe)"; }
  blob HEAD src/auth.txt | grep -q '^TOKEN_LEN = 16$' && ok "TOKEN_LEN = 16" || bad "TOKEN_LEN != 16"
  check_clean
}

t5() {
  local h c=""
  for h in $(g rev-list HEAD); do [ "$(g log -1 --format='%an' "$h")" = "Alex Fixer" ] && { c="$h"; break; }; done
  [ -n "$c" ] && ok "cherry-picked commit present, author Alex Fixer preserved" || bad "no Alex Fixer commit on HEAD"
  if [ -n "$c" ]; then
    g log -1 --format='%s' "$c" | grep -q 'BUG-123' && bad "[BUG-123] still in subject" || ok "[BUG-123] removed from reworded copy"
    [ "$(g log -1 --format='%s' "${c}^")" = "Add parser" ] && ok "lands right after 'Add parser'" \
      || bad "parent is '$(g log -1 --format=%s "${c}^")' (want 'Add parser')"
  fi
  if g rev-parse --verify -q stress/hotfix >/dev/null; then
    [ "$(g log -1 --format='%s' stress/hotfix)" = "Fix null deref in parser [BUG-123]" ] \
      && ok "stress/hotfix untouched (still [BUG-123])" || bad "stress/hotfix tip changed"
  else bad "stress/hotfix branch missing"; fi
  check_clean
}

t6() {
  [ -z "$(porcelain)" ] && ok "working copy clean (all dirt committed)" || bad "working copy still dirty"
  blob HEAD src/store.txt | grep -q 'def load'    && ok "store.txt has load()"    || bad "store.txt missing load()"
  blob HEAD src/store.txt | grep -q 'def backup'  && ok "store.txt has backup()"  || bad "store.txt missing backup()"
  blob HEAD src/stats.txt | grep -q 'def total'   && ok "stats.txt has total()"   || bad "stats.txt missing total()"
  blob HEAD src/stats.txt | grep -q 'def average' && ok "stats.txt has average()" || bad "stats.txt missing average()"
  blob HEAD src/backup.txt | grep -q 'def restore' && ok "backup.txt present with restore()" || bad "backup.txt missing/wrong"
  local sc; sc=$(sha_of 'Add storage and stats helpers')
  if [ -n "$sc" ]; then
    local fs fa; fs=$(blob "$sc" src/store.txt); fa=$(blob "$sc" src/stats.txt)
    grep -q 'def load' <<<"$fs" && ! grep -q 'def backup' <<<"$fs" \
      && ok "load() folded into buried commit (no backup leak)" || bad "store.txt fold wrong at buried commit"
    grep -q 'def total' <<<"$fa" && ! grep -q 'def average' <<<"$fa" \
      && ok "total() folded into buried commit (no average leak)" || bad "stats.txt fold wrong at buried commit"
  else bad "'Add storage and stats helpers' commit not found"; fi
  local ca; ca=$(sha_of 'Add backup command')
  if [ -n "$ca" ]; then
    local f; f=$(files_of "$ca")
    grep -qx 'src/backup.txt' <<<"$f" && grep -qx 'src/store.txt' <<<"$f" && ! grep -qx 'src/stats.txt' <<<"$f" \
      && ok "'Add backup command' partition ok (store+backup, not stats)" \
      || bad "'Add backup command' wrong file set: $(tr '\n' ' ' <<<"$f")"
  else bad "'Add backup command' commit not found"; fi
  local cb; cb=$(sha_of 'Add average stat')
  if [ -n "$cb" ]; then
    [ "$(files_of "$cb")" = "src/stats.txt" ] && ok "'Add average stat' touches only stats.txt" \
      || bad "'Add average stat' file set: $(files_of "$cb" | tr '\n' ' ')"
  else bad "'Add average stat' commit not found"; fi
  check_clean
}

t7() {
  local h m=""
  for h in $(g log --merges --format='%H' HEAD); do
    [ "$(g log -1 --format='%s' "$h")" = "Merge branch 'search'" ] && { m="$h"; break; }
  done
  if [ -n "$m" ]; then
    ok "'Merge branch '\''search'\''' present"
    local np; np=$(g log -1 --format='%P' "$m" | wc -w)
    [ "$np" -eq 2 ] && ok "merge still has 2 parents" || bad "merge has $np parents (want 2)"
    [ -n "$(g log -1 --format='%b' "$m")" ] && ok "merge now has a body" || bad "merge body still empty"
  else bad "'Merge branch '\''search'\''' not found"; fi
  subjects | grep -qxF 'Make search case-insensitive' \
    && bad "standalone 'Make search case-insensitive' still present" || ok "follow-up folded (no standalone commit)"
  blob HEAD src/search.txt | grep -q 'query.lower()' && ok "case-insensitive change present" || bad "case-insensitive change lost"
  tree_eq_base && ok "tree identical to base" || bad "tree differs from base: $(namestatus | tr '\n' ';')"
  check_clean
}

t8() {
  has_subject 'Add experimental telemetry' && ok "original telemetry commit retained" || bad "original telemetry commit missing"
  local h rev=""
  for h in $(g rev-list HEAD); do [ "$(status_of "$h")" = $'D\tsrc/telemetry.txt' ] && { rev="$h"; break; }; done
  [ -n "$rev" ] && ok "revert commit deletes src/telemetry.txt: $(g log -1 --format=%s "$rev")" || bad "no commit deletes src/telemetry.txt"
  in_tree src/telemetry.txt && bad "src/telemetry.txt still in final tree" || ok "src/telemetry.txt gone from final tree"
  has_subject 'Add metrics endpoint' && ok "'Add metrics endpoint' back in history" || bad "'Add metrics endpoint' missing"
  in_tree src/metrics.txt && ok "src/metrics.txt present" || bad "src/metrics.txt absent"
  local pm pr; pm=$(log_pos 'Add metrics endpoint'); pr=$(log_pos 'Add report footer')
  [ -n "$pm" ] && [ -n "$pr" ] && [ "$pm" -lt "$pr" ] \
    && ok "metrics restored to the tip (newer than report range)" \
    || bad "metrics not at tip (pos $pm vs report footer $pr) — an undo, not a restore?"
  [ "$(namestatus)" = $'D\tsrc/telemetry.txt' ] && ok "diff vs base = only src/telemetry.txt deleted" \
    || bad "diff vs base not just telemetry: $(namestatus | tr '\n' ';')"
  check_clean
}

t9() {
  local row subj exp c
  for row in 'Add report header:2025-01-25' 'Add report body:2025-01-26' 'Add report footer:2025-01-27'; do
    subj="${row%:*}"; exp="${row#*:}"; c=$(sha_of "$subj")
    if [ -z "$c" ]; then bad "$subj not found"; continue; fi
    [ "$(g log -1 --format='%an' "$c")" = "Jane Doe" ]              && ok "$subj author name"  || bad "$subj author name '$(g log -1 --format=%an "$c")'"
    [ "$(g log -1 --format='%ae' "$c")" = "jane.doe@example.com" ]  && ok "$subj author email" || bad "$subj author email '$(g log -1 --format=%ae "$c")'"
    [ "$(g log -1 --format='%cn' "$c")" = "Jane Doe" ]             && ok "$subj committer name" || bad "$subj committer name '$(g log -1 --format=%cn "$c")'"
    [ "$(g log -1 --format='%ce' "$c")" = "jane.doe@example.com" ] && ok "$subj committer email" || bad "$subj committer email '$(g log -1 --format=%ce "$c")'"
    [ "$(g log -1 --format='%ad' --date=short "$c")" = "$exp" ]     && ok "$subj author date $exp" || bad "$subj author date '$(g log -1 --format=%ad --date=short "$c")'"
    [ "$(g log -1 --format='%cd' --date=short "$c")" = "$exp" ]     && ok "$subj committer date $exp (not re-stamped)" || bad "$subj committer date '$(g log -1 --format=%cd --date=short "$c")' (want $exp)"
  done
  [ "$(parent_subj 'Add report body')"   = "Add report header" ] && ok "order: header parent of body" || bad "order: body parent is '$(parent_subj 'Add report body')'"
  [ "$(parent_subj 'Add report footer')" = "Add report body" ]   && ok "order: body parent of footer" || bad "order: footer parent is '$(parent_subj 'Add report footer')'"
  tree_eq_base && ok "tree identical to base (metadata-only)" || bad "tree differs from base: $(namestatus | tr '\n' ';')"
  check_clean
}

t10() {
  has_subject 'Bump cache capacity to 512' && ok "512-bump present" || bad "512-bump missing"
  has_subject 'Bump cache capacity to 256' && ok "256-bump present" || bad "256-bump missing"
  [ "$(parent_subj 'Bump cache capacity to 512')" = "Add cache module" ] \
    && ok "512-bump now follows cache module" \
    || bad "512-bump parent '$(parent_subj 'Bump cache capacity to 512')' (want 'Add cache module')"
  [ "$(parent_subj 'Bump cache capacity to 256')" = "Bump cache capacity to 512" ] \
    && ok "256-bump now follows 512-bump (reordered)" \
    || bad "256-bump parent '$(parent_subj 'Bump cache capacity to 256')' (want '512-bump')"
  blob HEAD src/cache.txt | grep -q '^capacity = 256$' && ok "final capacity = 256 (resolved oldest-first)" || bad "final capacity != 256"
  check_clean
  echo "  note: the abort beat (safety net) is transient — grade it from the Tool Log, not git."
}

t11() {
  # Cross-branch move. WT is the TRUNK worktree (HEAD = stress/t11-<solver>); the
  # feature sibling ref is derived from it (stress/t11s-<solver>) — refs are global,
  # so the off-worktree sibling is graded by ref without a checkout of its own.
  local cur sib
  cur=$(g symbolic-ref --quiet --short HEAD 2>/dev/null)
  sib="${cur/t11-/t11s-}"
  [ -n "$cur" ] && [ "$sib" != "$cur" ] && ok "trunk=$cur, sibling=$sib" || { bad "cannot derive sibling from HEAD ('$cur')"; return; }
  # trunk gained the misplaced commit + its file, but NOT the feature-only work
  has_subject 'Add version string' && ok "trunk now has 'Add version string'" || bad "trunk missing 'Add version string'"
  in_tree src/version.txt && ok "trunk has src/version.txt" || bad "trunk missing src/version.txt"
  in_tree src/flag.txt && bad "trunk wrongly has feature-only src/flag.txt (moved too much)" || ok "trunk free of feature-only src/flag.txt"
  # sibling: misplaced commit moved OUT, legit feature work kept (rebased)
  if g rev-parse --verify -q "$sib" >/dev/null; then
    g log --format='%s' "$sib" | grep -qxF 'Add version string' && bad "feature still has 'Add version string' (not moved out)" || ok "feature no longer has 'Add version string'"
    g cat-file -e "$sib:src/version.txt" 2>/dev/null && bad "feature still has src/version.txt" || ok "feature free of src/version.txt"
    g log --format='%s' "$sib" | grep -qxF 'Add experimental flag' && ok "feature keeps legit 'Add experimental flag'" || bad "feature lost 'Add experimental flag' (over-dropped)"
    g cat-file -e "$sib:src/flag.txt" 2>/dev/null && ok "feature keeps src/flag.txt" || bad "feature lost src/flag.txt"
  else bad "sibling branch $sib not found"; fi
  check_clean
}

echo "== verify ${TASK} @ ${WT} (base=${BASE}) =="
case "${TASK,,}" in
  t1) t1;; t2) t2;; t3) t3;; t4) t4;; t5) t5;;
  t6) t6;; t7) t7;; t8) t8;; t9) t9;; t10) t10;; t11) t11;;
  *) echo "unknown task: ${TASK} (want t1..t11)" >&2; exit 2;;
esac
if [ "$fail" -eq 0 ]; then echo "PASS: ${TASK}"; exit 0; else echo "FAIL: ${TASK}"; exit 1; fi
