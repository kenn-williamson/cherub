//! MCP (Model Context Protocol) server support.
//!
//! Spawns MCP server processes over stdio, discovers tools via `tools/list`,
//! and registers each as a `ToolImpl::Mcp` variant. All calls are routed
//! through the enforcement layer with `McpStructured` match source.

pub mod client;
pub mod config;
pub mod loader;
pub mod proxy;
