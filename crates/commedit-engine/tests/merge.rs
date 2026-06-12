//! End-to-end: editing history around (and on) *merge* commits. Message,
//! identity and (evil-merge) content edits must preserve both parents and the
//! merge topology; the structural operations — move, drop, restore, squash —
//! work anywhere in the graph and must keep a merge's 2-parent shape through
//! every rebase. Only the merge commit itself stays fixed: it is never a drag
//! source, though it is a valid squash destination. Each test asserts the
//! transparency triple and that no `.jjconflict-*` residue leaks into git.

mod common;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::diff::{commit_changes, ChangeKind};
use commedit_engine::graph::compute_graph;
use commedit_engine::history::{history, CommitInfo, ReorderCandidate};
use commedit_engine::repo::Repo;
use commedit_engine::rewrite::Identity;
use commedit_engine::squash::SquashMode;

/// Look up a commit in `commits` by its subject line.
fn by<'a>(commits: &'a [CommitInfo], subject: &str) -> &'a CommitInfo {
    commits
        .iter()
        .find(|c| c.subject == subject)
        .unwrap_or_else(|| panic!("{subject:?} commit present"))
}

/// The current history of `repo` (ancestors of HEAD, newest first).
fn current(repo: &Repo) -> Vec<CommitInfo> {
    history(&repo.repo, &repo.head_commit_id().expect("head")).expect("history")
}

/// The transparency triple every save must restore for a plain-git user, plus the
/// "no conflict residue in the tree" invariant.
fn assert_transparent(dir: &std::path::Path) {
    assert_eq!(
        common::git(dir, &["symbolic-ref", "HEAD"]),
        "refs/heads/main"
    );
    assert_eq!(common::git(dir, &["status", "--porcelain"]), "");
    let tree = common::git(dir, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(
        !tree.contains(".jjconflict"),
        "no .jjconflict-* in the tree: {tree}"
    );
    common::git(dir, &["fsck", "--no-progress"]);
}

/// Subjects of HEAD's parents (first-parent, then second-parent, …), sorted.
fn parent_subjects(dir: &std::path::Path) -> Vec<String> {
    let count = common::parent_count(dir, "HEAD");
    let mut subjects: Vec<String> = (1..=count)
        .map(|p| common::git(dir, &["log", "-1", "--format=%s", &format!("HEAD^{p}")]))
        .collect();
    subjects.sort();
    subjects
}

#[test]
fn message_edit_preserves_both_parents() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);

    let mut repo = Repo::open(dir).expect("open");
    let merge = by(&current(&repo), "merge").id.clone();
    repo.rewrite_message(&merge, "merge (edited)")
        .expect("rewrite message");

    // The tip is still the merge, now with the new subject and both parents.
    assert_eq!(
        common::git(dir, &["log", "-1", "--format=%s", "HEAD"]),
        "merge (edited)"
    );
    assert!(common::is_merge(dir, "HEAD"), "tip stays a 2-parent merge");
    assert_eq!(parent_subjects(dir), vec!["main-1", "side-1"]);
    assert_transparent(dir);
}

#[test]
fn identity_edit_preserves_both_parents() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);

    let mut repo = Repo::open(dir).expect("open");
    let merge = by(&current(&repo), "merge").id.clone();
    let id = Identity {
        author_name: "Ada Lovelace".to_string(),
        author_email: "ada@example.com".to_string(),
        author_time: "2026-06-05 14:30:00 +0200".to_string(),
        committer_name: "Grace Hopper".to_string(),
        committer_email: "grace@example.com".to_string(),
        committer_time: "2026-06-06 09:00:00 +0000".to_string(),
    };
    repo.rewrite_identity(&merge, &id)
        .expect("rewrite identity");

    // git sees the rewritten author/committer on the merge tip, both parents kept.
    let fmt = "%an|%ae|%ad|%cn|%ce|%cd";
    let line = common::git(
        dir,
        &[
            "show",
            "-s",
            &format!("--format={fmt}"),
            "--date=format:%Y-%m-%d %H:%M:%S %z",
            "HEAD",
        ],
    );
    let fields: Vec<&str> = line.split('|').collect();
    assert_eq!(fields[0], "Ada Lovelace");
    assert_eq!(fields[1], "ada@example.com");
    assert_eq!(fields[2], "2026-06-05 14:30:00 +0200");
    assert_eq!(fields[3], "Grace Hopper");
    assert_eq!(fields[4], "grace@example.com");
    assert_eq!(fields[5], "2026-06-06 09:00:00 +0000");
    assert!(common::is_merge(dir, "HEAD"), "tip stays a 2-parent merge");
    assert_eq!(parent_subjects(dir), vec!["main-1", "side-1"]);
    assert_transparent(dir);
}

