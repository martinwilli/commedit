//! Mapping engine and addressing failures onto MCP error responses.

use rmcp::ErrorData;

/// A caller mistake: unknown sha/change id, an impossible move, a refused
/// operation. The message is the agent's only feedback — say what was wrong
/// and, where possible, what would be valid instead.
pub fn invalid(msg: impl Into<String>) -> ErrorData {
    ErrorData::invalid_params(msg.into(), None)
}

/// An engine failure. `{:#}` keeps anyhow's whole context chain in one line.
pub fn internal(e: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(format!("{e:#}"), None)
}
