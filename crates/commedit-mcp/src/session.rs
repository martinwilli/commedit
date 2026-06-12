//! Session state and shared plumbing of the tool handlers: the blocking-work
//! wrapper, commit-ref addressing against a fresh history read, the session
//! trash, and the reorder/restore splice planner.

use std::collections::HashSet;
use std::sync::PoisonError;

use commedit_engine::conflict::{ConflictedCommit, SaveOutcome};
use commedit_engine::graph::compute_graph;
use commedit_engine::history::{history, history_limited, CommitInfo, ReorderMove};
use commedit_engine::repo::Repo;
use commedit_engine::rewrite::Identity;
use jj_lib::backend::{ChangeId, CommitId};
use jj_lib::object_id::ObjectId as _;
use rmcp::ErrorData;

use crate::dto::IdentityFieldsDto;
use crate::error::{internal, invalid};
use crate::server::CommeditServer;

/// Dropped commits of this session, restorable until the server exits, plus
/// the trash mutation staged by an in-flight drop/restore/squash-from-trash.
///
/// The staged op mirrors the GTK side's `PendingTrashOp`: a mutation's effect
/// on the trash must not land while its rewrite is held back conflicted — only
/// [`TrashState::settle`] on a `Clean` outcome applies it, and an abort or
/// time-travel discards it.
#[derive(Default)]
pub struct TrashState {
    pub entries: Vec<CommitInfo>,
    pub staged: Option<PendingTrashOp>,
}

/// A trash mutation waiting for its rewrite to settle clean.
pub enum PendingTrashOp {
    /// `drop_commit`: push the dropped commit into the trash.
    Push(Box<CommitInfo>),
    /// `restore_commit` / squash-from-trash: remove the re-used commit.
    Remove(CommitId),
}

impl TrashState {
    /// Apply the staged trash op if `outcome` landed clean; keep it staged
    /// while conflicts hold the rewrite back.
    pub fn settle(&mut self, outcome: &SaveOutcome) {
        if matches!(outcome, SaveOutcome::Conflicts { .. }) {
            return;
        }
        match self.staged.take() {
            Some(PendingTrashOp::Push(info)) => self.entries.push(*info),
            Some(PendingTrashOp::Remove(id)) => self.entries.retain(|c| c.id != id),
            None => {}
        }
    }
}

impl CommeditServer {
    /// Run `f` against the locked session on the blocking thread pool. The
    /// mutex serializes all tool work (single writer for free); it is taken
    /// inside the blocking task, never across an `.await`.
    pub(crate) async fn with_session<T, F>(&self, f: F) -> Result<T, ErrorData>
    where
        F: FnOnce(&mut Repo, &mut TrashState) -> Result<T, ErrorData> + Send + 'static,
        T: Send + 'static,
    {
        let repo = self.repo.clone();
        let trash = self.trash.clone();
        tokio::task::spawn_blocking(move || {
            let mut repo = repo.lock().unwrap_or_else(PoisonError::into_inner);
            let mut trash = trash.lock().unwrap_or_else(PoisonError::into_inner);
            f(&mut repo, &mut trash)
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("worker task failed: {e}"), None))?
    }
}

/// Refuse a mutation (or time-travel) while a conflicted rewrite is held: the
/// engine would silently drop the pending resolution; an explicit error
/// explaining the protocol is clearer for the agent.
pub fn ensure_not_pending(repo: &Repo) -> Result<(), ErrorData> {
    if repo.is_pending() {
        return Err(invalid(
            "a conflicted rewrite is pending: resolve it (pending_status, read_conflict, \
             resolve_conflicts) or abort_rewrite before any other operation",
        ));
    }
    Ok(())
}

/// The branch head plus the full ancestry walk. The full list (never a
/// truncated prefix) is what every planner runs on — the graph layout must
/// cover the whole history for the splice arithmetic to hold.
pub fn full_history(repo: &Repo) -> Result<(CommitId, Vec<CommitInfo>), ErrorData> {
    let head = head_commit(repo)?;
    let commits = history(&repo.repo, &head).map_err(internal)?;
    Ok((head, commits))
}

