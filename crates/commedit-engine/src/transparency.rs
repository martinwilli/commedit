//! Keep the git side of a colocated repo indistinguishable from a normal git
//! repo after jj rewrites history.
//!
//! jj manages colocated repos with a *detached* HEAD by design. To stay
//! invisible to a plain-git user we record the originally checked-out branch and
//! re-point HEAD at it (a symbolic ref) after exporting. For message- and
//! tree-level edits the rewritten commit has the same working-tree content at
//! HEAD, so the index/worktree stay consistent and `git status` reads clean.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

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

/// Delete the jj `refs/jj/keep/*` GC-protection refs that belong to commedit's
/// own history, so a plain-git user never sees them — neither in
/// `git for-each-ref`/`gitk`, nor as unreachable duplicate commits in
/// `git log --all`.
///
/// jj's git backend writes one such ref per commit it creates, to stop git from
/// garbage-collecting commits jj tracks. We delete a keep-ref when its commit is
/// either:
///   * still reachable from a real ref (branch/tag/remote/HEAD) — then the
///     keep-ref is redundant, the real ref already protects the commit; or
///   * part of the pre-operation branch (an ancestor of `old_head`) — i.e. a
///     pre-rewrite commit our own rewrite just abandoned.
///
/// `owned` lists extra commit ids that are likewise ours to drop — commedit's own
/// jj working-copy commit(s), which commedit never uses (it drives the worktree
/// through git) and which would otherwise linger as an empty, parent-less phantom
/// in `git log --all`.
///
/// We keep every other keep-ref, i.e. one whose commit is *neither* reachable
/// from a real ref, *nor* part of the branch we edited, *nor* one of `owned`.
/// That residue is exactly a *manual* jj user's un-bookmarked work — anonymous
/// heads, undo history — for which the keep-ref is the only thing standing between
/// it and `git gc`. We never run `git gc` ourselves; the objects left dangling by
/// our deletions are reclaimed by git's own maintenance.
/// The commit oids currently protected by `refs/jj/keep/*`. Lets the engine
/// inspect each (via jj) to decide which are its own working-copy commits.
pub fn keep_ref_oids(workspace_root: &Path) -> Vec<String> {
    let Ok(out) = Command::new("git")
        .current_dir(workspace_root)
        .args(["for-each-ref", "--format=%(objectname)", "refs/jj/keep/"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8(out.stdout)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn prune_orphaned_keep_refs(
    workspace_root: &Path,
    old_head: &str,
    owned: &[String],
) -> Result<()> {
    // The pre-rewrite branch commits (walks the old graph via objects still
    // present in the store). Empty/failed => nothing safe to attribute to us.
    let old_branch = rev_list(workspace_root, &[old_head])?;
    if old_branch.is_empty() {
        return Ok(());
    }
    // Everything still reachable from a real ref after the rewrite. If this can't
    // be computed we must not delete anything (we'd risk live commits).
    let reachable = rev_list(workspace_root, &["--branches", "--tags", "--remotes", "HEAD"])?;

    let list = Command::new("git")
        .current_dir(workspace_root)
        .args(["for-each-ref", "--format=%(objectname) %(refname)", "refs/jj/keep/"])
        .output()
        .context("listing jj keep refs")?;
    if !list.status.success() {
        return Ok(());
    }
    let refs = String::from_utf8(list.stdout).unwrap_or_default();
    // Batch all deletions through a single `update-ref --stdin` (keep-ref names
    // never contain spaces, so the plain `delete <ref>` line format is safe).
    let mut deletions = String::new();
    for line in refs.lines() {
        let Some((oid, name)) = line.split_once(' ') else {
            continue;
        };
        if reachable.contains(oid) || old_branch.contains(oid) || owned.iter().any(|o| o == oid) {
            deletions.push_str("delete ");
            deletions.push_str(name);
            deletions.push('\n');
        }
    }
    if deletions.is_empty() {
        return Ok(());
    }
    let mut child = Command::new("git")
        .current_dir(workspace_root)
        .args(["update-ref", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning git update-ref")?;
    child
        .stdin
        .take()
        .context("git update-ref stdin")?
        .write_all(deletions.as_bytes())
        .context("writing ref deletions")?;
    let out = child.wait_with_output().context("running git update-ref")?;
    if !out.status.success() {
        bail!(
            "failed to prune jj keep refs: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// A snapshot of every local branch (`refs/heads/*`) and the commit it points
/// at, taken straight from git. Used as the before-image for
/// [`restore_unrelated_heads`], the backstop that guarantees a rewrite only ever
/// moves the checked-out branch.
pub fn local_head_oids(workspace_root: &Path) -> BTreeMap<String, String> {
    let Ok(out) = Command::new("git")
        .current_dir(workspace_root)
        .args(["for-each-ref", "--format=%(objectname) %(refname)", "refs/heads/"])
        .output()
    else {
        return BTreeMap::new();
    };
    if !out.status.success() {
        return BTreeMap::new();
    }
    String::from_utf8(out.stdout)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(oid, name)| (name.to_string(), oid.to_string()))
        .collect()
}

/// Force every local branch *except* `current` (a full ref name like
/// `refs/heads/main`) back to the commit it pointed at in `before`, undoing any
/// move jj's ref export made to a branch other than the one being edited. This
/// is a git-level safety net behind the jj-bookmark confinement: whatever path
/// nudges an unrelated branch — a backup sharing the rewritten tip, a tracked
/// bookmark, a future jj quirk — it is reverted here before the user sees it.
///
/// Returns the branches it had to restore (empty in the common case), so callers
/// can surface that a leak occurred. A branch that vanished is recreated; one
/// that merely moved is reset. No-op when `current` is `None` (detached HEAD),
/// matching the rest of the transparency layer.
pub fn restore_unrelated_heads(
    workspace_root: &Path,
    current: Option<&str>,
    before: &BTreeMap<String, String>,
) -> Vec<String> {
    let after = local_head_oids(workspace_root);
    let mut restored = Vec::new();
    let mut updates = String::new();
    for (name, oid) in before {
        if Some(name.as_str()) == current {
            continue;
        }
        if after.get(name).map(String::as_str) != Some(oid.as_str()) {
            restored.push(name.clone());
            updates.push_str(&format!("update {name} {oid}\n"));
        }
    }
    if updates.is_empty() {
        return restored;
    }
    let _ = run_update_ref_stdin(workspace_root, &updates);
    restored
}

/// Pipe `commands` to `git update-ref --stdin`. Errors are returned for the
/// caller to log; callers treat ref bookkeeping as best-effort.
fn run_update_ref_stdin(workspace_root: &Path, commands: &str) -> Result<()> {
    let mut child = Command::new("git")
        .current_dir(workspace_root)
        .args(["update-ref", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning git update-ref")?;
    child
        .stdin
        .take()
        .context("git update-ref stdin")?
        .write_all(commands.as_bytes())
        .context("writing ref updates")?;
    let out = child.wait_with_output().context("running git update-ref")?;
    if !out.status.success() {
        bail!("git update-ref failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

/// Commit ids reachable from `args` (a `git rev-list` argument list), as a set.
/// Errors if git fails, so callers can refuse to delete on incomplete data.
fn rev_list(workspace_root: &Path, args: &[&str]) -> Result<std::collections::HashSet<String>> {
    let out = Command::new("git")
        .current_dir(workspace_root)
        .arg("rev-list")
        .args(args)
        .output()
        .context("running git rev-list")?;
    if !out.status.success() {
        bail!(
            "git rev-list failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8(out.stdout)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect())
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

/// The commit sha HEAD currently resolves to, or `None` if it can't be read.
pub fn head_commit(workspace_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Update the index and working tree from `old_rev` to `new_rev` with a two-way
/// merge, so the working tree mirrors the rewritten tip while preserving any
/// genuine local edits. Errors if local edits conflict with the update. A no-op
/// when the trees are identical (e.g. a message-only rewrite).
pub fn sync_worktree(workspace_root: &Path, old_rev: &str, new_rev: &str) -> Result<()> {
    if old_rev == new_rev {
        return Ok(());
    }
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args(["read-tree", "-m", "-u", old_rev, new_rev])
        .output()
        .context("running git read-tree")?;
    if !output.status.success() {
        bail!(
            "failed to update working tree to rewritten tip: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Reset the git index to `rev`'s tree **without** touching the working tree, so
/// `git status` reports the working-copy changes against the rewritten tip. Used
/// after materializing the rebased `@` to disk, where the worktree is already in
/// place and only the index needs to catch up to the new HEAD.
pub fn reset_index_to(workspace_root: &Path, rev: &str) -> Result<()> {
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args(["read-tree", rev])
        .output()
        .context("running git read-tree")?;
    if !output.status.success() {
        bail!(
            "failed to reset the index to the rewritten tip: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// If the git index holds staged content that is **not** reflected in the
/// working tree — a file staged then reverted or deleted on disk — jj's
/// worktree snapshot can't see it (jj reads the disk, never `.git/index`), so a
/// rewrite that resets the index would lose it. Capture the whole index as a
/// durable `refs/commedit/backup/index-*` commit so it stays recoverable
/// (`git read-tree`/`git checkout` the ref), and return that ref. Returns `None`
/// when there is no such index-only content. Best-effort: any git failure yields
/// `None` rather than blocking the rewrite.
pub fn backup_index_only_content(workspace_root: &Path) -> Option<String> {
    if !has_index_only_content(workspace_root) {
        return None;
    }
    let tree = git_line(workspace_root, &["write-tree"])?;
    let commit = git_line(
        workspace_root,
        &["commit-tree", &tree, "-m", "commedit: index backup (staged content not on disk)"],
    )?;
    let refname = format!("refs/commedit/backup/index-{}", &commit[..commit.len().min(12)]);
    let ok = Command::new("git")
        .current_dir(workspace_root)
        .args(["update-ref", &refname, &commit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok.then_some(refname)
}

/// Whether `git status` reports any path that is staged *and* differs again in
/// the working tree (codes like `MM`, `AD`, `MD`) — i.e. the staged version is
/// not the on-disk version, so it lives only in the index.
fn has_index_only_content(workspace_root: &Path) -> bool {
    let Ok(out) = Command::new("git")
        .current_dir(workspace_root)
        .args(["status", "--porcelain"])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    String::from_utf8(out.stdout)
        .unwrap_or_default()
        .lines()
        .any(|line| {
            let b = line.as_bytes();
            // X = index column, Y = worktree column. Index-only content ⟺ a
            // staged change (X not space, not the `?` of untracked) that the
            // worktree changed again (Y not space).
            b.len() >= 2 && b[0] != b' ' && b[0] != b'?' && b[1] != b' '
        })
}

/// Run a git command expected to print a single line (e.g. an object id),
/// returning the trimmed stdout, or `None` on failure/empty output.
fn git_line(workspace_root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(workspace_root)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// Read a single git config value (e.g. `user.name`) as git itself would see it
/// — honouring the system, global, and repo-local config hierarchy. `None` if
/// the key is unset or git can't be run. Whitespace-only values count as unset.
pub fn config_value(workspace_root: &Path, key: &str) -> Option<String> {
    let output = Command::new("git")
        .current_dir(workspace_root)
        .args(["config", "--get", key])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
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
