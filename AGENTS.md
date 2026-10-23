# AGENTS.md

- Keep code, comments, documentation, and diagnostics in English.
- The project, crate, and binary are named `qmcp` (lowercase).
- Support only stdio and Streamable HTTP.
- Require static Bearer or external-provider OAuth authentication for non-loopback HTTP binds; preserve Host/Origin validation and never log credentials.
- Keep OAuth minimal: resource discovery and token introspection only; login, PKCE, consent and token issuance belong to the provider.
- Keep stdout exclusively for MCP JSON-RPC in stdio mode; diagnostics use stderr.
- Use the flake devShell on NixOS (`nix develop -c ...`) and the declared Rust toolchain.
- Run `cargo check`, formatting checks, Clippy, and relevant tests before finishing.
- Keep tests isolated: use mock ACP endpoints and never access the user's session database or restart existing QueryMT services.
- Timeout, disconnect, and MCP cancellation must not implicitly close sessions or cancel remote work.
- Preserve qmacp's MIT attribution when reusing its ACP implementation.
