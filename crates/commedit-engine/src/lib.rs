//! commedit-engine — the history rewrite engine.
//!
//! All interaction with jujutsu (`jj-lib`) lives here. The crate has no GTK
//! dependency so it can be unit-tested headless against scratch git repos.

pub mod absorb;
pub mod blame;
pub mod cli;
pub mod conflict;
pub mod create;
pub mod diff;
pub mod graph;
pub mod history;
pub mod index_cache;
pub mod message;
pub mod patch_edit;
pub mod replay;
pub mod repo;
pub mod rewrite;
pub mod split;
pub mod squash;
pub mod tabwidth;
pub mod transparency;
pub mod tree;
pub mod workcopy;

/// jj-lib's commit/change identities, re-exported so GTK/MCP consumers can name
/// the types carried by [`history::CommitInfo`], [`history::ReorderMove`] et al.
/// without taking a direct `jj-lib` dependency (the crate's whole point is to be
/// the only thing that does). `ChangeId` is used mainly by the GTK pure-logic
/// unit tests that build scratch [`history::CommitInfo`]s.
pub use jj_lib::backend::{ChangeId, CommitId};