#[test]
fn evil_merge_content_edit_keeps_both_parents() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_evil_merge_repo(dir);

    let mut repo = Repo::open(dir).expect("open");
    let merge = by(&current(&repo), "evil-merge").id.clone();

    // Edit the merge's remerge delta (the evil change to base.txt).
    repo.rewrite_file(&merge, "base.txt", "1\nEVIL-EDITED\n3\n")
        .expect("rewrite file");

    assert_eq!(
        common::git(dir, &["show", "HEAD:base.txt"]),
        "1\nEVIL-EDITED\n3"
    );
    assert!(common::is_merge(dir, "HEAD"), "tip stays a 2-parent merge");
    assert_eq!(parent_subjects(dir), vec!["main-1", "side-1"]);
    assert_transparent(dir);
}

#[test]
fn editing_a_non_merge_ancestor_keeps_the_merge_a_merge() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);

    let mut repo = Repo::open(dir).expect("open");
    // "side-1" is reachable only via the merge's *second* parent — the classic
    // rebase-topology case. Its change id is stable, so we can re-find the merge.
    let commits = current(&repo);
    let side = by(&commits, "side-1").id.clone();
    let merge_change = by(&commits, "merge").change_id_hex();
    repo.rewrite_message(&side, "side-1 (edited)")
        .expect("rewrite");

    // The merge survived the rebase as a 2-parent merge, now over the edited side.
    let after = current(&repo);
    let merge = after
        .iter()
        .find(|c| c.change_id_hex() == merge_change)
        .expect("merge still present");
    assert_eq!(
        merge.parents.len(),
        2,
        "merge keeps both parents after the rebase"
    );
    assert!(common::is_merge(dir, "HEAD"));
    assert_eq!(parent_subjects(dir), vec!["main-1", "side-1 (edited)"]);
    assert_transparent(dir);
}

#[test]
fn merge_survives_unrelated_rewrite() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);

    let mut repo = Repo::open(dir).expect("open");
    let commits = current(&repo);
    let main1 = by(&commits, "main-1").id.clone();
    let merge_change = by(&commits, "merge").change_id_hex();

    // Edit a first-parent mainline ancestor; the merge must stay reachable and a
    // merge (it is never abandoned — only rebased through the rewrite).
    repo.rewrite_message(&main1, "main-1 (edited)")
        .expect("rewrite");

    assert_eq!(
        common::git(dir, &["rev-list", "--merges", "--count", "HEAD"]),
        "1",
        "exactly one merge survives in the rewritten history"
    );
    assert!(
        current(&repo)
            .iter()
            .any(|c| c.change_id_hex() == merge_change && c.parents.len() == 2),
        "the merge is still reachable from the new tip with both parents"
    );
    assert_transparent(dir);
}

/// Reorder candidates for dragging display row `from` to gap `to`, planned
/// against the freshly computed lane layout — the way the UI calls it.
fn reorder_candidates(
    repo: &Repo,
    commits: &[CommitInfo],
    from: usize,
    to: usize,
) -> Vec<ReorderCandidate> {
    let layout = compute_graph(commits, &repo.root_commit_id());
    repo.plan_reorder_candidates(commits, &layout, from, to)
}

