//! Integration tests for the persistent index cache: a second open primes from
//! the cached jj index instead of rebuilding it, imports the commits added since
//! incrementally, and yields identical, fully-editable history.

mod common;

use std::path::Path;

use commedit_engine::history::history;
use commedit_engine::index_cache::IndexCache;
use commedit_engine::repo::Repo;
use common::{git, git_log_subjects, init_repo};

/// Open `dir` with the index cache rooted at `cache_base`, flushing it back at the
/// end of the closure so the entry is populated for the next open.
fn subjects_via_cache(dir: &Path, cache_base: &Path) -> Vec<String> {
    let mut repo = Repo::open_with_cache(dir, IndexCache::At(cache_base)).expect("open");
    let head = repo.head_commit_id().expect("head");
    let subjects: Vec<String> = history(&repo.repo, &head)
        .expect("history")
        .into_iter()
        .map(|c| c.subject)
        .collect();
    repo.flush_index_cache();
    subjects
}

#[test]
fn second_open_primes_cache_and_imports_new_commits_incrementally() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let cache = tempfile::tempdir().unwrap();
    let cache_base = cache.path();

    init_repo(
        dir,
        &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
    );

    // First open: no entry yet → cold build; flush seeds the cache.
    assert_eq!(
        subjects_via_cache(dir, cache_base),
        vec!["second".to_string(), "first".to_string()]
    );
    // The cache entry now exists (one hex-keyed dir + its lock file).
    let entries: Vec<_> = std::fs::read_dir(cache_base)
        .unwrap()
        .flatten()
        .filter(|e| e.path().is_dir())
        .collect();
    assert_eq!(entries.len(), 1, "exactly one cache entry was written");
    assert!(entries[0].path().join("repo").is_dir(), "primeable repo/");
    assert!(entries[0].path().join("META").is_file(), "META stamp");

    // A new commit lands out of band; the second open must prime the cached index
    // (a,b) and import the new commit (c) incrementally on top.
    std::fs::write(dir.join("c.txt"), "c\n").unwrap();
    git(dir, &["add", "c.txt"]);
    git(dir, &["commit", "-q", "-m", "third"]);

    assert_eq!(
        subjects_via_cache(dir, cache_base),
        vec![
            "third".to_string(),
            "second".to_string(),
            "first".to_string()
        ],
        "primed session sees the full history including the incrementally-imported commit"
    );
}

#[test]
fn a_primed_session_can_rewrite_history() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let cache = tempfile::tempdir().unwrap();
    let cache_base = cache.path();

    init_repo(
        dir,
        &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
    );

    // Seed the cache.
    drop(Repo::open_with_cache(dir, IndexCache::At(cache_base)).expect("open seeds cache"));

    // Re-open (primed) and reword the tip — the loaded-from-cache repo must be a
    // fully functional jj workspace, and the rewrite must reach git.
    let mut repo = Repo::open_with_cache(dir, IndexCache::At(cache_base)).expect("reopen primed");
    let head = repo.head_commit_id().expect("head");
    let commits = history(&repo.repo, &head).expect("history");
    let tip = commits[0].id.clone();
    repo.rewrite_message(&tip, "second reworded")
        .expect("reword on a primed session");
    repo.flush_index_cache();
    drop(repo);

    assert_eq!(
        git_log_subjects(dir),
        vec!["second reworded".to_string(), "first".to_string()],
        "the rewrite from the primed session reached git"
    );
}
