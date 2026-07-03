//! Pure DTO conversion tests — response shapes, serde tags and mode defaults,
//! without a repository.

use std::collections::{BTreeMap, HashSet};

use commedit_engine::conflict::{ConflictedCommit, ConflictedPath, SaveOutcome};
use commedit_engine::diff::{ChangeKind, FileChange};
use commedit_engine::history::{CommitInfo, IdAbbrev};
use commedit_engine::squash::SquashMode;
use commedit_engine::transparency::{RefDecoration, RefKind};
use commedit_mcp::convert::{
    commit_dto, conflicted_commit_dto, file_change_dto, graph_adjacency, resolve_squash_mode,
    save_result_dto, topology_slice, DetailFields, CONFLICT_GUIDANCE,
};
use commedit_mcp::dto::{CommitField, SaveResultDto};
use jj_lib::backend::{ChangeId, CommitId};
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo_path::RepoPath;

fn ci(id: u8, parents: &[u8]) -> CommitInfo {
    CommitInfo {
        id: CommitId::new(vec![id]),
        change_id: ChangeId::new(vec![id]),
        subject: format!("subject {id}"),
        description: format!("subject {id}\n\nbody"),
        author_name: "Author".into(),
        author_email: "a@example.com".into(),
        committer_name: "Committer".into(),
        committer_email: "c@example.com".into(),
        author_time: "2026-01-01 10:00:00 +0100".into(),
        committer_time: "2026-01-02 10:00:00 +0100".into(),
        parents: parents.iter().map(|p| CommitId::new(vec![*p])).collect(),
    }
}

/// A commit with a DISTINCT commit-id and change-id (disjoint byte ranges), so a
/// `topology_slice` test catches any change-id/commit-id mix-up. `parents` name
/// the *commit-id* bytes of the parents (the graph edge keys).
fn node(commit: u8, change: u8, parents: &[u8]) -> CommitInfo {
    let mut c = ci(commit, parents);
    c.change_id = ChangeId::new(vec![change]);
    c.subject = format!("change {change}");
    c
}

fn change_hex(change: u8) -> String {
    ChangeId::new(vec![change]).hex()
}

/// The pre-mutation change_id set, as the handlers build it.
fn pre_set(commits: &[CommitInfo]) -> HashSet<String> {
    commits.iter().map(|c| c.change_id_hex()).collect()
}

#[test]
fn commit_dto_maps_fields_and_filters_the_root_parent() {
    let root_hex = CommitId::new(vec![0]).hex();
    let mut refs = BTreeMap::new();
    refs.insert(
        CommitId::new(vec![3]).hex(),
        vec![RefDecoration {
            name: "main".into(),
            kind: RefKind::Branch,
            current: true,
        }],
    );

    let dto = commit_dto(
        &ci(3, &[2]),
        &root_hex,
        &refs,
        &IdAbbrev::full(),
        DetailFields::ALL,
    );
    assert_eq!(dto.sha, CommitId::new(vec![3]).hex());
    assert_eq!(dto.change_id, ChangeId::new(vec![3]).hex());
    assert_eq!(dto.subject, "subject 3");
    assert_eq!(
        dto.detail.parent_shas.unwrap(),
        vec![CommitId::new(vec![2]).hex()]
    );
    assert!(!dto.is_merge);
    assert_eq!(dto.refs.len(), 1);
    assert_eq!(dto.refs[0].name, "main");
    assert_eq!(dto.refs[0].kind, "branch");
    assert!(dto.refs[0].current);

    // The oldest commit's parent is the virtual root: not a real commit.
    let oldest = commit_dto(
        &ci(1, &[0]),
        &root_hex,
        &BTreeMap::new(),
        &IdAbbrev::full(),
        DetailFields::ALL,
    );
    assert!(oldest.detail.parent_shas.unwrap().is_empty());
    assert!(oldest.refs.is_empty());

    let merge = commit_dto(
        &ci(4, &[3, 2]),
        &root_hex,
        &BTreeMap::new(),
        &IdAbbrev::full(),
        DetailFields::ALL,
    );
    assert!(merge.is_merge);
    assert_eq!(merge.detail.parent_shas.unwrap().len(), 2);
}

