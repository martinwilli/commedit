//! The tool surface, split by topic. Each module contributes a named router
//! (`#[tool_router(router = ...)]`), combined in `server.rs`.

pub mod conflict;
pub mod ops;
pub mod read;
pub mod workcopy;
