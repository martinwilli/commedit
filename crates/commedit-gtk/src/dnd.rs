//! The drag payload that crosses between commedit windows.
//!
//! Within one window the history drag carries its state in `Rc` cells; but a
//! drag onto *another* window (a separate process, see `main.rs`'s `NON_UNIQUE`
//! flag) must travel through the platform's drag-and-drop transport. We carry it
//! as plain text — the one content format GTK serializes across the process
//! boundary out of the box — holding the source process, the source repo's
//! object-store key (`commedit_engine::repo::Repo::object_store_key`), and the
//! dragged commits addressed by sha. The receiver compares the pid to tell its
//! own drag from a foreign one, and the repo key to tell a sibling-branch window
//! of the same repository (whose commit it can cherry-pick from the shared
//! object store) from a window on an unrelated repo.
//!
//! The format is the same hand-rolled, line-oriented `key=value` shape the rest
//! of the project uses (window state, the index-cache `META`); the leading magic
//! line lets the drop handler reject arbitrary text dragged in from other apps.

/// The marker (and version) on the first line of a serialized payload.
const MAGIC: &str = "commedit-dnd-v1";

/// One dragged commit, addressed by sha (stable across processes; change ids are
/// per-repo, so they ride along only for display/diagnostics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DraggedCommit {
    pub(crate) sha: String,
    pub(crate) change_id: String,
    pub(crate) subject: String,
}

/// A commit drag originating in some commedit window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DraggedCommits {
    /// The source process (`std::process::id()`), so a window can tell its own
    /// in-flight drag from one started in another window.
    pub(crate) pid: u32,
    /// The source repo's [`object_store_key`](commedit_engine::repo::Repo::object_store_key).
    pub(crate) repo_key: String,
    /// The source branch, for display only (`None` if unknown).
    pub(crate) branch: Option<String>,
    pub(crate) commits: Vec<DraggedCommit>,
}

impl DraggedCommits {
    /// Render to the text form carried by the drag's content provider.
    pub(crate) fn serialize(&self) -> String {
        let mut s = format!("{MAGIC}\npid={}\nrepo={}\n", self.pid, self.repo_key);
        if let Some(branch) = &self.branch {
            s.push_str(&format!("branch={branch}\n"));
        }
        for c in &self.commits {
            // sha and change id are hex; the subject is the rest of the line, so
            // flatten any stray newlines to keep it on one line.
            s.push_str(&format!(
                "commit {} {} {}\n",
                c.sha,
                c.change_id,
                c.subject.replace('\n', " ")
            ));
        }
        s
    }

    /// Parse the text form. `None` for anything that is not a commedit payload
    /// (e.g. plain text dragged from another application), so the drop handler
    /// can fall through to its other content types.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()?.trim() != MAGIC {
            return None;
        }
        let mut pid = None;
        let mut repo_key = None;
        let mut branch = None;
        let mut commits = Vec::new();
        for line in lines {
            if let Some(rest) = line.strip_prefix("commit ") {
                let mut parts = rest.splitn(3, ' ');
                let sha = parts.next().unwrap_or("");
                let change_id = parts.next().unwrap_or("");
                let subject = parts.next().unwrap_or("");
                if sha.is_empty() || change_id.is_empty() {
                    continue;
                }
                commits.push(DraggedCommit {
                    sha: sha.to_string(),
                    change_id: change_id.to_string(),
                    subject: subject.to_string(),
                });
            } else if let Some((k, v)) = line.split_once('=') {
                match k.trim() {
                    "pid" => pid = v.trim().parse().ok(),
                    "repo" => repo_key = Some(v.trim().to_string()),
                    "branch" => branch = Some(v.trim().to_string()),
                    _ => {}
                }
            }
        }
        Some(Self {
            pid: pid?,
            repo_key: repo_key?,
            branch,
            commits,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DraggedCommits {
        DraggedCommits {
            pid: 4242,
            repo_key: "abc123".to_string(),
            branch: Some("feature".to_string()),
            commits: vec![
                DraggedCommit {
                    sha: "deadbeef".to_string(),
                    change_id: "zzzz".to_string(),
                    subject: "Fix the thing with spaces".to_string(),
                },
                DraggedCommit {
                    sha: "cafef00d".to_string(),
                    change_id: "yyyy".to_string(),
                    subject: String::new(),
                },
            ],
        }
    }

    #[test]
    fn round_trips_through_text() {
        let p = sample();
        assert_eq!(DraggedCommits::parse(&p.serialize()), Some(p));
    }

    #[test]
    fn a_missing_branch_round_trips_as_none() {
        let mut p = sample();
        p.branch = None;
        let back = DraggedCommits::parse(&p.serialize()).unwrap();
        assert_eq!(back.branch, None);
        assert_eq!(back.commits.len(), 2);
    }

    #[test]
    fn rejects_non_commedit_text() {
        assert_eq!(DraggedCommits::parse("just some text\nmore"), None);
        assert_eq!(DraggedCommits::parse(""), None);
        // The magic but no required fields.
        assert_eq!(DraggedCommits::parse("commedit-dnd-v1\n"), None);
    }

    #[test]
    fn pid_distinguishes_self_from_foreign() {
        let back = DraggedCommits::parse(&sample().serialize()).unwrap();
        assert_eq!(back.pid, 4242);
        assert_ne!(back.pid, std::process::id());
    }
}
