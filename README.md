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

## Dependencies

The native ACP client comes from the published `qmacp` Git tag **`v0.1.1`**.
`Cargo.lock` records revision `5a8371446f49a559b57f106b2ca22c8d0fc75463`.
Cargo and Nix builds resolve that published source; no neighboring qmacp checkout,
local Cargo override, or separately installed qmacp executable is required.
The first build needs access to GitHub and the package registries unless the
dependencies are already cached.

## Install

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

The HTTP listener accepts loopback addresses only. Host validation accepts
`localhost`, `127.0.0.1`, `::1`, and the exact loopback IP selected with `--bind`
(for example `127.0.0.2` on Linux). Other hostnames/IPs are not allowed; selecting
one loopback alias does not allow the entire loopback range. Requests with a
browser `Origin` are rejected. Library callers with a custom bind should use
`transport::http_router_for_addr` for the same validation. There is no built-in
HTTP authentication. For remote use, put an authenticated TLS reverse proxy
in front of the loopback listener, preserve an allowed loopback upstream Host,
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
`tools/list`; unknown arguments are rejected. Schemas include session-selection
rules, explicit lifecycle patches, timeout/interval bounds, nonblank input text,
and batch size/entry constraints. Omitted optional fields and null values retain
their documented behavior, and explicit `false` lifecycle values are valid patches.
Discover profile/model/session ids instead of inventing them.

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

`find` scans at most **20 pages** or stops when it reaches the requested match
`limit`. Results include `scannedPages`, `scanLimitReached`, and `truncated`.
`scanLimitReached: true` means more backend pages exist after the safety cap;
`truncated: true` means part of the candidate list was not searched, whether due
to that cap or the match limit. An empty truncated result is not proof that no
matching session exists. `nextCursor` identifies the next page only when the
current page was fully consumed. If the match limit stops within a page, it is
null rather than a cursor that would skip unread entries. To inspect later
pages, pass a non-null `nextCursor` to `sessions` with the same `cwd`, lifecycle,
and pin filters, then apply the query/phase checks to those summaries. Otherwise
increase the match limit or restrict the server-side filters.

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

Failures after a partial operation include a `recovery` checkpoint separate
from the evictable event history. It records a known `sessionId`, its `cwd`,
and `sessionCreated` when creation succeeded. A queued/steered submission adds
`submission.clientInputId`, delivery, and state: `unconfirmed` means an identity
was generated but no reply was received; `acknowledged` includes the backend's
`acknowledgement` payload. An acknowledgement is not proof that a queued turn
has finished. Inspect the checkpoint and live runtime before retrying; remote
effects are not rolled back on a later error, timeout, or observed cancellation.

Each batch step carries its own `ok` and `textTruncated`; the batch-level
truncation flag is true if any step was truncated. A failed RPC step is exposed
as `failedResult` with its recovery data, alongside prior `results` and
`failedCall`. An ACP-cancelled prompt is retained as a failed step in `results`
and prevents later calls from running. Observed MCP cancellation also preserves
completed batch results and the active step's recovery checkpoint when a result
can still be delivered. Prompt/follow/watch deadlines include ACP connection
setup; batch step deadlines start when that step begins on the shared connection.

Each call owns an ACP connection, so a long prompt does not block a concurrent
`runtime`, `queue`, or `cancel` call. A completed operation, an operation deadline,
or cancellation observed by the active handler releases that local connection.
None of these paths **automatically cancel the remote turn**.

For stdio, the active MCP connection can deliver `notifications/cancelled` to
its in-flight request. Streamable HTTP is stateless: a separate POST containing
only `notifications/cancelled` receives 202 but is ignored by the current rmcp
transport; it does not cancel a handler running for an earlier POST. Aborting
the original HTTP POST is a different path, but clients must not assume it
guarantees immediate handler or ACP connection cleanup. A handler can continue
until its operation deadline or completion. A disconnected or cancelled client
also may not receive the final result or its recovery checkpoint.

To explicitly request remote cancellation, call the `cancel` tool separately
with the known `session_id`, then inspect `runtime` to verify the turn stopped.
The tool confirms the notification was written to the socket
(`cancellationRequested: true`), not that the remote turn has already stopped.
Before retrying any interrupted operation, inspect its recovery data if available
and current runtime rather than assuming no side effects occurred. Only use
`close` when the user explicitly wants to close the session.

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
