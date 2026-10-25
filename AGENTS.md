# AGENTS.md

## Working Rules

- Keep changes focused and keep code, comments, documentation, and diagnostics in English.
- Preserve staged, unstaged, and untracked work. Do not stage, unstage, commit, or push unless the user explicitly requests it.
- Keep this file limited to durable, repository-wide guidance.
- Never access the user's session database or stop/restart existing QueryMT services. Use mock ACP endpoints or isolated instances with fresh test databases; stop only processes created for the current test.

## Repository Overview

- The project, crate, and binary are named `qmcp` (lowercase).
- `src/main.rs` configures the server; `server.rs` exposes MCP tools and `commands.rs` implements ACP operations using qmacp.
- `transport.rs` handles stdio/Streamable HTTP; `oauth.rs` handles external-provider introspection; `local_oauth.rs` isolates embedded owner login, consent, PKCE, and volatile token issuance; `output.rs` assembles tool results, bounded events, and recovery information.
- `tests/bridge.rs` exercises the MCP-to-ACP bridge with mocks. `README.md` and `docs/how-to-connect.md` cover user setup; avoid duplicating their content.

## Development Environment

- Detect Nix first. When available, use the project devShell (`nix develop` or `nix develop -c <command>`); an active project direnv shell is sufficient.
- Without Nix, use the supported native toolchain and dependencies. Check `flake.nix` and `rust-toolchain.toml`; diagnose setup failures rather than silently switching environments or toolchains.
- Run validation from the repository root inside the selected environment.

## Validation

- After Rust edits, run `cargo check -p qmcp --tests`. Run formatting checks after the check passes.
- Use focused red/green tests where applicable. Each added case should protect a distinct required behavior or real regression; prefer existing tests and fixtures.
- Do not test deleted functionality, mirror implementation constants/logic, or multiply equivalent input combinations. Keep scaffolding proportional.
- Consult `.github/workflows/ci.yml` for project CI. Before finishing, run formatting checks, `cargo check --locked --all-targets`, `cargo test --locked --all-targets`, and `cargo clippy --locked --all-targets -- -D warnings` when applicable.
- Tests must not connect to the user's running server. Loading/resuming can restore runtime state; do not treat those operations as harmless history reads.
- Report checks actually run, failures, and remaining gaps. Documentation-only edits do not require a Rust build.

## Protocol and Code Conventions

- Support only stdio and Streamable HTTP. Keep stdout exclusively for MCP JSON-RPC in stdio mode; use stderr for diagnostics and never log credentials.
- Require static Bearer, embedded single-owner OAuth, or external-provider OAuth authentication for non-loopback HTTP binds. Preserve Host/Origin validation.
- Keep external-provider introspection in `oauth.rs`; keep embedded owner login, consent, PKCE, and volatile token issuance in `local_oauth.rs`. Never silently authorize or accept human-chosen owner passwords.
- Embedded OAuth is single-owner; keep its secret file protected, state bounded and volatile, consent explicit, and token material hashed at rest. Do not introduce persistence or multi-owner behavior without authorization.
- Timeout, disconnect, and MCP cancellation must not implicitly close sessions or cancel remote work. Retain submission/recovery information on failure.
- Preserve qmacp's MIT attribution when reusing its ACP implementation. Keep transport/client fixes in qmacp and MCP adaptation in qmcp; do not expand work into QueryMT or `querymt-desktop` without authorization.
- Follow surrounding code, rustfmt, and existing error conventions. Flag protocol/API changes and affected consumers.
