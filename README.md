# turbobench

MCP server benchmarking proxy — measure latency, token usage, and compare MCP servers.

turbobench sits between your MCP client (Claude Desktop, Claude Code, etc.) and any MCP server, transparently proxying all traffic while recording per-call metrics. Built on [turbomcp](https://crates.io/crates/turbomcp) with support for all MCP transports.

## Features

- **All transports** — stdio, HTTP/SSE, WebSocket, TCP, Unix sockets
- **Per-call instrumentation** — latency (P50/P95/P99), request/response bytes, estimated tokens
- **Per-tool breakdown** — see exactly which tools are expensive
- **Dual backend (shadow mode)** — A/B compare two MCP servers with the same request stream
- **Report comparison** — save JSON reports and compare across sessions
- **Zero config** — drop-in replacement in your MCP server config

## Install

```bash
cargo install turbobench
```

## Quick Start

### Benchmark a single MCP server

```bash
turbobench -- npx @anthropic-ai/playwright-mcp
```

### Save a report

```bash
turbobench -o playwright.json -- npx @anthropic-ai/playwright-mcp
```

### Compare two servers

Run each server in separate sessions, then compare:

```bash
turbobench -o server-a.json -- npx @anthropic-ai/playwright-mcp
# ... use Claude, perform some browsing ...

turbobench -o server-b.json -- /path/to/other-mcp-server
# ... repeat the same workflow ...

turbobench compare server-a.json server-b.json
```

### HTTP backend

```bash
turbobench --url http://localhost:3000
```

## Claude Desktop Integration

Add turbobench as a wrapper in your Claude Desktop config:

```json
{
  "mcpServers": {
    "playwright-bench": {
      "command": "turbobench",
      "args": ["-o", "/tmp/playwright-bench.json", "--", "npx", "@anthropic-ai/playwright-mcp"]
    }
  }
}
```

When the session ends, turbobench prints a report to stderr and saves the JSON report to the specified file.

## Config File

For advanced setups (dual backend, non-stdio transports, HTTP frontend), use a TOML config:

```toml
[primary]
type = "stdio"
command = "npx"
args = ["@anthropic-ai/playwright-mcp"]
name = "playwright"

[shadow]
type = "stdio"
command = "/path/to/other-mcp-server"
name = "other-server"

[frontend]
type = "stdio"

[options]
output = "benchmark.json"
quiet = false
```

```bash
turbobench -c bench.toml
```

### Transport types

```toml
# Stdio (most common)
[primary]
type = "stdio"
command = "python"
args = ["server.py"]

# HTTP/SSE
[primary]
type = "http"
url = "http://localhost:3000"

# WebSocket
[primary]
type = "websocket"
url = "ws://localhost:8080"

# TCP
[primary]
type = "tcp"
host = "localhost"
port = 5000

# Unix socket (Unix only)
[primary]
type = "unix"
path = "/tmp/mcp.sock"
```

## Report Output

turbobench prints a summary to stderr when the session ends:

```
=== TurboBench Report ===
  Session:  a1b2c3d4
  Duration: 45.2s
  Records:  42

--- playwright ---
┌──────────────────────┬─────────────────────┐
│ Metric               │ Value               │
├──────────────────────┼─────────────────────┤
│ Total calls          │ 42                  │
│ Tool calls           │ 35                  │
│ Success rate         │ 97.1%               │
│ Total bytes (in/out) │ 16.4 KB / 33.0 KB   │
│ Est. tokens (in/out) │ ~4,100 / ~8,250     │
│ Est. total tokens    │ ~12,350             │
│ Latency (mean/p50/…) │ 234 / 198 / 456 ms  │
└──────────────────────┴─────────────────────┘

┌──────────┬───────┬─────┬────────────┬─────────────┬───────┬───────┬───────┐
│ Tool     │ Calls │ OK% │ Tokens(in) │ Tokens(out) │ P50ms │ P95ms │ P99ms │
├──────────┼───────┼─────┼────────────┼─────────────┼───────┼───────┼───────┤
│ click    │ 15    │ 100%│ ~800       │ ~300        │ 45    │ 89    │ 234   │
│ navigate │ 8     │ 100%│ ~600       │ ~2,400      │ 345   │ 890   │ 1200  │
│ type     │ 5     │ 100%│ ~350       │ ~200        │ 34    │ 78    │ 123   │
└──────────┴───────┴─────┴────────────┴─────────────┴───────┴───────┴───────┘
```

## Token Estimation

Token counts are estimated at ~4 bytes per token, which is a reasonable heuristic for JSON/English text with Claude models. For exact counts, use Anthropic's [`/v1/messages/count_tokens`](https://docs.anthropic.com/en/api/messages-count-tokens) API on the saved report data.

## CLI Reference

```
turbobench [OPTIONS] [-- <BACKEND>...]
turbobench compare <REPORT_A> <REPORT_B>

Options:
  -n, --name <NAME>        Backend name (for reports)
  -o, --output <PATH>      Save report to JSON file
  -q, --quiet              Suppress report output on stderr
  -c, --config <PATH>      Config file (TOML)
      --url <URL>          Backend HTTP URL (instead of stdio command)
      --frontend <TYPE>    Frontend type: stdio (default) or http
      --bind <ADDR>        Frontend bind address [default: 127.0.0.1:3000]
  -h, --help               Print help
  -V, --version            Print version
```

## License

MIT
