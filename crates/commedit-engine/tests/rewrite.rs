//! End-to-end: rewrite a middle commit's message and confirm plain `git` sees
//! the rewritten history (descendants rebased, branch moved).

mod common;

use commedit_engine::history::{history, history_limited};
use commedit_engine::repo::Repo;
use commedit_engine::rewrite::Identity;

#[test]
fn history_limited_pages_newest_first_and_flags_more() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "first"),
            ("b.txt", "b\n", "second"),
            ("c.txt", "c\n", "third"),
            ("d.txt", "d\n", "fourth"),
        ],
    );

    let repo = Repo::open(dir).expect("open");
    let head = repo.head_commit_id().expect("head");

    // A short page returns the newest commits and reports more below it.
    let (page, has_more) = history_limited(&repo.repo, &head, 2).expect("history");
    assert!(has_more);
    let subjects: Vec<&str> = page.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, vec!["fourth", "third"]);

    // A limit at or above the history length loads everything and flags no more,
    // matching the unbounded walk.
    let (all, has_more) = history_limited(&repo.repo, &head, 10).expect("history");
    assert!(!has_more);
    assert_eq!(all.len(), history(&repo.repo, &head).unwrap().len());
}

#[test]
fn rewrites_middle_commit_message_visible_to_git() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "first"),
            ("b.txt", "b\n", "second"),
            ("c.txt", "c\n", "third"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");

    // Find the middle commit ("second").
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let target = commits
        .iter()
        .find(|c| c.subject == "second")
        .expect("second commit present")
        .id
        .clone();

    repo.rewrite_message(&target, "second (edited)")
        .expect("rewrite message");

    // Plain git must see the rewritten message with descendants preserved.
    let subjects = common::git_log_subjects(dir);
    assert_eq!(subjects, vec!["third", "second (edited)", "first"]);

    // Transparency invariants: HEAD attached to the original branch, and a
    // clean working tree — a plain-git user sees nothing unusual.
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");

    // Repository must remain intact.
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn rewrites_author_and_committer_identity_visible_to_git() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
    );

    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let target = commits
        .iter()
        .find(|c| c.subject == "first")
        .expect("first commit present")
        .id
        .clone();

    let id = Identity {
        author_name: "Ada Lovelace".to_string(),
        author_email: "ada@example.com".to_string(),
        author_time: "2026-06-05 14:30:00 +0200".to_string(),
        committer_name: "Grace Hopper".to_string(),
        committer_email: "grace@example.com".to_string(),
        committer_time: "2026-06-06 09:00:00 +0000".to_string(),
    };
    repo.rewrite_identity(&target, &id).expect("rewrite identity");

    // Plain git must see the rewritten author/committer and dates. The rewritten
    // commit is the history root, so resolve it via the first-parent chain.
    let root = common::git(dir, &["rev-list", "--max-parents=0", "HEAD"]);
    let fmt = "%an|%ae|%ad|%cn|%ce|%cd";
    let line = common::git(
        dir,
        &["show", "-s", &format!("--format={fmt}"), "--date=format:%Y-%m-%d %H:%M:%S %z", &root],
    );
    let fields: Vec<&str> = line.split('|').collect();
    assert_eq!(fields[0], "Ada Lovelace");
    assert_eq!(fields[1], "ada@example.com");
    assert_eq!(fields[2], "2026-06-05 14:30:00 +0200");
    assert_eq!(fields[3], "Grace Hopper");
    assert_eq!(fields[4], "grace@example.com");
    assert_eq!(fields[5], "2026-06-06 09:00:00 +0000");

    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn reorders_commit_to_a_new_position_visible_to_git() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "first"),
            ("b.txt", "b\n", "second"),
            ("c.txt", "c\n", "third"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history"); // [third, second, first]
    let by = |s: &str| {
        commits
            .iter()
            .find(|c| c.subject == s)
            .unwrap_or_else(|| panic!("{s} commit present"))
    };
    let third = by("third");
    let second = by("second");
    let first = by("first");

    // Move "third" (the tip) down to the oldest position: parent the root, with
    // "first" rebased on top of it, so "second" becomes the new head.
    repo.reorder_commit(
        &third.id,
        first.parents.clone(),
        vec![first.id.clone()],
        &second.id,
    )
    .expect("reorder");

    // The branch now reads second <- first <- third <- root, and the diffs were
    // re-applied (distinct files commute, so nothing is empty or conflicted).
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["second", "first", "third"]
    );

    // Transparency invariants: HEAD attached, clean tree, intact repo.
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn reorder_stamps_the_git_configured_committer_not_jjs_default() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "first"),
            ("b.txt", "b\n", "second"),
            ("c.txt", "c\n", "third"),
        ],
    );
    // The user's git identity, configured the ordinary way. The GUI launches with
    // no GIT_AUTHOR_* env set, so this config is the only signal — the rebased
    // commits must be stamped with it, not jj's generic "commedit" fallback.
    common::git(dir, &["config", "user.name", "Repo Config"]);
    common::git(dir, &["config", "user.email", "config@example.com"]);

    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let by = |s: &str| commits.iter().find(|c| c.subject == s).unwrap();
    let (third, second, first) = (by("third"), by("second"), by("first"));

    // Move "third" to the bottom; "first"/"second" get rebased and re-stamped.
    repo.reorder_commit(
        &third.id,
        first.parents.clone(),
        vec![first.id.clone()],
        &second.id,
    )
    .expect("reorder");

    // The new head "second" was rebased: its committer is the git-configured
    // identity, while its original author ("Tester") is preserved.
    let line = common::git(dir, &["show", "-s", "--format=%cn|%ce|%an", "main"]);
    let fields: Vec<&str> = line.split('|').collect();
    assert_eq!(fields[0], "Repo Config");
    assert_eq!(fields[1], "config@example.com");
    assert_eq!(fields[2], "Tester");
}

