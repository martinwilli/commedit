//! Read-only tools over the history and the session trash.

use commedit_engine::diff::commit_changes;
use commedit_engine::history::IdAbbrev;
use commedit_engine::squash::{parse_squash_mode, SquashMode};
use jj_lib::object_id::ObjectId as _;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, ErrorData};

use crate::convert::{commit_dto, file_change_dto, graph_adjacency, DetailFields};

/// Default `list_history` page size when the caller gives no `limit`. Bounds the
/// response so an unbounded walk can't blow the tool's token budget; deeper
/// history is reachable via `limit` or `offset` paging.
const DEFAULT_HISTORY_LIMIT: usize = 30;
use crate::dto::{
    BlameCandidateDto, BlameSquashReq, BlameSquashResp, ListHistoryReq, ListHistoryResp,
    ListTrashResp, SessionSel, ShowCommitReq, ShowCommitResp, ShowGraphResp, SuggestSquashReq,
    SuggestSquashResp,
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
        description = "List the commits of the checked-out branch (the ancestors of HEAD, newest first, like `git log`), with their branch/tag decorations. Returns up to `limit` commits (default 30) from `offset`; when `has_more`, page on with the returned `next_offset`. Each commit always carries sha, change_id and subject; `is_merge` and `refs` appear only when set (a merge, a decorated commit) and are otherwise omitted (absent means a non-merge / no decoration). Use `fields` to pick which verbose fields (message, identity, parents) come with it — omit for all of them, pass a subset (e.g. just the timestamps when re-dating) or `[]` for a header-only overview, then show_commit for any one commit's full message and diff. Merge commits are included but cannot be moved, dropped, split or used as a squash source. Shas/change_ids are abbreviated to the shortest repo-unique prefix (>= 8 chars) — pass them straight back as a commit ref; shas change on every mutation while the change_id is stable, so prefer change ids over re-listing. Set `working_copy: true` to also get the uncommitted-changes status inline (same as working_copy_status), saving a round-trip."
    )]
    pub async fn list_history(
        &self,
        Parameters(req): Parameters<ListHistoryReq>,
    ) -> Result<Yaml<ListHistoryResp>, ErrorData> {
        self.with_session(req.session.session.clone(), move |repo, trash| {
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
        self.with_session(req.session.session.clone(), move |repo, trash| {
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
    pub async fn list_trash(
        &self,
        Parameters(req): Parameters<SessionSel>,
    ) -> Result<Yaml<ListTrashResp>, ErrorData> {
        self.with_session(req.session, |repo, trash| {
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
        description = "Show the commit graph of the checked-out branch: every commit reachable from HEAD (newest first), each with its parents AND children by change_id — the merge and side-branch structure at a glance, which the newest-first list_history can't convey on its own. This is the standalone, whole-branch read of the same `topology` shape a topology-changing mutation folds into its result. Reach for it to see how merges and side branches connect before reordering, merging out or restoring; list_history stays the source for per-commit detail (messages, identities, diffs, refs)."
    )]
    pub async fn show_graph(
        &self,
        Parameters(req): Parameters<SessionSel>,
    ) -> Result<Yaml<ShowGraphResp>, ErrorData> {
        self.with_session(req.session, |repo, _| {
            let Some(_) = repo.head_commit_id() else {
                return Ok(ShowGraphResp {
                    head_sha: None,
                    commits: Vec::new(),
                });
            };
            let (head, commits) = full_history(repo)?;
            let abbrev = IdAbbrev::new(&repo.repo);
            Ok(ShowGraphResp {
                head_sha: Some(head.hex()),
                commits: graph_adjacency(&commits, &abbrev),
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
        self.with_session(req.session.session.clone(), move |repo, trash| {
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

    #[tool(
        description = "Find where a change should be folded by content-blaming the lines it touches — the squash-target finder for the common case where you don't know which commit introduced the code you just fixed. Blames the lines the source removes/modifies against history and returns `candidates`: the branch commits that introduced them, ranked by how many of those lines each owns (`lines`). Pass the top candidate's change_id straight to squash_commit as `dest` — or to squash_working_copy for the default working-copy source. `source` is a history or working-copy commit by sha/change id; omit it to blame the working copy (all uncommitted changes), the default and primary case. `unattributed` counts changed lines tracing to a merge/boundary or an ancestor outside the listed history. Read-only. The content-blame complement to suggest_squash_targets, which instead routes an explicit `fixup!`/`squash!`/`amend!` subject prefix."
    )]
    pub async fn blame_squash_targets(
        &self,
        Parameters(req): Parameters<BlameSquashReq>,
    ) -> Result<Yaml<BlameSquashResp>, ErrorData> {
        self.with_session(req.session.session.clone(), move |repo, _| {
            let (_, commits) = full_history(repo)?;
            // Capture on-disk edits into @ so the working copy is a current blame
            // source (no-op off-worktree), like working_copy_status.
            repo.snapshot_working_copy().map_err(internal)?;
            let wc = repo.working_copy_chain();
            // Resolve the source: an explicit ref over history ∪ working copy, else
            // the working-copy leaf @ (all uncommitted changes when unsplit). A
            // clean tree with no explicit source has nothing to blame.
            let source = match &req.source {
                Some(r) => {
                    let mut entries: Vec<RefEntry<_>> =
                        commits.iter().map(|c| RefEntry::of(c, c.clone())).collect();
                    entries.extend(wc.iter().map(|e| RefEntry::of(&e.info, e.info.clone())));
                    resolve_ref(r, entries, || {
                        invalid(format!(
                            "source {r} not found in the branch history or the working copy \
                             (see list_history / working_copy_status)"
                        ))
                    })?
                }
                None => {
                    let Some(leaf) = wc.into_iter().next() else {
                        return Ok(BlameSquashResp {
                            mode: None,
                            candidates: Vec::new(),
                            unattributed: 0,
                        });
                    };
                    leaf.info
                }
            };
            let origins = repo.blame_change_origins(&source, &commits);
            let refs = repo.commit_refs();
            let root = repo.root_commit_id().hex();
            let abbrev = IdAbbrev::new(&repo.repo);
            let candidates = origins
                .candidates
                .iter()
                .map(|&(row, lines)| BlameCandidateDto {
                    commit: commit_dto(&commits[row], &root, &refs, &abbrev, DetailFields::NONE),
                    lines,
                })
                .collect();
            // Parity with suggest_squash_targets: surface any autosquash prefix the
            // source carries (a working-copy source has none).
            let mode = parse_squash_mode(&source.subject).map(|m| {
                match m {
                    SquashMode::Fixup => "fixup",
                    SquashMode::Squash => "squash",
                    SquashMode::Amend => "amend",
                }
                .to_string()
            });
            Ok(BlameSquashResp {
                mode,
                candidates,
                unattributed: origins.unattributed,
            })
        })
        .await
        .map(Yaml)
    }
}
