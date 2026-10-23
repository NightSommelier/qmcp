# qmcp

MCP server for a running QueryMT ACP WebSocket server (`qmtcode --acp-ws`).
Built with Rust and `rmcp`, depending directly on the
[qmacp crate from GitHub](https://github.com/querymt/qmacp) for ACP communication,
permission handling, URL validation, event decoding, and session helpers.

`qmcp` bridges MCP to ACP; it does not launch QueryMT, require a `qmacp` binary,
or access QueryMT's session database. QueryMT owns agent sessions and execution.

Exactly two MCP transports are supported:

- **stdio** (default): newline-delimited MCP JSON-RPC on stdin/stdout.
- **Streamable HTTP**: a single `/mcp` endpoint with JSON responses.

There is no legacy HTTP+SSE transport, `/sse` endpoint, or standalone event stream.
ACP events are collected inside each tool result, not published over SSE.

## Dependency Status

The GitHub `qmacp` dependency needs its new library API (`src/lib.rs` and
`AcpClient::connect_with_events`). The currently published revision
`f3f0a3c6138ef140ef098efec97e5583051c6544` is binary-only. The companion library
change is implemented in the neighboring `../qmtacp` checkout but is not pushed.
Until it is published, use this **local-only** Cargo override:

```toml
# .cargo/config.toml (gitignored)
[patch."https://github.com/querymt/qmacp"]
qmacp = { path = "../qmtacp" }
```

This override is already installed for this development checkout. Build and run
locally with `nix develop -c cargo build --release` and
`./target/release/qmcp`. A fresh Git-only checkout, CI, and the standalone Nix
package remain blocked until the qmacp library change is published. After
publication, remove the override and run `cargo update -p qmacp` to record the
Git revision in `Cargo.lock` before publishing qmcp. No code is fetched or
copied at build time outside Cargo's normal dependency handling.

## Install

The standalone install commands below require the qmacp library publication
and lockfile update described above.

From this checkout, prefer Nix when available:

```sh
nix profile install .
```

Otherwise use Cargo with the toolchain declared in `rust-toolchain.toml`:

```sh
cargo install --locked --path .
```

`qmcp --help` and `qmcp --version` do not connect to QueryMT.

## Start QueryMT

Run your configured QueryMT instance with ACP WebSockets enabled:

```sh
qmtcode --acp-ws
```

The default ACP endpoint is `ws://127.0.0.1:3030/acp/ws`.
`qmcp` connects lazily when a tool is called, so MCP initialization and tool
listing also work while the backend is offline. Backend failures become
MCP tool errors with `isError: true`.

## stdio

Configure an MCP client to launch `qmcp`:

```json
{
  "mcpServers": {
    "qmcp": {
      "command": "qmcp",
      "args": ["--url", "ws://127.0.0.1:3030/acp/ws"]
    }
  }
}
```

Use an absolute binary path if the client does not inherit your `PATH`.
All diagnostics go to stderr; stdout is reserved for MCP.
Do not pipe prompt text into stdin: send it as a tool's `text` argument.
Even `"text": "-"` is literal text, not an instruction to read stdin.

## Streamable HTTP

```sh
qmcp --transport http --bind 127.0.0.1:3031
```

Point the MCP client at `http://127.0.0.1:3031/mcp` using its **Streamable HTTP**
transport setting. MCP clients differ in how they configure HTTP servers;
this is not an SSE URL.

HTTP is stateless at the MCP layer. Agent session ids returned by QueryMT
remain valid across HTTP requests and `qmcp` restarts. The same tools and
arguments are available on both transports. GET event streams are disabled;
requests receive JSON responses rather than SSE framing.

The HTTP listener accepts loopback addresses only. Host validation prevents
DNS rebinding, and requests with a browser `Origin` are rejected. There is no
built-in HTTP authentication. For remote use, put an authenticated TLS reverse
proxy in front of the loopback listener, preserve a loopback upstream Host,
and authorize access before forwarding. This server can run agent tools with
the privileges of the configured QueryMT instance; never expose it openly.

## Options

```text
--transport stdio|http       MCP transport (default: stdio)
--url, -u ENDPOINT           ACP WebSocket URL or host[:port][/path]
--allow-insecure            Allow plaintext ws:// to non-loopback ACP hosts
--permission POLICY         allow-once (default), reject-once, or cancel
--bind ADDRESS              HTTP listener (default: 127.0.0.1:3031)
```

`QMCP_ACP_URL` sets the ACP endpoint; `--url` takes precedence.
`wss://` uses native trust roots through rustls. Remote plaintext ACP endpoints
require explicit `--allow-insecure`. `RUST_LOG=info` enables connection/listener
diagnostics on stderr.

The permission policy is fixed by the server operator, not by tool arguments.
The default `allow-once` permits agent tool execution when the ACP server asks
for approval. Permission selection follows qmacp: it prefers the requested
kind but may fall back to the first advertised option, including an
`allowAlways` option. `--permission cancel` always fails closed. ACP elicitation is cancelled
and reported as an event; interactive elicitation is not forwarded to MCP.

## Tools

Tool names use snake_case. Parameters and JSON schemas are advertised by
`tools/list`; unknown arguments are rejected. Discover profile/model/session
ids instead of inventing them.

| Tools | Purpose |
| --- | --- |
| `caps`, `profiles`, `models` | Discover protocol features, profiles, and models |
| `new`, `sessions`, `find` | Create, list, and search sessions |
| `status`, `inspect`, `runtime` | Read configuration, history, and live runtime |
| `get_lifecycle`, `set_lifecycle` | Read or explicitly patch resolved/archived state |
| `pin`, `unpin` | Set session pin state |
| `set_mode`, `set_model`, `set_effort` | Configure the session |
| `prompt`, `follow`, `watch` | Submit work, collect events, or poll until idle |
| `steer`, `queue`, `discard_queued` | Control active and queued input |
| `cancel`, `close` | Request turn cancellation or explicitly close a session |
| `exec` | Run a batch sequentially on one ACP connection |

Create a session and send work in one call:

```json
{"name":"prompt","arguments":{"new":true,"cwd":"/absolute/workspace","text":"Inspect the project and report findings","timeout":900}}
```

Continue an existing session using its returned `sessionId`:

```json
{"name":"prompt","arguments":{"session_id":"<returned-session-id>","cwd":"/absolute/workspace","text":"Run the focused tests","delivery":"auto","timeout":900}}
```

Exactly one of `new: true` or `session_id` is required for `prompt`.
`delivery: auto` prompts an idle session, steers a steerable turn, and otherwise
queues. `until_idle` defaults to true; set it to false to return after submission
for steer/queue delivery. Direct prompt delivery still waits for that ACP turn.
A queued/steered prompt includes its submission response in `submitted`,
including the generated `clientInputId`.

`prompt` defaults to a 600-second total operation deadline; `follow` and `watch`
default to 120 seconds. Timeouts must be 1..86400 seconds. Polling intervals
default to one second. `watch` with `idle: false` returns one observation.
Pass `cwd` for new/load/resume/inspect/status/prompt/follow operations; omitted
paths resolve against the `qmcp` process working directory. Remote ACP workspace
paths refer to the server, not the MCP client machine.

`sessions` and `find` accept `resolution: any|open|resolved`,
`archive: any|excluded|only`, and `pinned: true|false`. Omitted filters stay
omitted. Filters are sent on every page; lifecycle/pin operations fail explicitly
if the server does not advertise support. `set_lifecycle` requires at least one
of `resolved` or `archived`; explicit false values are preserved.

Batch calls use the same tool names and argument objects, not shell commands:

```json
{"name":"exec","arguments":{"calls":[{"name":"runtime","arguments":{"session_id":"<returned-session-id>"}},{"name":"get_lifecycle","arguments":{"session_id":"<returned-session-id>"}}]}}
```

All batch arguments are validated before execution. Batches contain 1..100
calls, do not allow nested `exec`, stop at the first runtime failure, and retain
prior results. They are not transactions; earlier effects are not rolled back.
A call cannot interpolate a session id created by an earlier batch entry.

## Results And Cancellation

Results contain the qmacp-style JSON payload in both MCP `structuredContent`
and a JSON text content block. Success has `ok: true`; failures have `ok: false`
and MCP `isError: true`. Error codes are 1 (arguments), 2 (connection), 3 (RPC),
and 4 (timeout/cancellation). `prompt` and `follow` finish with `type: done` and
assembled assistant `text`. `events` contains compact ACP session/text/tool/plan/
permission/input-state events; it retains at most 256 events and 1 MiB, with
`droppedEvents` reporting eviction. Assembled assistant text is capped at 1 MiB;
`textTruncated` reports truncation. This is a bounded history, not a live stream.

Each call owns an ACP connection, so a long prompt does not block a concurrent
`runtime`, `queue`, or `cancel` call. Disconnecting, cancelling the MCP request,
or reaching a timeout cleans up that connection but **does not automatically
cancel the remote turn**. Inspect `runtime` before retrying or cancelling.
The explicit `cancel` tool confirms the notification was written to the socket
(`cancellationRequested: true`), not that the remote turn has already stopped.
Only use `close` when the user explicitly wants to close the session.

## Development

The repository follows qmacp's Rust layout, stable toolchain, flake-parts,
rust-overlay devShell, lockfiles, and CI conventions.

```sh
nix develop -c cargo check
nix develop -c cargo fmt --all -- --check
nix develop -c cargo clippy --all-targets -- -D warnings
nix develop -c cargo test --all-targets
nix develop -c alejandra --check .
nix build
```

Integration tests use a mock ACP WebSocket server and disposable listeners;
they do not need providers, a live QueryMT instance, or a session database.

ACP transport and shared helpers live in qmacp, not in a vendored copy here.
The MCP-facing operations are adapted from qmacp; the original MIT license is
retained.
