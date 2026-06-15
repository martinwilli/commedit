//! A persistent, per-repository cache of jj's commit index.
//!
//! ## Why
//!
//! Opening a repo builds jj's commit index over the **whole** ancestry of HEAD by
//! reading every ancestor commit object from git ([`crate::repo::Repo::import_git`]).
//! On a huge history (e.g. the Linux kernel, ~1.4M commits) that is ~30s, and
//! because each session's jj metadata lives in a throwaway temp dir it is rebuilt
//! from scratch *every launch*. jj's indexer is actually **incremental** — given a
//! prior operation whose index is already on disk, it reads only the commits not
//! yet indexed — but a fresh temp dir never has such a prior op.
//!
//! This module persists the session's `repo/` tree (op store + the built index)
//! into a per-user cache dir **outside the user's repository**, so the next launch
//! *primes* a temp session from it ([`crate::repo::Repo::load_detached`]) and the
//! import only has to index commits added since — turning the ~30s cold open into a
//! ~1s incremental one.
//!
//! ## Concurrency (prime-and-flush, shared lock)
//!
//! A session never operates *in* the cache: at open it **copies** the cached
//! `repo/` into its own temp dir and runs its op log there in isolation (so the
//! "no shared live op log between sessions" invariant the throwaway design relies
//! on is preserved). The only shared artifact is the cache entry, guarded by a
//! per-key `flock`:
//!
//! * **Use = shared lock** ([`Handle`] holds it for the session). Any number of
//!   commedit views — GTK windows, an MCP agent — take it concurrently and all
//!   prime from the cache.
//! * **Flush = try-exclusive** (a lock *conversion* on the same fd). It succeeds
//!   only when this is the last view on the entry; otherwise the flush is skipped
//!   (cheap to forgo — the expensive index base is already cached; only the small
//!   delta since prime is lost, re-indexed incrementally next time).
//! * **Eviction = try-exclusive** on a fresh fd. It only ever deletes an entry no
//!   session is using, so a live reader is never pulled out from under. While any
//!   view is open on a repo, its entry simply is not evicted.
//!
//! ## Safety net
//!
//! Caching only ever makes a launch *faster*, never *fail*: a corrupt/partial
//! entry, an index referencing an object the user has since GC'd, or a jj-lib
//! on-disk format change all surface as a load error, and the caller falls back to
//! a clean cold open after [`Handle::invalidate`]ing the bad entry — which the next
//! flush repopulates in the current format. So the version stamp below is a
//! fast-path hint; the fallback is the real guarantee.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Where the index cache lives, chosen by the frontend at [`crate::repo::Repo::open_with_cache`].
#[derive(Clone, Copy)]
pub enum IndexCache<'a> {
    /// No caching: a plain cold open with no flush (the engine's default, and what
    /// tests use so they never touch the user's real cache).
    Disabled,
    /// The per-user default location: `$XDG_CACHE_HOME/commedit/index` (falling
    /// back to `$HOME/.cache/commedit/index`). Used by the GTK and MCP frontends.
    Default,
    /// An explicit base directory — for integration tests and power users.
    At(&'a Path),
}

/// Bump when the on-disk cache layout changes incompatibly. Combined with the
/// crate version into the entry [`stamp`]; a mismatch invalidates the entry.
const CACHE_FORMAT: u32 = 1;

/// Rebuild the index from scratch after this many cached generations, to bound the
/// op-log growth that accumulates as each session's ops are flushed on top of the
/// prior ones. One slow (cold) open every `MAX_GENERATIONS` launches.
const MAX_GENERATIONS: u64 = 100;

/// Evict cache entries unused for longer than this.
const TTL_SECS: u64 = 30 * 24 * 60 * 60;

/// Keep the whole cache pool under this size, evicting least-recently-used entries
/// over the cap (skipping any currently in use).
const MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The validity stamp for entries written by this build. Includes the crate
/// version so a commedit upgrade (which is the only way the pinned jj-lib version,
/// and thus jj's index format, can change) starts fresh.
fn stamp() -> String {
    format!("commedit-{}-fmt{}", env!("CARGO_PKG_VERSION"), CACHE_FORMAT)
}

/// Resolve the cache base directory for `cache`, creating it if needed. `None`
/// when caching is [`IndexCache::Disabled`] or no base can be determined.
pub(crate) fn resolve_base(cache: IndexCache) -> Option<PathBuf> {
    let base = match cache {
        IndexCache::Disabled => return None,
        IndexCache::At(p) => p.to_path_buf(),
        IndexCache::Default => default_base()?,
    };
    fs::create_dir_all(&base).ok()?;
    Some(base)
}

