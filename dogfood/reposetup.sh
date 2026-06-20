#!/usr/bin/env bash
# commedit dogfood stress-test: build a fresh ~24-commit orphan history with
# baked "smells", a merge, a side hotfix branch, and per-(task x solver)
# branches + worktrees. All refs namespaced stress/* ; nothing touches the
# user's real branches.
set -euo pipefail

REPO=/home/mwilli/repos/commedit
W="$REPO/.worktrees/commedit-stress-build"

if git -C "$REPO" show-ref --verify --quiet refs/heads/stress/base; then
  echo "ERROR: stress/base already exists. Clean first (see Teardown)." >&2
  exit 1
fi

git -C "$REPO" worktree add --detach "$W" >/dev/null
cd "$W"
git checkout --orphan stress/base >/dev/null 2>&1
git rm -rf . >/dev/null 2>&1 || true
git clean -fdxq || true
mkdir -p src tests

DAY=1
ci() { # $1 = message. Honors AN/AE env for a one-commit author override.
  local d; d=$(printf "2025-01-%02dT12:00:00" "$DAY")
  git add -A
  GIT_AUTHOR_DATE="$d" GIT_COMMITTER_DATE="$d" \
  GIT_AUTHOR_NAME="${AN:-Jane Doe}"  GIT_AUTHOR_EMAIL="${AE:-jane.doe@example.com}" \
  GIT_COMMITTER_NAME="${AN:-Jane Doe}" GIT_COMMITTER_EMAIL="${AE:-jane.doe@example.com}" \
  git commit -q -m "$1"
  DAY=$((DAY+1))
}

# C1 Initial
cat > README.md      <<'EOF'
# todo

A tiny todo CLI (test fixture).
EOF
cat > CHANGELOG.md   <<'EOF'
# Changelog
EOF
cat > src/main.txt   <<'EOF'
# todo CLI
def main():
    print("todo")
EOF
cat > src/util.txt   <<'EOF'
MAX_RETRIES = 1

def log(msg):
    print("LGO: " + msg)
EOF
cat > server.txt     <<'EOF'
host = localhost
timeout = 30
workers = 4
EOF
ci "Initial commit"

# C2 kitchen-sink (T2): new config.txt + util.txt LGO->LOG
cat > src/config.txt <<'EOF'
def load_config(path):
    settings = {}
    for line in open(path):
        k, _, v = line.partition("=")
        settings[k.strip()] = v.strip()
    return settings
EOF
cat > src/util.txt   <<'EOF'
MAX_RETRIES = 1

def log(msg):
    print("LOG: " + msg)
EOF
ci "Add config loader and fix logging typo"

# C3 Add parser (T1 fixup target / T5 anchor)
cat > src/parser.txt <<'EOF'
def parse(line):
    parts = line.split(",")
    return parts[0].strip()

def parse_all(lines):
    return [parse(l) for l in lines]
EOF
ci "Add parser"

# C4 use helper BEFORE adding it (T1 reorder)
cat > src/main.txt   <<'EOF'
# todo CLI
def main():
    rows = load()
    for r in rows:
        print(format_row(r))
EOF
ci "Use format_row in main"

# C5 add the helper (T1 reorder)
cat > src/util.txt   <<'EOF'
MAX_RETRIES = 1

def log(msg):
    print("LOG: " + msg)

def format_row(row):
    return " | ".join(row)
EOF
ci "Add util helper format_row"

# search side lane + merge
git checkout -q -b stress/search
cat > src/search.txt <<'EOF'
def search(rows, query):
    return [r for r in rows if query in r]
EOF
ci "Add search module"
cat > src/main.txt   <<'EOF'
# todo CLI
def main():
    rows = load()
    for r in rows:
        print(format_row(r))
    if query:
        rows = search(rows, query)
EOF
ci "Wire search into main"
git checkout -q stress/base
D=$(printf "2025-01-%02dT12:00:00" "$DAY")
GIT_AUTHOR_DATE="$D" GIT_COMMITTER_DATE="$D" \
GIT_AUTHOR_NAME="Jane Doe" GIT_AUTHOR_EMAIL="jane.doe@example.com" \
GIT_COMMITTER_NAME="Jane Doe" GIT_COMMITTER_EMAIL="jane.doe@example.com" \
git merge -q --no-ff stress/search -m "Merge branch 'search'"
DAY=$((DAY+1))

# C9 Add athentication (T4): wrong author, typo subject, TOKEN_LEN bug
cat > src/auth.txt   <<'EOF'
TOKEN_LEN = 8

def authenticate(user, token):
    return len(token) == TOKEN_LEN
EOF
AN="temp" AE="temp@example.com" ci "Add athentication"

# C10 Set timeout to 60 (T3 drop target)
cat > server.txt     <<'EOF'
host = localhost
timeout = 60
workers = 4
EOF
ci "Set timeout to 60"