#[test]
fn moving_a_commit_out_of_the_merge_ancestry_keeps_the_merge() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);

    let mut repo = Repo::open(dir).expect("open");
    let commits = current(&repo);
    let from = commits.iter().position(|c| c.subject == "side-1").unwrap();

    // Drag side-1 out of the merge's second-parent line to the very top.
    let cands = reorder_candidates(&repo, &commits, from, 0);
    assert_eq!(cands.len(), 1, "the top gap has a single destination");
    let mv = cands[0].mv.clone();
    let outcome = repo
        .reorder_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
        .expect("reorder");
    assert!(matches!(outcome, SaveOutcome::Clean), "got {outcome:?}");

    // side-1 now tops the branch; the merge below kept both parents (its
    // emptied side line degenerates to the fork base rather than vanishing).
    assert_eq!(
        common::git(dir, &["log", "-1", "--format=%s", "HEAD"]),
        "side-1"
    );
    assert!(
        common::is_merge(dir, "HEAD~1"),
        "the merge keeps a 2-parent shape"
    );
    assert_eq!(common::git(dir, &["show", "HEAD:side.txt"]), "side");
    assert_transparent(dir);
}

#[test]
fn moving_a_commit_into_a_sibling_lane_threads_that_line() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);

    let mut repo = Repo::open(dir).expect("open");
    let commits = current(&repo);
    let from = commits.iter().position(|c| c.subject == "main-1").unwrap();
    let side = by(&commits, "side-1").id.clone();

    // The gap just below the merge is crossed by both of its parent lines, but
    // main-1's own line skips out as the no-op — leaving the side line, which
    // threads main-1 between the merge and side-1.
    let cands = reorder_candidates(&repo, &commits, from, 1);
    assert_eq!(cands.len(), 1, "only the sibling line remains a candidate");
    let mv = cands[0].mv.clone();
    assert_eq!(
        mv.new_parents,
        vec![side.clone()],
        "the side line is the destination"
    );
    let outcome = repo
        .reorder_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
        .expect("reorder");
    assert!(matches!(outcome, SaveOutcome::Clean), "got {outcome:?}");

    // The merge survives with two parents; main-1 now sits on the side line.
    assert!(common::is_merge(dir, "HEAD"));
    let main1_parent = common::git(dir, &["log", "-1", "--format=%s", "HEAD^2^"]);
    assert_eq!(
        common::git(dir, &["log", "-1", "--format=%s", "HEAD^2"]),
        "main-1"
    );
    assert_eq!(main1_parent, "side-1");
    assert_eq!(common::git(dir, &["show", "HEAD:main.txt"]), "main");
    assert_transparent(dir);
}

#[test]
fn restoring_a_dropped_commit_into_a_chosen_lane_rebuilds_the_side_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);

    let mut repo = Repo::open(dir).expect("open");
    let commits = current(&repo);
    let side = by(&commits, "side-1").clone();

    // Drop side-1 (by raw id — it sits inside the merge ancestry); the merge
    // degenerates onto the fork base but keeps two parents.
    let outcome = repo.abandon_commit(&side.id).expect("drop");
    assert!(matches!(outcome, SaveOutcome::Clean), "got {outcome:?}");
    assert!(common::is_merge(dir, "HEAD"));
    assert_eq!(parent_subjects(dir), vec!["base", "main-1"]);

    // Both surviving lines descend to base, so the gap above it offers two
    // lanes; restoring into the merge's (degenerated) second-parent line
    // rebuilds the original side branch exactly.
    let commits = current(&repo);
    let merge_id = by(&commits, "merge").id.clone();
    let to = commits.iter().position(|c| c.subject == "base").unwrap();
    let layout = compute_graph(&commits, &repo.root_commit_id());
    let cands = repo.plan_restore_candidates(&commits, &layout, &side, to);
    assert_eq!(
        cands.len(),
        2,
        "both lines into base cross the gap above it"
    );
    let mv = cands
        .iter()
        .map(|c| &c.mv)
        .find(|mv| mv.new_children == vec![merge_id.clone()])
        .expect("the merge's own line is one of the candidates")
        .clone();
    let outcome = repo
        .restore_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
        .expect("restore");
    assert!(matches!(outcome, SaveOutcome::Clean), "got {outcome:?}");

    // The original topology is back: a 2-parent merge over main-1 and side-1.
    assert!(common::is_merge(dir, "HEAD"));
    assert_eq!(parent_subjects(dir), vec!["main-1", "side-1"]);
    assert_eq!(common::git(dir, &["show", "HEAD:side.txt"]), "side");
    assert_transparent(dir);
}

