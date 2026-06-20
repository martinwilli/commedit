//! Shared helpers for the MCP handler tests: building scratch git repos and
//! inspecting them with plain `git` (copied from the engine's test helpers),
//! plus constructing a server against such a repo.
//!
//! Each test binary uses a subset of these helpers, so some appear unused per
//! binary.
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

use commedit_engine::graph::compute_graph;
use commedit_engine::history::{CommitInfo, ReorderMove};
use commedit_engine::repo::Repo;
use commedit_mcp::dto::SessionSel;
use commedit_mcp::server::CommeditServer;

/// Open a server session over the repo at `dir`, the way `main` does. The launch
/// session's id is the checked-out branch's short-name — `init_repo` uses `main`,
/// so [`sel`]`("main")` addresses it.
pub fn open_server(dir: &Path) -> CommeditServer {
    CommeditServer::new(Repo::open(dir).expect("opening the scratch repo"))
}

/// The session selector for id `id` — every session-operating tool now requires
/// one. The launch session of [`open_server`] is `sel("main")`; of
/// [`open_server_branch`] it is `sel(branch)`.
pub fn sel(id: &str) -> SessionSel {
    SessionSel {
        session: id.to_string(),
    }
}

/// Open a server session editing `branch` (which need not be checked out), the
/// off-worktree way `main` does with a branch argument.
pub fn open_server_branch(dir: &Path, branch: &str) -> CommeditServer {
    CommeditServer::new(
        Repo::open_branch(
            dir,
            commedit_engine::index_cache::IndexCache::Disabled,
            Some(branch),
        )
        .expect("opening the scratch repo on a branch"),
    )
}

/// Unwrap a tool result's error side (the `Yaml` result wrapper carries no
/// `Debug`, so `unwrap_err` can't).
pub fn expect_err<T>(result: Result<T, rmcp::ErrorData>) -> rmcp::ErrorData {
    match result {
        Ok(_) => panic!("expected the tool call to fail"),
        Err(e) => e,
    }
}

/// Plan moving display row `from` to insertion gap `to`, expecting exactly one
/// destination line — the linear shape most tests drop into. Computes the lane
/// layout on the fly, the way the UI feeds the planner.
pub fn plan_reorder_single(
    repo: &Repo,
    commits: &[CommitInfo],
    from: usize,
    to: usize,
) -> ReorderMove {
    let layout = compute_graph(commits, &repo.root_commit_id());
    let mut cands = repo.plan_reorder_candidates(commits, &layout, from, to);
    assert_eq!(
        cands.len(),
        1,
        "expected exactly one destination line for the gap"
    );
    cands.remove(0).mv
}

/// Plan grafting the trashed `restored` back in at gap `to`, expecting exactly
/// one destination line. See [`plan_reorder_single`].
pub fn plan_restore_single(
    repo: &Repo,
    commits: &[CommitInfo],
    restored: &CommitInfo,
    to: usize,
) -> ReorderMove {
    let layout = compute_graph(commits, &repo.root_commit_id());
    let mut cands = repo.plan_restore_candidates(commits, &layout, restored, to);
    assert_eq!(
        cands.len(),
        1,
        "expected exactly one destination line for the gap"
    );
    cands.remove(0).mv
}

/// Run a `git` command in `dir`, asserting success, returning trimmed stdout.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let (ok, stdout, stderr) = git_raw(dir, args);
    assert!(ok, "git {args:?} failed: {stderr}");
    stdout
}

/// Run a `git` command in `dir` *without* asserting success, returning whether it
/// exited zero plus trimmed stdout. For commands that exit non-zero by design —
/// notably a `git merge` that stops on a conflict for the test to resolve.
pub fn git_allow_failure(dir: &Path, args: &[&str]) -> (bool, String) {
    let (ok, stdout, _stderr) = git_raw(dir, args);
    (ok, stdout)
}