/// Like [`full_history`] but skip `offset` and cut to `limit` commits, for
/// `list_history`'s paging.
pub fn limited_history(
    repo: &Repo,
    offset: usize,
    limit: usize,
) -> Result<(CommitId, Vec<CommitInfo>, bool), ErrorData> {
    let head = head_commit(repo)?;
    let (commits, has_more) =
        history_limited(&repo.repo, &head, offset, limit).map_err(internal)?;
    Ok((head, commits, has_more))
}

fn head_commit(repo: &Repo) -> Result<CommitId, ErrorData> {
    repo.head_commit_id().ok_or_else(|| {
        invalid("the repository has no branch head (detached or unborn HEAD); commedit edits the checked-out branch")
    })
}

/// A mutation outcome as the response DTO, with the (possibly moved) branch
/// tip read back after the save.
pub fn save_result(repo: &Repo, outcome: &SaveOutcome) -> crate::dto::SaveResultDto {
    crate::convert::save_result_dto(outcome, repo.head_commit_id().map(|id| id.hex()))
}

/// One commit a flexible ref can resolve to: its two full lowercase-hex
/// identities, a subject for ambiguity listings, and the caller's payload.
pub struct RefEntry<T> {
    pub sha: String,
    pub change_id: String,
    pub subject: String,
    pub value: T,
}

impl<T> RefEntry<T> {
    pub fn of(c: &CommitInfo, value: T) -> Self {
        Self {
            sha: c.id_hex(),
            change_id: c.change_id_hex(),
            subject: c.subject.clone(),
            value,
        }
    }
}

/// Resolve a flexible commit ref — full sha (40 hex), full change id
/// (32 hex), or a case-insensitive unique prefix (>= 4 chars) of either —
/// against `entries`. `Ok(None)` means no match (the caller words the
/// contextual not-found error); `Err` means too short or ambiguous, where
/// typing-more-characters is the fix the message suggests.
pub fn lookup_ref<T>(input: &str, mut entries: Vec<RefEntry<T>>) -> Result<Option<T>, ErrorData> {
    let needle = input.to_ascii_lowercase();
    // Non-hex junk can never match; let the caller's contextual not-found
    // error name it (checked before the length rule, so junk isn't reported
    // as "too short").
    if needle.is_empty() || !needle.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    if needle.len() < 4 {
        return Err(invalid(format!(
            "commit ref \"{input}\" is too short: use at least 4 characters of a sha or \
             change id"
        )));
    }

    // The same commit can be listed twice (history and trash both hold it
    // after a drop + undo); keep the first occurrence — caller order is
    // precedence — so the duplicate can never make a ref ambiguous.
    let mut seen = HashSet::new();
    entries.retain(|e| seen.insert(e.sha.clone()));

    // A full-length id resolves exactly within its own namespace, so a full
    // change id that happens to prefix some sha never reads as ambiguous.
    let exact: Vec<usize> = match needle.len() {
        40 => index_of(&entries, |e| e.sha == needle),
        32 => index_of(&entries, |e| e.change_id == needle),
        _ => Vec::new(),
    };
    let matches = if exact.is_empty() {
        // The per-entry `||` makes a prefix matching one commit via *both*
        // its ids a single match.
        index_of(&entries, |e| {
            e.sha.starts_with(&needle) || e.change_id.starts_with(&needle)
        })
    } else {
        exact
    };

    match matches.as_slice() {
        [] => Ok(None),
        [idx] => Ok(Some(entries.swap_remove(*idx).value)),
        _ => {
            let listed: Vec<String> = matches
                .iter()
                .map(|&i| format!("{} ({})", entries[i].sha, entries[i].subject))
                .collect();
            Err(invalid(format!(
                "commit ref \"{input}\" is ambiguous; it matches: {} — use more characters \
                 or a full sha/change id",
                listed.join(", ")
            )))
        }
    }
}

fn index_of<T>(entries: &[RefEntry<T>], pred: impl Fn(&RefEntry<T>) -> bool) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| pred(e))
        .map(|(i, _)| i)
        .collect()
}

/// [`lookup_ref`] with a caller-supplied not-found error.
pub fn resolve_ref<T>(
    input: &str,
    entries: Vec<RefEntry<T>>,
    not_found: impl FnOnce() -> ErrorData,
) -> Result<T, ErrorData> {
    lookup_ref(input, entries)?.ok_or_else(not_found)
}