#[test]
fn dropping_a_side_branch_commit_keeps_the_merge() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);

    let mut repo = Repo::open(dir).expect("open");
    let commits = current(&repo);
    let from = commits.iter().position(|c| c.subject == "side-1").unwrap();
    let target = repo
        .plan_drop(&commits, from)
        .expect("a side-branch commit is droppable");
    let outcome = repo.abandon_commit(&target).expect("drop");
    assert!(matches!(outcome, SaveOutcome::Clean), "got {outcome:?}");

    // side-1 is gone; the merge degenerates onto the fork base but keeps its
    // 2-parent shape, and side.txt left the tip's tree.
    assert!(common::is_merge(dir, "HEAD"));
    assert_eq!(parent_subjects(dir), vec!["base", "main-1"]);
    let tree = common::git(dir, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(
        !tree.contains("side.txt"),
        "side-1's change is gone: {tree}"
    );
    assert_transparent(dir);
}

#[test]
fn dropping_a_commit_below_the_merge_keeps_the_merge() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);

    let mut repo = Repo::open(dir).expect("open");
    let commits = current(&repo);
    let from = commits.iter().position(|c| c.subject == "base").unwrap();
    let target = repo
        .plan_drop(&commits, from)
        .expect("the fork base is droppable");
    let outcome = repo.abandon_commit(&target).expect("drop");
    assert!(matches!(outcome, SaveOutcome::Clean), "got {outcome:?}");

    // Both of the merge's lines re-root; the merge itself survives.
    assert!(common::is_merge(dir, "HEAD"));
    assert_eq!(parent_subjects(dir), vec!["main-1", "side-1"]);
    let tree = common::git(dir, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(!tree.contains("base.txt"), "base's change is gone: {tree}");
    assert_transparent(dir);
}

#[test]
fn the_merge_commit_itself_stays_fixed() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);
    // Put two linear commits on top of the merge, so the merge is *not* the
    // branch tip and has both ancestors and descendants to (not) move between.
    for (file, msg) in [("t1.txt", "tip-1"), ("t2.txt", "tip-2")] {
        std::fs::write(dir.join(file), "x\n").unwrap();
        common::git(dir, &["add", file]);
        common::git(dir, &["commit", "-q", "-m", msg]);
    }

    let repo = Repo::open(dir).expect("open");
    let commits = current(&repo);
    let pos = |s: &str| commits.iter().position(|c| c.subject == s).unwrap();
    let (merge, tip1, tip2) = (pos("merge"), pos("tip-1"), pos("tip-2"));

    // A merge node is never a drag source — dropping, moving or squashing it
    // away would dissolve the join point's shape…
    assert_eq!(
        repo.plan_drop(&commits, merge),
        None,
        "merge is not droppable"
    );
    assert!(
        reorder_candidates(&repo, &commits, merge, 0).is_empty(),
        "merge is not reorderable"
    );
    assert_eq!(
        repo.plan_squash(&commits, merge, tip1),
        None,
        "merge is not a squash source"
    );

    // …but it is a valid squash *destination* (an evil-merge style fold), and
    // the commits around it remain fully operable.
    assert!(
        repo.plan_squash(&commits, tip1, merge).is_some(),
        "merge is a squash target"
    );
    assert!(
        repo.plan_drop(&commits, tip1).is_some(),
        "a linear commit is droppable"
    );
    assert!(
        repo.plan_squash(&commits, tip2, tip1).is_some(),
        "linear commits squash"
    );
}