#[test]
fn commit_dto_includes_only_the_selected_fields() {
    let root_hex = CommitId::new(vec![0]).hex();

    // A header-only row: every verbose field omitted.
    let none = DetailFields::from_request(Some(&[]));
    let dto = commit_dto(
        &ci(3, &[2]),
        &root_hex,
        &BTreeMap::new(),
        &IdAbbrev::full(),
        none,
    );
    let d = &dto.detail;
    assert!(d.description.is_none() && d.author_time.is_none() && d.parent_shas.is_none());
    // The header is still populated.
    assert_eq!(dto.subject, "subject 3");

    // An explicit subset includes exactly those fields.
    let times =
        DetailFields::from_request(Some(&[CommitField::AuthorTime, CommitField::CommitterTime]));
    let dto = commit_dto(
        &ci(3, &[2]),
        &root_hex,
        &BTreeMap::new(),
        &IdAbbrev::full(),
        times,
    );
    let d = &dto.detail;
    assert_eq!(d.author_time.as_deref(), Some("2026-01-01 10:00:00 +0100"));
    assert_eq!(
        d.committer_time.as_deref(),
        Some("2026-01-02 10:00:00 +0100")
    );
    assert!(d.description.is_none() && d.author_name.is_none() && d.parent_shas.is_none());

    // An absent list (the `None` request) is a header-only overview — verbose
    // detail is opt-in, so the default carries none of it.
    let default = DetailFields::from_request(None);
    let dto = commit_dto(
        &ci(3, &[2]),
        &root_hex,
        &BTreeMap::new(),
        &IdAbbrev::full(),
        default,
    );
    assert!(dto.detail.description.is_none() && dto.detail.parent_shas.is_none());
    assert_eq!(dto.subject, "subject 3");
}

#[test]
fn file_change_dto_renders_a_diff_and_gates_contents() {
    let fc = FileChange {
        path: "src/a.txt".into(),
        kind: ChangeKind::Modified,
        old_text: Some("one\ntwo\n".into()),
        new_text: Some("one\nTWO\n".into()),
        is_binary: false,
        conflicted_base: false,
    };
    let without = file_change_dto(&fc, false);
    assert_eq!(without.kind, "modified");
    let diff = without.diff.expect("text file has a diff");
    assert!(diff.contains("-two"), "diff shows the removed line: {diff}");
    assert!(diff.contains("+TWO"), "diff shows the added line: {diff}");
    assert!(without.old_text.is_none() && without.new_text.is_none());

    let with = file_change_dto(&fc, true);
    assert_eq!(with.old_text.as_deref(), Some("one\ntwo\n"));
    assert_eq!(with.new_text.as_deref(), Some("one\nTWO\n"));
}

#[test]
fn binary_files_carry_no_diff_or_contents() {
    let fc = FileChange {
        path: "blob.bin".into(),
        kind: ChangeKind::Added,
        old_text: None,
        new_text: None,
        is_binary: true,
        conflicted_base: false,
    };
    let dto = file_change_dto(&fc, true);
    assert_eq!(dto.kind, "added");
    assert!(dto.is_binary);
    assert!(dto.diff.is_none());
    assert!(dto.old_text.is_none() && dto.new_text.is_none());
}