#[test]
fn committer_config_overrides_user_config_like_git() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
    );
    // git resolves the committer as committer.* over user.*. A repo may set both
    // (e.g. authoring under user.email but committing under a project address);
    // commedit must follow the same precedence, not silently pick user.*.
    common::git(dir, &["config", "user.name", "User Name"]);
    common::git(dir, &["config", "user.email", "user@example.com"]);
    common::git(dir, &["config", "committer.name", "Committer Name"]);
    common::git(dir, &["config", "committer.email", "committer@example.com"]);

    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let first = commits.iter().find(|c| c.subject == "first").unwrap();

    // Rewriting "first" re-stamps the committer and rebases "second" (the tip).
    repo.rewrite_message(&first.id, "first (edited)")
        .expect("rewrite message");

    // The rebased tip carries the committer.* identity, not user.*, while its
    // original author (init_repo's "Tester") is preserved.
    let line = common::git(dir, &["show", "-s", "--format=%cn|%ce|%an|%ae", "main"]);
    let fields: Vec<&str> = line.split('|').collect();
    assert_eq!(fields[0], "Committer Name");
    assert_eq!(fields[1], "committer@example.com");
    assert_eq!(fields[2], "Tester");
    assert_eq!(fields[3], "tester@example.com");
}

#[test]
fn a_clean_move_to_the_top_keeps_the_worktree_on_the_new_tip() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    // Independent files, so the move rebases clean — no spurious resolution
    // (whose chain rebuild would mask a stranded working copy) gets involved.
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B"), ("c.txt", "c\n", "C")],
    );
    // An uncommitted edit that must survive the move.
    std::fs::write(dir.join("local.txt"), "local\n").unwrap();

    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let from = commits.iter().position(|c| c.subject == "B").unwrap();
    let mv = common::plan_reorder_single(&repo, &commits, from, 0);
    repo.reorder_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
        .expect("reorder");

    // B tops the branch, and the worktree followed it there: b.txt is on disk
    // (not reported deleted), only the local edit shows as uncommitted.
    assert_eq!(common::git_log_subjects(dir), vec!["B", "C", "A"]);
    assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "b\n");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "?? local.txt");
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn history_has_no_duplicate_rows_after_a_reorder() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "A"),
            ("b.txt", "b\n", "B"),
            ("c.txt", "c\n", "C"),
            ("d.txt", "d\n", "D"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let from = commits.iter().position(|c| c.subject == "D").unwrap();
    let mv = common::plan_reorder_single(&repo, &commits, from, commits.len());
    repo.reorder_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
        .expect("reorder");

    // The reorder abandons the pre-reorder commits, which jj keeps pinned (via
    // `git_head` and `refs/jj/keep/*`). The history view must not surface those
    // as duplicate rows — exactly four distinct commits, one per subject.
    let after = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let mut subjects: Vec<_> = after.iter().map(|c| c.subject.clone()).collect();
    subjects.sort();
    assert_eq!(subjects, vec!["A", "B", "C", "D"]);
    let unique_ids: std::collections::HashSet<_> = after.iter().map(|c| c.id.clone()).collect();
    assert_eq!(unique_ids.len(), after.len());

    // The plain-git side must be clean too: no `refs/jj/keep/*` clutter and no
    // unreachable pre-reorder commits surfacing in `git log --all`.
    assert_eq!(
        common::git(dir, &["for-each-ref", "--format=%(refname)", "refs/jj/keep/"]),
        ""
    );
    let mut all_subjects: Vec<_> = common::git(dir, &["log", "--all", "--format=%s"])
        .lines()
        .map(str::to_string)
        .collect();
    all_subjects.sort();
    assert_eq!(all_subjects, vec!["A", "B", "C", "D"]);
}

