//! Vyer server library — the secure MCP front end for the code-context engine.
//!
//! Layering (Rule §8: async only at the edges; the core is sync and pure):
//!
//! * [`engine`] — transport-independent: index, search, fuse, pack, order,
//!   apply, freshness, audit, sandbox. Fully unit-testable.
//! * [`lexical`] — ripgrep-library lexical search over the warm core's text.
//! * [`apply`] — the deterministic, re-parse-validated splice.
//! * [`jsonrpc`] — a tiny, shared MCP JSON-RPC dispatch (drives the HTTP
//!   surface and the integration tests; one source of truth).
//! * [`mcp`] — the `rmcp` (official SDK) wrapper for the stdio transport.
//! * [`http`] — a localhost-only, bearer-token-gated HTTP transport.

pub mod apply;
pub mod engine;
pub mod http;
pub mod jsonrpc;
pub mod lexical;
pub mod mcp;
pub mod semantic;
pub mod watch;

pub use engine::{ApplyRequest, CodeRequest, Engine, EngineConfig, Query};

/// Server name + version advertised to MCP clients.
pub const SERVER_NAME: &str = "vyer";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PROTOCOL_VERSION: &str = "2025-06-18";