#[test]
fn squashing_across_merge_branches_lands_on_the_targets_line() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);

    let mut repo = Repo::open(dir).expect("open");
    let commits = current(&repo);
    let pos = |s: &str| commits.iter().position(|c| c.subject == s).unwrap();

    // Fold side-1 (second-parent line) into its cousin main-1 (first-parent
    // line): the change crosses the fork, landing on the target's line.
    let (source, dest) = repo
        .plan_squash(&commits, pos("side-1"), pos("main-1"))
        .expect("cousins on different sides squash");
    let outcome = repo
        .squash_into(&source, &dest, SquashMode::Fixup)
        .expect("squash");
    assert!(matches!(outcome, SaveOutcome::Clean), "got {outcome:?}");

    // main-1 now carries side.txt; the emptied side line degenerates onto the
    // fork base but the merge keeps its 2-parent shape.
    assert!(common::is_merge(dir, "HEAD"));
    assert_eq!(parent_subjects(dir), vec!["base", "main-1"]);
    let main1 = (1..=2)
        .map(|p| format!("HEAD^{p}"))
        .find(|r| common::git(dir, &["log", "-1", "--format=%s", r]) == "main-1")
        .expect("main-1 is a parent of the merge");
    assert_eq!(
        common::git(dir, &["show", &format!("{main1}:side.txt")]),
        "side"
    );
    assert_eq!(common::git(dir, &["show", "HEAD:side.txt"]), "side");
    assert_transparent(dir);
}

#[test]
fn squashing_into_a_merge_folds_the_change_into_its_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);
    // A commit above the merge whose change will fold *into* the merge.
    std::fs::write(dir.join("top.txt"), "top\n").unwrap();
    common::git(dir, &["add", "top.txt"]);
    common::git(dir, &["commit", "-q", "-m", "top"]);

    let mut repo = Repo::open(dir).expect("open");
    let commits = current(&repo);
    let pos = |s: &str| commits.iter().position(|c| c.subject == s).unwrap();

    let (source, dest) = repo
        .plan_squash(&commits, pos("top"), pos("merge"))
        .expect("the merge is a squash target");
    let outcome = repo
        .squash_into(&source, &dest, SquashMode::Fixup)
        .expect("squash");
    assert!(matches!(outcome, SaveOutcome::Clean), "got {outcome:?}");

    // The merge is the tip again, with both parents, now carrying top.txt as
    // its remerge delta — an evil merge by construction.
    assert!(common::is_merge(dir, "HEAD"));
    assert_eq!(
        common::git(dir, &["log", "-1", "--format=%s", "HEAD"]),
        "merge"
    );
    assert_eq!(parent_subjects(dir), vec!["main-1", "side-1"]);
    assert_eq!(common::git(dir, &["show", "HEAD:top.txt"]), "top");
    assert_transparent(dir);
}

#[test]
fn clean_merge_has_no_remerge_delta() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_merge_repo(dir);

    let repo = Repo::open(dir).expect("open");
    let merge = by(&current(&repo), "merge").id.clone();
    // A clean merge's tree equals the auto-merge of its parents — nothing to edit.
    let changes = commit_changes(&repo.repo, &merge).expect("changes");
    assert!(
        changes.is_empty(),
        "clean merge has an empty remerge delta: {changes:?}"
    );
}

#[test]
fn evil_merge_exposes_its_remerge_delta() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_evil_merge_repo(dir);

    let repo = Repo::open(dir).expect("open");
    let merge = by(&current(&repo), "evil-merge").id.clone();
    let changes = commit_changes(&repo.repo, &merge).expect("changes");

    // The merge's only delta vs. its (clean) auto-merged base is the evil edit.
    let base = changes
        .iter()
        .find(|c| c.path == "base.txt")
        .expect("base.txt delta");
    assert_eq!(base.kind, ChangeKind::Modified);
    assert_eq!(base.old_text.as_deref(), Some("1\n2\n3\n"));
    assert_eq!(base.new_text.as_deref(), Some("1\nEVIL\n3\n"));
    assert!(
        !base.conflicted_base,
        "a clean auto-merge has a resolvable base"
    );
}

#[test]
fn conflicted_merge_base_is_flagged_read_only() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    common::init_conflicted_merge_repo(dir);

    let repo = Repo::open(dir).expect("open");
    let merge = by(&current(&repo), "conflict-merge").id.clone();
    let changes = commit_changes(&repo.repo, &merge).expect("changes");

    // The parents disagree at base.txt, so the auto-merged base is conflicted:
    // there is no single old side, hence the file is flagged not-editable.
    let base = changes
        .iter()
        .find(|c| c.path == "base.txt")
        .expect("base.txt delta");
    assert!(
        base.conflicted_base,
        "a disagreeing merge base is flagged conflicted"
    );
    assert_eq!(base.new_text.as_deref(), Some("1\nRESOLVED\n3\n"));
}
