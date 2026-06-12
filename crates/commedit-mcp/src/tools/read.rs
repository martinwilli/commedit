//! Read-only tools over the history and the session trash.

use commedit_engine::diff::commit_changes;
use commedit_engine::history::IdAbbrev;
use commedit_engine::squash::{parse_squash_mode, SquashMode};
use jj_lib::object_id::ObjectId as _;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, ErrorData};

use crate::convert::{commit_dto, file_change_dto, DetailFields};

/// Default `list_history` page size when the caller gives no `limit`. Bounds the
/// response so an unbounded walk can't blow the tool's token budget; deeper
/// history is reachable via `limit` or `offset` paging.
const DEFAULT_HISTORY_LIMIT: usize = 30;
use crate::dto::{
    ListHistoryReq, ListHistoryResp, ListTrashResp, ShowCommitReq, ShowCommitResp,
    SuggestSquashReq, SuggestSquashResp,
};
use crate::error::{internal, invalid};
use crate::server::CommeditServer;
use crate::session::{
    full_history, limited_history, lookup_ref, resolve_ref, working_copy_status_resp, RefEntry,
};
use crate::wrapper::Yaml;

#[tool_router(router = router_read, vis = "pub")]
impl CommeditServer {
    #[tool(
        description = "List the commits of the checked-out branch (the ancestors of HEAD, newest first, like `git log`), with their branch/tag decorations. Returns up to `limit` commits (default 30) from `offset`; when `has_more`, page on with the returned `next_offset`. Each commit always carries its header (sha, change_id, subject, is_merge, refs); use `fields` to pick which verbose fields (message, identity, parents) come with it — omit for all of them, pass a subset (e.g. just the timestamps when re-dating) or `[]` for a header-only overview, then show_commit for any one commit's full message and diff. Merge commits are included but cannot be moved, dropped, split or used as a squash source. Shas/change_ids are abbreviated to the shortest repo-unique prefix (>= 8 chars) — pass them straight back as a commit ref; shas change on every mutation while the change_id is stable, so prefer change ids over re-listing. Set `working_copy: true` to also get the uncommitted-changes status inline (same as working_copy_status), saving a round-trip."
    )]
    pub async fn list_history(
        &self,
        Parameters(req): Parameters<ListHistoryReq>,
    ) -> Result<Yaml<ListHistoryResp>, ErrorData> {
        self.with_session(move |repo, trash| {
            let trash_count = trash.entries.len();
            // Opt-in working-copy block (snapshots the disk), folded in to save a
            // separate working_copy_status round-trip.
            let working_copy = if req.working_copy.unwrap_or(false) {
                Some(working_copy_status_resp(repo)?)
            } else {
                None
            };
            let Some(_) = repo.head_commit_id() else {
                return Ok(ListHistoryResp {
                    head_sha: None,
                    commits: Vec::new(),
                    has_more: false,
                    offset: 0,
                    next_offset: None,
                    trash_count,
                    working_copy,
                });
            };
            let offset = req.offset.unwrap_or(0);
            let (head, commits, has_more) =
                limited_history(repo, offset, req.limit.unwrap_or(DEFAULT_HISTORY_LIMIT))?;
            let refs = repo.commit_refs();
            let root = repo.root_commit_id().hex();
            let abbrev = IdAbbrev::new(&repo.repo);
            let fields = DetailFields::from_request(req.fields.as_deref());
            let next_offset = has_more.then_some(offset + commits.len());
            Ok(ListHistoryResp {
                head_sha: Some(head.hex()),
                commits: commits
                    .iter()
                    .map(|c| commit_dto(c, &root, &refs, &abbrev, fields))
                    .collect(),
                has_more,
                offset,
                next_offset,
                trash_count,
                working_copy,
            })
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Show one commit's metadata and the files it changes, each as a unified diff. Accepts a history commit, a working-copy entry (the uncommitted diff) or a trashed commit — by sha or change id, full or a unique prefix. Set include_contents to also get each text file's full old/new content."
    )]
    pub async fn show_commit(
        &self,
        Parameters(req): Parameters<ShowCommitReq>,
    ) -> Result<Yaml<ShowCommitResp>, ErrorData> {
        self.with_session(move |repo, trash| {
            let (_, commits) = crate::session::full_history(repo)?;
            // One union in precedence order — history, working copy, trash —
            // so a ref present in several sets resolves to the history commit.
            let mut entries: Vec<RefEntry<_>> =
                commits.iter().map(|c| RefEntry::of(c, c.clone())).collect();
            entries.extend(
                repo.working_copy_chain()
                    .into_iter()
                    .map(|e| RefEntry::of(&e.info, e.info.clone())),
            );
            entries.extend(trash.entries.iter().map(|c| RefEntry::of(c, c.clone())));
            let info = resolve_ref(&req.commit, entries, || {
                invalid(format!(
                    "commit {} not found in the branch history, the working copy or the \
                     trash; use the stable change_id or call list_history for fresh refs",
                    req.commit
                ))
            })?;
            let refs = repo.commit_refs();
            let root = repo.root_commit_id().hex();
            let abbrev = IdAbbrev::new(&repo.repo);
            let include = req.include_contents.unwrap_or(false);
            let files = commit_changes(&repo.repo, &info.id)
                .map_err(internal)?
                .iter()
                .map(|fc| file_change_dto(fc, include))
                .collect();
            Ok(ShowCommitResp {
                commit: commit_dto(&info, &root, &refs, &abbrev, DetailFields::ALL),
                files,
            })
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "List the commits dropped to the session trash. They stay restorable (restore_commit, or squash_commit with a trashed source) until the session ends."
    )]
    pub async fn list_trash(&self) -> Result<Yaml<ListTrashResp>, ErrorData> {
        self.with_session(|repo, trash| {
            let refs = repo.commit_refs();
            let root = repo.root_commit_id().hex();
            let abbrev = IdAbbrev::new(&repo.repo);
            Ok(ListTrashResp {
                commits: trash
                    .entries
                    .iter()
                    .map(|c| commit_dto(c, &root, &refs, &abbrev, DetailFields::ALL))
                    .collect(),
            })
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Suggest where a fixup/squash/amend commit should be folded. Reads the source commit's leading `fixup!`/`squash!`/`amend!` subject token and returns the matching branch commit(s) as `targets` (pass one straight to squash_commit as `dest`), the `mode` that prefix requests, and any sibling autosquash commits aimed at the same target. Both lists are empty when the source carries no such prefix or nothing matches. The source may be a history or trashed commit and is never modified — this is read-only."
    )]
    pub async fn suggest_squash_targets(
        &self,
        Parameters(req): Parameters<SuggestSquashReq>,
    ) -> Result<Yaml<SuggestSquashResp>, ErrorData> {
        self.with_session(move |repo, trash| {
            let (_, commits) = full_history(repo)?;
            // Resolve the source: history first, then the trash (so a ref in both
            // means the live commit), and pick the matching recommendation walk.
            let src_entries = commits.iter().enumerate().map(|(i, c)| RefEntry::of(c, i));
            let (highlights, subject) =
                if let Some(idx) = lookup_ref(&req.source, src_entries.collect())? {
                    (
                        repo.squash_recommendations(&commits, idx),
                        commits[idx].subject.clone(),
                    )
                } else {
                    let trash_entries = trash.entries.iter().map(|c| RefEntry::of(c, c.clone()));
                    let info = resolve_ref(&req.source, trash_entries.collect(), || {
                        invalid(format!(
                            "source {} is neither in the branch history nor in the trash \
                             (see list_history / list_trash)",
                            req.source
                        ))
                    })?;
                    (
                        repo.squash_recommendations_for(&commits, &info),
                        info.subject.clone(),
                    )
                };
            let mode = parse_squash_mode(&subject).map(|m| {
                match m {
                    SquashMode::Fixup => "fixup",
                    SquashMode::Squash => "squash",
                    SquashMode::Amend => "amend",
                }
                .to_string()
            });
            let refs = repo.commit_refs();
            let root = repo.root_commit_id().hex();
            let abbrev = IdAbbrev::new(&repo.repo);
            let to_dtos = |idxs: &[usize]| -> Vec<_> {
                idxs.iter()
                    .map(|&i| commit_dto(&commits[i], &root, &refs, &abbrev, DetailFields::NONE))
                    .collect()
            };
            Ok(SuggestSquashResp {
                mode,
                targets: to_dtos(&highlights.targets),
                siblings: to_dtos(&highlights.siblings),
            })
        })
        .await
        .map(Yaml)
    }
}
