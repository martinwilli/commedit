//! The conflict-resolution loop: status while a rewrite is held back, reading
//! conflicted files, applying resolutions, and bailing out.

use commedit_engine::conflict::{ConflictEdit, FileResolution};
use commedit_engine::tree::ReplaceError;
use jj_lib::object_id::ObjectId as _;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, ErrorData};

use crate::convert::conflicted_commit_dto;
use crate::dto::{
    AbortResp, ConflictFileContentDto, PendingStatusResp, ReadConflictReq, ReadConflictResp,
    ResolveConflictsReq, SaveResultDto, SessionSel,
};
use crate::error::{internal, invalid};
use crate::server::CommeditServer;
use crate::session::{find_conflicted, save_result};
use crate::wrapper::Yaml;

#[tool_router(router = router_conflict, vis = "pub")]
impl CommeditServer {
    #[tool(
        description = "Whether a conflicted rewrite is pending. While pending, git still shows the pre-rewrite history (git_head_sha) and the held rewrite's tip is jj_head_sha; no other mutation is allowed until the conflicts resolve or the rewrite is aborted."
    )]
    pub async fn pending_status(
        &self,
        Parameters(req): Parameters<SessionSel>,
    ) -> Result<Yaml<PendingStatusResp>, ErrorData> {
        self.with_session(req.session, |repo, _| {
            Ok(PendingStatusResp {
                pending: repo.is_pending(),
                git_head_sha: repo.head_commit_id().map(|id| id.hex()),
                jj_head_sha: repo.jj_head_commit_id().map(|id| id.hex()),
                conflicts: repo
                    .pending_conflicts()
                    .unwrap_or(&[])
                    .iter()
                    .map(conflicted_commit_dto)
                    .collect(),
            })
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Read conflicted files of a pending rewrite, each materialized with git-style conflict markers. Pass a single `path`, several `paths`, or omit both to read every resolvable file of the commit in one call. Address the commit by change id or sha (full or a unique prefix); prefer the change id — shas churn on every resolution step. Resolve commits oldest-first — fixing the earliest often auto-clears its descendants. The response is files: [{ path, text, marker_len, num_sides }] (a stable schema): `text` carries the git-style conflict markers, `num_sides` is the number of conflicting sides (normally 2). To resolve, hand the file back to resolve_conflicts either as `edits` (old→new patches against this exact `text` — preferred) or as the full resolved `text` echoing its `marker_len`."
    )]
    pub async fn read_conflict(
        &self,
        Parameters(req): Parameters<ReadConflictReq>,
    ) -> Result<Yaml<ReadConflictResp>, ErrorData> {
        self.with_session(req.session.session.clone(), move |repo, _| {
            let conflicts = repo
                .pending_conflicts()
                .ok_or_else(|| invalid("no conflicted rewrite is pending"))?;
            let commit = &conflicts[find_conflicted(conflicts, &req.commit)?];
            let change_hex = commit.change_id_hex();

            // Resolve the target paths: explicit `path`/`paths`, or — when
            // neither is given — every resolvable file of the commit.
            let mut targets: Vec<String> = req.path.into_iter().collect();
            targets.extend(req.paths.unwrap_or_default());
            if targets.is_empty() {
                targets = commit
                    .files
                    .iter()
                    .filter(|f| f.resolvable)
                    .map(|f| f.path_str())
                    .collect();
            }

            let mut files = Vec::with_capacity(targets.len());
            for path in targets {
                let entry = commit
                    .files
                    .iter()
                    .find(|f| f.path_str() == path)
                    .ok_or_else(|| {
                        invalid(format!(
                            "{} is not a conflicted path of change {}; its conflicted files are: {}",
                            path,
                            change_hex,
                            commit
                                .files
                                .iter()
                                .map(|f| f.path_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))
                    })?;
                if !entry.resolvable {
                    return Err(invalid(format!(
                        "{path} is a structural conflict (not plain file content) and cannot be \
                         resolved as text; abort_rewrite is the only way out"
                    )));
                }
                let file = repo.read_conflict(&change_hex, &path).map_err(internal)?;
                files.push(ConflictFileContentDto {
                    path,
                    text: file.text,
                    marker_len: file.marker_len,
                    num_sides: file.num_sides,
                });
            }
            Ok(ReadConflictResp { files })
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Apply resolutions for one conflicted commit's files. Per file pick exactly one mode: `edits` — targeted old→new patches against the conflict-marker text read_conflict returned (each `old` must match exactly once unless replace_all), usually a single edit swapping the whole `<<<<<<< … >>>>>>>` block for the chosen lines; `text` — the complete resolved file with all markers removed, echoing its marker_len from read_conflict; or delete=true to remove the file. PREFER edits over full text for anything but a tiny file: it sends only the delta (cheaper) and can't corrupt content you never touched while reconstructing it. A deletion is how a modify/delete conflict settles (e.g. a revert that drops a file), and it also works on structural (resolvable=false) paths. Re-rebases the chain: the result is either still-conflicted (continue with the remaining commits) or clean — at which point the whole held-back rewrite is exported to git."
    )]
    pub async fn resolve_conflicts(
        &self,
        Parameters(req): Parameters<ResolveConflictsReq>,
    ) -> Result<Yaml<SaveResultDto>, ErrorData> {
        self.with_session(req.session.session.clone(), move |repo, trash| {
            if !repo.is_pending() {
                return Err(invalid("no conflicted rewrite is pending"));
            }
            if req.files.is_empty() {
                return Err(invalid("files must not be empty"));
            }
            let conflicts = repo.pending_conflicts().unwrap_or(&[]);
            let commit = &conflicts[find_conflicted(conflicts, &req.commit)?];
            let change_hex = commit.change_id_hex();
            let conflicted_paths: Vec<String> = commit.files.iter().map(|f| f.path_str()).collect();

            let mut files: Vec<(String, FileResolution)> = Vec::with_capacity(req.files.len());
            for f in req.files {
                let path = f.path;
                let delete = f.delete.unwrap_or(false);
                // An empty `edits` vec is an edits mode with nothing to do —
                // reject it explicitly rather than falling through to "no mode".
                if let Some(edits) = &f.edits {
                    if edits.is_empty() {
                        return Err(invalid(format!("the edits for {path} must not be empty")));
                    }
                }
                // Exactly one of full content / patch / delete per file.
                let modes = delete as u8 + f.edits.is_some() as u8 + f.text.is_some() as u8;
                if modes > 1 {
                    return Err(invalid(format!(
                        "{path}: pick exactly one of text (full content), edits (patch), or \
                         delete=true"
                    )));
                }
                if delete {
                    if !conflicted_paths.contains(&path) {
                        return Err(invalid(format!(
                            "{path} is not a conflicted path of this commit; cannot delete it. \
                             Its conflicted files are: {}",
                            conflicted_paths.join(", ")
                        )));
                    }
                    files.push((path, FileResolution::Delete));
                } else if let Some(edits) = f.edits {
                    for e in &edits {
                        if e.old.is_empty() {
                            return Err(invalid(format!("the edit for {path} has an empty `old`")));
                        }
                    }
                    let edits = edits
                        .into_iter()
                        .map(|e| ConflictEdit {
                            old: e.old,
                            new: e.new,
                            all: e.replace_all.unwrap_or(false),
                        })
                        .collect();
                    files.push((path, FileResolution::Patch { edits }));
                } else {
                    let (Some(text), Some(marker_len)) = (f.text, f.marker_len) else {
                        return Err(invalid(format!(
                            "{path}: provide text and marker_len to resolve with full content, \
                             edits to patch the conflict-marker text, or set delete=true to \
                             remove the file"
                        )));
                    };
                    files.push((path, FileResolution::Content { text, marker_len }));
                }
            }
            // A bad patch (missing / ambiguous `old`) is the caller's mistake,
            // fixable by amending the edit — surface it as invalid, not internal.
            let outcome = repo
                .resolve_conflicts_ext(&change_hex, &files)
                .map_err(|e| match e.downcast::<ReplaceError>() {
                    Ok(re) => invalid(re.to_string()),
                    Err(e) => internal(e),
                })?;
            trash.settle(&outcome);
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Discard the pending conflicted rewrite, rolling history back to before it. Git was never touched while it was held back, so the pre-rewrite state is simply still in place — making this the cheap way out when the conflict came from a mutation you just issued (fix the input and redo), as well as the only escape from a structural (resolvable=false) conflict that can't be resolved as text."
    )]
    pub async fn abort_rewrite(
        &self,
        Parameters(req): Parameters<SessionSel>,
    ) -> Result<Yaml<AbortResp>, ErrorData> {
        self.with_session(req.session, |repo, trash| {
            if !repo.is_pending() {
                return Err(invalid("no conflicted rewrite is pending"));
            }
            repo.abort().map_err(internal)?;
            // The aborted mutation's trash effect must not land.
            trash.staged = None;
            Ok(AbortResp {
                ok: true,
                head_sha: repo.head_commit_id().map(|id| id.hex()),
            })
        })
        .await
        .map(Yaml)
    }
}