/// The display index of commit ref `r` in the (newest-first) history.
pub fn find_commit(commits: &[CommitInfo], r: &str) -> Result<usize, ErrorData> {
    let entries = commits
        .iter()
        .enumerate()
        .map(|(i, c)| RefEntry::of(c, i))
        .collect();
    resolve_ref(r, entries, || {
        invalid(format!(
            "commit {r} is not in the current branch history; shas change after every \
             mutation — use the stable change_id, or call list_history for fresh refs"
        ))
    })
}

/// The trash entry commit ref `r` resolves to.
pub fn find_trashed(trash: &TrashState, r: &str) -> Result<CommitInfo, ErrorData> {
    let entries = trash
        .entries
        .iter()
        .map(|c| RefEntry::of(c, c.clone()))
        .collect();
    resolve_ref(r, entries, || {
        invalid(format!(
            "commit {r} is not in the session trash (see list_trash)"
        ))
    })
}

/// The index of the pending conflicted commit ref `r` resolves to.
pub fn find_conflicted(conflicts: &[ConflictedCommit], r: &str) -> Result<usize, ErrorData> {
    let entries = conflicts
        .iter()
        .enumerate()
        .map(|(i, c)| RefEntry {
            sha: c.commit_id.hex(),
            change_id: c.change_id_hex(),
            subject: c.subject.clone(),
            value: i,
        })
        .collect();
    resolve_ref(r, entries, || {
        invalid(format!(
            "{r} does not match a pending conflicted commit (see pending_status)"
        ))
    })
}

/// What `plan_splice` is moving: a commit at a display index, a trashed commit
/// being grafted back, or a brand-new commit being inserted (no graph position).
pub enum SpliceTarget {
    InHistory(usize),
    Trashed(Box<CommitInfo>),
    New,
}

/// A placeholder [`CommitInfo`] for planning a brand-new commit's insertion. Its
/// id is empty, so it equals no real commit and is no commit's parent or child —
/// [`Repo::plan_restore_candidates`] then yields the lines crossing the gap with
/// none of the own-line no-op cases (a new commit has no line to drop back onto).
fn synthetic_new_commit() -> CommitInfo {
    CommitInfo {
        id: CommitId::new(Vec::new()),
        change_id: ChangeId::new(Vec::new()),
        subject: String::new(),
        description: String::new(),
        author_name: String::new(),
        author_email: String::new(),
        committer_name: String::new(),
        committer_email: String::new(),
        author_time: String::new(),
        committer_time: String::new(),
        parents: Vec::new(),
    }
}

/// Resolve optional author/committer request fields to an engine [`Identity`]
/// for a newly created commit, overlaying any supplied field on the repo's
/// git-configured default (at "now"). `None` when nothing was supplied, so the
/// engine applies jj's own defaults.
pub fn new_commit_identity(repo: &Repo, fields: IdentityFieldsDto) -> Option<Identity> {
    let IdentityFieldsDto {
        author_name,
        author_email,
        author_time,
        committer_name,
        committer_email,
        committer_time,
    } = fields;
    if author_name.is_none()
        && author_email.is_none()
        && author_time.is_none()
        && committer_name.is_none()
        && committer_email.is_none()
        && committer_time.is_none()
    {
        return None;
    }
    let base = repo.default_identity();
    Some(Identity {
        author_name: author_name.unwrap_or(base.author_name),
        author_email: author_email.unwrap_or(base.author_email),
        author_time: author_time.unwrap_or(base.author_time),
        committer_name: committer_name.unwrap_or(base.committer_name),
        committer_email: committer_email.unwrap_or(base.committer_email),
        committer_time: committer_time.unwrap_or(base.committer_time),
    })
}

