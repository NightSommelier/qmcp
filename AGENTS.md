# AGENTS.md

## Working Rules

- Keep changes minimal and focused; avoid drive-by refactors and unrelated cleanup.
- Preserve staged, unstaged and untracked work. Do not stage, unstage, commit or
  push unless the user explicitly requests that action. Editing an already-staged
  file does not authorize updating the index.
- Keep this file limited to durable, repository-wide guidance, not task-specific
  corrections. Keep code, comments, documentation and diagnostics in English.
- Never access the user's session database or stop/restart existing QueryMT
  services. Use mock ACP endpoints or isolated instances with fresh test databases;
  stop only processes created for the current test.

## Repository Overview

- The project, crate and binary are named `qmcp` (lowercase).
- `src/main.rs` configures the server; `server.rs` exposes MCP tools and
  `commands.rs` implements their ACP operations using the qmacp client library.
- `transport.rs` handles stdio/Streamable HTTP; `oauth.rs` and `local_oauth.rs`
  handle external and embedded OAuth; `output.rs` assembles results and recovery.
- `tests/bridge.rs` exercises the MCP-to-ACP bridge with mocks. `README.md` and
  `docs/how-to-connect.md` cover user setup; avoid duplicating their content.

## Development Environment

- Detect Nix first. When available, use the project devShell (`nix develop` or
  `nix develop -c <command>`); an active project direnv shell is sufficient.
- Without Nix, use the supported native toolchain and dependencies. Check
  `flake.nix` and `rust-toolchain.toml`; diagnose setup failures rather than
  silently switching environments or toolchains.
- Run validation from the repository root inside the selected environment.

## Validation

- After Rust edits, run `cargo check -p qmcp` (include `--tests` for test edits).
  Run `cargo fmt` only after that check passes.
- Use focused red/green tests where applicable. Each added case must protect a
  distinct required behavior or real regression; prefer existing tests/fixtures.
- Do not test deleted functionality, mirror implementation constants/logic, or
  multiply equivalent input combinations. Keep scaffolding proportional; moving
  excess into another file is not simplification.
- Start with affected checks, then expand to consumers/features as needed. Consult
  `.github/workflows/ci.yml` for final CI commands:
  ```sh
  cargo check -p qmcp --tests
  cargo test -p qmcp --test bridge <test_filter>
  cargo fmt --all -- --check
  cargo clippy --all-targets --all-features -- -D warnings
  ```
- Tests must not connect to the user's running server. Load/resume can restore
  runtime state; do not treat them as harmless history reads.
- Report checks actually run, failures and remaining gaps. Documentation-only
  edits do not require a Rust build.

## Protocol and Code Conventions

- Support only stdio and Streamable HTTP. Keep stdout exclusively for MCP JSON-RPC
  in stdio mode; use stderr for diagnostics and never log credentials.
- Require authentication for non-loopback HTTP binds; preserve Host/Origin
  validation and the existing Bearer, external OAuth, and embedded OAuth modes.
- Existing sessions use backend cwd metadata by default. Do not require callers
  to repeat cwd or silently create/rebind sessions to change projects.
- Timeout, disconnect and MCP cancellation must not implicitly close sessions or
  cancel remote work. Retain submission/recovery information on failure.
- Follow surrounding code, rustfmt and existing error conventions.
- Do not add speculative compatibility layers. Flag API/protocol changes and
  affected consumers; ask before making compatibility decisions.
- Keep transport/client fixes in qmacp and MCP adaptation in qmcp. Do not expand
  work into QueryMT or `querymt-desktop` without authorization.