#[test]
fn save_result_serializes_with_a_status_tag() {
    let clean = save_result_dto(&SaveOutcome::Clean, Some("abc123".into()), None);
    let json = serde_json::to_value(&clean).unwrap();
    assert_eq!(json["status"], "clean");
    assert_eq!(json["head_sha"], "abc123");

    let conflicted = SaveOutcome::Conflicts {
        commits: vec![ConflictedCommit {
            change_id: ChangeId::new(vec![7]),
            commit_id: CommitId::new(vec![8]),
            subject: "subject 7".into(),
            files: vec![ConflictedPath {
                path: RepoPath::from_internal_string("a.txt").unwrap().to_owned(),
                resolvable: true,
            }],
        }],
    };
    let dto = save_result_dto(&conflicted, None, None);
    let json = serde_json::to_value(&dto).unwrap();
    assert_eq!(json["status"], "conflicts");
    assert_eq!(json["guidance"], CONFLICT_GUIDANCE);
    assert_eq!(
        json["commits"][0]["change_id"],
        ChangeId::new(vec![7]).hex()
    );
    assert_eq!(json["commits"][0]["files"][0]["path"], "a.txt");
    assert_eq!(json["commits"][0]["files"][0]["resolvable"], true);
    match dto {
        SaveResultDto::Conflicts { commits, .. } => assert_eq!(commits.len(), 1),
        SaveResultDto::Clean { .. } => panic!("expected conflicts"),
    }
}

#[test]
fn conflicted_commit_dto_keys_on_the_change_id() {
    let c = ConflictedCommit {
        change_id: ChangeId::new(vec![9]),
        commit_id: CommitId::new(vec![1]),
        subject: "s".into(),
        files: vec![ConflictedPath {
            path: RepoPath::from_internal_string("dir/f").unwrap().to_owned(),
            resolvable: false,
        }],
    };
    let dto = conflicted_commit_dto(&c);
    assert_eq!(dto.change_id, ChangeId::new(vec![9]).hex());
    assert_eq!(dto.sha, CommitId::new(vec![1]).hex());
    assert_eq!(dto.files[0].path, "dir/f");
    assert!(!dto.files[0].resolvable);
}

#[test]
fn squash_mode_resolution_prefers_explicit_then_prefix_then_fixup() {
    assert_eq!(
        resolve_squash_mode(Some("squash"), "fixup! x"),
        Ok(SquashMode::Squash)
    );
    assert_eq!(
        resolve_squash_mode(Some("amend"), "x"),
        Ok(SquashMode::Amend)
    );
    assert_eq!(
        resolve_squash_mode(Some("fixup"), "x"),
        Ok(SquashMode::Fixup)
    );
    assert!(resolve_squash_mode(Some("merge"), "x").is_err());
    assert_eq!(
        resolve_squash_mode(None, "squash! x"),
        Ok(SquashMode::Squash)
    );
    assert_eq!(resolve_squash_mode(None, "amend! x"), Ok(SquashMode::Amend));
    assert_eq!(
        resolve_squash_mode(None, "plain subject"),
        Ok(SquashMode::Fixup)
    );
}

#[test]
fn topology_slice_emits_anchor_adjacency_with_children_by_inversion() {
    // Linear chain, newest first: C -> B -> A (A's parent is the virtual root).
    let c = node(3, 103, &[2]);
    let b = node(2, 102, &[1]);
    let a = node(1, 101, &[0]);
    let commits = vec![c, b, a];
    let pre = pre_set(&commits);

    // Anchor the middle commit B (the "moved" commit).
    let topo = topology_slice(&commits, &[change_hex(102)], &pre, &IdAbbrev::full())
        .expect("an anchor yields a slice");
    assert_eq!(topo.affected.len(), 1, "only the anchor is affected");
    let adj = &topo.affected[0];
    assert_eq!(adj.change_id, change_hex(102));
    // B's parent is A and its child (derived by inverting parents) is C.
    assert_eq!(adj.parents, vec![change_hex(101)]);
    assert_eq!(adj.children, vec![change_hex(103)]);
    // A single-parent tip is not a merge.
    assert!(topo.merge_tip.is_none());

    // The root commit reports no parents (its parent is the virtual root).
    let topo = topology_slice(&commits, &[change_hex(101)], &pre, &IdAbbrev::full()).unwrap();
    assert!(topo.affected[0].parents.is_empty());
    assert_eq!(topo.affected[0].children, vec![change_hex(102)]);
}

