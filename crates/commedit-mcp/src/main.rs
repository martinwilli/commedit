//! commedit-mcp — an MCP (Model Context Protocol) stdio server over the
//! commedit engine, exposing history editing to AI agents.
//!
//! Launched per repository: `commedit-mcp [path]` (default `.`, resolved like
//! the GTK app via the engine's `find_git_root`). The MCP client owns the
//! process lifecycle — one server instance is one editing session.

use std::path::PathBuf;

use anyhow::{Context, Result};
use rmcp::transport::stdio;
use rmcp::ServiceExt;

#[tokio::main]
async fn main() -> Result<()> {
    // stdout is the JSON-RPC channel; all diagnostics go to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let path = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".to_string()));

    // Repo::open does blocking git/jj work; keep it off the async runtime. Use the
    // index cache so repeated launches against the same repo skip rebuilding jj's
    // commit index from scratch (see `commedit_engine::index_cache`).
    let repo = tokio::task::spawn_blocking(move || {
        commedit_engine::repo::Repo::open_with_cache(
            &path,
            commedit_engine::index_cache::IndexCache::Default,
        )
    })
    .await
    .context("opening repository")??;
    tracing::info!(root = %repo.workspace_root().display(), "repository opened");

    let server = commedit_mcp::server::CommeditServer::new(repo);
    // Grab the repo handle before `serve` consumes the server, so the index cache
    // can be flushed at clean shutdown (the engine's `Drop` is the backstop).
    let repo = server.repo_handle();
    let service = server.serve(stdio()).await.context("starting MCP server")?;
    service.waiting().await.context("serving MCP")?;

    // Persist the (now up-to-date) jj index back to the cache. Blocking file IO,
    // so keep it off the async runtime.
    tokio::task::spawn_blocking(move || {
        repo.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .flush_index_cache();
    })
    .await
    .ok();
    Ok(())
}