/// Resolve the agent-facing move semantics — "make `new_parent` the
/// parent of the moved commit" (`"root"` = make it the repository's first
/// commit) — to the one concrete splice the graph planner offers, or a precise
/// error naming the alternatives.
pub fn plan_splice(
    repo: &Repo,
    commits: &[CommitInfo],
    target: SpliceTarget,
    new_parent: &str,
    child: Option<&str>,
) -> Result<ReorderMove, ErrorData> {
    let layout = compute_graph(commits, &repo.root_commit_id());

    // The insertion gap directly above the requested parent: gap i sits
    // between display rows i-1 and i, so the gap whose lower neighbor is the
    // parent P at index i is gap i; "root" is the synthetic gap below the
    // oldest row (commits.len()).
    let (to, parent_id) = if new_parent == "root" {
        (commits.len(), repo.root_commit_id())
    } else {
        let idx = find_commit(commits, new_parent).map_err(|_| {
            invalid(format!(
                "new_parent {new_parent} is not in the current branch history; \
                 use a ref from list_history or the literal \"root\""
            ))
        })?;
        (idx, commits[idx].id.clone())
    };

    let candidates = match &target {
        SpliceTarget::InHistory(from) => {
            let moved = &commits[*from];
            if moved.id == parent_id {
                return Err(invalid("a commit cannot become its own parent"));
            }
            if moved.parents.len() != 1 {
                return Err(invalid("merge commits cannot be moved"));
            }
            if moved.parents[0] == parent_id {
                return Err(invalid(format!(
                    "commit {} is already a child of {new_parent}",
                    moved.id_hex()
                )));
            }
            repo.plan_reorder_candidates(commits, &layout, *from, to)
        }
        SpliceTarget::Trashed(info) => {
            if info.id == parent_id {
                return Err(invalid("a commit cannot become its own parent"));
            }
            repo.plan_restore_candidates(commits, &layout, info, to)
        }
        SpliceTarget::New => {
            // A brand-new commit has no graph position; plan it like a restore
            // of a commit absent from the history (the engine computes the tip
            // itself, so only new_parents/new_children of the result are used).
            let synthetic = synthetic_new_commit();
            repo.plan_restore_candidates(commits, &layout, &synthetic, to)
        }
    };

    let mut matching: Vec<ReorderMove> = candidates
        .iter()
        .filter(|c| c.mv.new_parents == [parent_id.clone()])
        .map(|c| c.mv.clone())
        .collect();

    match matching.len() {
        0 => {
            let offered: Vec<String> = candidates
                .iter()
                .flat_map(|c| c.mv.new_parents.iter().map(|p| p.hex()))
                .collect();
            if offered.is_empty() {
                Err(invalid(format!(
                    "no way to splice the commit under {new_parent}: the move is a no-op \
                     or the target is not reachable from the branch head"
                )))
            } else {
                Err(invalid(format!(
                    "no ancestry line at that position leads to parent {new_parent}; \
                     the gap's candidate parents are: {}",
                    offered.join(", ")
                )))
            }
        }
        1 => Ok(matching.remove(0)),
        _ => {
            // A fork: several child lines converge on the parent. The caller
            // picks which child the moved commit goes under.
            if let Some(child) = child {
                // Every candidate child is a history commit (the only
                // childless candidate is the top gap, never named here), so
                // the ref resolves against the history slice.
                let child_id = commits[find_commit(commits, child)?].id.clone();
                matching
                    .into_iter()
                    .find(|mv| mv.new_children.contains(&child_id))
                    .ok_or_else(|| {
                        invalid(format!(
                            "child {child} is not a child on any line converging on \
                             {new_parent}"
                        ))
                    })
            } else {
                let choices: Vec<String> = matching
                    .iter()
                    .map(|mv| {
                        let names: Vec<String> = mv
                            .new_children
                            .iter()
                            .map(|c| describe_commit(commits, c))
                            .collect();
                        names.join(" + ")
                    })
                    .collect();
                Err(invalid(format!(
                    "several lines converge on {new_parent}; pass child to pick which \
                     child the commit goes under: {}",
                    choices.join(" | ")
                )))
            }
        }
    }
}

fn describe_commit(commits: &[CommitInfo], id: &CommitId) -> String {
    match commits.iter().find(|c| c.id == *id) {
        Some(c) => format!("{} ({})", c.id_hex(), c.subject),
        None => id.hex(),
    }
}

#[cfg(test)]
mod tests {
    use super::{lookup_ref, RefEntry};