# C11 changelog
cat > CHANGELOG.md   <<'EOF'
# Changelog

- config loader
- parser
- search
EOF
ci "Update CHANGELOG"

# C12 refactor config
cat > src/config.txt <<'EOF'
def load_config(path):
    settings = {}
    with open(path) as fh:
        for line in fh:
            k, _, v = line.partition("=")
            settings[k.strip()] = v.strip()
    return settings
EOF
ci "Refactor config parsing"

# C13 fixup! Add parser (T1 squash) — edits parse() (distinct from hotfix's parse_all)
cat > src/parser.txt <<'EOF'
def parse(line):
    parts = line.split(",")
    return parts[0].strip().lower()

def parse_all(lines):
    return [parse(l) for l in lines]
EOF
ci "fixup! Add parser"

# C14 Set timeout to 120 (T3 conflicting descendant)
cat > server.txt     <<'EOF'
host = localhost
timeout = 120
workers = 4
EOF
ci "Set timeout to 120"

# C15 stray debug commit (T1 drop) — own file so the drop is CLEAN
cat > src/debug.txt  <<'EOF'
def dump_state(rows):
    print("DEBUG state", rows)
EOF
ci "Debug: add state dump helper"

# C16..C24 believable padding (CHANGELOG/README/main/tests only — no smell regions)
cat > src/main.txt   <<'EOF'
# todo CLI
def main():
    rows = load()
    for r in rows:
        print(format_row(r))
    if query:
        rows = search(rows, query)

def help():
    print("usage: todo [add|list|search]")
EOF
ci "Add help text"
cat > src/main.txt   <<'EOF'
# todo CLI
def main():
    rows = load()
    for r in rows:
        print(format_row(r))
    if query:
        rows = search(rows, query)

def help():
    print("usage: todo [add|list|search]")

def fail(msg):
    print("error: " + msg)
EOF
ci "Improve error messages"
cat > README.md      <<'EOF'
# todo

A tiny todo CLI (test fixture).

## Config
Settings live in server.txt (host, timeout, workers).
EOF
ci "Document config options"
cat > tests/parser.txt <<'EOF'
def test_parse():
    assert parse("a,b") == "a"
EOF
ci "Add tests for parser"
cat > src/main.txt   <<'EOF'
# todo CLI
from util import format_row, log
from search import search

def main():
    rows = load()
    for r in rows:
        print(format_row(r))
    if query:
        rows = search(rows, query)

def help():
    print("usage: todo [add|list|search]")

def fail(msg):
    print("error: " + msg)
EOF
ci "Tidy imports in main"
cat > README.md      <<'EOF'
# todo

A tiny todo CLI (test fixture).

## Usage
    todo add "buy milk"
    todo list

## Config
Settings live in server.txt (host, timeout, workers).
EOF
ci "Update README usage"
cat > CHANGELOG.md   <<'EOF'
# Changelog

## 0.2.0
- config loader
- parser
- search
- authentication
EOF
ci "Bump version to 0.2.0"
cat > README.md      <<'EOF'
# todo

A tiny todo CLI (test fixture).

## Usage
    todo add "buy milk"
    todo list

## Configuration
Settings live in server.txt (host, timeout, workers).
EOF
ci "Fix typo in README"
cat > CHANGELOG.md   <<'EOF'
# Changelog

## 0.2.0
- config loader
- parser
- search
- authentication

Release notes: first public preview.
EOF
ci "Prepare 0.2.0 notes"

# stress/hotfix (T5 source): guard parse_all against None, distinct author
C3=$(git rev-list --reverse stress/base | sed -n '3p')
git checkout -q -b stress/hotfix "$C3"
cat > src/parser.txt <<'EOF'
def parse(line):
    parts = line.split(",")
    return parts[0].strip()

def parse_all(lines):
    return [parse(l) for l in lines if l is not None]
EOF
D="2025-02-01T09:00:00"
git add -A
GIT_AUTHOR_DATE="$D" GIT_COMMITTER_DATE="$D" \
GIT_AUTHOR_NAME="Alex Fixer" GIT_AUTHOR_EMAIL="alex@example.com" \
GIT_COMMITTER_NAME="Alex Fixer" GIT_COMMITTER_EMAIL="alex@example.com" \
git commit -q -m "Fix null deref in parser [BUG-123]"
git checkout -q stress/base

# per-(task x solver) branches + worktrees, plus a calibration worktree.
# solvers: op=operator, ctl=skill-less MCP control, git=plain-git baseline (§5).
for t in 1 2 3 4 5; do for s in op ctl git; do
  git branch -q "stress/t${t}-${s}" stress/base
  git -C "$REPO" worktree add -q "$REPO/.worktrees/commedit-stress-t${t}-${s}" "stress/t${t}-${s}"
done; done
git branch -q stress/cal stress/base
git -C "$REPO" worktree add -q "$REPO/.worktrees/commedit-stress-cal" stress/cal
echo "DONE"