#[test]
fn keep_ref_for_a_manual_jj_anonymous_head_is_preserved() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B"), ("c.txt", "c\n", "C")],
    );

    // Simulate a manual jj user's un-bookmarked work: a commit off B with no
    // branch pointing at it, protected only by a `refs/jj/keep/*` ref — exactly
    // how jj guards anonymous heads from `git gc`.
    g(&["checkout", "-q", "-b", "tmp", "main~1"]);
    std::fs::write(dir.join("x.txt"), "x\n").unwrap();
    g(&["add", "."]);
    g(&["commit", "-q", "-m", "anonymous work"]);
    let anon = g(&["rev-parse", "HEAD"]);
    g(&["checkout", "-q", "main"]);
    g(&["branch", "-q", "-D", "tmp"]);
    g(&["update-ref", &format!("refs/jj/keep/{anon}"), &anon]);

    // commedit reorders its own branch.
    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let from = commits.iter().position(|c| c.subject == "C").unwrap();
    let mv = common::plan_reorder_single(&repo, &commits, from, commits.len());
    repo.reorder_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
        .expect("reorder");

    // The anonymous head's keep-ref (and thus the commit) must still be there:
    // commedit only prunes its own history's keep-refs.
    assert_eq!(
        common::git(dir, &["rev-parse", "--verify", &format!("refs/jj/keep/{anon}")]),
        anon
    );
}

