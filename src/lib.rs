//! # turbobench
//!
//! MCP server benchmarking proxy — measure latency, token usage, and compare
//! MCP servers. Sits transparently between client and server, instrumenting
//! every JSON-RPC call.

/// Core benchmarking proxy built on `turbomcp-proxy`.
pub mod bench;
/// Compare two saved benchmark reports.
pub mod compare;
/// Configuration types and TOML config file parsing.
pub mod config;
/// Metric collection, aggregation, and report types.
pub mod metrics;
/// Terminal and JSON report output.
pub mod report;
/// Token estimation heuristics.
pub mod tokens;