fn default_base() -> Option<PathBuf> {
    let cache_home = std::env::var_os("XDG_CACHE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(cache_home.join("commedit").join("index"))
}

/// The cache key for a repository, identifying its object store: the SHA-256 of
/// the canonical objects-dir path. All worktrees of one repo share the objects dir
/// (hence one entry), which is exactly right — jj's index is additive over commit
/// ids, so accumulating several branches' commits in one index just maximizes
/// reuse.
fn key_for(objects_dir: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(objects_dir.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// A held cache slot for one session: the shared lock plus the resolved paths. A
/// `Handle` exists only while caching is active for this session; dropping it
/// releases the shared lock.
pub(crate) struct Handle {
    entry_dir: PathBuf,
    /// Sibling lock file `<base>/<key>.lock`, kept separate from the entry dir so
    /// the lock survives the entry being replaced/evicted. Holds the shared lock.
    lock: File,
    /// An existing, stamp-valid, under-generation-cap entry is present and should
    /// be primed; otherwise this session cold-builds and the flush seeds the entry.
    pub valid: bool,
    /// The generation read from a valid entry (the flush writes `gen + 1`); `0`
    /// when there was none, so the flush starts at generation `1`.
    generation: u64,
}

/// Acquire the cache slot for the repo whose object store is `objects_dir`. Takes a
/// shared lock held for the session. `None` when the lock is contended (another
/// process is mid-flush or mid-eviction on this entry) — the caller then runs an
/// ordinary uncached cold open.
pub(crate) fn acquire(base: &Path, objects_dir: &Path) -> Option<Handle> {
    let key = key_for(objects_dir);
    let lock_path = base.join(format!("{key}.lock"));
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .ok()?;
    // Non-blocking: if a flush/eviction holds the exclusive lock, skip caching for
    // this session rather than stalling the open (`Err` is `WouldBlock` or a real
    // error — either way, run uncached).
    if lock.try_lock_shared().is_err() {
        return None;
    }
    let entry_dir = base.join(&key);
    let (valid, generation) = match read_meta(&entry_dir) {
        Some(meta) if meta.stamp == stamp() && meta.generation < MAX_GENERATIONS => {
            (entry_dir.join("repo").is_dir(), meta.generation)
        }
        _ => (false, 0),
    };
    Some(Handle {
        entry_dir,
        lock,
        valid,
        generation,
    })
}

impl Handle {
    /// Copy the cached `repo/` into `state_dir/repo` so [`crate::repo::Repo::load_detached`]
    /// can load it. Only call when [`Handle::valid`].
    pub(crate) fn prime(&self, state_dir: &Path) -> Result<()> {
        let src = self.entry_dir.join("repo");
        let dst = state_dir.join("repo");
        copy_dir(&src, &dst).with_context(|| {
            format!(
                "priming jj index from {} to {}",
                src.display(),
                dst.display()
            )
        })?;
        Ok(())
    }

    /// Mark the entry unusable after a failed prime/load: drop its `repo/` so the
    /// next [`acquire`] cold-builds. Best-effort.
    pub(crate) fn invalidate(&self) {
        let _ = fs::remove_dir_all(self.entry_dir.join("repo"));
        let _ = fs::remove_file(self.entry_dir.join("META"));
    }

    /// Persist this session's `state_dir/repo` back into the cache, so the next
    /// launch primes from it. Best-effort and **non-blocking**: it upgrades the
    /// held shared lock to exclusive (a flock conversion on the same fd, which
    /// succeeds only when no other view is open on this entry); if that fails, or
    /// any copy step fails, the flush is silently skipped — the session was still
    /// fast, and only the small delta since prime is not persisted.
    pub(crate) fn flush(&self, state_dir: &Path) {
        let repo = state_dir.join("repo");
        if !repo.is_dir() {
            return;
        }
        // Only one view may write the entry. Converting our own shared lock to
        // exclusive (non-blocking `try_lock`) succeeds iff no *other* process holds
        // the lock; `Err` (`WouldBlock` or error) means another view is open, so we
        // skip the flush.
        if self.lock.try_lock().is_err() {
            return;
        }
        if let Err(e) = self.flush_locked(&repo) {
            eprintln!("commedit: could not update the index cache ({e}); will rebuild next time");
        }
        // Downgrade back to the shared lock (a conversion on the same fd, always
        // succeeds since we still hold it), so other views aren't blocked by a
        // lingering exclusive until this handle drops.
        let _ = self.lock.try_lock_shared();
    }

    fn flush_locked(&self, repo: &Path) -> Result<()> {
        fs::create_dir_all(&self.entry_dir).context("creating the cache entry dir")?;
        // Copy into a unique staging dir, then swap by rename so a reader (once the
        // lock frees) never sees a half-written `repo/`.
        let staging = self
            .entry_dir
            .join(format!("repo.tmp.{}", std::process::id()));
        let _ = fs::remove_dir_all(&staging);
        let bytes = copy_dir(repo, &staging).context("staging the index for the cache")?;
        let live = self.entry_dir.join("repo");
        let old = self
            .entry_dir
            .join(format!("repo.old.{}", std::process::id()));
        if live.exists() {
            fs::rename(&live, &old).context("rotating the previous cache entry")?;
        }
        fs::rename(&staging, &live).context("publishing the new cache entry")?;
        let _ = fs::remove_dir_all(&old);
        write_meta(
            &self.entry_dir,
            &Meta {
                stamp: stamp(),
                generation: self.generation.saturating_add(1),
                last_used: now_secs(),
                size: bytes,
            },
        )?;
        Ok(())
    }
}

/// Opportunistic, best-effort cache hygiene over the whole `base`: drop entries
/// unused past [`TTL_SECS`], then evict least-recently-used entries while the pool
/// exceeds [`MAX_TOTAL_BYTES`]. Never touches an entry currently in use (its lock
/// won't grant exclusive). Sizes/timestamps come from each entry's `META`, so the
/// sweep is cheap (no tree walks).
pub(crate) fn sweep(base: &Path) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    let now = now_secs();
    let mut live: Vec<(PathBuf, Meta)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // Entry dirs are the 64-char hex keys; skip lock files and stray staging.
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.len() != 64 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let Some(meta) = read_meta(&path) else {
            // No readable META → an aborted/foreign dir; try to remove it.
            try_evict(base, name, &path);
            continue;
        };
        if now.saturating_sub(meta.last_used) > TTL_SECS {
            try_evict(base, name, &path);
        } else {
            live.push((path, meta));
        }
    }
    let mut total: u64 = live.iter().map(|(_, m)| m.size).sum();
    if total <= MAX_TOTAL_BYTES {
        return;
    }
    // Oldest first, evict until under the cap.
    live.sort_by_key(|(_, m)| m.last_used);
    for (path, meta) in live {
        if total <= MAX_TOTAL_BYTES {
            break;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if try_evict(base, name, &path) {
                total = total.saturating_sub(meta.size);
            }
        }
    }
}

/// Try to delete cache entry `name` at `path`, taking its exclusive lock first so
/// an in-use entry is never removed. Returns whether it was evicted. The lock file
/// itself is left in place (deleting it would race a concurrent opener).
fn try_evict(base: &Path, name: &str, path: &Path) -> bool {
    let lock_path = base.join(format!("{name}.lock"));
    let Ok(lock) = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
    else {
        return false;
    };
    if lock.try_lock().is_err() {
        return false;
    }
    let removed = fs::remove_dir_all(path).is_ok();
    let _ = lock.unlock();
    removed
}

/// Recursively copy `src` into `dst` (created), returning the total bytes of file
/// content copied. Preserves symlinks as symlinks (so the cached tree never
/// dereferences anything — though jj's `repo/` is symlink-free in practice, the
/// session git dir with its `objects` symlink is not cached). Unix permission bits
/// are carried over.
fn copy_dir(src: &Path, dst: &Path) -> Result<u64> {
    fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    let mut total = 0u64;
    for entry in fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            total += copy_dir(&from, &to)?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(&from)?;
            symlink(&target, &to)
                .with_context(|| format!("linking {} -> {}", to.display(), target.display()))?;
        } else {
            total += fs::copy(&from, &to)
                .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
        }
    }
    Ok(total)
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "symlinks unsupported on this platform",
    ))
}

