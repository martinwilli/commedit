//! Read-only tools over the history and the session trash.

use commedit_engine::diff::commit_changes;
use jj_lib::object_id::ObjectId as _;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, ErrorData};

use crate::convert::{commit_dto, file_change_dto};
use crate::dto::{
    ListHistoryReq, ListHistoryResp, ListTrashResp, ShowCommitReq, ShowCommitResp,
};
use crate::error::{internal, invalid};
use crate::server::CommeditServer;
use crate::session::{limited_history, resolve_ref, RefEntry};
use crate::wrapper::Yaml;

/// Default `list_history` page size when the caller gives no `limit`. Bounds the
/// response so an unbounded walk can't blow the tool's token budget; deeper
/// history is reachable via `limit` or `offset` paging.
const DEFAULT_HISTORY_LIMIT: usize = 30;

#[tool_router(router = router_read, vis = "pub")]
impl CommeditServer {
    #[tool(
        description = "List the commits of the checked-out branch (the ancestors of HEAD, newest first, like `git log`), with their branch/tag decorations. Returns up to `limit` commits (default 30) from `offset`; when `has_more`, page on with the returned `next_offset`. Pass brief=true for a compact overview (sha, change_id, subject, is_merge, refs only) of a long history, then show_commit for any one commit's full message and diff. Merge commits are included but cannot be moved, dropped, split or used as a squash source. Shas change on every mutation; every tool also accepts the stable change_id (or a unique >= 4-char prefix of either id), so prefer change ids over re-listing."
    )]
    pub async fn list_history(
        &self,
        Parameters(req): Parameters<ListHistoryReq>,
    ) -> Result<Yaml<ListHistoryResp>, ErrorData> {
        self.with_session(move |repo, trash| {
            let trash_count = trash.entries.len();
            let Some(_) = repo.head_commit_id() else {
                return Ok(ListHistoryResp {
                    head_sha: None,
                    commits: Vec::new(),
                    has_more: false,
                    offset: 0,
                    next_offset: None,
                    trash_count,
                });
            };
            let offset = req.offset.unwrap_or(0);
            let (head, commits, has_more) =
                limited_history(repo, offset, req.limit.unwrap_or(DEFAULT_HISTORY_LIMIT))?;
            let refs = repo.commit_refs();
            let root = repo.root_commit_id().hex();
            let brief = req.brief.unwrap_or(false);
            let next_offset = has_more.then_some(offset + commits.len());
            Ok(ListHistoryResp {
                head_sha: Some(head.hex()),
                commits: commits
                    .iter()
                    .map(|c| {
                        let mut dto = commit_dto(c, &root, &refs);
                        if brief {
                            dto.detail = None;
                        }
                        dto
                    })
                    .collect(),
                has_more,
                offset,
                next_offset,
                trash_count,
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
            let include = req.include_contents.unwrap_or(false);
            let files = commit_changes(&repo.repo, &info.id)
                .map_err(internal)?
                .iter()
                .map(|fc| file_change_dto(fc, include))
                .collect();
            Ok(ShowCommitResp { commit: commit_dto(&info, &root, &refs), files })
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
            Ok(ListTrashResp {
                commits: trash.entries.iter().map(|c| commit_dto(c, &root, &refs)).collect(),
            })
        })
        .await
        .map(Yaml)
    }
}