/// Run a `git` command with the test identity, returning `(success, stdout, stderr)`.
fn git_raw(dir: &Path, args: &[&str]) -> (bool, String, String) {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .output()
        .expect("failed to run git");
    (
        output.status.success(),
        String::from_utf8(output.stdout).unwrap().trim().to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

/// Initialize a git repo at `dir` with `commits` linear commits on `main`. Each
/// entry is `(filename, contents, message)`.
pub fn init_repo(dir: &Path, commits: &[(&str, &str, &str)]) {
    git(dir, &["-c", "init.defaultBranch=main", "init", "-q"]);
    for (file, contents, message) in commits {
        std::fs::write(dir.join(file), contents).unwrap();
        git(dir, &["add", file]);
        git(dir, &["commit", "-q", "-m", message]);
    }
}

/// Subjects on `main`, newest first.
pub fn git_log_subjects(dir: &Path) -> Vec<String> {
    git(dir, &["log", "--format=%s", "main"])
        .lines()
        .map(str::to_string)
        .collect()
}

/// Number of parents of `rev`. `git rev-list --parents -n1` prints the commit's
/// own id followed by each parent id, so the parent count is the token count
/// minus one.
pub fn parent_count(dir: &Path, rev: &str) -> usize {
    git(dir, &["rev-list", "--parents", "-n", "1", rev])
        .split_whitespace()
        .count()
        .saturating_sub(1)
}

/// Whether `rev` is a merge (two or more parents).
pub fn is_merge(dir: &Path, rev: &str) -> bool {
    parent_count(dir, rev) >= 2
}

/// Build a repo with a real, *clean* 2-parent merge on `main`. Layout (oldest
/// first):
///
/// ```text
///   base      base.txt
///   |\
///   | side-1  side.txt   (branch `side` off base)
///   main-1    main.txt   (on main)
///   |/
///   merge                (main: `git merge --no-ff side`)
/// ```
///
/// The two sides touch different files, so the merge auto-resolves and its
/// remerge delta (merge tree vs. auto-merged parent tree) is empty. The branch
/// tip is the merge commit "merge"; "main-1" is reachable via its first parent,
/// "side-1" only via its second.
pub fn init_merge_repo(dir: &Path) {
    git(dir, &["-c", "init.defaultBranch=main", "init", "-q"]);
    commit_file(dir, "base.txt", "base\n", "base");
    commit_file(dir, "main.txt", "main\n", "main-1");
    git(dir, &["checkout", "-q", "-b", "side", "main~1"]);
    commit_file(dir, "side.txt", "side\n", "side-1");
    git(dir, &["checkout", "-q", "main"]);
    let (ok, _) = git_allow_failure(dir, &["merge", "--no-ff", "-m", "merge", "side"]);
    assert!(ok, "clean merge should succeed");
}

/// Build a repo with an *evil* merge on `main`: a clean auto-merge of two sides
/// touching different files, but the merge commit then hand-edits `base.txt`, so
/// the merge carries a non-empty remerge delta (`base.txt` modified from
/// `1\n2\n3\n` to `1\nEVIL\n3\n`). The branch tip is the merge "evil-merge".
pub fn init_evil_merge_repo(dir: &Path) {
    git(dir, &["-c", "init.defaultBranch=main", "init", "-q"]);
    commit_file(dir, "base.txt", "1\n2\n3\n", "base");
    commit_file(dir, "main.txt", "main\n", "main-1");
    git(dir, &["checkout", "-q", "-b", "side", "main~1"]);
    commit_file(dir, "side.txt", "side\n", "side-1");
    git(dir, &["checkout", "-q", "main"]);
    // Stop before committing the (clean) merge, then introduce an evil change.
    git_allow_failure(dir, &["merge", "--no-ff", "--no-commit", "side"]);
    std::fs::write(dir.join("base.txt"), "1\nEVIL\n3\n").unwrap();
    git(dir, &["add", "base.txt"]);
    git(dir, &["commit", "-q", "-m", "evil-merge"]);
}

/// Build a repo whose merge resolves a *real* conflict, so the merge's two
/// parents disagree at `base.txt` and the auto-merged parent tree is conflicted
/// there. Both sides change the middle line of `base.txt`; the merge is resolved
/// to `1\nRESOLVED\n3\n`. The branch tip is the merge "conflict-merge".
pub fn init_conflicted_merge_repo(dir: &Path) {
    git(dir, &["-c", "init.defaultBranch=main", "init", "-q"]);
    commit_file(dir, "base.txt", "1\n2\n3\n", "base");
    commit_file(dir, "base.txt", "1\nMAIN\n3\n", "main-1");
    git(dir, &["checkout", "-q", "-b", "side", "main~1"]);
    commit_file(dir, "base.txt", "1\nSIDE\n3\n", "side-1");
    git(dir, &["checkout", "-q", "main"]);
    // The merge conflicts on base.txt's middle line; resolve and commit it.
    let (ok, _) = git_allow_failure(dir, &["merge", "--no-ff", "side"]);
    assert!(!ok, "the conflicting merge should stop for resolution");
    std::fs::write(dir.join("base.txt"), "1\nRESOLVED\n3\n").unwrap();
    git(dir, &["add", "base.txt"]);
    git(dir, &["commit", "-q", "-m", "conflict-merge"]);
}

/// Write `file` with `contents`, stage it, and commit with `message`.
fn commit_file(dir: &Path, file: &str, contents: &str, message: &str) {
    std::fs::write(dir.join(file), contents).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", message]);
}
