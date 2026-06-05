//! Shared helpers for engine integration tests: building scratch git repos and
//! inspecting them with plain `git`.
//!
//! Each test binary uses a subset of these helpers, so some appear unused per
//! binary.
#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

/// Run a `git` command in `dir`, asserting success, returning trimmed stdout.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .output()
        .expect("failed to run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
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
