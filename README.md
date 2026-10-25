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
need to re-add the server in ChatGPT. Only unapproved registrations expire after
one hour; approved registrations survive until restart. Discovery advertises
PKCE authorization-code and rotating refresh-token grants. Access tokens last
30 minutes (capped at family expiry); clients registered for refresh receive
one-use refresh tokens with an absolute 24-hour family lifetime from the initial
code exchange, never extended by rotation. Replay revokes the family's access
and refresh tokens. Restart invalidates refresh state too; see the guide for
capacity limits and serialized-refresh requirements.

Local stdio, static Bearer over HTTPS, private tunnels, and optional external
OAuth introspection remain available in the [connection guide](docs/how-to-connect.md).

## Tools

The MCP client discovers the tools and their argument schemas automatically.
Start with `caps`, `profiles`, and `models` to discover available capabilities.

| Tools | Purpose |
| --- | --- |
| `caps`, `profiles`, `models` | Discover protocol features, profiles, and models |
| `workspace`, `new`, `sessions`, `find` | Select a workspace without prompting; create, list, and search sessions |
| `status`, `inspect`, `runtime` | Read configuration, history, and live runtime |
| `get_lifecycle`, `set_lifecycle` | Read or explicitly patch resolved/archived state |
| `pin`, `unpin` | Set session pin state |
| `set_mode`, `set_model`, `set_effort` | Configure the session |
| `prompt`, `follow`, `watch` | Submit work, collect events, or poll until idle |
| `steer`, `queue`, `discard_queued` | Control active and queued input |
| `cancel`, `close` | Request turn cancellation or explicitly close a session |
| `exec` | Run a batch sequentially on one ACP connection |

Select a workspace once, without sending a prompt, then reuse its returned ID:

```text
workspace({"cwd":"/srv/projects/app","profile":"my-profile"})
  -> {"sessionId":"A","cwd":"/srv/projects/app","created":true,"switched":false,...}
prompt({"session_id":"A","text":"Explain this project"})
status({"session_id":"A"})
workspace({"session_id":"A","cwd":"../other"})
  -> {"sessionId":"B","previousSessionId":"A","cwd":"/srv/projects/other","created":true,"switched":true,...}
prompt({"session_id":"B","text":"Work in the other project"})
```

`profile` is optional; discover actual IDs with `profiles`. A different directory
creates a **new QueryMT session**, not a rebind of the old one. Always use the
returned **new** `sessionId` as `session_id` in subsequent calls. The old ID keeps
its workspace and work; switching does not close, cancel, or alias it. There is
no global active chat workspace: subsequent calls still need an explicit ID.

`new` and `prompt` with `new: true` also remember cwd. Continuing `prompt` can
omit cwd; an explicit different cwd transparently creates a new session and
returns `previousSessionId`. `status`, `inspect`, and `follow` use remembered cwd
when omitted and reject a different cwd, directing you to `workspace` instead.
`profiles`, `models`, lifecycle, and pin tools return provider data under
`result`, separate from bridge status and event metadata.

### Workspace Rules

- Paths refer to the **QueryMT host**, not necessarily the qmcp host. Paths are
  normalized lexically (`.` and `..`), without filesystem checks, symlink
  resolution, or local canonicalization; the backend owns path validation.
  This is not a sandbox or directory allowlist.
- `workspace` without `session_id` requires an explicit initial cwd. Relative
  initial/new paths use qmcp's once-captured startup cwd; omitted cwd for `new`
  or `prompt(new=true)` preserves that startup-directory default. Prefer absolute
  backend paths, especially remotely. Relative paths with a selected ID use that
  session's remembered cwd. Only POSIX paths are supported; empty/control-character,
  Windows drive/UNC, and `~` paths are rejected. Home expansion is never guessed.
- Profiles explicitly selected at creation, or reported in authoritative list
  metadata, can be inherited on switches. Otherwise a new session uses the backend
  default, reported as `profileKnown:false` and `profileSource:"backend-default-unknown"`.
  Same-workspace profile changes are rejected (including explicit profiles when
  the old profile is unknown); create a workspace without an ID to choose one.
- Bindings are process-local, shared across server clones and HTTP requests, and
  immutable per ID. `close` retains the binding for later resume. The store holds
  at most 4096 IDs, with no eviction; capacity errors occur before session creation.
  No workspace lock is held across network requests or long prompts.
- After restart, or for an unknown ID, qmcp first discovers its authoritative cwd
  through unfiltered, paginated ACP `session/list`, without loading history.
  Discovery scans at most 20 pages and rejects repeated cursors. Missing sessions,
  unsupported/unavailable metadata, absent/non-absolute cwd, or incomplete scans
  fail closed, even if a caller supplies cwd. No guessed cwd or implicit creation.
  Metadata must accurately describe the backend's session; external backend changes
  and symlink equivalence cannot be detected from a cached lexical binding.
- Acknowledged new IDs and cwd are recorded before configuration/status/prompt
  steps. Later failure or cancellation includes recovery details. If a remote
  creation is never acknowledged, qmcp cannot know its ID; inspect backend metadata
  before retrying. Timeouts/disconnects never implicitly cancel remote work.

See [the connection guide](docs/how-to-connect.md#select-a-session-workspace)
for the complete selection workflow.

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
- Queue/steer delivery skips session history. On `prompt`, `until_idle: false`
  returns after submission when either is selected; idle prompt delivery still waits.
- A receipt confirms acceptance, not task success. Check `observationState`;
  invalid receipts fail and keep submission recovery unconfirmed.
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
