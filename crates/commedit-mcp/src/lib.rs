//! commedit-mcp — an MCP (Model Context Protocol) server over the commedit
//! engine, exposing history editing to AI agents.
//!
//! The crate is a library plus a thin stdio binary so the tool handlers can be
//! integration-tested directly (constructing [`server::CommeditServer`] against
//! a scratch repo and calling tool methods), without driving JSON-RPC.

pub mod convert;
pub mod dto;
pub mod error;
pub mod server;
pub mod session;
pub mod tools;