#[test]
fn reorder_works_on_a_linear_branch_with_a_divergent_side_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);

    // A linear main A <- B <- C, plus a divergent branch `side` (with commit X
    // off A) left as a ref. This is the davici shape: the edited branch is
    // linear, but the gitk-style view also shows the side branch.
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B"), ("c.txt", "c\n", "C")],
    );
    g(&["checkout", "-q", "-b", "side", "main~2"]);
    std::fs::write(dir.join("x.txt"), "x\n").unwrap();
    g(&["add", "."]);
    g(&["commit", "-q", "-m", "X"]);
    g(&["checkout", "-q", "main"]);

    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    // The view is a DAG (side diverges), yet reordering the linear main branch
    // must still work — this is what the over-strict whole-view gate broke.
    let third = commits.iter().find(|c| c.subject == "C").expect("C present");
    let from = commits.iter().position(|c| c.id == third.id).unwrap();
    let mv = common::plan_reorder_single(&repo, &commits, from, commits.len());
    repo.reorder_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
        .expect("reorder");

    // main is rearranged and stays linear (no spurious merge), the side branch
    // is untouched, and the repo is intact.
    assert_eq!(common::git_log_subjects(dir), vec!["B", "A", "C"]);
    assert_eq!(common::git(dir, &["rev-list", "--merges", "--count", "main"]), "0");
    assert_eq!(common::git(dir, &["log", "--format=%s", "side"]).lines().next(), Some("X"));
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn rewrite_leaves_a_backup_branch_on_the_same_tip_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let g = |args: &[&str]| common::git(dir, args);

    // Linear main A <- B <- C, plus a `backup` branch pointing at the *same* tip
    // C — the common "I made a backup before rewriting" shape. (init leaves us on
    // main; `git branch` creates the ref without moving HEAD.)
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "A"), ("b.txt", "b\n", "B"), ("c.txt", "c\n", "C")],
    );
    g(&["branch", "backup"]);
    let backup_before = g(&["rev-parse", "backup"]);
    let main_before = g(&["rev-parse", "main"]);
    assert_eq!(backup_before, main_before, "backup starts on main's tip");

    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let target = commits.iter().find(|c| c.subject == "B").expect("B present");
    repo.rewrite_message(&target.id, "B (edited)").expect("rewrite");

    // main is rewritten, but the backup branch must still point at the original
    // commits — rewriting one branch must never drag an unrelated one along.
    assert_eq!(common::git_log_subjects(dir), vec!["C", "B (edited)", "A"]);
    assert_eq!(g(&["rev-parse", "backup"]), backup_before, "backup unmoved");
    assert_ne!(g(&["rev-parse", "main"]), main_before, "main did move");
    assert_eq!(g(&["log", "--format=%s", "backup"]), "C\nB\nA");
    assert_eq!(g(&["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(g(&["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn drops_middle_commit_visible_to_git() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "first"),
            ("b.txt", "b\n", "second"),
            ("c.txt", "c\n", "third"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history"); // [third, second, first]
    let from = commits.iter().position(|c| c.subject == "second").unwrap();
    let target = repo.plan_drop(&commits, from).expect("droppable");
    repo.abandon_commit(&target).expect("drop");

    // "second" is gone; its descendant "third" rebased onto "first".
    assert_eq!(common::git_log_subjects(dir), vec!["third", "first"]);

    // Transparency invariants: HEAD attached, clean tree, intact repo.
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn drops_then_restores_commit_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "first"),
            ("b.txt", "b\n", "second"),
            ("c.txt", "c\n", "third"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history"); // [third, second, first]
    let from = commits.iter().position(|c| c.subject == "second").unwrap();
    // Remember the pre-drop commit (its id stays resolvable) like the trash does.
    let second = commits[from].clone();
    let target = repo.plan_drop(&commits, from).expect("droppable");
    repo.abandon_commit(&target).expect("drop");
    assert_eq!(common::git_log_subjects(dir), vec!["third", "first"]);

    // Graft it back between "third" and "first" (gap 1), reproducing the original
    // order. This proves a dropped commit is still resolvable and re-graftable.
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history"); // [third, first]
    let mv = common::plan_restore_single(&repo, &commits, &second, 1);
    repo.restore_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
        .expect("restore");

    assert_eq!(common::git_log_subjects(dir), vec!["third", "second", "first"]);
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn drops_branch_tip_moving_the_branch_to_its_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "first"),
            ("b.txt", "b\n", "second"),
            ("c.txt", "c\n", "third"),
        ],
    );

    let mut repo = Repo::open(dir).expect("open");
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history"); // [third, second, first]
    let target = repo.plan_drop(&commits, 0).expect("droppable"); // the tip "third"
    repo.abandon_commit(&target).expect("drop");

    // The branch bookmark followed to the parent "second"; the tree is clean.
    assert_eq!(common::git_log_subjects(dir), vec!["second", "first"]);
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn revert_all_restores_the_original_session_state_to_git() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[
            ("a.txt", "a\n", "first"),
            ("b.txt", "b\n", "second"),
            ("c.txt", "c\n", "third"),
        ],
    );

    // The state to revert back to: the original subjects and the exact HEAD.
    let original_subjects = common::git_log_subjects(dir);
    let original_head = common::git(dir, &["rev-parse", "HEAD"]);

    let mut repo = Repo::open(dir).expect("open");

    // Stack two distinct mutations on top of the session-start state: edit a
    // middle message, then drop the tip.
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let second = commits
        .iter()
        .find(|c| c.subject == "second")
        .expect("second commit present")
        .id
        .clone();
    repo.rewrite_message(&second, "second (edited)").expect("rewrite");

    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history"); // [third, second (edited), first]
    let tip = repo.plan_drop(&commits, 0).expect("droppable"); // "third"
    repo.abandon_commit(&tip).expect("drop");

    // Sanity: git now sees the rewritten, shortened history.
    assert_eq!(
        common::git_log_subjects(dir),
        vec!["second (edited)", "first"]
    );

    // Revert the whole session.
    repo.revert_all().expect("revert all");

    // git sees exactly the original history again — same subjects AND the same
    // commit object at HEAD, proving a true revert rather than a fresh rewrite.
    assert_eq!(common::git_log_subjects(dir), original_subjects);
    assert_eq!(common::git(dir, &["rev-parse", "HEAD"]), original_head);

    // Transparency invariants hold after the revert.
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}

#[test]
fn revert_all_discards_session_working_copy_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_repo(
        dir,
        &[("a.txt", "a\n", "first"), ("b.txt", "b\n", "second")],
    );

    // Session starts with a clean working tree.
    let mut repo = Repo::open(dir).expect("open");

    // Make an uncommitted on-disk edit, then a history rewrite that snapshots it
    // into the working-copy commit and carries it through the rewrite.
    std::fs::write(dir.join("a.txt"), "a changed this session\n").unwrap();
    let commits = history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history");
    let first = commits
        .iter()
        .find(|c| c.subject == "first")
        .expect("first commit present")
        .id
        .clone();
    repo.rewrite_message(&first, "first (edited)").expect("rewrite");

    // The edit survived the rewrite (working-copy preservation).
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "a changed this session\n"
    );

    // Reverting to the session-start state discards the working-copy edit too:
    // the original content is checked back out and the tree is clean again.
    repo.revert_all().expect("revert all");
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "a\n");
    assert_eq!(common::git_log_subjects(dir), vec!["second", "first"]);
    assert_eq!(common::git(dir, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    common::git(dir, &["fsck", "--no-progress"]);
}
