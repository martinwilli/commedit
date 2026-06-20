#!/usr/bin/env bash
# Seed the T6 dirty working copy into a stress/* worktree (arg $1), AFTER the
# pristine reset+clean of §5 step 1. Deterministic whole-file writes, so it is
# idempotent and identical for every solver and every run. The dirt is spread
# so that each tracked file carries hunks bound for DIFFERENT destinations:
#
#   src/store.txt : hunk 0 (load)   -> fold into "Add storage and stats helpers"
#                   hunk 1 (backup) -> new commit A ("Add backup command")
#   src/stats.txt : hunk 0 (total)  -> fold into "Add storage and stats helpers"
#                   hunk 1 (average)-> new commit B ("Add average stat")
#   src/backup.txt: brand-new file  -> new commit A (untracked; needs add_paths)
#
# So the fold gathers hunk 0 ACROSS both files; once it lands, each file has a
# single remaining hunk and the two new commits fall out as whole-file picks on
# disjoint files (order-independent, stable answer key). See dogfood/README.md §4.
set -euo pipefail
WT="${1:?usage: t6-dirty.sh <worktree-path>}"

cat > "$WT/src/store.txt" <<'EOF'
# storage layer
FORMAT = "csv"

def load(path):
    with open(path) as fh:
        return [l.rstrip() for l in fh]

def save(rows, path):
    with open(path, "w") as fh:
        for r in rows:
            fh.write(r + "\n")

def clear(path):
    open(path, "w").close()

def backup(path):
    save(load(path), path + ".bak")
EOF

cat > "$WT/src/stats.txt" <<'EOF'
# stats helpers
def count(rows):
    return len(rows)

def total(rows, key):
    return sum(r[key] for r in rows)

def empty(rows):
    return count(rows) == 0

def first(rows):
    return rows[0] if rows else None

def last(rows):
    return rows[-1] if rows else None

def average(rows, key):
    return total(rows, key) / count(rows)
EOF

cat > "$WT/src/backup.txt" <<'EOF'
def restore(path):
    return load(path + ".bak")
EOF
