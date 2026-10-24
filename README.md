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

For a single-owner ChatGPT connection on macOS, qmcp includes OAuth login,
consent, public-client registration and PKCE. No external provider or Docker
service is required. Use an HTTPS reverse proxy and a generated owner credential
in a private `0600` file:

```sh
qmcp --transport http --permission cancel \
  --oauth-local-resource https://mcp.example.com/mcp \
  --oauth-owner-secret-file /absolute/private/path/to/owner-secret
```

These are example values, not a deployment configuration. See
[How to connect](docs/how-to-connect.md#embedded-oauth-on-macos-chatgpt) for secret
generation, proxy setup and ChatGPT sign-in. Embedded OAuth is single-owner and
volatile: restarting invalidates tokens **and registered client IDs**, so you may
need to re-add the server in ChatGPT. Registrations also expire after one hour;
tokens last 30 minutes, with no refresh tokens.

Local stdio, static Bearer over HTTPS, private tunnels, and optional external
OAuth introspection remain available in the [connection guide](docs/how-to-connect.md).

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
paths refer to the QueryMT host. `profiles`, `models`, lifecycle, and pin tools
return provider data under `result`, separate from bridge status and event metadata.

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
