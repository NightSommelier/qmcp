# qmcp

MCP server that lets an MCP client control a running QueryMT agent.
Built with Rust and [qmacp](https://github.com/querymt/qmacp), connecting to
QueryMT over ACP WebSockets. Supports **stdio** (default) and **Streamable HTTP**.

## Install

From a checkout, install with Nix:

```sh
nix profile install .
```

Or use Cargo with the toolchain in `rust-toolchain.toml`:

```sh
cargo install --locked --path .
```

## Connect

Run your configured QueryMT instance with ACP WebSockets enabled:

```sh
qmtcode --acp-ws
```

The default ACP endpoint is `ws://127.0.0.1:3030/acp/ws`.
Configure QueryMT and any required provider credentials; qmcp does not start it.

### stdio

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

Use an absolute binary path if your client cannot find `qmcp` on `PATH`.
Stdin/stdout are reserved for MCP; logs go to stderr.

### Streamable HTTP

```sh
qmcp --transport http
```

Connect your MCP client to `http://127.0.0.1:3031/mcp` using **Streamable HTTP**.
Responses are JSON; there is no legacy SSE endpoint.

HTTP is loopback-only and has no built-in authentication. For remote access,
use an authenticated TLS reverse proxy forwarding an allowed loopback Host.
Browser Origin headers are rejected. Do not expose the server openly: its
tools can execute work through your QueryMT instance.

Use `--url` or `QMCP_ACP_URL` to change the ACP endpoint, and `--bind` to change
the HTTP listen address. Prefer `wss://` for remote ACP connections; plaintext
connections to non-loopback hosts require `--allow-insecure`.
Run `qmcp --help` for all options. Set `RUST_LOG=info` for connection logs.

## Tools

The MCP client discovers the tools and their argument schemas automatically.
Start with `caps`, `profiles`, and `models` to discover available capabilities.

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

Use `prompt` with `new: true` to start a session, then its returned `sessionId`
as `session_id` to continue. Pass an explicit `cwd` for workspace operations;
paths refer to the QueryMT host.

## Important Behavior

- Tools can execute work through QueryMT. The default permission policy is
  `allow-once`, with a fallback to the first offered option (possibly `allowAlways`).
  Use `--permission cancel` to deny all approval requests. Interactive elicitation
  is not supported.
- Timeouts and MCP cancellation do **not** stop remote work. Use `cancel`, then
  verify with `runtime`. Stateless HTTP ignores separate MCP cancellation
  notifications; disconnecting is not a guarantee that the operation stopped.
- Failed calls may include `recovery` details. Check them and current runtime
  before retrying: prior effects are not rolled back, including in `exec` batches.
- Events and assistant text are returned with the result, not streamed live.
  Check `droppedEvents` and `textTruncated` for incomplete output.
- `find` scans at most 20 pages. Check `truncated` and `scanLimitReached` before
  treating its results as exhaustive.

## Development

```sh
nix develop -c cargo fmt --all -- --check
nix develop -c cargo clippy --locked --all-targets -- -D warnings
nix develop -c cargo test --locked --all-targets
nix build
```

Tests use mock ACP servers; no running QueryMT instance or provider is needed.
Licensed under [MIT](LICENSE); qmacp's original MIT attribution is retained.
