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
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use jj_lib::git;
use jj_lib::repo::MutableRepo;

/// Export jj bookmarks to git refs within the current transaction. jj already
/// moved bookmarks that pointed at rewritten commits during
/// `rebase_descendants`, so this makes those moves visible to git.
pub fn export_to_git(mut_repo: &mut MutableRepo) -> Result<()> {
    // NB: `export_refs` returns per-ref failures in `GitExportStats`, but we
    // deliberately *don't* turn them into an error. The two interesting cases
    // here are both already handled better elsewhere or must stay tolerated:
    //   * A bookmark whose *new* target is conflicted is silently skipped by jj
    //     (`diff_refs_to_export`) and never reported as failed at all — so this
    //     check couldn't catch it. That case is refused up front by
    //     `Repo::ensure_branch_exportable`, before any rewrite is committed.
    //   * A `FailedToSet` arises legitimately when two app instances race (one
    //     already moved git's ref, so the other's "must-match-old" precondition
    //     fails); that divergence is reconciled on the next `Repo::open`, so
    //     bailing here would break an intentionally-tolerated flow.
    git::export_refs(mut_repo).context("exporting refs to git")?;
    Ok(())
}

/// Create the throwaway git directory jj operates on for this session, with its
/// object database **shared** with the user's repository but its refs kept
/// private. Returns nothing; the caller already knows the path.
///
/// `git_dir` is a session-local path (under jj's temp workdir). We initialize a
/// bare repo there, then replace its `objects` directory with a symlink to the
/// user's object store, so:
///   * every object jj writes (the rewritten commits) lands in the user's ODB —
///     this is what keeps plain `git` able to see the rewrite (transparency);
///   * every ref jj creates — its `refs/jj/keep/*` GC anchors, its detached
///     HEAD, the bookmark it exports — lives here, **out of the user's `.git`**.
///
/// The checked-out branch (or a detached HEAD) and its tip are seeded so jj's
/// import sees the live history through the shared objects. The one branch ref
/// jj later moves is mirrored back into the user's repo by
/// [`crate::repo::Repo::bridge_branch_to_git`].
pub fn init_shared_git_dir(git_dir: &Path, workspace_root: &Path) -> Result<()> {
    let objects = git_objects_dir(workspace_root)?;
    // A known-valid bare layout (HEAD, config, refs/, …) for gix to open.
    let out = Command::new("git")
        .args(["init", "--bare", "-q"])
        .arg(git_dir)
        .output()
        .context("initializing the session git dir")?;
    if !out.status.success() {
        bail!(
            "failed to initialize the session git dir: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // Point its object store at the user's, so the two repos share one ODB.
    let local_objects = git_dir.join("objects");
    std::fs::remove_dir_all(&local_objects)
        .with_context(|| format!("clearing {}", local_objects.display()))?;
    symlink_dir(&objects, &local_objects)?;
    // Seed the checked-out branch / detached HEAD and its tip (resolvable now via
    // the shared objects) so jj imports the current history.
    seed_session_head(git_dir, workspace_root)
}

/// Point the session git dir's checked-out branch ref (and HEAD) at the user
/// repository's current tip, so a following jj import picks it up. The ODB is
/// already shared, so the tip is resolvable. Run once by [`init_shared_git_dir`]
/// at open, and again by the in-session catch-up
/// ([`crate::repo::Repo::sync_to_git_head`]) when the user moved HEAD out of band
/// (a plain `git commit`): jj imports refs from *this* session-local dir, not the
/// user's `.git`, so its branch ref must be re-pointed at the user's new tip
/// before the import can see the new commit.
pub fn seed_session_head(git_dir: &Path, workspace_root: &Path) -> Result<()> {
    if let Some(tip) = head_commit(workspace_root) {
        match head_branch(workspace_root) {
            Some(branch) => {
                git_in_dir(git_dir, &["update-ref", &branch, &tip])?;
                git_in_dir(git_dir, &["symbolic-ref", "HEAD", &branch])?;
            }
            None => git_in_dir(git_dir, &["update-ref", "--no-deref", "HEAD", &tip])?,
        }
    }
    Ok(())
}

/// The absolute path to the user's git object store, as git itself resolves it —
/// honouring linked worktrees and a separate common dir. Used to share the ODB
/// with jj's session-local git dir (see [`init_shared_git_dir`]).
pub fn git_objects_dir(workspace_root: &Path) -> Result<PathBuf> {
    let out = Command::new("git")
        .current_dir(workspace_root)
        .args(["rev-parse", "--git-path", "objects"])
        .output()
        .context("locating the git object store")?;
    if !out.status.success() {
        bail!(
            "git rev-parse --git-path objects failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let raw = String::from_utf8(out.stdout)
        .context("decoding the object store path")?
        .trim()
        .to_string();
    // `--git-path` prints relative to the workspace root (e.g. `.git/objects`) or
    // absolute (a worktree's common dir); join handles both, canonicalize resolves.
    let joined = workspace_root.join(&raw);
    std::fs::canonicalize(&joined)
        .with_context(|| format!("resolving object store path {}", joined.display()))
}

/// Point `ref_name` at `new` in the user's repository — the single git ref move
/// commedit performs itself now that jj exports into a session-local git dir.
/// `old`, when given, is git's compare-and-swap precondition (so a racing
/// commedit instance that already moved the ref is detected rather than
/// clobbered). `no_deref` updates the ref itself rather than its target, for a
/// detached HEAD. Errors carry git's message; the caller tolerates a
/// precondition miss (reconciled on the next open).
pub fn update_user_ref(
    workspace_root: &Path,
    ref_name: &str,
    new: &str,
    old: Option<&str>,
    no_deref: bool,
) -> Result<()> {
    let mut args: Vec<&str> = vec!["update-ref"];
    if no_deref {
        args.push("--no-deref");
    }
    args.push(ref_name);
    args.push(new);
    if let Some(old) = old {
        args.push(old);
    }
    let out = Command::new("git")
        .current_dir(workspace_root)
        .args(&args)
        .output()
        .context("running git update-ref")?;
    if !out.status.success() {
        bail!(
            "git update-ref failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Run `git --git-dir <git_dir> <args>`, erroring on a non-zero exit. Used to
/// seed [`init_shared_git_dir`]'s session git dir.
fn git_in_dir(git_dir: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .output()
        .with_context(|| format!("running git {args:?}"))?;
    if !out.status.success() {
        bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

/// Symlink the directory `target` at `link`. commedit shares jj's object store
/// with the user's by symlink; this is a unix-only mechanism (there is no
/// Windows release).
#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("symlinking {} -> {}", link.display(), target.display()))
}

#[cfg(not(unix))]
fn symlink_dir(_target: &Path, _link: &Path) -> Result<()> {
    bail!("commedit needs a unix-like platform to share the git object store")
}

/// A snapshot of every local branch (`refs/heads/*`) and the commit it points
/// at, taken straight from git. Used as the before-image for
/// [`restore_unrelated_heads`], the backstop that guarantees a rewrite only ever
/// moves the checked-out branch.
pub fn local_head_oids(workspace_root: &Path) -> BTreeMap<String, String> {
    let Ok(out) = Command::new("git")
        .current_dir(workspace_root)
        .args([
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/heads/",
        ])
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

/// What kind of git ref a [`RefDecoration`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    /// A local branch (`refs/heads/*`).
    Branch,
    /// A tag (`refs/tags/*`).
    Tag,
}

/// A branch or tag name pointing at a commit, for decorating the history view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefDecoration {
    /// Short name (`main`, `v1.0`), without the `refs/heads/`/`refs/tags/` prefix.
    pub name: String,
    pub kind: RefKind,
    /// The currently checked-out branch. Set by [`crate::repo::Repo::commit_refs`],
    /// which knows HEAD's branch; [`ref_decorations`] alone lacks that context and
    /// always leaves it `false`.
    pub current: bool,
}

/// Every local branch and tag of the user's repo, grouped by the hex id of the
/// commit it points at (annotated tags peeled to their target commit). Read
/// straight from the user's git refs — jj's view deliberately imports only the
/// checked-out branch, so it can't supply these. Best-effort: empty on failure.
pub fn ref_decorations(workspace_root: &Path) -> BTreeMap<String, Vec<RefDecoration>> {
    let Ok(out) = Command::new("git")
        .current_dir(workspace_root)
        .args([
            "for-each-ref",
            // `%(*objectname)` is the peeled target — empty except for
            // annotated tags, whose `%(objectname)` is the tag object itself.
            "--format=%(objectname) %(*objectname) %(refname)",
            "refs/heads/",
            "refs/tags/",
        ])
        .output()
    else {
        return BTreeMap::new();
    };
    if !out.status.success() {
        return BTreeMap::new();
    }
    let mut map: BTreeMap<String, Vec<RefDecoration>> = BTreeMap::new();
    for line in String::from_utf8(out.stdout).unwrap_or_default().lines() {
        // Refnames cannot contain spaces, so token-splitting is unambiguous:
        // 2 tokens when the peeled field is empty, 3 for an annotated tag.
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let (commit, refname) = match tokens[..] {
            [oid, refname] => (oid, refname),
            [_tag_obj, peeled, refname] => (peeled, refname),
            _ => continue,
        };
        let decoration = if let Some(name) = refname.strip_prefix("refs/heads/") {
            RefDecoration {
                name: name.to_string(),
                kind: RefKind::Branch,
                current: false,
            }
        } else if let Some(name) = refname.strip_prefix("refs/tags/") {
            RefDecoration {
                name: name.to_string(),
                kind: RefKind::Tag,
                current: false,
            }
        } else {
            continue;
        };
        map.entry(commit.to_string()).or_default().push(decoration);
    }
    map
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
        bail!(
            "git update-ref failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
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
    // Name the ref after the index *tree*, so identical staged content reuses
    // (overwrites) one ref instead of piling up a new ref on every rewrite.
    let refname = format!("refs/commedit/backup/index-{}", &tree[..tree.len().min(12)]);
    let commit = git_line(
        workspace_root,
        &[
            "commit-tree",
            &tree,
            "-m",
            "commedit: index backup (staged content not on disk)",
        ],
    )?;
    let ok = Command::new("git")
        .current_dir(workspace_root)
        .args(["update-ref", &refname, &commit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok.then_some(refname)
}

/// Keep only the most recently-created `refs/commedit/backup/index-*` ref,
/// deleting any older ones. Each rewrite that finds index-only content writes a
/// full snapshot of the index as its own ref; without pruning they accumulate
/// one per distinct staged state across sessions. The newest is the freshest
/// recovery point, so it is retained and the rest dropped. Best-effort: git
/// failures are ignored (a stale ref is harmless clutter, never a loss).
pub fn prune_backup_refs(workspace_root: &Path) {
    let Ok(out) = Command::new("git")
        .current_dir(workspace_root)
        .args([
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname)",
            "refs/commedit/backup/",
        ])
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let refs = String::from_utf8(out.stdout).unwrap_or_default();
    // The first line is the newest; delete everything after it.
    for refname in refs.lines().skip(1) {
        let _ = Command::new("git")
            .current_dir(workspace_root)
            .args(["update-ref", "-d", refname])
            .status();
    }
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
