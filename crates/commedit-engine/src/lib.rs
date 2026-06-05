//! commedit-engine — the history rewrite engine.
//!
//! All interaction with jujutsu (`jj-lib`) lives here. The crate has no GTK
//! dependency so it can be unit-tested headless against scratch git repos.

pub mod history;
pub mod repo;
pub mod transparency;