#[test]
fn topology_slice_sets_merge_tip_and_dedups_it_when_affected() {
    // A merge M at the tip over two lanes: M(parents = first Y, second X).
    let m = node(5, 105, &[2, 4]);
    let x = node(4, 104, &[1]);
    let y = node(2, 102, &[1]);
    let z = node(1, 101, &[0]);
    let commits = vec![m, x, y, z];
    let pre = pre_set(&commits);

    // No anchor, nothing freshly minted: only the merge tip is reported — the
    // shape a linear history can't show.
    let topo = topology_slice(&commits, &[], &pre, &IdAbbrev::full())
        .expect("a merge tip yields a slice even with no affected commits");
    assert!(topo.affected.is_empty());
    let tip = topo.merge_tip.expect("the tip is a merge");
    assert_eq!(tip.change_id, change_hex(105));
    // Parents in the merge's own order: first Y, then X.
    assert_eq!(tip.parents, vec![change_hex(102), change_hex(104)]);

    // When the tip is itself affected, it appears in `affected` and is NOT
    // duplicated into `merge_tip`.
    let topo = topology_slice(&commits, &[change_hex(105)], &pre, &IdAbbrev::full()).unwrap();
    assert_eq!(topo.affected[0].change_id, change_hex(105));
    assert_eq!(topo.affected[0].parents.len(), 2);
    assert!(
        topo.merge_tip.is_none(),
        "the tip is deduped out of merge_tip"
    );
}

#[test]
fn topology_slice_finds_freshly_minted_and_returns_none_when_empty() {
    // A freshly created commit N (absent from the pre-mutation history) on top
    // of C is found via post − pre, with no anchor passed.
    let n = node(9, 109, &[3]);
    let c = node(3, 103, &[2]);
    let b = node(2, 102, &[1]);
    let commits = vec![n, c, b];
    // Pre-mutation history did not contain N.
    let pre: HashSet<String> = [change_hex(103), change_hex(102)].into_iter().collect();

    let topo = topology_slice(&commits, &[], &pre, &IdAbbrev::full())
        .expect("a post − pre commit is affected");
    assert_eq!(topo.affected.len(), 1);
    assert_eq!(topo.affected[0].change_id, change_hex(109));
    assert_eq!(topo.affected[0].parents, vec![change_hex(103)]);
    assert!(
        topo.affected[0].children.is_empty(),
        "the new tip has no child"
    );

    // Nothing anchored, nothing minted, a single-parent tip: no slice at all.
    let full_pre = pre_set(&commits);
    assert!(topology_slice(&commits, &[], &full_pre, &IdAbbrev::full()).is_none());
}

#[test]
fn graph_adjacency_emits_every_commit_with_parents_and_children() {
    // A merge M over two lanes joining at the root Z: M(105) -> [Y(102), X(104)],
    // X -> [Z(101)], Y -> [Z], Z -> virtual root.
    let m = node(5, 105, &[2, 4]);
    let x = node(4, 104, &[1]);
    let y = node(2, 102, &[1]);
    let z = node(1, 101, &[0]);
    let commits = vec![m, x, y, z];

    let graph = graph_adjacency(&commits, &IdAbbrev::full());
    assert_eq!(graph.len(), 4, "every commit appears, unfiltered");

    // The merge tip: both parents in its own order, no children.
    assert_eq!(graph[0].change_id, change_hex(105));
    assert_eq!(graph[0].parents, vec![change_hex(102), change_hex(104)]);
    assert!(graph[0].children.is_empty());

    // X and Y each converge on the merge.
    assert_eq!(graph[1].children, vec![change_hex(105)]);
    assert_eq!(graph[2].children, vec![change_hex(105)]);

    // The fork base Z: no parents (its parent is the virtual root), both lanes as
    // children in history order (X before Y, as they appear in the list).
    assert_eq!(graph[3].change_id, change_hex(101));
    assert!(graph[3].parents.is_empty());
    assert_eq!(graph[3].children, vec![change_hex(104), change_hex(102)]);
}