    fn entry(sha: &str, change_id: &str, tag: u32) -> RefEntry<u32> {
        RefEntry {
            sha: sha.into(),
            change_id: change_id.into(),
            subject: format!("subject-{tag}"),
            value: tag,
        }
    }

    fn sha(prefix: &str) -> String {
        format!("{prefix}{}", "0".repeat(40 - prefix.len()))
    }

    fn cid(prefix: &str) -> String {
        format!("{prefix}{}", "f".repeat(32 - prefix.len()))
    }

    #[test]
    fn a_full_sha_matches_exactly() {
        let entries = vec![
            entry(&sha("aa"), &cid("11"), 1),
            entry(&sha("bb"), &cid("22"), 2),
        ];
        assert_eq!(lookup_ref(&sha("aa"), entries).unwrap(), Some(1));
    }

    #[test]
    fn a_full_change_id_beats_a_colliding_sha_prefix() {
        // Entry 2's sha starts with entry 1's full change id: the 32-hex
        // input must resolve in the change-id namespace, not ambiguously.
        let full_cid = cid("11");
        let colliding_sha = format!("{full_cid}{}", "0".repeat(8));
        let entries = vec![
            entry(&sha("aa"), &full_cid, 1),
            entry(&colliding_sha, &cid("22"), 2),
        ];
        assert_eq!(lookup_ref(&full_cid, entries).unwrap(), Some(1));
    }

    #[test]
    fn a_prefix_matches_in_either_namespace() {
        let entries = vec![
            entry(&sha("abcd12"), &cid("9911"), 1),
            entry(&sha("ff00"), &cid("8822"), 2),
        ];
        assert_eq!(lookup_ref("abcd", entries).unwrap(), Some(1));
        let entries = vec![
            entry(&sha("abcd12"), &cid("9911"), 1),
            entry(&sha("ff00"), &cid("8822"), 2),
        ];
        assert_eq!(lookup_ref("9911", entries).unwrap(), Some(1));
    }

    #[test]
    fn a_prefix_hitting_both_ids_of_one_commit_is_a_single_match() {
        let entries = vec![
            entry(&sha("abcd12"), &cid("abcd34"), 1),
            entry(&sha("ff00"), &cid("8822"), 2),
        ];
        assert_eq!(lookup_ref("abcd", entries).unwrap(), Some(1));
    }

    #[test]
    fn a_shared_prefix_is_ambiguous_and_lists_the_matches() {
        let entries = vec![
            entry(&sha("abcd12"), &cid("1111"), 1),
            entry(&sha("abcd34"), &cid("2222"), 2),
        ];
        let err = lookup_ref("abcd", entries).unwrap_err();
        assert!(
            err.message.contains("ambiguous"),
            "message: {}",
            err.message
        );
        assert!(err.message.contains("subject-1") && err.message.contains("subject-2"));
    }

    #[test]
    fn input_is_case_insensitive() {
        let entries = vec![entry(&sha("abcd12"), &cid("1111"), 1)];
        assert_eq!(lookup_ref("ABCD12", entries).unwrap(), Some(1));
    }

    #[test]
    fn a_short_hex_ref_is_rejected() {
        let entries = vec![entry(&sha("abcd12"), &cid("1111"), 1)];
        let err = lookup_ref("abc", entries).unwrap_err();
        assert!(
            err.message.contains("too short"),
            "message: {}",
            err.message
        );
    }

    #[test]
    fn non_hex_input_is_not_found_rather_than_too_short() {
        let entries = vec![entry(&sha("abcd12"), &cid("1111"), 1)];
        assert_eq!(lookup_ref("no", entries).unwrap(), None);
        let entries = vec![entry(&sha("abcd12"), &cid("1111"), 1)];
        assert_eq!(lookup_ref("not-a-ref", entries).unwrap(), None);
    }

    #[test]
    fn a_duplicate_sha_dedupes_to_the_first_entry() {
        // The same commit listed twice (history first, trash second) must
        // resolve to the first occurrence, never read as ambiguous.
        let entries = vec![
            entry(&sha("abcd12"), &cid("1111"), 1),
            entry(&sha("abcd12"), &cid("1111"), 2),
        ];
        assert_eq!(lookup_ref("abcd", entries).unwrap(), Some(1));
    }
}
