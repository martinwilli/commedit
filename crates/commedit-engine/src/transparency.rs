//! Keep the git side of a colocated repo indistinguishable from a normal git
//! repo after jj rewrites history.
//!
//! jj manages colocated repos with a *detached* HEAD by design. To stay
//! invisible to a plain-git user we record the originally checked-out branch and
//! re-point HEAD at it (a symbolic ref) after exporting. For message- and
//! tree-level edits the rewritten commit has the same working-tree content at
//! HEAD, so the index/worktree stay consistent and `git status` reads clean.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use jj_lib::git;
use jj_lib::repo::MutableRepo;

/// Export jj bookmarks to git refs within the current transaction. jj already
/// moved bookmarks that pointed at rewritten commits during
/// `rebase_descendants`, so this makes those moves visible to git.
pub fn export_to_git(mut_repo: &mut MutableRepo) -> Result<()> {
    git::export_refs(mut_repo).context("exporting refs to git")?;
    Ok(())
}

/// Ensure git ignores jj's `.jj` metadata directory via `.git/info/exclude`,
/// so it never shows up in `git status`. This is repo-local and does not touch
/// the user's tracked `.gitignore`.
pub fn ensure_jj_excluded(workspace_root: &Path) -> Result<()> {
    let exclude = workspace_root.join(".git").join("info").join("exclude");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == ".jj/") {
        return Ok(());
    }
    if let Some(parent) = exclude.parent() {
        std::fs::create_dir_all(parent).context("creating .git/info")?;
    }
    let mut contents = existing;
    if !contents.is_empty() && !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents.push_str(".jj/\n");
    std::fs::write(&exclude, contents).context("writing .git/info/exclude")?;
    Ok(())
}

/// The full ref name (e.g. `refs/heads/main`) HEAD symbolically points at, or
/// `None` if HEAD is detached.
pub fn head_branch(workspace_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// Re-attach git HEAD to `branch` (a full ref name) as a symbolic ref, undoing
/// jj's detached-HEAD colocated layout.
pub fn reattach_head(workspace_root: &Path, branch: &str) -> Result<()> {
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args(["symbolic-ref", "HEAD", branch])
        .output()
        .context("running git symbolic-ref")?;
    if !output.status.success() {
        bail!(
            "failed to re-attach HEAD to {branch}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}