/// The metadata sidecar (`<entry>/META`), a tiny `key=value` file (same hand-rolled
/// shape as the GTK config files), holding the validity stamp, the generation
/// counter, the last-used timestamp, and the entry size for eviction.
struct Meta {
    stamp: String,
    generation: u64,
    last_used: u64,
    size: u64,
}

fn read_meta(entry_dir: &Path) -> Option<Meta> {
    let text = fs::read_to_string(entry_dir.join("META")).ok()?;
    let mut stamp = None;
    let mut generation = 0u64;
    let mut last_used = 0u64;
    let mut size = 0u64;
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "stamp" => stamp = Some(v.trim().to_string()),
            "generation" => generation = v.trim().parse().unwrap_or(0),
            "last_used" => last_used = v.trim().parse().unwrap_or(0),
            "size" => size = v.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    Some(Meta {
        stamp: stamp?,
        generation,
        last_used,
        size,
    })
}

fn write_meta(entry_dir: &Path, meta: &Meta) -> Result<()> {
    let body = format!(
        "stamp={}\ngeneration={}\nlast_used={}\nsize={}\n",
        meta.stamp, meta.generation, meta.last_used, meta.size
    );
    let path = entry_dir.join("META");
    let mut f = File::create(&path).with_context(|| format!("writing {}", path.display()))?;
    f.write_all(body.as_bytes())?;
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tree(root: &Path) {
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/file1"), b"hello").unwrap();
        fs::write(root.join("a/b/file2"), b"world!!").unwrap();
    }

    #[test]
    fn key_is_stable_and_path_specific() {
        let a = key_for(Path::new("/repos/foo/.git/objects"));
        let a2 = key_for(Path::new("/repos/foo/.git/objects"));
        let b = key_for(Path::new("/repos/bar/.git/objects"));
        assert_eq!(a, a2, "same path → same key");
        assert_ne!(a, b, "different repos → different keys");
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn copy_dir_reproduces_the_tree_and_counts_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        write_tree(&src);
        let bytes = copy_dir(&src, &dst).unwrap();
        assert_eq!(bytes, 5 + 7, "content byte total");
        assert_eq!(fs::read(dst.join("a/file1")).unwrap(), b"hello");
        assert_eq!(fs::read(dst.join("a/b/file2")).unwrap(), b"world!!");
    }

    /// A round trip through a real cache base: acquire (cold), flush, then a second
    /// acquire sees a valid entry and primes the same `repo/` content back.
    #[test]
    fn acquire_flush_prime_round_trip() {
        let home = tempfile::tempdir().unwrap();
        let base = resolve_base(IndexCache::At(home.path())).unwrap();
        let objects = Path::new("/some/repo/.git/objects");

        // First session: no entry yet → invalid → cold; flush seeds it.
        let session1 = tempfile::tempdir().unwrap();
        fs::create_dir_all(session1.path().join("repo")).unwrap();
        write_tree(&session1.path().join("repo"));
        let h1 = acquire(&base, objects).expect("acquire 1");
        assert!(!h1.valid, "no entry on first acquire");
        h1.flush(session1.path());
        drop(h1);

        // Second session: entry now valid → prime restores the tree.
        let h2 = acquire(&base, objects).expect("acquire 2");
        assert!(h2.valid, "entry valid after flush");
        assert_eq!(h2.generation, 1, "generation advanced");
        let session2 = tempfile::tempdir().unwrap();
        h2.prime(session2.path()).expect("prime");
        assert_eq!(
            fs::read(session2.path().join("repo/a/file1")).unwrap(),
            b"hello"
        );
    }

    /// A stamp mismatch (e.g. after a commedit upgrade) invalidates an entry, so
    /// the next session cold-rebuilds rather than loading a stale-format index.
    #[test]
    fn a_stamp_mismatch_invalidates_the_entry() {
        let home = tempfile::tempdir().unwrap();
        let base = resolve_base(IndexCache::At(home.path())).unwrap();
        let objects = Path::new("/r/.git/objects");
        let session = tempfile::tempdir().unwrap();
        fs::create_dir_all(session.path().join("repo")).unwrap();
        let h = acquire(&base, objects).unwrap();
        h.flush(session.path());
        drop(h); // release the slot, as a real session does at close
        let entry = base.join(key_for(objects));
        // Rewrite META with a foreign stamp.
        write_meta(
            &entry,
            &Meta {
                stamp: "commedit-0.0.0-fmt0".into(),
                generation: 1,
                last_used: now_secs(),
                size: 0,
            },
        )
        .unwrap();
        let h2 = acquire(&base, objects).unwrap();
        assert!(!h2.valid, "foreign stamp → invalid");
    }
}
